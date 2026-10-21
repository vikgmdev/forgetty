//! Protocol v2 server: ALPN `forgetty/rpc/2` (PROTOCOL_V2, AD-018).
//!
//! One [`RpcV2`] per server. Each accepted connection runs [`RpcV2::serve`]:
//!
//! - **Handshake** (`Hello` / `HandshakeReply`), authorization against the
//!   device registry, `proto_mismatch` with finish plus a 2 s wait.
//! - **Control stream**: `Request` / `Response` / `Failure`, `Ping` / `Pong`,
//!   `Event`. Methods that exist on the local socket run through the same
//!   `forgetty_socket::dispatch` table, so there is one copy of each handler.
//!   Only the methods that need a connection (`attach_pane`, `detach_pane`,
//!   `subscribe_events`, `resize_pane`, `list_clients`) live here.
//! - **Input stream**: frames `[pane uuid][seq][bytes]`, exactly-once per
//!   `(client_id, input_epoch)`.
//! - **Pane output streams**: one uni stream per attached pane. The stream
//!   pulls from the pane byte log by absolute offset, so a slow reader waits
//!   on QUIC flow control and never loses bytes (section 8).
//! - **Size policy** (AD-020): the PTY follows the latest active client.
//!
//! Nothing here polls. Every wait is a `Notify`, a channel or a stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use forgetty_core::PaneId;
use forgetty_proto::{
    cap, close_code, error_code, from_value, method, read_input_frame, read_msg, to_value,
    write_msg, write_output_frame, AttachPaneParams, ClientEntry, ClientKind, ControlMsg,
    DetachPaneParams, ErrorMsg, HandshakeReply, Hello, OkResult, PaneStreamHeader,
    ResizePaneParams, ServerEvent, SubscribeResult, Value, Welcome, PROTO_MAJOR, PROTO_MINOR,
};
use forgetty_pty::PtySize;
use forgetty_session::{
    read_disk, LogPlan, LogShared, PaneTreeLayout, SessionEvent, SessionManager,
};
use forgetty_socket::protocol::{self, methods, Request};
use forgetty_sync::registry::{iso8601_now, DeviceRegistry};
use forgetty_sync::SyncEndpoint;
use iroh::endpoint::Connection;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Application close code of [`RpcV2::drop_connections`] (test harness).
pub const DROP_CODE: u32 = 0x10;
/// Application error code a pane stream is reset with when its reader fell
/// behind the whole log. The client redials and resumes with a reset.
pub const LOG_LOST_CODE: u32 = 0x11;

/// Bytes per pane output frame.
const CHUNK: usize = 128 * 1024;
/// How long a silent peer may hold the handshake open.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Input `seq` state is kept this long after a client leaves (section 5).
const INPUT_KEEP: Duration = Duration::from_secs(600);
/// Events buffered per connection before it is told to resync.
const EVENT_BUFFER: usize = 1024;
/// Messages queued for one connection's Control stream.
const CONTROL_QUEUE: usize = 1024;
/// A connection whose Control queue stays (half) full this long with no write
/// completing is closed. The client
/// reconnects and gets the state again.
const STALL_LIMIT: Duration = Duration::from_secs(10);
/// At most one lag resync (`LayoutChanged`) per connection in this time.
const RESYNC_MIN: Duration = Duration::from_millis(250);
/// Application close code for a connection whose Control queue stalled.
pub const STALLED_CODE: u32 = 0x12;

/// Capabilities the server offers.
pub const SERVER_CAPS: [&str; 5] =
    [cap::RESUME, cap::LAYOUT_EVENTS, cap::STATUS_EVENTS, cap::CAPTURE, cap::SIZE_POLICY_LATEST];

/// Methods a v2 client may call through the shared socket dispatch table.
/// `send_input` is not here (input goes on the Input stream), nor are the
/// `pair_*` methods (a paired device does not open pairing windows).
const DISPATCH_ALLOWED: [&str; 26] = [
    methods::LIST_TABS,
    methods::GET_LAYOUT,
    methods::NEW_TAB,
    methods::CLOSE_TAB,
    methods::FOCUS_TAB,
    methods::SET_ACTIVE_WORKSPACE,
    methods::SPLIT_PANE,
    methods::MOVE_TAB,
    methods::GET_PANE_INFO,
    methods::SEND_SIGINT,
    methods::NOTIFY,
    methods::CLOSE_PANE,
    methods::CREATE_WORKSPACE,
    methods::RENAME_WORKSPACE,
    methods::DELETE_WORKSPACE,
    methods::DUPLICATE_WORKSPACE,
    methods::SET_WORKSPACE_COLOR,
    methods::MOVE_WORKSPACE,
    methods::SET_ACTIVE_PANE,
    methods::UPDATE_SPLIT_RATIOS,
    methods::SET_PINNED,
    methods::GET_PINNED,
    methods::SERVER_INFO,
    methods::PANE_EXIT_STATUS,
    methods::LIST_DEVICES,
    methods::REVOKE_DEVICE,
];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Last input `seq` written for one `(client_id, input_epoch)`.
#[derive(Default)]
struct InputRec {
    last: u64,
}

/// One connected client, by `client_id` (several connections can overlap
/// briefly during a reconnect).
struct ClientRec {
    kind: ClientKind,
    name: String,
    connected_at: String,
    conns: usize,
    /// Bumped each time the last connection leaves, so the 10 minute cleanup
    /// of a client that came back does not run.
    left_gen: u64,
}

/// Per pane size state (AD-020).
#[derive(Default)]
struct PaneSizes {
    /// The latest active client for this pane.
    active: Option<Uuid>,
    /// The last size each client asked for.
    by_client: HashMap<Uuid, (u16, u16)>,
}

/// Counters and switches for the protocol harness. Cheap, always compiled.
pub struct Qa {
    /// Input frames dropped as duplicates.
    pub dedup_drops: AtomicU64,
    /// While set, `Pong.input_acked` stays 0.
    pub hold_acks: AtomicBool,
    /// Completed handshakes.
    pub handshakes: AtomicU64,
    /// Handshakes answered `server_stopping`.
    pub stopping_refusals: AtomicU64,
    /// While true, handshakes stall before the reply.
    pub paused: watch::Sender<bool>,
    /// `(pane, from_offset)` of every `attach_pane`.
    pub attaches: Mutex<Vec<(Uuid, Option<u64>)>>,
    /// Test only: skip the registry check.
    #[cfg(feature = "qa-tools")]
    pub open_access: AtomicBool,
    /// Test only: what each pane received as input.
    #[cfg(feature = "qa-tools")]
    pub input_log: Mutex<HashMap<Uuid, Vec<u8>>>,
}

/// The v2 server.
pub struct RpcV2 {
    sm: Arc<SessionManager>,
    sync: OnceLock<Arc<SyncEndpoint>>,
    server_id: Uuid,
    hostname: String,
    events: broadcast::Sender<(u64, ServerEvent)>,
    event_seq: Mutex<u64>,
    inputs: Mutex<HashMap<(Uuid, Uuid), InputRec>>,
    clients: Mutex<HashMap<Uuid, ClientRec>>,
    sizes: Mutex<HashMap<Uuid, PaneSizes>>,
    live: Mutex<Vec<(u64, Connection)>>,
    next_conn: AtomicU64,
    /// Set while the server shuts down: input is no longer written, so the
    /// saved `seq` state matches what reached the PTYs.
    inputs_frozen: AtomicBool,
    /// Harness counters.
    pub qa: Qa,
}

impl RpcV2 {
    /// Create the v2 server and start its event pump. Call inside a runtime.
    pub fn new(sm: Arc<SessionManager>, server_id: Uuid) -> Arc<Self> {
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "forgetty".to_string());
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let (paused, _) = watch::channel(false);
        let rpc = Arc::new(RpcV2 {
            sm,
            sync: OnceLock::new(),
            server_id,
            hostname,
            events,
            event_seq: Mutex::new(0),
            inputs: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            sizes: Mutex::new(HashMap::new()),
            live: Mutex::new(Vec::new()),
            next_conn: AtomicU64::new(0),
            inputs_frozen: AtomicBool::new(false),
            qa: Qa {
                dedup_drops: AtomicU64::new(0),
                hold_acks: AtomicBool::new(false),
                handshakes: AtomicU64::new(0),
                stopping_refusals: AtomicU64::new(0),
                paused,
                attaches: Mutex::new(Vec::new()),
                #[cfg(feature = "qa-tools")]
                open_access: AtomicBool::new(false),
                #[cfg(feature = "qa-tools")]
                input_log: Mutex::new(HashMap::new()),
            },
        });
        rpc.load_inputs();
        tokio::spawn(event_pump(rpc.clone()));
        let saver = rpc.clone();
        forgetty_socket::add_shutdown_hook(Box::new(move || saver.save_inputs()));
        rpc
    }

    // -----------------------------------------------------------------------
    // Input state across a restart
    // -----------------------------------------------------------------------

    fn inputs_path() -> std::path::PathBuf {
        forgetty_core::platform::data_dir().join("input_seq.json")
    }

    /// Save the last written input `seq` per `(client_id, input_epoch)`.
    ///
    /// Section 5 keeps this state in memory. A restarted server would forget it,
    /// the client would resend its unacked frames (up to one `Ping` interval of
    /// typing), and a new shell would run them a second time. A graceful stop
    /// therefore writes the table, and the next start reads it back.
    pub fn save_inputs(&self) {
        self.inputs_frozen.store(true, Ordering::SeqCst);
        let entries: Vec<serde_json::Value> = lock(&self.inputs)
            .iter()
            .map(
                |((c, e), r)| serde_json::json!({"client_id": c, "input_epoch": e, "last": r.last}),
            )
            .collect();
        let saved_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let doc = serde_json::json!({"saved_at": saved_at, "entries": entries});
        let path = Self::inputs_path();
        if let Err(e) =
            forgetty_core::fsutil::write_private_atomic(&path, doc.to_string().as_bytes())
        {
            warn!("rpc2: could not save the input state to {}: {e}", path.display());
        }
    }

    /// Read the table saved by [`Self::save_inputs`]. The file is consumed, and
    /// entries older than the 10 minute keep time are dropped.
    fn load_inputs(self: &Arc<Self>) {
        let path = Self::inputs_path();
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let _ = std::fs::remove_file(&path);
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else { return };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let saved_at = doc["saved_at"].as_u64().unwrap_or(0);
        if now.saturating_sub(saved_at) > INPUT_KEEP.as_secs() {
            return;
        }
        let mut loaded = 0;
        {
            let mut inputs = lock(&self.inputs);
            for e in doc["entries"].as_array().into_iter().flatten() {
                let (Some(c), Some(ep), Some(last)) = (
                    e["client_id"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
                    e["input_epoch"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
                    e["last"].as_u64(),
                ) else {
                    continue;
                };
                inputs.insert((c, ep), InputRec { last });
                loaded += 1;
            }
        }
        if loaded > 0 {
            info!("rpc2: restored the input state of {loaded} client epochs");
            // Entries of clients that never come back go after the keep time.
            let rpc = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(INPUT_KEEP).await;
                let live: std::collections::HashSet<Uuid> = lock(&rpc.clients)
                    .iter()
                    .filter(|(_, r)| r.conns > 0)
                    .map(|(c, _)| *c)
                    .collect();
                lock(&rpc.inputs).retain(|(c, _), _| live.contains(c));
            });
        }
    }

    /// Give the server the sync endpoint (device list, revoke). Set once,
    /// after the endpoint is bound.
    pub fn set_sync(&self, sync: Arc<SyncEndpoint>) {
        let _ = self.sync.set(sync);
    }

    /// The handler to register for ALPN `forgetty/rpc/2`.
    pub fn alpn_handler(self: &Arc<Self>) -> forgetty_sync::AlpnHandler {
        let rpc = self.clone();
        Arc::new(move |conn, registry| {
            let rpc = rpc.clone();
            tokio::spawn(async move { rpc.serve(conn, registry).await });
        })
    }

    /// Close every v2 connection with [`DROP_CODE`] (test harness).
    pub fn drop_connections(&self) {
        let conns: Vec<Connection> = lock(&self.live).drain(..).map(|(_, c)| c).collect();
        for c in conns {
            c.close(DROP_CODE.into(), b"test drop");
        }
    }

    /// Number of v2 connections that finished the handshake and are open.
    pub fn connection_count(&self) -> usize {
        lock(&self.live).len()
    }

    /// Send an event to every subscribed client.
    pub fn emit(&self, event: ServerEvent) {
        let mut seq = lock(&self.event_seq);
        *seq += 1;
        let _ = self.events.send((*seq, event));
    }

    fn subscribe(&self) -> (u64, broadcast::Receiver<(u64, ServerEvent)>) {
        let seq = lock(&self.event_seq);
        (*seq, self.events.subscribe())
    }

    fn current_seq(&self) -> u64 {
        *lock(&self.event_seq)
    }

    /// `get_layout` as a MessagePack value.
    pub(crate) fn layout_value(&self) -> Value {
        let req = Request {
            jsonrpc: "2.0".into(),
            method: methods::GET_LAYOUT.into(),
            params: serde_json::Value::Null,
            id: None,
        };
        let resp = forgetty_socket::dispatch(
            &req,
            self.sm.clone(),
            self.sync.get().cloned(),
            Some(self.server_id),
        );
        json_to_mp(&resp.result.unwrap_or(serde_json::Value::Null))
    }

    // -----------------------------------------------------------------------
    // Connection
    // -----------------------------------------------------------------------

    /// Serve one connection until it ends.
    pub async fn serve(self: Arc<Self>, conn: Connection, registry: Arc<Mutex<DeviceRegistry>>) {
        let remote = conn.remote_id();
        let device_id = remote.to_string();

        // Authorization (section 1): unknown node, close code 0x01.
        let allowed = lock(&registry).is_authorized(&remote) || self.open_access();
        if !allowed {
            warn!("rpc2: refused unauthorized device {device_id}");
            conn.close(close_code::NOT_AUTHORIZED.into(), b"not_authorized");
            return;
        }

        // Handshake (section 3).
        let hs = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let (send, mut recv) = conn.accept_bi().await.ok()?;
            let hello: Hello = read_msg(&mut recv).await.ok().flatten()?;
            Some((send, recv, hello))
        })
        .await;
        let Ok(Some((mut send, recv, hello))) = hs else {
            conn.close(close_code::OK.into(), b"bad hello");
            return;
        };

        if hello.proto_major != PROTO_MAJOR {
            let reply = HandshakeReply::Error(ErrorMsg {
                code: error_code::PROTO_MISMATCH.into(),
                message: format!("server speaks protocol {PROTO_MAJOR}"),
            });
            let _ = write_msg(&mut send, &reply).await;
            let _ = send.finish();
            // Let the client read the error before the connection goes away.
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            conn.close(close_code::OK.into(), b"proto_mismatch");
            return;
        }

        // Harness switch: stall before the reply, as if the server were unreachable.
        let mut paused = self.qa.paused.subscribe();
        if paused.wait_for(|p| !*p).await.is_err() {
            return;
        }

        // AD-017: this connection is an attached client until it ends. The
        // guard drops on every exit path. A stopping server takes no new
        // clients: answer `server_stopping`, which the client retries.
        let kind = hello.client_kind;
        let Ok(_guard) = self.sm.clients().register(
            hello.client_id.to_string(),
            kind.as_str(),
            &hello.client_name,
        ) else {
            info!("rpc2: client {} refused, server is stopping", hello.client_name);
            self.qa.stopping_refusals.fetch_add(1, Ordering::SeqCst);
            let reply = HandshakeReply::Error(ErrorMsg {
                code: error_code::SERVER_STOPPING.into(),
                message: "the server is stopping".into(),
            });
            let _ = write_msg(&mut send, &reply).await;
            let _ = send.finish();
            // Let the client read the error before the connection goes away.
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            conn.close(close_code::OK.into(), b"server-stopping");
            return;
        };

        let caps: Vec<String> = SERVER_CAPS
            .iter()
            .filter(|c| hello.capabilities.iter().any(|h| h == *c))
            .map(|c| c.to_string())
            .collect();
        let welcome = Welcome {
            proto_major: PROTO_MAJOR,
            proto_minor: PROTO_MINOR,
            server_id: self.server_id,
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            hostname: self.hostname.clone(),
            capabilities: SERVER_CAPS.iter().map(|c| c.to_string()).collect(),
        };
        if write_msg(&mut send, &HandshakeReply::Welcome(welcome)).await.is_err() {
            return;
        }
        self.qa.handshakes.fetch_add(1, Ordering::SeqCst);
        {
            let registry = registry.clone();
            let id = device_id.clone();
            tokio::task::spawn_blocking(move || {
                let _ = lock(&registry).update_last_seen(&id);
            });
        }

        let conn_key = self.next_conn.fetch_add(1, Ordering::Relaxed);
        lock(&self.live).push((conn_key, conn.clone()));
        self.client_joined(&hello);
        info!(
            client = %hello.client_name, kind = kind.as_str(), id = %hello.client_id,
            "rpc2: client connected"
        );

        Session::new(self.clone(), conn.clone(), &hello, caps).run(send, recv).await;

        lock(&self.live).retain(|(k, _)| *k != conn_key);
        self.client_left(&hello);
        conn.close(close_code::OK.into(), b"bye");
        info!(client = %hello.client_name, "rpc2: client disconnected");
    }

    fn open_access(&self) -> bool {
        #[cfg(feature = "qa-tools")]
        {
            self.qa.open_access.load(Ordering::SeqCst)
        }
        #[cfg(not(feature = "qa-tools"))]
        {
            false
        }
    }

    fn client_joined(&self, hello: &Hello) {
        let first = {
            let mut clients = lock(&self.clients);
            let rec = clients.entry(hello.client_id).or_insert_with(|| ClientRec {
                kind: hello.client_kind,
                name: hello.client_name.clone(),
                connected_at: iso8601_now(),
                conns: 0,
                left_gen: 0,
            });
            rec.conns += 1;
            rec.left_gen += 1;
            (rec.conns == 1).then(|| rec.connected_at.clone())
        };
        if let Some(connected_at) = first {
            self.emit(ServerEvent::ClientConnected {
                client_id: hello.client_id,
                client_kind: hello.client_kind,
                client_name: hello.client_name.clone(),
                connected_at,
            });
        }
    }

    fn client_left(self: &Arc<Self>, hello: &Hello) {
        let gen = {
            let mut clients = lock(&self.clients);
            let Some(rec) = clients.get_mut(&hello.client_id) else { return };
            rec.conns = rec.conns.saturating_sub(1);
            if rec.conns > 0 {
                return;
            }
            rec.left_gen += 1;
            rec.left_gen
        };
        self.emit(ServerEvent::ClientDisconnected { client_id: hello.client_id });
        // Sizes asked for by a client that left no longer count.
        {
            let mut sizes = lock(&self.sizes);
            for ps in sizes.values_mut() {
                ps.by_client.remove(&hello.client_id);
                if ps.active == Some(hello.client_id) {
                    ps.active = None;
                }
            }
        }
        // Input `seq` state outlives the client by 10 minutes (section 5). One
        // deadline per departure, not a poll.
        let rpc = self.clone();
        let client_id = hello.client_id;
        tokio::spawn(async move {
            tokio::time::sleep(INPUT_KEEP).await;
            let gone = {
                let mut clients = lock(&rpc.clients);
                match clients.get(&client_id) {
                    Some(rec) if rec.conns == 0 && rec.left_gen == gen => {
                        clients.remove(&client_id);
                        true
                    }
                    Some(_) => false,
                    None => true,
                }
            };
            if gone {
                lock(&rpc.inputs).retain(|(c, _), _| *c != client_id);
            }
        });
    }

    // -----------------------------------------------------------------------
    // Input (section 5)
    // -----------------------------------------------------------------------

    /// Write one input frame to its PTY unless it was written before.
    /// Dedup and write happen under one lock, so a frame resent on a new
    /// connection can never overtake or double an earlier one.
    fn handle_input(
        &self,
        client: Uuid,
        epoch: Uuid,
        pane: Uuid,
        seq: u64,
        bytes: &[u8],
        policy: bool,
    ) {
        {
            let mut inputs = lock(&self.inputs);
            if self.inputs_frozen.load(Ordering::SeqCst) {
                return; // shutting down: not written, so not acked, so resent
            }
            let rec = inputs.entry((client, epoch)).or_default();
            if seq <= rec.last {
                self.qa.dedup_drops.fetch_add(1, Ordering::SeqCst);
                return;
            }
            rec.last = seq;
            match self.sm.write_pty(PaneId(pane), bytes) {
                Ok(()) => {}
                Err(e) => debug!("rpc2: input for pane {pane}: {e}"),
            }
            #[cfg(feature = "qa-tools")]
            lock(&self.qa.input_log).entry(pane).or_default().extend_from_slice(bytes);
        }
        if policy {
            self.touch_active(client, pane);
        }
    }

    fn acked(&self, client: Uuid, epoch: Uuid) -> u64 {
        if self.qa.hold_acks.load(Ordering::SeqCst) {
            return 0;
        }
        lock(&self.inputs).get(&(client, epoch)).map(|r| r.last).unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // Size policy (section 6, AD-020)
    // -----------------------------------------------------------------------

    /// `client` is now the latest active client of `pane`: the PTY takes the
    /// size it asked for.
    fn touch_active(&self, client: Uuid, pane: Uuid) {
        let apply = {
            let mut sizes = lock(&self.sizes);
            let ps = sizes.entry(pane).or_default();
            if ps.active == Some(client) {
                return;
            }
            ps.active = Some(client);
            ps.by_client.get(&client).copied()
        };
        if let Some((rows, cols)) = apply {
            self.apply_size(pane, rows, cols);
        }
    }

    fn apply_size(&self, pane: Uuid, rows: u16, cols: u16) {
        let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
        if let Err(e) = self.sm.resize_pane(PaneId(pane), size) {
            debug!("rpc2: resize pane {pane}: {e}");
        }
    }

    fn resize(&self, client: Uuid, p: &ResizePaneParams, policy: bool) {
        let apply = if policy {
            let mut sizes = lock(&self.sizes);
            let ps = sizes.entry(p.pane_id).or_default();
            ps.by_client.insert(client, (p.rows, p.cols));
            match ps.active {
                None => {
                    ps.active = Some(client);
                    true
                }
                Some(a) => a == client,
            }
        } else {
            true
        };
        if apply {
            self.apply_size(p.pane_id, p.rows, p.cols);
        }
    }

    // -----------------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------------

    /// `list_clients`: the v2 clients, shaped like `ClientEntry`.
    fn list_clients(&self) -> Result<Value, (&'static str, String)> {
        let mut entries: Vec<ClientEntry> = lock(&self.clients)
            .iter()
            .filter(|(_, r)| r.conns > 0)
            .map(|(id, r)| ClientEntry {
                client_id: *id,
                client_kind: r.kind,
                client_name: r.name.clone(),
                connected_at: r.connected_at.clone(),
            })
            .collect();
        entries.sort_by(|a, b| {
            a.connected_at.cmp(&b.connected_at).then(a.client_id.cmp(&b.client_id))
        });
        let list: Vec<Value> = entries.iter().map(|e| to_value(e).unwrap_or(Value::Nil)).collect();
        Ok(Value::Array(list))
    }

    /// Run a socket method through the shared dispatch table.
    fn dispatch_socket(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Value, (&'static str, String)> {
        if !DISPATCH_ALLOWED.contains(&method) && method != methods::CAPTURE {
            return Err((error_code::UNKNOWN_METHOD, format!("unknown method {method}")));
        }
        let req = Request {
            jsonrpc: "2.0".into(),
            method: method.to_string(),
            params: mp_to_json(params),
            id: Some(serde_json::json!(1)),
        };
        let resp = forgetty_socket::dispatch(
            &req,
            self.sm.clone(),
            self.sync.get().cloned(),
            Some(self.server_id),
        );
        if let Some(err) = resp.error {
            return Err(map_error(err.code, err.message));
        }
        Ok(json_to_mp(&resp.result.unwrap_or(serde_json::Value::Null)))
    }

    /// `focus_tab` marks the client active in every pane of that tab.
    fn focus_panes(&self, client: Uuid, params: &Value) {
        let Some(tab) = param_uuid(params, "tab_id") else { return };
        let layout = self.sm.layout();
        for ws in &layout.workspaces {
            if let Some(t) = ws.tabs.iter().find(|t| t.id == tab) {
                let mut panes = Vec::new();
                collect_panes(&t.pane_tree, &mut panes);
                for pane in panes {
                    self.touch_active(client, pane.0);
                }
            }
        }
    }
}

fn collect_panes(tree: &PaneTreeLayout, out: &mut Vec<PaneId>) {
    match tree {
        PaneTreeLayout::Leaf { pane_id } => out.push(*pane_id),
        PaneTreeLayout::Split { first, second, .. } => {
            collect_panes(first, out);
            collect_panes(second, out);
        }
    }
}

fn param_uuid(params: &Value, key: &str) -> Option<Uuid> {
    params
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))?
        .1
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
}

fn map_error(code: i32, message: String) -> (&'static str, String) {
    let c = match code {
        protocol::METHOD_NOT_FOUND => error_code::UNKNOWN_METHOD,
        protocol::INVALID_PARAMS if message.contains("not found") => error_code::NOT_FOUND,
        protocol::INVALID_PARAMS | protocol::INVALID_REQUEST | protocol::PARSE_ERROR => {
            error_code::BAD_REQUEST
        }
        _ => error_code::INTERNAL,
    };
    (c, message)
}

// ---------------------------------------------------------------------------
// MessagePack <-> JSON
// ---------------------------------------------------------------------------

/// MessagePack value to the JSON the socket handlers read. Binary becomes
/// base64, the form the socket API uses.
pub fn mp_to_json(v: &Value) -> serde_json::Value {
    use base64::Engine as _;
    match v {
        Value::Nil => serde_json::Value::Null,
        Value::Boolean(b) => (*b).into(),
        Value::Integer(i) => {
            if let Some(u) = i.as_u64() {
                u.into()
            } else if let Some(s) = i.as_i64() {
                s.into()
            } else {
                serde_json::Value::Null
            }
        }
        Value::F32(f) => serde_json::Number::from_f64(f64::from(*f))
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::F64(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => s.as_str().unwrap_or_default().into(),
        Value::Binary(b) => base64::engine::general_purpose::STANDARD.encode(b).into(),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(mp_to_json).collect()),
        Value::Map(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let key = match k {
                        Value::String(s) => s.as_str().unwrap_or_default().to_string(),
                        other => other.to_string(),
                    };
                    (key, mp_to_json(v))
                })
                .collect(),
        ),
        Value::Ext(..) => serde_json::Value::Null,
    }
}

/// JSON handler result to a MessagePack value.
pub fn json_to_mp(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Value::from(u)
            } else if let Some(i) = n.as_i64() {
                Value::from(i)
            } else {
                Value::F64(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::String(s.as_str().into()),
        serde_json::Value::Array(a) => Value::Array(a.iter().map(json_to_mp).collect()),
        serde_json::Value::Object(o) => Value::Map(
            o.iter().map(|(k, v)| (Value::String(k.as_str().into()), json_to_mp(v))).collect(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Event pump
// ---------------------------------------------------------------------------

/// Turns session events into protocol events. One task per server, on the
/// metadata channel, which never carries PTY bytes and so does not lag under
/// output load. Layout changes that arrive together become one `LayoutChanged`.
async fn event_pump(rpc: Arc<RpcV2>) {
    let mut rx = rpc.sm.subscribe_meta();
    loop {
        let mut layout_dirty = false;
        match rx.recv().await {
            Ok(ev) => pump_event(&rpc, ev, &mut layout_dirty),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("rpc2: event pump lagged by {n}, sending the layout again");
                layout_dirty = true;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
        loop {
            match rx.try_recv() {
                Ok(ev) => pump_event(&rpc, ev, &mut layout_dirty),
                Err(broadcast::error::TryRecvError::Lagged(_)) => layout_dirty = true,
                Err(_) => break,
            }
        }
        if layout_dirty {
            rpc.emit(ServerEvent::LayoutChanged { layout: rpc.layout_value() });
        }
    }
}

fn pump_event(rpc: &RpcV2, ev: SessionEvent, layout_dirty: &mut bool) {
    match ev {
        SessionEvent::TitleChanged { pane_id, title } => {
            rpc.emit(ServerEvent::TitleChanged { pane_id: pane_id.0, title })
        }
        SessionEvent::CwdChanged { pane_id, cwd } => {
            rpc.emit(ServerEvent::CwdChanged { pane_id: pane_id.0, cwd })
        }
        SessionEvent::StatusChanged { pane_id, status, message } => {
            rpc.emit(ServerEvent::StatusChanged {
                pane_id: pane_id.0,
                status,
                message: message.unwrap_or_default(),
            })
        }
        SessionEvent::Notification { pane_id, title, body } => {
            rpc.emit(ServerEvent::Notification {
                pane_id: pane_id.0,
                title: title.unwrap_or_default(),
                body,
            })
        }
        SessionEvent::PaneExited { pane_id, exit_code } => {
            rpc.emit(ServerEvent::PaneExited { pane_id: pane_id.0, exit_code })
        }
        SessionEvent::PaneResized { pane_id, rows, cols } => {
            rpc.emit(ServerEvent::PaneResized { pane_id: pane_id.0, rows, cols })
        }
        SessionEvent::PaneClosed { pane_id } => {
            lock(&rpc.sizes).remove(&pane_id.0);
            rpc.emit(ServerEvent::PaneClosed { pane_id: pane_id.0 });
            *layout_dirty = true;
        }
        SessionEvent::PaneCreated { .. }
        | SessionEvent::TabCreated { .. }
        | SessionEvent::TabClosed { .. }
        | SessionEvent::PaneSplit { .. }
        | SessionEvent::TabMoved { .. }
        | SessionEvent::ActiveTabChanged { .. }
        | SessionEvent::ActiveWorkspaceChanged { .. }
        | SessionEvent::WorkspaceCreated { .. }
        | SessionEvent::WorkspaceRenamed { .. }
        | SessionEvent::WorkspaceDeleted { .. }
        | SessionEvent::WorkspaceColorChanged { .. }
        | SessionEvent::WorkspacesReordered { .. }
        | SessionEvent::ActivePaneChanged { .. } => *layout_dirty = true,
        SessionEvent::PtyOutput { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Session: one connection after the handshake
// ---------------------------------------------------------------------------

struct Session {
    rpc: Arc<RpcV2>,
    conn: Connection,
    client_id: Uuid,
    epoch: Uuid,
    /// Capabilities both sides listed.
    caps: Vec<String>,
}

impl Session {
    fn new(rpc: Arc<RpcV2>, conn: Connection, hello: &Hello, caps: Vec<String>) -> Self {
        Session { rpc, conn, client_id: hello.client_id, epoch: hello.input_epoch, caps }
    }

    fn has(&self, c: &str) -> bool {
        self.caps.iter().any(|x| x == c)
    }

    async fn run(self, mut send: iroh::endpoint::SendStream, mut recv: iroh::endpoint::RecvStream) {
        let session = Arc::new(self);
        let (out_tx, mut out_rx) = mpsc::channel::<ControlMsg>(CONTROL_QUEUE);
        let mut tasks: JoinSet<()> = JoinSet::new();

        // Control writer. `written` counts completed writes, so a slow reader
        // can be told from one that reads nothing.
        let written = Arc::new(AtomicU64::new(0));
        {
            let written = written.clone();
            tasks.spawn(async move {
                while let Some(msg) = out_rx.recv().await {
                    if write_msg(&mut send, &msg).await.is_err() {
                        break;
                    }
                    written.fetch_add(1, Ordering::Relaxed);
                }
            });
        }

        // Input reader: the client's first uni stream.
        {
            let s = session.clone();
            tasks.spawn(async move {
                let Ok(mut input) = s.conn.accept_uni().await else { return };
                let policy = s.has(cap::SIZE_POLICY_LATEST);
                while let Ok(Some((pane, seq, bytes))) = read_input_frame(&mut input).await {
                    let rpc = s.rpc.clone();
                    let (client, epoch) = (s.client_id, s.epoch);
                    // The PTY write can block; keep it off the async threads.
                    // Awaited here, so the frames of one client stay in order.
                    let _ = tokio::task::spawn_blocking(move || {
                        rpc.handle_input(client, epoch, pane, seq, &bytes, policy)
                    })
                    .await;
                }
            });
        }

        // Control request loop.
        let mut streams: HashMap<Uuid, AbortHandle> = HashMap::new();
        let mut events: Option<broadcast::Receiver<(u64, ServerEvent)>> = None;
        // When the Control queue became (half) full with no write completing
        // since, and the write count then.
        let mut full_since: Option<(tokio::time::Instant, u64)> = None;
        // A lag resync is owed at this time (coalesced, section 7).
        let mut resync_due: Option<tokio::time::Instant> = None;
        let mut last_resync: Option<tokio::time::Instant> = None;
        loop {
            let stalled = async {
                match full_since {
                    Some((t, _)) => tokio::time::sleep_until(t + STALL_LIMIT).await,
                    None => std::future::pending().await,
                }
            };
            let resync = async {
                match resync_due {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                msg = read_msg::<_, ControlMsg>(&mut recv) => {
                    let msg = match msg {
                        Ok(Some(m)) => m,
                        Ok(None) => break,
                        Err(e) => {
                            // A frame this server cannot decode is skipped, a
                            // broken stream ends the session (section 3a).
                            if matches!(
                                e,
                                forgetty_proto::FrameError::Io(_)
                                    | forgetty_proto::FrameError::UnexpectedEof
                                    | forgetty_proto::FrameError::TooLarge { .. }
                            ) {
                                if matches!(e, forgetty_proto::FrameError::TooLarge { .. }) {
                                    session.conn.close(close_code::FRAME_TOO_LARGE.into(), b"frame_too_large");
                                }
                                debug!("rpc2: control stream ended: {e}");
                                break;
                            }
                            warn!("rpc2: skipped an undecodable control frame: {e}");
                            continue;
                        }
                    };
                    match msg {
                        ControlMsg::Ping { nonce } => {
                            let input_acked = session.rpc.acked(session.client_id, session.epoch);
                            // A Pong is never dropped: wait for room, up to the stall limit.
                            if !push(&out_tx, &session.conn, ControlMsg::Pong { nonce, input_acked }).await {
                                break;
                            }
                        }
                        ControlMsg::Request { id, method: m, params } => {
                            session.on_request(
                                id, m, params, &out_tx, &mut tasks, &mut streams, &mut events,
                            );
                        }
                        _ => {} // Response, Failure, Event, Pong come from the server only.
                    }
                }
                // Take the next event only when the queue has room for it. A
                // client that does not read leaves the events in the hub's ring
                // (it lags, it does not grow a queue).
                (permit, ev) = async {
                    let permit = out_tx.reserve().await;
                    (permit, next_event(&mut events).await)
                } => {
                    let Ok(permit) = permit else { break };
                    match ev {
                        Ok((seq, event)) => {
                            if !session.event_allowed(&event) {
                                // not for this peer
                            } else if matches!(event, ServerEvent::LayoutChanged { .. })
                                && out_tx.capacity() < CONTROL_QUEUE / 2
                            {
                                // A layout is a full snapshot: with the queue
                                // half full, one later resync replaces any
                                // number of these.
                                drop(permit);
                                let now = tokio::time::Instant::now();
                                let due = last_resync.map_or(now, |t| (t + RESYNC_MIN).max(now));
                                resync_due = Some(resync_due.map_or(due, |d| d.min(due)));
                            } else {
                                permit.send(ControlMsg::Event { seq, event });
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            drop(permit);
                            warn!("rpc2: client {} lagged by {n} events", session.client_id);
                            // Skip to the head and owe one resync, at most one
                            // per RESYNC_MIN however many lags happen.
                            let (_, rx) = session.rpc.subscribe();
                            events = Some(rx);
                            let now = tokio::time::Instant::now();
                            let due = last_resync.map_or(now, |t| (t + RESYNC_MIN).max(now));
                            resync_due = Some(resync_due.map_or(due, |d| d.min(due)));
                        }
                        Err(broadcast::error::RecvError::Closed) => events = None,
                    }
                }
                _ = resync => {
                    resync_due = None;
                    if session.has(cap::LAYOUT_EVENTS) {
                        let seq = session.rpc.current_seq();
                        let event = ServerEvent::LayoutChanged { layout: session.rpc.layout_value() };
                        match out_tx.try_send(ControlMsg::Event { seq, event }) {
                            Ok(()) => last_resync = Some(tokio::time::Instant::now()),
                            // No room: try again in a moment.
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                resync_due = Some(tokio::time::Instant::now() + RESYNC_MIN);
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                }
                _ = stalled => {
                    let now_written = written.load(Ordering::Relaxed);
                    if full_since.is_some_and(|(_, w)| w != now_written) {
                        // Slow, not stalled: the writer moved. Measure again.
                        full_since = Some((tokio::time::Instant::now(), now_written));
                        continue;
                    }
                    warn!(
                        "rpc2: client {} did not read Control for {:?}, closing",
                        session.client_id, STALL_LIMIT
                    );
                    session.conn.close(STALLED_CODE.into(), b"control-stalled");
                    break;
                }
            }
            // "Full" is the half mark: past it layouts are coalesced, so the
            // queue does not get fuller while the writer is stuck.
            let now_written = written.load(Ordering::Relaxed);
            full_since = if out_tx.capacity() < CONTROL_QUEUE / 2 {
                match full_since {
                    Some((_, w)) if w == now_written => full_since,
                    _ => Some((tokio::time::Instant::now(), now_written)),
                }
            } else {
                None
            };
        }
        for (_, h) in streams {
            h.abort();
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    /// Queue a message from the request loop without waiting. When the queue
    /// is full the message waits on its own task: an answer is never dropped.
    fn send_now(&self, out: &mpsc::Sender<ControlMsg>, tasks: &mut JoinSet<()>, msg: ControlMsg) {
        match out.try_send(msg) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(msg)) => {
                let (out, conn) = (out.clone(), self.conn.clone());
                tasks.spawn(async move {
                    push(&out, &conn, msg).await;
                });
            }
        }
    }

    fn event_allowed(&self, event: &ServerEvent) -> bool {
        match event {
            ServerEvent::LayoutChanged { .. } => self.has(cap::LAYOUT_EVENTS),
            ServerEvent::StatusChanged { .. }
            | ServerEvent::Notification { .. }
            | ServerEvent::PaneExited { .. } => self.has(cap::STATUS_EVENTS),
            _ => true,
        }
    }

    /// Handle one `Request`. The cheap, connection-local methods answer here.
    /// Everything else runs on its own task, so `Ping` is never held up.
    #[allow(clippy::too_many_arguments)]
    fn on_request(
        self: &Arc<Self>,
        id: u64,
        m: String,
        params: Value,
        out: &mpsc::Sender<ControlMsg>,
        tasks: &mut JoinSet<()>,
        streams: &mut HashMap<Uuid, AbortHandle>,
        events: &mut Option<broadcast::Receiver<(u64, ServerEvent)>>,
    ) {
        let bad = |e: forgetty_proto::FrameError| (error_code::BAD_REQUEST, e.to_string());
        let reply = move |r: Result<Value, (&'static str, String)>| match r {
            Ok(result) => ControlMsg::Response { id, result },
            Err((code, message)) => ControlMsg::Failure { id, code: code.into(), message },
        };
        match m.as_str() {
            method::SUBSCRIBE_EVENTS => {
                let seq = if events.is_some() {
                    self.rpc.current_seq()
                } else {
                    let (seq, rx) = self.rpc.subscribe();
                    *events = Some(rx);
                    seq
                };
                self.send_now(out, tasks, reply(to_value(&SubscribeResult { seq }).map_err(bad)));
            }
            method::ATTACH_PANE => {
                let r = from_value::<AttachPaneParams>(params)
                    .map_err(bad)
                    .and_then(|p| self.attach(p, tasks, streams));
                self.send_now(out, tasks, reply(r));
            }
            method::DETACH_PANE => {
                let r = from_value::<DetachPaneParams>(params).map_err(bad).and_then(|p| {
                    if let Some(h) = streams.remove(&p.pane_id) {
                        h.abort();
                    }
                    to_value(&OkResult { ok: true }).map_err(bad)
                });
                self.send_now(out, tasks, reply(r));
            }
            method::RESIZE_PANE => {
                let s = self.clone();
                let out = out.clone();
                tasks.spawn(async move {
                    let r = from_value::<ResizePaneParams>(params).map_err(bad).and_then(|p| {
                        if p.rows == 0 || p.cols == 0 {
                            return Err((
                                error_code::BAD_REQUEST,
                                "rows and cols must be above 0".into(),
                            ));
                        }
                        if s.rpc.sm.pane_info(PaneId(p.pane_id)).is_none() {
                            return Err((error_code::NOT_FOUND, "no such pane".into()));
                        }
                        Ok(p)
                    });
                    let r = match r {
                        Ok(p) => {
                            let rpc = s.rpc.clone();
                            let client = s.client_id;
                            let policy = s.has(cap::SIZE_POLICY_LATEST);
                            let _ =
                                tokio::task::spawn_blocking(move || rpc.resize(client, &p, policy))
                                    .await;
                            to_value(&OkResult { ok: true }).map_err(bad)
                        }
                        Err(e) => Err(e),
                    };
                    push(&out, &s.conn, reply(r)).await;
                });
            }
            method::LIST_CLIENTS => {
                self.send_now(out, tasks, reply(self.rpc.list_clients()));
            }
            other => {
                if other == method::CAPTURE && !self.has(cap::CAPTURE) {
                    self.send_now(
                        out,
                        tasks,
                        ControlMsg::Failure {
                            id,
                            code: error_code::CAPABILITY_MISSING.into(),
                            message: "capture needs the capture capability".into(),
                        },
                    );
                    return;
                }
                let s = self.clone();
                let out = out.clone();
                let other = other.to_string();
                tasks.spawn(async move {
                    let rpc = s.rpc.clone();
                    let (client, name, p) = (s.client_id, other.clone(), params);
                    let r = tokio::task::spawn_blocking(move || {
                        let r = rpc.dispatch_socket(&name, &p);
                        if r.is_ok() && name == methods::FOCUS_TAB {
                            rpc.focus_panes(client, &p);
                        }
                        r
                    })
                    .await
                    .unwrap_or_else(|e| {
                        Err((error_code::INTERNAL, format!("handler failed: {e}")))
                    });
                    push(&out, &s.conn, reply(r)).await;
                });
            }
        }
    }

    /// `attach_pane`: pick the start offset (section 8) and open the stream.
    fn attach(
        self: &Arc<Self>,
        p: AttachPaneParams,
        tasks: &mut JoinSet<()>,
        streams: &mut HashMap<Uuid, AbortHandle>,
    ) -> Result<Value, (&'static str, String)> {
        let pane = PaneId(p.pane_id);
        let Some(info) = self.rpc.sm.pane_info(pane) else {
            return Err((error_code::NOT_FOUND, "no such pane".into()));
        };
        let (Some((first, total)), Some(shared)) =
            (self.rpc.sm.log_bounds(pane), self.rpc.sm.log_shared(pane))
        else {
            return Err((error_code::INTERNAL, "pane has no byte log".into()));
        };
        let from = if self.has(cap::RESUME) { p.from_offset } else { None };
        let (start, reset) = match from {
            None => (first, true),
            Some(o) if o > total => (first, true),
            Some(o) if o < first => (first, true),
            Some(o) => (o, false),
        };
        lock(&self.rpc.qa.attaches).push((p.pane_id, p.from_offset));
        if let Some(old) = streams.remove(&p.pane_id) {
            old.abort();
        }
        let header = PaneStreamHeader {
            pane_id: p.pane_id,
            start_offset: start,
            reset,
            rows: info.rows,
            cols: info.cols,
        };
        let handle = tasks.spawn(pane_stream(self.rpc.clone(), self.conn.clone(), shared, header));
        streams.insert(p.pane_id, handle);
        to_value(&OkResult { ok: true }).map_err(|e| (error_code::INTERNAL, e.to_string()))
    }
}

/// Queue a message that must not be dropped (a response or a Pong). Waits for
/// room. A connection that leaves the queue full for [`STALL_LIMIT`] is closed
/// and `false` is returned.
async fn push(out: &mpsc::Sender<ControlMsg>, conn: &Connection, msg: ControlMsg) -> bool {
    match tokio::time::timeout(STALL_LIMIT, out.send(msg)).await {
        Ok(Ok(())) => true,
        Ok(Err(_)) => false,
        Err(_) => {
            conn.close(STALLED_CODE.into(), b"control-stalled");
            false
        }
    }
}

async fn next_event(
    rx: &mut Option<broadcast::Receiver<(u64, ServerEvent)>>,
) -> Result<(u64, ServerEvent), broadcast::error::RecvError> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

// ---------------------------------------------------------------------------
// Pane output stream
// ---------------------------------------------------------------------------

/// One Pane output stream: the header, then raw frames pulled from the pane
/// byte log by offset. Back-pressure from QUIC slows this task only. The log
/// keeps filling, and the task catches up from the ring or the disk file.
async fn pane_stream(
    rpc: Arc<RpcV2>,
    conn: Connection,
    shared: Arc<LogShared>,
    header: PaneStreamHeader,
) {
    let pane = PaneId(header.pane_id);
    let Ok(mut send) = conn.open_uni().await else { return };
    if write_msg(&mut send, &header).await.is_err() {
        return;
    }
    let mut next = header.start_offset;
    // While this stream lives, the PTY is not read more than half a ring ahead
    // of it, so it is served from the ring and is not reset.
    let reader = shared.register_reader(next);
    loop {
        // Arm both wake-ups before looking at the log, so nothing is missed.
        let wake = shared.wake.notified();
        let progress = shared.disk_progress.notified();
        tokio::pin!(wake, progress);
        wake.as_mut().enable();
        progress.as_mut().enable();

        match rpc.sm.plan_log(pane, next, CHUNK) {
            None => {
                // The pane closed: finish cleanly (FIN).
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
                return;
            }
            Some(LogPlan::Caught) => wake.await,
            Some(LogPlan::Ring(bytes)) => {
                if write_output_frame(&mut send, &bytes).await.is_err() {
                    return;
                }
                next += bytes.len() as u64;
                reader.advance(next);
            }
            Some(LogPlan::Disk { disk, len }) => match read_disk(&shared, disk, next, len).await {
                Ok(Some(bytes)) if !bytes.is_empty() => {
                    if write_output_frame(&mut send, &bytes).await.is_err() {
                        return;
                    }
                    next += bytes.len() as u64;
                    reader.advance(next);
                }
                // The file has not caught up yet: wait for the appender.
                Ok(Some(_)) => progress.await,
                // The file moved under the read: wait for the appender to
                // publish the new extent, then plan again.
                Ok(None) => progress.await,
                Err(e) => {
                    warn!("rpc2: pane {pane} log read failed: {e}");
                    let _ = send.reset(LOG_LOST_CODE.into());
                    return;
                }
            },
            Some(LogPlan::Lost { first }) => {
                // The reader fell behind everything the log still holds. The
                // stream cannot say "reset" in the middle, so it breaks and the
                // client resumes with a reset (section 8).
                warn!("rpc2: pane {pane} reader at {next} is behind the log (starts at {first})");
                let _ = send.reset(LOG_LOST_CODE.into());
                return;
            }
        }
    }
}
