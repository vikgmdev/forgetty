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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use crate::control_queue::ControlQueue;
use crate::snapshot_hub::SnapshotHub;
use forgetty_core::PaneId;
use forgetty_proto::{
    cap, close_code, encode_msg, error_code, from_value, method, read_input_frame,
    read_input_frame_flags, read_msg, stream_code, to_value, write_frame, write_msg,
    write_output_frame, AttachPaneParams, ClientEntry, ClientKind, ControlMsg, DetachPaneParams,
    ErrorMsg, HandshakeReply, Hello, OkResult, PaneStreamHeader, ResizePaneParams, ServerEvent,
    SubscribeResult, Value, Welcome, INPUT_FLAG_AUTO, PROTO_MAJOR, PROTO_MINOR,
};
use forgetty_pty::PtySize;
use forgetty_session::{
    read_disk, InputOutcome, LogPlan, LogShared, PaneTreeLayout, SessionEvent, SessionManager,
    SizeGovernor, SizeOwner,
};
use forgetty_socket::protocol::{self, methods, Request};
use forgetty_sync::registry::{iso8601_from_secs, iso8601_now, DeviceRegistry};
use forgetty_sync::SyncEndpoint;
use iroh::endpoint::Connection;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Application close code of [`RpcV2::drop_connections`] (test harness).
pub const DROP_CODE: u32 = 0x10;
/// Application error code a pane stream is reset with when its reader fell
/// behind the whole log and the peer has no `snapshot_catchup`. The client
/// redials and resumes with a reset.
pub const LOG_LOST_CODE: u32 = stream_code::LOG_LOST;

/// One line of the connect timeline (`RUST_LOG=timeline=debug`): what a
/// connection did, in milliseconds since the server accepted it.
macro_rules! tl {
    ($born:expr, $who:expr, $($arg:tt)*) => {
        debug!(
            target: "timeline",
            "server {} +{}ms {}",
            $who,
            $born.elapsed().as_millis(),
            format_args!($($arg)*)
        )
    };
}

/// Bytes per pane output frame.
const CHUNK: usize = 128 * 1024;
/// How long a silent peer may hold the handshake open.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a session looks for lost input to report.
const INPUT_LOSS_POLL: Duration = Duration::from_millis(200);
/// Graceful stop waits this long for the pane input queues to drain (AD-024).
const INPUT_DRAIN_LIMIT: Duration = Duration::from_secs(2);
/// Input `seq` state is kept this long after a client leaves (section 5).
const INPUT_KEEP: Duration = Duration::from_secs(600);
/// Events buffered per connection before it is told to resync.
const EVENT_BUFFER: usize = 256;
/// Memory one connection's outgoing Control queue may hold. Past it the
/// connection stops reading requests (AD-023 amendment, section 7).
const CONTROL_QUEUE_BYTES: usize = 8 * 1024 * 1024;
/// Requests a connection may have running at once. One more closes it.
const MAX_INFLIGHT: usize = 64;

/// A `detach_pane` is answered after this long at most, whatever the size
/// policy or the session manager is doing.
const DETACH_ANSWER_BOUND: Duration = Duration::from_millis(250);

/// Marks of detached panes one connection keeps before it drops the old ones,
/// and how many requests old a mark must be to go. A mark of a pane that has
/// an attach in flight is never dropped, however old: that attach may be
/// stuck on a lock, and the mark is what stops it from starting a stream the
/// client has stopped. Marks of other panes are bookkeeping for requests that
/// finished, and a peer cannot grow them past [`MARKS_KEPT`] plus the panes
/// with an attach in flight (at most [`MAX_INFLIGHT`]).
const MARKS_KEPT: usize = 256;
const MARK_AGE: u64 = 128;
/// A connection whose Control queue stays (half) full this long without
/// draining at least [`MIN_DRAIN_RATE`] is closed. The client reconnects and
/// gets the state again.
const STALL_LIMIT: Duration = Duration::from_secs(10);
/// Bytes per second a connection with a (half) full Control queue must take
/// off it. A client that reads one small frame now and then does not count.
const MIN_DRAIN_RATE: u64 = 32 * 1024;
/// Bytes set aside against the Control budget for each request that runs on
/// its own task, until its answer is queued. Answers are small except
/// `capture`.
const ANSWER_RESERVE: usize = 64 * 1024;
/// The same for `capture`, whose answer can fill a whole frame.
const CAPTURE_RESERVE: usize = forgetty_proto::MAX_FRAME_LEN;
/// Attach records the harness keeps (the oldest are dropped).
const QA_ATTACHES_KEPT: usize = 1024;
/// Longest `Hello.client_name` kept, in characters.
const MAX_CLIENT_NAME_CHARS: usize = 64;
/// Connections one device may hold. A new one past this closes the oldest.
const MAX_CONNS_PER_DEVICE: usize = 4;
/// Application close code for the oldest connection of a device that opened
/// more than [`MAX_CONNS_PER_DEVICE`].
pub const TOO_MANY_CONNECTIONS_CODE: u32 = 0x15;
/// At most one lag resync (`LayoutChanged`) per connection in this time.
const RESYNC_MIN: Duration = Duration::from_millis(250);
/// Application close code for a connection whose Control queue stalled.
pub const STALLED_CODE: u32 = 0x12;
/// Application close code for a connection with more than [`MAX_INFLIGHT`]
/// requests in flight.
pub const TOO_MANY_REQUESTS_CODE: u32 = 0x14;
/// A pane stream whose write makes no progress this long gets a snapshot
/// catch-up (AD-023).
const NO_PROGRESS: Duration = Duration::from_secs(2);
/// One stream takes a snapshot catch-up at most this often. A stream that needs
/// them again soon waits twice as long each time, up to [`MAX_CATCHUP_BACKOFF`].
const MIN_CATCHUP_INTERVAL: Duration = Duration::from_secs(1);
/// Longest wait between two catch-ups of one stream.
const MAX_CATCHUP_BACKOFF: Duration = Duration::from_secs(10);
/// Spacing of catch-ups for a stream that keeps pace with the program (it
/// took live bytes at [`KEEPING_PACE_RATE`] since its last snapshot, or the
/// snapshot was late by the build time alone): no backoff, only this floor.
const KEEPING_PACE_INTERVAL: Duration = Duration::from_millis(100);
/// Live bytes a cycle must have delivered to prove that the peer reads. A
/// peer that never reads still takes one 128 KiB chunk (or two) into whatever
/// room its QUIC window has left, and that is 1 GB/s when the cycle lasts
/// 100 us. A peer that reads takes more than the room that was left.
const KEEPING_PACE_MIN_LIVE: u64 = 4 * CHUNK as u64;

/// What a catch-up says about its peer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Claim {
    /// The write stalled, or the peer reads too slowly to tell.
    Stalled,
    /// The snapshot went out quickly and the log ran past it before a live
    /// byte: the build was too slow for the program, not the peer.
    Weak,
    /// One or two chunks in a few microseconds: a good rate on paper, and what
    /// a peer that never reads gets into the room its QUIC window has left.
    /// Only [`KEEPING_PACE_TINY_BURST`] of these in a row are taken at face
    /// value.
    Tiny,
    /// The peer took live bytes at a good rate: it reads.
    Strong,
}

/// Tiny claims in a row, without a strong one between, that still count as
/// keeping pace.
const KEEPING_PACE_TINY_BURST: u32 = 4;
/// Live bytes per second since its last snapshot that put a stream among the
/// peers that read (a 100 KB/s link does not, a loaded localhost reader does).
const KEEPING_PACE_RATE: f64 = 1024.0 * 1024.0;
/// A snapshot that took longer than this to reach the peer says nothing
/// about the program being fast: the peer is slow.
const SNAPSHOT_SEND_OK: Duration = Duration::from_millis(500);

/// Capabilities the server offers.
pub const SERVER_CAPS: [&str; 11] = [
    cap::RESUME,
    cap::LAYOUT_EVENTS,
    cap::STATUS_EVENTS,
    cap::CAPTURE,
    cap::SIZE_POLICY_LATEST,
    cap::SNAPSHOT_CATCHUP,
    cap::INPUT_FLAGS,
    cap::SIZE_OWNER,
    cap::SPLIT_BEFORE,
    cap::WORKSPACE_ID,
    cap::SHELL_STATE,
];

/// Methods a v2 client may call through the shared socket dispatch table.
/// `send_input` is not here (input goes on the Input stream), nor are the
/// `pair_*` methods (a paired device does not open pairing windows).
const DISPATCH_ALLOWED: [&str; 29] = [
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
    methods::API_LIST,
    methods::API_DESCRIBE,
    methods::SET_LABEL,
];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Input state of one `(client_id, input_epoch)`.
#[derive(Default)]
struct InputRec {
    /// Last `seq` that entered its pane's queue (or was dropped by a full one):
    /// what `Pong.input_acked` says, and what a resend is checked against.
    last: AtomicU64,
    /// Held across dedup and the enqueue, so frames of one client epoch stay
    /// in order and are queued once, even on overlapping connections.
    gate: tokio::sync::Mutex<()>,
    /// Panes where a frame of this client was dropped (AD-024). Later frames
    /// for that pane are dropped too until its queue is empty, so the program
    /// never sees input with a hole in the middle.
    holes: Mutex<HashMap<Uuid, Hole>>,
}

/// Input of one client dropped for one pane.
struct Hole {
    /// Bytes dropped so far.
    bytes: u64,
    /// The pane queue is empty again: input flows, the loss is due to be
    /// reported to the client.
    closed: bool,
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
    /// Clients with a live stream on this pane, in the order they attached.
    attached: Vec<Uuid>,
    /// When each client last sent `user` input here (a tick of `user_tick`).
    last_user: HashMap<Uuid, u64>,
}

impl PaneSizes {
    /// `client` leaves the pane (AD-025). `forget_size` is set when it leaves
    /// the server, not only the pane. If it was the owner, the attached
    /// client with the most recent user input takes over (the earliest
    /// attached one when nobody has typed). Returns the size to apply when
    /// the owner changed to a client that asked for one.
    fn leave(&mut self, client: Uuid, forget_size: bool) -> Option<(u16, u16)> {
        self.attached.retain(|c| *c != client);
        if forget_size {
            self.by_client.remove(&client);
            self.last_user.remove(&client);
        }
        if self.active != Some(client) {
            return None;
        }
        let mut best: Option<(Uuid, u64)> = None;
        for c in &self.attached {
            let tick = self.last_user.get(c).copied().unwrap_or(0);
            if best.is_none_or(|(_, t)| tick > t) {
                best = Some((*c, tick));
            }
        }
        self.active = best.map(|(c, _)| c);
        self.active.and_then(|c| self.by_client.get(&c).copied())
    }

    /// May `client`'s `auto` input reach the PTY? Only the owner's may. A
    /// pane without an owner takes its first attached client as owner.
    fn auto_allowed(&self, client: Uuid) -> bool {
        self.owner() == Some(client)
    }

    /// The size owner. The latest active client owns the pane while it has a
    /// stream on it. One that does not (it focused a tab it never attached)
    /// hands the pane to the first attached client. `None` when nobody has a
    /// stream.
    fn owner(&self) -> Option<Uuid> {
        match self.active {
            Some(owner) if self.attached.contains(&owner) => Some(owner),
            _ => self.attached.first().copied(),
        }
    }
}

/// Counters and switches for the protocol harness. Cheap, always compiled.
pub struct Qa {
    /// Input frames dropped as duplicates.
    pub dedup_drops: AtomicU64,
    /// While set, `Pong.input_acked` stays 0.
    pub hold_acks: AtomicBool,
    /// Milliseconds an `attach_pane` sleeps in its pane lookups (stands for a
    /// lock a busy pane holds).
    pub attach_delay_ms: AtomicU64,
    /// Completed handshakes.
    pub handshakes: AtomicU64,
    /// Handshakes answered `server_stopping`.
    pub stopping_refusals: AtomicU64,
    /// While true, handshakes stall before the reply.
    pub paused: watch::Sender<bool>,
    /// `(pane, from_offset)` of every `attach_pane`.
    pub attaches: Mutex<Vec<(Uuid, Option<u64>)>>,
    /// Snapshot catch-ups served.
    pub catchups: AtomicU64,
    /// Input frames dropped because the pane's queue was full (AD-024).
    pub input_drops: AtomicU64,
    /// `auto` input frames dropped because their sender is not the size owner
    /// (AD-025).
    pub auto_drops: AtomicU64,
    /// Connections closed for [`MAX_INFLIGHT`].
    pub too_many_closes: AtomicU64,
    /// Size-policy syncs started by `detach_pane` (one per pane at a time).
    pub sync_jobs: AtomicU64,
    /// Connections closed for [`STALL_LIMIT`].
    pub stall_closes: AtomicU64,
    /// Test only: `(pane, n)`. Each change makes the streams of `pane` take a
    /// snapshot catch-up as if their reader had stalled.
    pub force_catchup: watch::Sender<(Uuid, u64)>,
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
    /// New for every run of this process (`Welcome.boot_id`).
    boot_id: Uuid,
    hostname: String,
    events: broadcast::Sender<(u64, ServerEvent)>,
    event_seq: Mutex<u64>,
    inputs: Mutex<HashMap<(Uuid, Uuid), Arc<InputRec>>>,
    /// Catch-up pacing per `(remote node id, pane)`, kept across attaches and
    /// connections: a peer cannot reset its backoff by attaching again.
    pacing: Mutex<HashMap<(iroh::EndpointId, Uuid), Pace>>,
    clients: Mutex<HashMap<Uuid, ClientRec>>,
    /// Lock order: `sizes`, then `clients`, then the session manager's `inner`.
    sizes: Mutex<HashMap<Uuid, PaneSizes>>,
    /// Orders `user` input across clients, for the owner hand over (AD-025).
    user_tick: AtomicU64,
    live: Mutex<Vec<(u64, Connection)>>,
    next_conn: AtomicU64,
    /// Set while the server shuts down: input is no longer written, so the
    /// saved `seq` state matches what reached the PTYs.
    inputs_frozen: AtomicBool,
    /// Snapshot builds shared by every stream (AD-023).
    pub snapshots: Arc<SnapshotHub>,
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
        let (force_catchup, _) = watch::channel((Uuid::nil(), 0));
        let rpc = Arc::new(RpcV2 {
            sm,
            sync: OnceLock::new(),
            server_id,
            boot_id: Uuid::new_v4(),
            hostname,
            events,
            event_seq: Mutex::new(0),
            inputs: Mutex::new(HashMap::new()),
            pacing: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            sizes: Mutex::new(HashMap::new()),
            user_tick: AtomicU64::new(0),
            live: Mutex::new(Vec::new()),
            next_conn: AtomicU64::new(0),
            inputs_frozen: AtomicBool::new(false),
            snapshots: Arc::new(SnapshotHub::default()),
            qa: Qa {
                dedup_drops: AtomicU64::new(0),
                hold_acks: AtomicBool::new(false),
                attach_delay_ms: AtomicU64::new(0),
                handshakes: AtomicU64::new(0),
                stopping_refusals: AtomicU64::new(0),
                paused,
                attaches: Mutex::new(Vec::new()),
                catchups: AtomicU64::new(0),
                input_drops: AtomicU64::new(0),
                auto_drops: AtomicU64::new(0),
                too_many_closes: AtomicU64::new(0),
                sync_jobs: AtomicU64::new(0),
                stall_closes: AtomicU64::new(0),
                force_catchup,
                #[cfg(feature = "qa-tools")]
                open_access: AtomicBool::new(false),
                #[cfg(feature = "qa-tools")]
                input_log: Mutex::new(HashMap::new()),
            },
        });
        rpc.load_inputs();
        // Local socket clients take part in the size policy (AD-020).
        let governor = Arc::new(LocalSizes(Arc::downgrade(&rpc)));
        rpc.sm.set_size_governor(governor.clone());
        rpc.sm.clients().set_leave_hook(move |id| {
            if let Ok(id) = Uuid::parse_str(id) {
                governor.local_left(id);
            }
        });
        tokio::spawn(event_pump(rpc.clone()));
        tokio::spawn(input_watchdog(rpc.sm.clone()));
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
        // Freeze first: no frame is recorded from here on. Then let what the
        // panes already queued reach the PTYs, so the saved `seq` matches what
        // the shells got (AD-024). A hung pane does not hold the stop longer
        // than the limit.
        self.inputs_frozen.store(true, Ordering::SeqCst);
        let undrained = self.sm.close_input_queues_and_drain(INPUT_DRAIN_LIMIT);
        if !undrained.is_empty() {
            // What stays in those queues was acked and is lost: input never
            // crosses a restart (a client that sees the new `boot_id` drops
            // what it holds), and the programs it was for are gone.
            let detail: Vec<String> = undrained.iter().map(|r| r.to_string()).collect();
            warn!(
                "rpc2: {} pane input queue(s) did not drain in {INPUT_DRAIN_LIMIT:?}: {}",
                undrained.len(),
                detail.join("; ")
            );
        }
        let entries: Vec<serde_json::Value> = lock(&self.inputs)
            .iter()
            .map(|((c, e), r)| {
                serde_json::json!({
                    "client_id": c, "input_epoch": e, "last": r.last.load(Ordering::SeqCst),
                })
            })
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
                inputs.insert(
                    (c, ep),
                    Arc::new(InputRec { last: AtomicU64::new(last), ..Default::default() }),
                );
                loaded += 1;
            }
        }
        if loaded > 0 {
            info!("rpc2: restored the input state of {loaded} client epochs");
            // Entries of clients that never come back go after the keep time.
            let rpc = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(INPUT_KEEP).await;
                // A client that connected since the start has a record, and
                // its own departure timer drops its state 10 minutes after it
                // left. This sweep only takes the clients that never came back.
                let known: std::collections::HashSet<Uuid> =
                    lock(&rpc.clients).keys().copied().collect();
                prune_unknown_clients(&mut lock(&rpc.inputs), &known);
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
            Box::pin(async move { rpc.serve(conn, registry).await })
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

    /// Send the shell state of every pane that has one (P7.1). For a client
    /// that just subscribed: it sees the state as it is, then the changes. The
    /// state reaches every subscriber; one that has it drops the repeat by `rev`.
    fn emit_shell_states(&self) {
        for (pane_id, state) in self.sm.shell_states() {
            self.emit(shell_state_event(pane_id.0, &state));
        }
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
        let born = std::time::Instant::now();
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
        let Ok(Some((mut send, recv, mut hello))) = hs else {
            conn.close(close_code::OK.into(), b"bad hello");
            return;
        };
        // The name is peer text that reaches logs, `list_clients` and the
        // client events: strip control characters and cap the length.
        hello.client_name =
            forgetty_core::sanitize::untrusted_max(&hello.client_name, MAX_CLIENT_NAME_CHARS);
        tl!(born, hello.client_name, "hello read");

        // `local` names the server's own socket clients. A remote client is
        // not one, and must not pass for it in `list_clients` or as a size
        // owner. `unknown` is what a decoder makes of a kind it does not
        // know: a newer client is accepted and shown as `unknown`.
        if hello.client_kind == ClientKind::Local {
            let reply = HandshakeReply::Error(ErrorMsg {
                code: error_code::BAD_REQUEST.into(),
                message: format!(
                    "bad_hello: client_kind {} is not allowed",
                    hello.client_kind.as_str()
                ),
            });
            let _ = write_msg(&mut send, &reply).await;
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            conn.close(close_code::OK.into(), b"bad hello");
            return;
        }

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
            boot_id: Some(self.boot_id),
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
        {
            // One device holds a few connections (a reconnect overlaps the
            // old one for a while). Past the cap the oldest goes.
            let mut live = lock(&self.live);
            live.push((conn_key, conn.clone()));
            let mine: Vec<usize> = live
                .iter()
                .enumerate()
                .filter(|(_, (_, c))| c.remote_id() == remote)
                .map(|(i, _)| i)
                .collect();
            if mine.len() > MAX_CONNS_PER_DEVICE {
                let excess = mine.len() - MAX_CONNS_PER_DEVICE;
                for i in &mine[..excess] {
                    warn!("rpc2: device {device_id} has too many connections, closing the oldest");
                    live[*i].1.close(TOO_MANY_CONNECTIONS_CODE.into(), b"too-many-connections");
                }
            }
        }
        self.client_joined(&hello);
        info!(
            client = %hello.client_name, kind = kind.as_str(), id = %hello.client_id,
            "rpc2: client connected"
        );

        tl!(born, hello.client_name, "welcome sent, session starts");
        Session::new(self.clone(), conn.clone(), &hello, caps, born).run(send, recv).await;

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
        self.forget_client_sizes(hello.client_id);
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
    /// Dedup and write happen under one per-client lock, so a frame resent on
    /// a new connection can never overtake or double an earlier one. The write
    /// goes through the pane's input queue and no server lock is held while it
    /// waits: a program that does not read stalls this client's input stream
    /// and nothing else. `seq` counts as written (and acked) only after the
    /// PTY took the bytes.
    async fn handle_input(
        &self,
        client: Uuid,
        epoch: Uuid,
        pane: Uuid,
        seq: u64,
        bytes: Vec<u8>,
        auto: bool,
        policy: bool,
    ) {
        let rec = lock(&self.inputs).entry((client, epoch)).or_default().clone();
        {
            // Dedup and enqueue under one lock, so frames of one client epoch
            // keep their order and a resend is never queued twice. Nothing
            // here waits on the PTY (AD-024): a frame is done when it is in
            // the pane's queue, and a pane that does not read costs its own
            // frames only.
            let _gate = rec.gate.lock().await;
            if self.inputs_frozen.load(Ordering::SeqCst) {
                return; // shutting down: not recorded, so not acked, so resent
            }
            if seq <= rec.last.load(Ordering::SeqCst) {
                self.qa.dedup_drops.fetch_add(1, Ordering::SeqCst);
                return;
            }
            if auto && !self.auto_allowed(client, pane) {
                // A terminal reply from a client that does not own the pane:
                // another VT answers the same query (AD-025). Spent, acked,
                // never written, and never a sign of life for the size policy.
                self.qa.auto_drops.fetch_add(1, Ordering::SeqCst);
                rec.last.fetch_max(seq, Ordering::SeqCst);
                return;
            }
            let len = bytes.len();
            // The byte that makes the tty signal the program, only while ISIG
            // is on. Terminal replies never interrupt.
            let interrupt_at = if auto {
                None
            } else {
                self.sm.intr_byte(PaneId(pane)).and_then(|v| bytes.iter().rposition(|b| *b == v))
            };
            let busy = self.sm.queued_input_bytes(PaneId(pane)).unwrap_or(0) > 0;
            // Is this client's input to the pane in a hole? It stays so until
            // the pane's queue is empty.
            let in_hole = {
                let mut holes = lock(&rec.holes);
                match holes.get_mut(&pane) {
                    Some(h) if !h.closed && busy => true,
                    Some(h) => {
                        h.closed = true; // empty again: flows, loss to be reported
                        false
                    }
                    None => false,
                }
            };
            // A Ctrl+C must work although the program does not read: the
            // write is stuck, so the byte would wait behind it. Signal now and
            // drop what was queued for the program that is being interrupted.
            let (queue_bytes, interrupted) = match interrupt_at {
                Some(at) if in_hole || busy => {
                    self.interrupt_pane(pane).await;
                    (bytes[at + 1..].to_vec(), true)
                }
                _ => (bytes, false),
            };
            let in_hole = in_hole && !interrupted;
            #[cfg(feature = "qa-tools")]
            let copy = queue_bytes.clone();
            let mut dropped = in_hole;
            if !in_hole && !queue_bytes.is_empty() {
                match self.sm.queue_input(PaneId(pane), queue_bytes) {
                    InputOutcome::Queued => {
                        #[cfg(feature = "qa-tools")]
                        lock(&self.qa.input_log).entry(pane).or_default().extend_from_slice(&copy);
                    }
                    InputOutcome::Full => {
                        dropped = true;
                        // The program in that pane has not read 256 KiB of
                        // input. Drop this frame and, to keep the stream free
                        // of holes, the ones behind it.
                        if interrupt_at.is_some() {
                            self.interrupt_pane(pane).await;
                        }
                    }
                    // A pane that is gone takes no more input; the frame is
                    // spent. During a stop the queue is closed on purpose:
                    // the frame was not written, so it is not recorded and
                    // the client resends it.
                    InputOutcome::Gone => {
                        if self.inputs_frozen.load(Ordering::SeqCst) {
                            return;
                        }
                        debug!("rpc2: input for pane {pane}: pane is gone");
                    }
                }
            }
            if dropped {
                let n = self.qa.input_drops.fetch_add(1, Ordering::SeqCst) + 1;
                if n.is_power_of_two() {
                    warn!(
                        "rpc2: pane {pane} input queue full, dropped {len} bytes from client \
                         {client} ({n} frames dropped so far)"
                    );
                }
                let mut holes = lock(&rec.holes);
                let h = holes.entry(pane).or_insert(Hole { bytes: 0, closed: false });
                h.closed = false;
                h.bytes += len as u64;
            }
            rec.last.fetch_max(seq, Ordering::SeqCst);
        }
        // `auto` input is no sign that the user is here (AD-025).
        if !auto {
            self.note_user(client, pane);
            if policy {
                self.touch_active(client, pane);
            }
        }
    }

    /// Ctrl+C for a pane that cannot take it in order: drop the input queued
    /// behind the interrupted program, then interrupt it once. In that order, so the shell that gets the prompt back finds no stale
    /// paste waiting. The drop waits for a stuck write, so it runs on the
    /// blocking pool.
    async fn interrupt_pane(&self, pane: Uuid) {
        let sm = self.sm.clone();
        let dropped = tokio::task::spawn_blocking(move || {
            let dropped = sm.discard_queued_input(PaneId(pane));
            // One SIGINT: the byte through the tty, or the group signal when
            // no byte went in (see `SessionManager::interrupt`).
            sm.interrupt(PaneId(pane));
            dropped
        })
        .await
        .unwrap_or(0);
        debug!("rpc2: interrupt for pane {pane}, {dropped} queued bytes dropped");
    }

    /// Losses of input to report to the client of `rec`: the holes whose pane
    /// queue is empty again. Each is reported once.
    fn take_input_losses(&self, rec: &InputRec) -> Vec<(Uuid, u64)> {
        let mut holes = lock(&rec.holes);
        let ready: Vec<Uuid> = holes
            .iter()
            .filter(|(pane, h)| {
                h.closed || self.sm.queued_input_bytes(PaneId(**pane)).is_none_or(|b| b == 0)
            })
            .map(|(p, _)| *p)
            .collect();
        ready.into_iter().filter_map(|p| holes.remove(&p).map(|h| (p, h.bytes))).collect()
    }

    fn auto_allowed(&self, client: Uuid, pane: Uuid) -> bool {
        lock(&self.sizes).get(&pane).is_some_and(|ps| ps.auto_allowed(client))
    }

    /// `client` just sent `user` input to `pane`.
    fn note_user(&self, client: Uuid, pane: Uuid) {
        let tick = self.user_tick.fetch_add(1, Ordering::SeqCst) + 1;
        lock(&self.sizes).entry(pane).or_default().last_user.insert(client, tick);
    }

    /// `client` attached a stream on `pane`.
    fn note_attached(&self, client: Uuid, pane: Uuid) {
        let apply = {
            let mut sizes = lock(&self.sizes);
            let ps = sizes.entry(pane).or_default();
            if !ps.attached.contains(&client) {
                ps.attached.push(client);
            }
            // No owner with a stream, and this client came first and already
            // asked for a size (its resize came before its attach): it owns
            // the pane now.
            let owned = ps.active.is_some_and(|a| ps.attached.contains(&a));
            if !owned && ps.attached.first() == Some(&client) && ps.by_client.contains_key(&client)
            {
                ps.active = Some(client);
                ps.by_client.get(&client).copied()
            } else {
                None
            }
        };
        self.publish_owner(pane);
        if let Some((rows, cols)) = apply {
            self.apply_size(pane, rows, cols);
        }
    }

    /// `client` detached its stream from `pane` and stays connected.
    fn note_detached(&self, client: Uuid, pane: Uuid) {
        let apply = lock(&self.sizes).get_mut(&pane).and_then(|ps| ps.leave(client, false));
        self.publish_owner(pane);
        if let Some((rows, cols)) = apply {
            self.apply_size(pane, rows, cols);
        }
    }

    fn acked(&self, client: Uuid, epoch: Uuid) -> u64 {
        if self.qa.hold_acks.load(Ordering::SeqCst) {
            return 0;
        }
        lock(&self.inputs).get(&(client, epoch)).map(|r| r.last.load(Ordering::SeqCst)).unwrap_or(0)
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
            // A client with no stream on the pane cannot own it (AD-025).
            if !ps.attached.contains(&client) {
                return;
            }
            ps.active = Some(client);
            ps.by_client.get(&client).copied()
        };
        // The owner is announced before the size it causes.
        self.publish_owner(pane);
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
            let attached = ps.attached.contains(&client);
            // An owner with no stream on the pane is no owner.
            match ps.active.filter(|a| ps.attached.contains(a)) {
                // Only a client that has a stream on the pane takes it.
                None if attached => {
                    ps.active = Some(client);
                    true
                }
                None => false,
                Some(a) => a == client,
            }
        } else {
            true
        };
        if policy {
            self.publish_owner(p.pane_id);
        }
        if apply {
            self.apply_size(p.pane_id, p.rows, p.cols);
        }
    }

    // -----------------------------------------------------------------------
    // Size owner (AD-020)
    // -----------------------------------------------------------------------

    /// What clients are told about `client` as a size owner: a remote client
    /// with its device kind, a local socket client as `local`.
    fn size_owner_info(&self, client: Uuid) -> Option<SizeOwner> {
        if let Some(r) = lock(&self.clients).get(&client) {
            return Some(SizeOwner {
                client_id: client,
                client_kind: r.kind.as_str().to_string(),
                client_name: r.name.clone(),
            });
        }
        let info = self.sm.clients().find(&client.to_string())?;
        (info.client_kind == "socket").then(|| SizeOwner {
            client_id: client,
            client_kind: ClientKind::Local.as_str().to_string(),
            client_name: info.client_name,
        })
    }

    /// Tell the session manager who owns `pane` now. It sends
    /// `SizeOwnerChanged` when that changed. Call it before the resize the
    /// owner's size causes, so the events arrive in that order.
    ///
    /// Lock order: `sizes`, then `clients` (here), then the session manager's
    /// `inner` (in `set_size_owner`). The owner is worked out and recorded
    /// under one `sizes` lock, so two racing calls cannot record an older
    /// owner after a newer one. Nothing takes `sizes` while holding `inner`:
    /// `inner` only sends session events, and this server reads those on its
    /// own task.
    fn publish_owner(&self, pane: Uuid) {
        let sizes = lock(&self.sizes);
        let owner = sizes.get(&pane).and_then(|ps| ps.owner());
        let info = owner.and_then(|c| self.size_owner_info(c));
        self.sm.set_size_owner(PaneId(pane), info);
    }

    /// Harness: hold the size policy lock for `ms`, as a busy pane would.
    #[cfg(feature = "qa-tools")]
    pub(crate) fn hold_sizes(self: &Arc<Self>, ms: u64) {
        let rpc = self.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _held = lock(&rpc.sizes);
            let _ = tx.send(());
            std::thread::sleep(Duration::from_millis(ms));
        });
        let _ = rx.recv();
    }

    /// Harness: how many (pane, client) pairs the size policy holds as
    /// attached. Zero when no client is connected.
    #[cfg(feature = "qa-tools")]
    pub(crate) fn attached_total(&self) -> usize {
        lock(&self.sizes).values().map(|ps| ps.attached.len()).sum()
    }

    /// A client left the server: its sizes no longer count and the panes it
    /// owned go to the attached client that typed last.
    fn forget_client_sizes(&self, client: Uuid) {
        let (panes, handed_over) = {
            let mut sizes = lock(&self.sizes);
            let handed: Vec<(Uuid, (u16, u16))> = sizes
                .iter_mut()
                .filter_map(|(pane, ps)| ps.leave(client, true).map(|sz| (*pane, sz)))
                .collect();
            (sizes.keys().copied().collect::<Vec<Uuid>>(), handed)
        };
        for pane in panes {
            self.publish_owner(pane);
        }
        for (pane, (rows, cols)) in handed_over {
            self.apply_size(pane, rows, cols);
        }
    }

    /// A registered local client resizes `pane`. A user at a local window is
    /// the active user: a client with a `subscribe_output` on the pane takes
    /// the pane's size and ownership. One without it only sets the size.
    fn local_resize(&self, client: Uuid, pane: Uuid, rows: u16, cols: u16) {
        let (rows, cols) =
            (forgetty_session::clamp_pane_dim(rows), forgetty_session::clamp_pane_dim(cols));
        // A remote client already uses this id: no ownership, the size applies.
        if !lock(&self.clients).contains_key(&client) {
            let owns = {
                let mut sizes = lock(&self.sizes);
                let ps = sizes.entry(pane).or_default();
                let attached = ps.attached.contains(&client);
                if attached {
                    ps.by_client.insert(client, (rows, cols));
                    ps.active = Some(client);
                }
                attached
            };
            if owns {
                self.publish_owner(pane);
            }
        }
        self.apply_size(pane, rows, cols);
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
        // Local socket clients that gave themselves an id (the streaming
        // connections of one client share it, so it shows once).
        for c in self.sm.clients().list() {
            let Ok(id) = Uuid::parse_str(&c.client_id) else { continue };
            if c.client_kind != "socket" || entries.iter().any(|e| e.client_id == id) {
                continue;
            }
            entries.push(ClientEntry {
                client_id: id,
                client_kind: ClientKind::Local,
                client_name: c.client_name,
                connected_at: iso8601_from_secs(c.connected_at),
            });
        }
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
        let (sm, sync, id) = (self.sm.clone(), self.sync.get().cloned(), self.server_id);
        // A handler that panics answers with an error and a log line, not a
        // dead request.
        let resp = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            forgetty_socket::dispatch(&req, sm, sync, Some(id))
        })) {
            Ok(r) => r,
            Err(payload) => {
                let why = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".to_string());
                error!("rpc2: {method} panicked: {why}");
                return Err((
                    error_code::INTERNAL,
                    format!("{method} failed inside the server: {why}"),
                ));
            }
        };
        if let Some(err) = &resp.error {
            warn!("rpc2: {method} failed: code {}: {}", err.code, err.message);
        }
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

/// The size policy for local socket clients. Holds the server weakly: the
/// session manager outlives nothing it needs.
struct LocalSizes(std::sync::Weak<RpcV2>);

impl SizeGovernor for LocalSizes {
    fn local_resize(&self, client: Uuid, pane: PaneId, size: PtySize) {
        if let Some(rpc) = self.0.upgrade() {
            rpc.local_resize(client, pane.0, size.rows, size.cols);
        }
    }

    fn local_attached(&self, client: Uuid, pane: PaneId) {
        if let Some(rpc) = self.0.upgrade() {
            if !lock(&rpc.clients).contains_key(&client) {
                rpc.note_attached(client, pane.0);
            }
        }
    }

    fn local_detached(&self, client: Uuid, pane: PaneId) {
        if let Some(rpc) = self.0.upgrade() {
            if !lock(&rpc.clients).contains_key(&client) {
                rpc.note_detached(client, pane.0);
            }
        }
    }

    fn local_left(&self, client: Uuid) {
        if let Some(rpc) = self.0.upgrade() {
            // A remote client with this id is not a local one.
            if !lock(&rpc.clients).contains_key(&client) {
                rpc.forget_client_sizes(client);
            }
        }
    }
}

/// The wire form of a size owner.
fn wire_owner(o: &SizeOwner) -> forgetty_proto::SizeOwner {
    let client_kind = match o.client_kind.as_str() {
        "windows" => ClientKind::Windows,
        "android" => ClientKind::Android,
        "cli" => ClientKind::Cli,
        "gtk" => ClientKind::Gtk,
        "local" => ClientKind::Local,
        _ => ClientKind::Unknown,
    };
    forgetty_proto::SizeOwner {
        client_id: o.client_id,
        client_kind,
        client_name: o.client_name.clone(),
    }
}

/// The wire form of the shell state of a pane.
fn shell_state_event(pane_id: Uuid, s: &forgetty_session::ShellSnapshot) -> ServerEvent {
    ServerEvent::ShellState {
        pane_id,
        rev: s.rev,
        phase: s.phase.as_str().to_string(),
        exit_code: s.exit_code,
        cwd: s.cwd.clone(),
        canonical: s.tty.map(|t| t.canonical),
        echo: s.tty.map(|t| t.echo),
        mark: s.mark.map(|m| m.letter().to_string()),
        at_ms: s.at_ms,
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
        SessionEvent::ShellState { pane_id, state } => {
            rpc.emit(shell_state_event(pane_id.0, &state));
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
        SessionEvent::PaneResized { pane_id, rows, cols, size_owner } => {
            rpc.emit(ServerEvent::PaneResized {
                pane_id: pane_id.0,
                rows,
                cols,
                size_owner: size_owner.as_ref().map(wire_owner),
            })
        }
        SessionEvent::SizeOwnerChanged { pane_id, owner } => {
            rpc.emit(ServerEvent::SizeOwnerChanged {
                pane_id: pane_id.0,
                owner: owner.as_ref().map(wire_owner),
            })
        }
        SessionEvent::PaneClosed { pane_id } => {
            lock(&rpc.sizes).remove(&pane_id.0);
            rpc.sm.forget_size_owner(pane_id);
            RpcV2::forget_pane_pacing(&mut lock(&rpc.pacing), pane_id);
            rpc.snapshots.forget(pane_id);
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
        | SessionEvent::LabelChanged { .. }
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
    /// When the server accepted the connection, and the client's name (timeline).
    born: std::time::Instant,
    name: String,
    conn: Connection,
    client_id: Uuid,
    epoch: Uuid,
    /// Capabilities both sides listed.
    caps: Vec<String>,
    /// Number of the next request, in the order the control reader gave them.
    req_n: AtomicU64,
    /// The pane streams of this connection, by the request that made them.
    ops: Mutex<PaneOps>,
    /// The size-policy syncs that run now, one per pane.
    syncs: SyncSlots,
    /// A `subscribe_events` was taken. The receiver goes to the control loop
    /// through `events_tx`.
    subscribed: AtomicBool,
    events_tx: mpsc::UnboundedSender<EventRx>,
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<EventRx>>>,
}

type EventRx = broadcast::Receiver<(u64, ServerEvent)>;

/// The pane streams of one connection. `attach_pane` and `detach_pane` run
/// as tasks of their own, so two of them for one pane can finish out of
/// order. Each entry keeps the number of the request that made it, and a
/// request with a lower number than the entry does nothing.
#[derive(Default)]
struct PaneOps {
    /// The connection is ending: no stream starts any more.
    closed: bool,
    map: HashMap<Uuid, (u64, Option<AbortHandle>)>,
    /// Attaches read from the control stream and not finished, by pane.
    attaching: HashMap<Uuid, usize>,
}

/// An attach in flight for a pane. While one exists, the mark a detach left
/// for that pane stays (see [`MARK_AGE`]).
struct AttachTicket {
    session: Arc<Session>,
    pane: Uuid,
}

impl AttachTicket {
    fn new(session: &Arc<Session>, pane: Uuid) -> Self {
        *lock(&session.ops).attaching.entry(pane).or_insert(0) += 1;
        AttachTicket { session: session.clone(), pane }
    }
}

impl Drop for AttachTicket {
    fn drop(&mut self) {
        let mut ops = lock(&self.session.ops);
        if let Some(n) = ops.attaching.get_mut(&self.pane) {
            *n -= 1;
            if *n == 0 {
                ops.attaching.remove(&self.pane);
            }
        }
    }
}

/// The size-policy syncs of one connection. A sync re-reads the final stream
/// state of its pane, so one running sync per pane is enough: a request that
/// finds one running marks it dirty and the sync goes again. A peer that
/// sends detaches while the policy lock is busy then holds at most one
/// blocking thread per pane, not one per request.
#[derive(Default)]
struct SyncSlots {
    /// Panes with a sync running (or orphaned), and what each needs.
    running: Mutex<HashMap<Uuid, Slot>>,
    done: Condvar,
}

/// State of one pane's sync slot.
#[derive(Default, Clone, Copy)]
struct Slot {
    /// A request came in while the sync ran: it must go again.
    dirty: bool,
    /// The runner panicked with `dirty` set, so the final state was never
    /// applied. The next claimer or waiter takes the slot over and runs it.
    orphaned: bool,
}

impl SyncSlots {
    /// `true`: the caller runs the sync. `false`: one runs, and goes again.
    fn claim(&self, pane: Uuid) -> bool {
        let mut running = lock(&self.running);
        match running.get_mut(&pane) {
            Some(slot) if slot.orphaned => {
                // A runner died with work left: this caller takes it over.
                *slot = Slot::default();
                true
            }
            Some(slot) => {
                slot.dirty = true;
                false
            }
            None => {
                running.insert(pane, Slot::default());
                true
            }
        }
    }

    /// A pass ended. `true`: it was marked dirty, go again.
    fn pass_done(&self, pane: Uuid) -> bool {
        let mut running = lock(&self.running);
        match running.get_mut(&pane) {
            Some(slot) if slot.dirty => {
                slot.dirty = false;
                true
            }
            _ => {
                running.remove(&pane);
                self.done.notify_all();
                false
            }
        }
    }

    /// The runner of `pane` ended without [`SyncSlots::pass_done`] (a panic).
    /// With nothing marked dirty the slot is freed. With a dirty mark the
    /// final state was never applied, so the slot stays as orphaned for the
    /// next claimer, and a waiter takes it over.
    fn abandon(&self, pane: Uuid) {
        let mut running = lock(&self.running);
        match running.get_mut(&pane) {
            Some(slot) if slot.dirty => {
                slot.orphaned = true;
            }
            _ => {
                running.remove(&pane);
            }
        }
        self.done.notify_all();
    }

    /// Block until no sync runs for `pane`. `true`: the last runner died with
    /// work left, and the caller now owns the slot and must run the sync.
    fn wait_idle(&self, pane: Uuid) -> bool {
        let mut running = lock(&self.running);
        loop {
            match running.get_mut(&pane) {
                None => return false,
                Some(slot) if slot.orphaned => {
                    *slot = Slot::default();
                    return true;
                }
                Some(_) => {
                    running = self.done.wait(running).unwrap_or_else(|e| e.into_inner());
                }
            }
        }
    }
}

/// Frees the claimed slot of a pane when its runner unwinds.
struct SyncSlotGuard<'a> {
    slots: &'a SyncSlots,
    pane: Uuid,
    /// `pass_done` already removed the slot.
    ended: bool,
}

impl Drop for SyncSlotGuard<'_> {
    fn drop(&mut self) {
        if !self.ended {
            self.slots.abandon(self.pane);
        }
    }
}

impl Session {
    fn new(
        rpc: Arc<RpcV2>,
        conn: Connection,
        hello: &Hello,
        caps: Vec<String>,
        born: std::time::Instant,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        Session {
            rpc,
            conn,
            born,
            name: hello.client_name.clone(),
            client_id: hello.client_id,
            epoch: hello.input_epoch,
            caps,
            req_n: AtomicU64::new(0),
            ops: Mutex::new(PaneOps::default()),
            syncs: SyncSlots::default(),
            subscribed: AtomicBool::new(false),
            events_tx,
            events_rx: Mutex::new(Some(events_rx)),
        }
    }

    fn has(&self, c: &str) -> bool {
        self.caps.iter().any(|x| x == c)
    }

    async fn run(self, mut send: iroh::endpoint::SendStream, mut recv: iroh::endpoint::RecvStream) {
        let session = Arc::new(self);
        let queue = Arc::new(ControlQueue::new());
        let inflight = Arc::new(AtomicUsize::new(0));
        // Tasks that live as long as the session: the writer, the request
        // reader and the stall watchdog. One ending ends the session.
        let mut core: JoinSet<()> = JoinSet::new();
        // One task per request in flight, per pane stream and for the input
        // stream. Finished ones are reaped in the loop below.
        let mut work: JoinSet<()> = JoinSet::new();

        // Control writer.
        {
            let (q, s) = (queue.clone(), session.clone());
            core.spawn(async move {
                while let Some((frame, queued)) = q.pop_timed().await {
                    let t = std::time::Instant::now();
                    if write_frame(&mut send, &frame).await.is_err() {
                        break;
                    }
                    q.wrote(frame.len());
                    // A frame that waited or took long to write holds up
                    // every answer behind it on this stream.
                    if queued > Duration::from_millis(20) || t.elapsed() > Duration::from_millis(20) {
                        tl!(
                            s.born,
                            s.name,
                            "control frame of {} bytes waited {} ms in the queue, written in {} ms",
                            frame.len(),
                            queued.as_millis(),
                            t.elapsed().as_millis()
                        );
                    }
                }
            });
        }
        core.spawn(watchdog(queue.clone(), session.conn.clone(), session.rpc.clone()));
        // Tell a client that lost input (AD-024) once its pane reads again.
        if session.has(cap::INPUT_FLAGS) {
            let (s, q) = (session.clone(), queue.clone());
            core.spawn(async move {
                let rec = lock(&s.rpc.inputs).entry((s.client_id, s.epoch)).or_default().clone();
                let mut tick = tokio::time::interval(INPUT_LOSS_POLL);
                loop {
                    tick.tick().await;
                    for (pane_id, bytes) in s.rpc.take_input_losses(&rec) {
                        let seq = s.rpc.current_seq();
                        let event = ServerEvent::InputDropped { pane_id, bytes };
                        q.push(control_frame(&ControlMsg::Event { seq, event }));
                    }
                }
            });
        }

        // Control reader. It reads only while the queue is under its byte
        // budget. A full queue leaves the requests in the QUIC receive
        // window, so a client that does not read its answers cannot push the
        // server any further (QUIC flow control, no buffering here).
        let (req_tx, mut req_rx) = mpsc::channel::<ControlMsg>(1);
        {
            let (q, conn, s) = (queue.clone(), session.conn.clone(), session.clone());
            core.spawn(async move {
                loop {
                    q.wait_below_committed(CONTROL_QUEUE_BYTES).await;
                    match read_msg::<_, ControlMsg>(&mut recv).await {
                        Ok(Some(m)) => {
                            if let ControlMsg::Request { id, .. } = &m {
                                tl!(s.born, s.name, "request id {id} read from the control stream");
                            }
                            if req_tx.send(m).await.is_err() {
                                break;
                            }
                        }
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
                                    conn.close(
                                        close_code::FRAME_TOO_LARGE.into(),
                                        b"frame_too_large",
                                    );
                                }
                                debug!("rpc2: control stream ended: {e}");
                                break;
                            }
                            warn!("rpc2: skipped an undecodable control frame: {e}");
                        }
                    }
                }
            });
        }

        // Input reader: the client's first uni stream.
        {
            let s = session.clone();
            work.spawn(async move {
                let Ok(mut input) = s.conn.accept_uni().await else { return };
                let policy = s.has(cap::SIZE_POLICY_LATEST);
                let flagged = s.has(cap::INPUT_FLAGS);
                loop {
                    // With `input_flags` every frame has a flags byte after
                    // `seq`; without it every frame is `user` input.
                    let frame = if flagged {
                        read_input_frame_flags(&mut input).await
                    } else {
                        read_input_frame(&mut input)
                            .await
                            .map(|f| f.map(|(pane, seq, bytes)| (pane, seq, 0u8, bytes)))
                    };
                    match frame {
                        Ok(Some((pane, seq, flags, bytes))) => {
                            let auto = flags & INPUT_FLAG_AUTO != 0;
                            // Awaited here, so the frames of one client stay in
                            // order. A pane that does not read holds this loop
                            // and nothing else.
                            s.rpc
                                .handle_input(s.client_id, s.epoch, pane, seq, bytes, auto, policy)
                                .await;
                        }
                        // The client finished the stream, or the connection is
                        // going down anyway.
                        Ok(None)
                        | Err(
                            forgetty_proto::FrameError::Io(_)
                            | forgetty_proto::FrameError::UnexpectedEof,
                        ) => break,
                        // A frame this server cannot take ends the input for
                        // good. Say so, so the client redials with a fresh
                        // input stream instead of typing into nothing.
                        Err(e) => {
                            warn!("rpc2: bad frame on the input stream: {e}");
                            let reason: &[u8] =
                                if matches!(e, forgetty_proto::FrameError::TooLarge { .. }) {
                                    b"frame_too_large"
                                } else {
                                    b"bad_input_frame"
                                };
                            s.conn.close(close_code::FRAME_TOO_LARGE.into(), reason);
                            break;
                        }
                    }
                }
            });
        }

        // Control request loop. It only hands requests on: every method runs
        // as a task of its own and answers when it is done, so a slow one
        // (a lock held by a busy pane, a snapshot build) holds up no other.
        let mut events: Option<broadcast::Receiver<(u64, ServerEvent)>> = None;
        let mut events_in = lock(&session.events_rx).take().expect("events channel taken once");
        // A lag resync is owed at this time (coalesced, section 7).
        let mut resync_due: Option<tokio::time::Instant> = None;
        let mut last_resync: Option<tokio::time::Instant> = None;
        loop {
            let resync = async {
                match resync_due {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                msg = req_rx.recv() => {
                    let Some(msg) = msg else { break };
                    match msg {
                        ControlMsg::Ping { nonce } => {
                            let input_acked = session.rpc.acked(session.client_id, session.epoch);
                            queue.push(control_frame(&ControlMsg::Pong { nonce, input_acked }));
                        }
                        ControlMsg::Request { id, method: m, params } => {
                            if !session.on_request(id, m, params, &queue, &inflight, &mut work) {
                                break;
                            }
                        }
                        _ => {} // Response, Failure, Event, Pong come from the server only.
                    }
                }
                Some(rx) = events_in.recv() => events = Some(rx),
                // Take the next event only while the queue is under half its
                // budget. A client that does not read leaves the events in the
                // hub's ring (it lags, it does not grow a queue).
                ev = async {
                    queue.wait_below(CONTROL_QUEUE_BYTES / 2).await;
                    next_event(&mut events).await
                } => {
                    match ev {
                        Ok((seq, event)) => {
                            if !session.event_allowed(&event) {
                                // not for this peer
                            } else if matches!(event, ServerEvent::LayoutChanged { .. }) {
                                // A layout is a full snapshot: one pending
                                // layout per connection, replaced in place.
                                queue.put_layout(control_frame(&ControlMsg::Event { seq, event }));
                            } else {
                                queue.push(control_frame(&ControlMsg::Event { seq, event }));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
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
                        queue.put_layout(control_frame(&ControlMsg::Event { seq, event }));
                        last_resync = Some(tokio::time::Instant::now());
                    }
                }
                Some(_) = work.join_next(), if !work.is_empty() => {}
                _ = core.join_next() => break,
            }
        }
        queue.close();
        {
            let mut ops = lock(&session.ops);
            ops.closed = true;
            for (_, (_, h)) in ops.map.drain() {
                if let Some(h) = h {
                    h.abort();
                }
            }
        }
        work.abort_all();
        core.abort_all();
        while work.join_next().await.is_some() {}
        while core.join_next().await.is_some() {}
    }

    fn event_allowed(&self, event: &ServerEvent) -> bool {
        match event {
            ServerEvent::LayoutChanged { .. } => self.has(cap::LAYOUT_EVENTS),
            ServerEvent::SizeOwnerChanged { .. } => self.has(cap::SIZE_OWNER),
            ServerEvent::ShellState { .. } => self.has(cap::SHELL_STATE),
            ServerEvent::StatusChanged { .. }
            | ServerEvent::Notification { .. }
            | ServerEvent::PaneExited { .. } => self.has(cap::STATUS_EVENTS),
            _ => true,
        }
    }

    /// Count one request that runs on its own task. `None` (and the connection
    /// closed) when [`MAX_INFLIGHT`] are running already.
    fn begin_task(
        &self,
        inflight: &Arc<AtomicUsize>,
        out: &Arc<ControlQueue>,
        reserve: usize,
    ) -> Option<InflightGuard> {
        if inflight.fetch_add(1, Ordering::SeqCst) >= MAX_INFLIGHT {
            inflight.fetch_sub(1, Ordering::SeqCst);
            warn!(
                "rpc2: client {} has more than {MAX_INFLIGHT} requests in flight, closing",
                self.client_id
            );
            self.rpc.qa.too_many_closes.fetch_add(1, Ordering::SeqCst);
            self.conn.close(TOO_MANY_REQUESTS_CODE.into(), b"too-many-requests");
            return None;
        }
        out.reserve(reserve);
        Some(InflightGuard { count: inflight.clone(), queue: out.clone(), reserved: reserve })
    }

    /// Handle one `Request`. The cheap, connection-local methods answer here.
    /// Everything else runs on its own task, so `Ping` is never held up. At
    /// most [`MAX_INFLIGHT`] such tasks run per connection. Returns `false`
    /// when the connection was closed for sending more.
    #[allow(clippy::too_many_arguments)]
    fn on_request(
        self: &Arc<Self>,
        id: u64,
        m: String,
        params: Value,
        out: &Arc<ControlQueue>,
        inflight: &Arc<AtomicUsize>,
        work: &mut JoinSet<()>,
    ) -> bool {
        let bad = |e: forgetty_proto::FrameError| (error_code::BAD_REQUEST, e.to_string());
        let reply = move |r: Result<Value, (&'static str, String)>| match r {
            Ok(result) => ControlMsg::Response { id, result },
            Err((code, message)) => ControlMsg::Failure { id, code: code.into(), message },
        };
        let (born, name) = (self.born, self.name.clone());
        // The method name is peer text: keep control bytes out of the log.
        let m_log = forgetty_core::sanitize::untrusted_max(&m, 64);
        tl!(born, name, "request {m_log:?} id {id} received");
        match m.as_str() {
            // The methods below take locks the program output also takes. A
            // lock wait on a runtime thread would hold up every task that
            // thread has queued, the control reader among them, so each runs
            // on the blocking pool and answers when it is done.
            method::SUBSCRIBE_EVENTS => {
                let Some(guard) = self.begin_task(inflight, out, 0) else { return false };
                let (s, out) = (self.clone(), out.clone());
                work.spawn(async move {
                    let _guard = guard;
                    let t = std::time::Instant::now();
                    let s2 = s.clone();
                    let seq = tokio::task::spawn_blocking(move || {
                        if s2.subscribed.swap(true, Ordering::SeqCst) {
                            s2.rpc.current_seq()
                        } else {
                            let (seq, rx) = s2.rpc.subscribe();
                            let _ = s2.events_tx.send(rx);
                            // The shell state as it is now, for panes that have
                            // one. It goes out as events after the answer; the
                            // `rev` keeps an older state from replacing a newer.
                            if s2.has(cap::SHELL_STATE) {
                                s2.rpc.emit_shell_states();
                            }
                            seq
                        }
                    })
                    .await;
                    let r = match seq {
                        Ok(seq) => to_value(&SubscribeResult { seq }).map_err(bad),
                        Err(e) => Err((error_code::INTERNAL, format!("handler failed: {e}"))),
                    };
                    out.push(control_frame(&reply(r)));
                    tl!(
                        born,
                        s.name,
                        "subscribe_events id {id} answered, handled in {} us",
                        t.elapsed().as_micros()
                    );
                });
            }
            method::ATTACH_PANE => {
                // An attach counts as a request in flight until it is answered.
                // Its stream then runs on its own task (a far-behind pane waits
                // for the pacing and the snapshot gate there, not here).
                let Some(guard) = self.begin_task(inflight, out, 0) else { return false };
                let (s, out) = (self.clone(), out.clone());
                let n = self.req_n.fetch_add(1, Ordering::SeqCst);
                // Counted here, in the order of the control stream, so a detach
                // that follows keeps its mark for as long as this attach runs.
                let parsed = from_value::<AttachPaneParams>(params).map_err(bad);
                let ticket = parsed.as_ref().ok().map(|p| AttachTicket::new(self, p.pane_id));
                work.spawn(async move {
                    let t = std::time::Instant::now();
                    let s2 = s.clone();
                    let r = tokio::task::spawn_blocking(move || {
                        let _ticket = ticket;
                        parsed.and_then(|p| s2.attach(n, p, guard))
                    })
                    .await
                    .unwrap_or_else(|e| Err((error_code::INTERNAL, format!("handler failed: {e}"))));
                    out.push(control_frame(&reply(r)));
                    tl!(
                        born,
                        s.name,
                        "attach_pane id {id} answered, handled in {} us",
                        t.elapsed().as_micros()
                    );
                });
            }
            method::DETACH_PANE => {
                let Some(guard) = self.begin_task(inflight, out, 0) else { return false };
                let (s, out) = (self.clone(), out.clone());
                let n = self.req_n.fetch_add(1, Ordering::SeqCst);
                work.spawn(async move {
                    let _guard = guard;
                    let r = match from_value::<DetachPaneParams>(params).map_err(bad) {
                        Ok(p) => {
                            // The stream stops now. The size policy follows on
                            // the blocking pool, and the answer waits for it
                            // only for DETACH_ANSWER_BOUND: a lock a busy pane
                            // holds must not keep a detach unanswered.
                            s.detach_stream(n, p.pane_id);
                            // One sync per pane runs at a time. When one runs
                            // already it goes again and sees this detach.
                            if s.syncs.claim(p.pane_id) {
                                let s2 = s.clone();
                                let job = tokio::task::spawn_blocking(move || {
                                    s2.run_claimed_sync(p.pane_id)
                                });
                                match tokio::time::timeout(DETACH_ANSWER_BOUND, job).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => warn!(
                                        "rpc2: the size sync of detach_pane {} of client {} \
                                         failed: {e}",
                                        p.pane_id, s.client_id
                                    ),
                                    Err(_) => warn!(
                                        "rpc2: detach_pane {} of client {} answered before the \
                                         size policy was updated (a lock is busy)",
                                        p.pane_id, s.client_id
                                    ),
                                }
                            }
                            to_value(&OkResult { ok: true }).map_err(bad)
                        }
                        Err(e) => Err(e),
                    };
                    out.push(control_frame(&reply(r)));
                });
            }
            method::RESIZE_PANE => {
                let Some(guard) = self.begin_task(inflight, out, ANSWER_RESERVE) else {
                    return false;
                };
                let s = self.clone();
                let out = out.clone();
                work.spawn(async move {
                    let _guard = guard;
                    let r =
                        from_value::<ResizePaneParams>(params).map_err(bad).and_then(|mut p| {
                            if p.rows == 0 || p.cols == 0 {
                                return Err((
                                    error_code::BAD_REQUEST,
                                    "rows and cols must be above 0".into(),
                                ));
                            }
                            // Asked-for sizes are kept per client (size policy), so
                            // clamp before they are stored, not only when applied.
                            p.rows = forgetty_session::clamp_pane_dim(p.rows);
                            p.cols = forgetty_session::clamp_pane_dim(p.cols);
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
                    out.push(control_frame(&reply(r)));
                });
            }
            // Cheap, answered here: the clients table has short locks and no
            // program output on it. A flood of these fills the byte budget
            // and the stall watchdog closes the connection.
            method::LIST_CLIENTS => {
                out.push(control_frame(&reply(self.rpc.list_clients())));
            }
            other => {
                if other == method::CAPTURE && !self.has(cap::CAPTURE) {
                    out.push(control_frame(&ControlMsg::Failure {
                        id,
                        code: error_code::CAPABILITY_MISSING.into(),
                        message: "capture needs the capability".into(),
                    }));
                    return true;
                }
                let reserve =
                    if other == method::CAPTURE { CAPTURE_RESERVE } else { ANSWER_RESERVE };
                let Some(guard) = self.begin_task(inflight, out, reserve) else { return false };
                let s = self.clone();
                let out = out.clone();
                let other = other.to_string();
                let who = name.clone();
                let m_log = m_log.clone();
                work.spawn(async move {
                    let _guard = guard;
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
                    out.push(control_frame(&reply(r)));
                    tl!(born, who, "request {m_log:?} id {id} answered");
                });
            }
        }
        true
    }

    /// `attach_pane`: pick the start offset (section 8) and open the stream.
    /// `n` is the number of the request. A newer attach or detach of the same
    /// pane that already ran wins over this one.
    fn attach(
        self: &Arc<Self>,
        n: u64,
        p: AttachPaneParams,
        guard: InflightGuard,
    ) -> Result<Value, (&'static str, String)> {
        let pane = PaneId(p.pane_id);
        let t = std::time::Instant::now();
        let Some(info) = self.rpc.sm.pane_info(pane) else {
            return Err((error_code::NOT_FOUND, "no such pane".into()));
        };
        let (Some((first, total)), Some(shared)) =
            (self.rpc.sm.log_bounds(pane), self.rpc.sm.log_shared(pane))
        else {
            return Err((error_code::INTERNAL, "pane has no byte log".into()));
        };
        let looked_up = t.elapsed();
        let from = if self.has(cap::RESUME) { p.from_offset } else { None };
        let (start, reset) = match from {
            None => (first, true),
            Some(o) if o > total => (first, true),
            Some(o) if o < first => (first, true),
            Some(o) => (o, false),
        };
        {
            // Harness record. Bounded: it only has to show recent attaches.
            let mut attaches = lock(&self.rpc.qa.attaches);
            if attaches.len() >= QA_ATTACHES_KEPT {
                attaches.remove(0);
            }
            attaches.push((p.pane_id, p.from_offset));
        }
        // Harness: a pane lookup that takes long (a lock a busy pane holds).
        let ms = self.rpc.qa.attach_delay_ms.load(Ordering::SeqCst);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
        let header = PaneStreamHeader {
            pane_id: p.pane_id,
            start_offset: start,
            reset,
            rows: info.rows,
            cols: info.cols,
            snapshot_len: 0,
            // Everything the log holds now is replay. What comes after is live.
            replay_end: Some(total),
        };
        let catchup = self.has(cap::SNAPSHOT_CATCHUP);
        tl!(
            self.born,
            self.name,
            "attach_pane {} from {:?}: start {start} reset {reset} gap {} (limit {}), lookups {} us",
            p.pane_id,
            p.from_offset,
            total.saturating_sub(start),
            shared.ring_capacity() / 2,
            looked_up.as_micros()
        );
        // One live stream per (connection, pane): the old one is aborted.
        {
            let mut ops = lock(&self.ops);
            if ops.closed {
                return Err((error_code::INTERNAL, "connection is closing".into()));
            }
            if ops.map.get(&p.pane_id).is_some_and(|(seq, _)| *seq > n) {
                // A newer request for this pane ran first.
                return to_value(&OkResult { ok: true })
                    .map_err(|e| (error_code::INTERNAL, e.to_string()));
            }
            if let Some((_, Some(old))) = ops.map.remove(&p.pane_id) {
                old.abort();
            }
            let handle = tokio::spawn(pane_stream(
                self.rpc.clone(),
                self.conn.clone(),
                shared,
                header,
                catchup,
                (self.born, self.name.clone()),
            ))
            .abort_handle();
            ops.map.insert(p.pane_id, (n, Some(handle)));
        }
        // The request is done: its slot is free before the answer goes out.
        drop(guard);
        self.sync_attached(p.pane_id);
        to_value(&OkResult { ok: true }).map_err(|e| (error_code::INTERNAL, e.to_string()))
    }

    /// `detach_pane`: stop the stream of a pane. See [`Self::attach`] for `n`.
    /// This is the part that answers: it only touches the `ops` map and takes
    /// no other lock, so it cannot wait on a pane, a snapshot or the session
    /// manager. [`Self::sync_attached`] follows.
    fn detach_stream(&self, n: u64, pane: Uuid) {
        let mut ops = lock(&self.ops);
        if ops.map.get(&pane).is_some_and(|(seq, _)| *seq > n) {
            return;
        }
        if let Some((_, Some(old))) = ops.map.remove(&pane) {
            old.abort();
        }
        if ops.closed {
            return;
        }
        // A mark for an attach of this pane that is still on its way (one with
        // a lower number does nothing). Marks older than any request that can
        // still be running are dropped, so a peer cannot grow the map.
        ops.map.insert(pane, (n, None));
        if ops.map.len() > MARKS_KEPT {
            let PaneOps { map, attaching, .. } = &mut *ops;
            map.retain(|pane, (seq, h)| {
                h.is_some() || *seq + MARK_AGE > n || attaching.contains_key(pane)
            });
        }
    }

    /// Make the size policy's view of this client on `pane` match the stream
    /// state: attached when a stream runs, detached when none does or the
    /// connection is ending. It runs outside `ops` (it takes the size policy,
    /// the clients table and the session manager locks), so it re-reads the
    /// state afterwards and goes again if it changed: the last caller always
    /// applies the final state, and a client that closed in between is left
    /// as no one's.
    fn sync_attached(&self, pane: Uuid) {
        if self.syncs.claim(pane) {
            self.run_claimed_sync(pane);
        } else {
            // One runs and goes again after us, so the state we changed is
            // applied before it ends. Wait for that: the answer of an attach
            // promises the size policy knows about the stream.
            if self.syncs.wait_idle(pane) {
                // The runner panicked with work left: this call owns it now.
                self.run_claimed_sync(pane);
            }
        }
    }

    /// Run the sync for `pane`, which the caller claimed with
    /// [`SyncSlots::claim`]. Goes again while others marked it dirty.
    fn run_claimed_sync(&self, pane: Uuid) {
        self.rpc.qa.sync_jobs.fetch_add(1, Ordering::SeqCst);
        // If a pass panics, the guard frees the slot while the stack unwinds.
        let mut slot = SyncSlotGuard { slots: &self.syncs, pane, ended: false };
        loop {
            self.sync_pass(pane);
            if !self.syncs.pass_done(pane) {
                slot.ended = true;
                return;
            }
        }
    }

    fn sync_pass(&self, pane: Uuid) {
        loop {
            let want = {
                let ops = lock(&self.ops);
                !ops.closed && matches!(ops.map.get(&pane), Some((_, Some(_))))
            };
            if want {
                self.rpc.note_attached(self.client_id, pane);
            } else {
                self.rpc.note_detached(self.client_id, pane);
            }
            let now = {
                let ops = lock(&self.ops);
                !ops.closed && matches!(ops.map.get(&pane), Some((_, Some(_))))
            };
            if now == want {
                return;
            }
        }
    }
}

/// Counts one running request. Drops with the task.
struct InflightGuard {
    count: Arc<AtomicUsize>,
    /// The queue the request's answer goes to, and the bytes set aside on it.
    queue: Arc<ControlQueue>,
    reserved: usize,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
        self.queue.release(self.reserved);
    }
}

/// Encode a Control message as one frame payload. An answer too large for a
/// frame becomes a `Failure`, so the caller still gets an answer.
fn control_frame(msg: &ControlMsg) -> Vec<u8> {
    match encode_msg(msg) {
        Ok(bytes) if bytes.len() <= forgetty_proto::MAX_FRAME_LEN => bytes,
        other => {
            let reason = match other {
                Ok(b) => format!("answer of {} bytes is over the frame limit", b.len()),
                Err(e) => e.to_string(),
            };
            match msg {
                ControlMsg::Response { id, .. } => encode_msg(&ControlMsg::Failure {
                    id: *id,
                    code: error_code::INTERNAL.into(),
                    message: reason,
                })
                .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
    }
}

/// Close a connection that leaves its Control queue (half) full for
/// [`STALL_LIMIT`] with no write completing.
async fn watchdog(queue: Arc<ControlQueue>, conn: Connection, rpc: Arc<RpcV2>) {
    let half = CONTROL_QUEUE_BYTES / 2;
    loop {
        queue.wait_at_least(half).await;
        let before = queue.written_bytes();
        tokio::time::sleep(STALL_LIMIT).await;
        let drained = queue.written_bytes().saturating_sub(before);
        if queue.bytes() >= half && drained < MIN_DRAIN_RATE * STALL_LIMIT.as_secs() {
            warn!(
                "rpc2: a client drained {drained} bytes of Control in {STALL_LIMIT:?} \
                 with the queue half full, closing"
            );
            rpc.qa.stall_closes.fetch_add(1, Ordering::SeqCst);
            conn.close(STALLED_CODE.into(), b"control-stalled");
            return;
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

/// Why a pane stream ended.
enum StreamEnd {
    /// The pane closed: the stream was finished.
    Closed,
    /// The peer or the log is gone. Nothing more to do.
    Gone,
    /// The reader fell behind: replace the stream with a snapshot.
    /// `next` is the offset it had reached. `stalled`: a write made no
    /// progress for [`NO_PROGRESS`], so the peer itself is the slow part.
    Catchup { next: u64, stalled: bool },
}

impl StreamEnd {
    /// Fill in the offset a catch-up request stands at.
    fn at(self, next: u64) -> Self {
        match self {
            StreamEnd::Catchup { stalled, .. } => StreamEnd::Catchup { next, stalled },
            other => other,
        }
    }
}

/// How often the input watchdog looks at the panes.
const INPUT_WATCH_TICK: Duration = Duration::from_millis(500);

/// Logs a pane whose input queue stands still (see `forgetty_session::input_watch`).
async fn input_watchdog(sm: Arc<SessionManager>) {
    use forgetty_session::input_watch::{InputWatchdog, WatchEvent};
    let mut dog = InputWatchdog::default();
    let mut tick = tokio::time::interval(INPUT_WATCH_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let reports = sm.input_reports();
        for event in dog.tick(std::time::Instant::now(), &reports) {
            match event {
                // A program that does not read is its own business: INFO.
                // Everything else about a stall points at the server: WARN.
                WatchEvent::Stalled(r) if r.program_not_reading() => {
                    info!("rpc2: pane input stalled: {r}");
                }
                WatchEvent::Stalled(r) => warn!("rpc2: pane input stalled: {r}"),
                WatchEvent::Cleared { pane_id, stalled_for } => {
                    info!("rpc2: pane input moves again: pane {pane_id} after {stalled_for:?}");
                }
            }
        }
    }
}

/// One Pane output stream: the header, then raw frames pulled from the pane
/// byte log by offset.
///
/// A slow reader never slows the program (AD-023). The log keeps filling no
/// matter what this task does. With `snapshot_catchup`, a reader that falls
/// more than half the ring behind, or whose write makes no progress for
/// [`NO_PROGRESS`], gets its stream reset (`SUPERSEDED`) and a new one that
/// starts with a VT snapshot of the screen, then continues live. Without the
/// capability the old rule stands: bytes come from the ring or the disk file,
/// and a reader behind everything is reset with `LOG_LOST`.
async fn pane_stream(
    rpc: Arc<RpcV2>,
    conn: Connection,
    shared: Arc<LogShared>,
    first: PaneStreamHeader,
    catchup: bool,
    (born, name): (std::time::Instant, String),
) {
    let pane = PaneId(first.pane_id);
    let lag_limit = (shared.ring_capacity() / 2) as u64;
    let mut force = rpc.qa.force_catchup.subscribe();
    force.mark_unchanged();
    let mut header = first;
    let mut snapshot: Vec<u8> = Vec::new();
    // A client that resumes far behind starts with the snapshot, paced like
    // any other catch-up of this client and pane.
    if catchup && shared.total().saturating_sub(header.start_offset) > lag_limit {
        let wait = rpc.resume_delay(conn.remote_id(), pane);
        tl!(born, name, "pane {pane}: resume far behind, pacing wait {wait:?}");
        tokio::time::sleep(wait).await;
        let Some((h, snap)) = take_snapshot(&rpc, conn.remote_id(), pane, &shared).await else {
            return;
        };
        tl!(born, name, "pane {pane}: snapshot ready, {} bytes", snap.len());
        header = h;
        snapshot = snap;
    }
    loop {
        let Ok(mut send) = conn.open_uni().await else { return };
        let sent_snapshot = std::mem::take(&mut snapshot);
        let after_snapshot = !sent_snapshot.is_empty();
        let cycle_start = tokio::time::Instant::now();
        header.snapshot_len = sent_snapshot.len() as u32;
        if sent_snapshot.is_empty() {
            // Replay ends where the log stands as this stream starts.
            header.replay_end = Some(shared.total().max(header.start_offset));
        }
        if write_msg(&mut send, &header).await.is_err() {
            return;
        }
        tl!(born, name, "pane {pane}: stream header written (snapshot {} bytes)", sent_snapshot.len());
        for part in sent_snapshot.chunks(CHUNK) {
            if write_output_frame(&mut send, part).await.is_err() {
                return;
            }
        }
        let snapshot_sent_in = cycle_start.elapsed();
        match stream_from(
            &rpc,
            &shared,
            &mut send,
            pane,
            header.start_offset,
            catchup,
            after_snapshot,
            &mut force,
        )
        .await
        {
            StreamEnd::Gone => return,
            StreamEnd::Closed => {
                // Finish cleanly (FIN) and give the peer time to read it.
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
                return;
            }
            StreamEnd::Catchup { next, stalled } => {
                // A peer that keeps pace with the program is not slow, however
                // often a fast program leaves it behind: since its last
                // snapshot it took live bytes at a good rate, or the log ran
                // past that snapshot before a single live byte went out
                // although the snapshot itself got through quickly (the build
                // took longer than the ring holds at the program's speed).
                // Only a peer that is slow itself waits and backs off.
                let live = next.saturating_sub(header.start_offset);
                let cycle = cycle_start.elapsed();
                let rate = live as f64 / cycle.as_secs_f64().max(0.001);
                // `Strong` is evidence that the peer reads: it took a good rate
                // of live bytes, more than the room a peer that never reads has
                // left in its QUIC window. Only `Strong` ends the backoff that
                // stalls built up. `Tiny` (one chunk in a few microseconds) is
                // what a peer that never reads shows too, so it is believed
                // only a few times in a row. `Weak` (a slow build, no live
                // byte) is the server's doing and costs a token.
                let claim = if stalled {
                    Claim::Stalled
                } else if live == 0 {
                    if snapshot_sent_in < SNAPSHOT_SEND_OK {
                        Claim::Weak
                    } else {
                        Claim::Stalled
                    }
                } else if rate >= KEEPING_PACE_RATE {
                    if live >= KEEPING_PACE_MIN_LIVE {
                        Claim::Strong
                    } else {
                        Claim::Tiny
                    }
                } else {
                    Claim::Stalled
                };
                let wait = rpc.catchup_delay(conn.remote_id(), pane, claim);
                tl!(born, name, "pane {pane}: catch-up {claim:?}, pacing wait {wait:?}");
                tokio::time::sleep(wait).await;
                // Stop the old stream, so it queues nothing more for the peer.
                let _ = send.reset(stream_code::SUPERSEDED.into());
                let Some((h, snap)) = take_snapshot(&rpc, conn.remote_id(), pane, &shared).await
                else {
                    return;
                };
                header = h;
                snapshot = snap;
            }
        }
    }
}

/// Spacing of one stream's catch-ups: at least [`MIN_CATCHUP_INTERVAL`] apart,
/// doubling up to [`MAX_CATCHUP_BACKOFF`] while the stream keeps needing them.
struct CatchupLimiter {
    last: Option<tokio::time::Instant>,
    interval: Duration,
}

impl Default for CatchupLimiter {
    fn default() -> Self {
        CatchupLimiter { last: None, interval: MIN_CATCHUP_INTERVAL }
    }
}

impl CatchupLimiter {
    /// How long until `gap` has passed since the last catch-up.
    fn floor(&self, gap: Duration) -> Duration {
        let now = tokio::time::Instant::now();
        self.last.map_or(Duration::ZERO, |l| (l + gap).saturating_duration_since(now))
    }

    /// The next backed off catch-up starts from the minimum again.
    fn reset_backoff(&mut self) {
        self.interval = MIN_CATCHUP_INTERVAL;
    }

    /// A catch-up that is not backed off happens at `at`.
    fn mark(&mut self, at: tokio::time::Instant) {
        self.last = Some(at);
    }

    /// [`Self::delay`] for a peer that has no resume token left. It keeps
    /// doubling however slowly the peer comes back, as long as it does come
    /// back within [`MAX_CATCHUP_BACKOFF`] twice over (a peer that re-attaches
    /// every 2 s must not start over each time). Only a `Strong` claim or a
    /// quiet time that long ends it. The wait is never over
    /// [`MAX_CATCHUP_BACKOFF`], so waits that stack up do not grow for ever.
    fn delay_sticky(&mut self) -> Duration {
        let now = tokio::time::Instant::now();
        let wait = match self.last {
            None => Duration::ZERO,
            Some(last) => {
                self.interval = if now.saturating_duration_since(last) < MAX_CATCHUP_BACKOFF * 2 {
                    (self.interval * 2).min(MAX_CATCHUP_BACKOFF)
                } else {
                    MIN_CATCHUP_INTERVAL
                };
                (last + self.interval).saturating_duration_since(now).min(MAX_CATCHUP_BACKOFF)
            }
        };
        self.last = Some(now + wait);
        wait
    }

    /// How long to wait before the next backed off catch-up. Records it as
    /// done.
    fn delay(&mut self) -> Duration {
        let now = tokio::time::Instant::now();
        let wait = match self.last {
            None => Duration::ZERO,
            Some(last) => {
                // Needed again within two intervals: back off. Otherwise start over.
                self.interval = if now.duration_since(last) < self.interval * 2 {
                    (self.interval * 2).min(MAX_CATCHUP_BACKOFF)
                } else {
                    MIN_CATCHUP_INTERVAL
                };
                (last + self.interval).saturating_duration_since(now)
            }
        };
        self.last = Some(now + wait);
        wait
    }
}

/// Catch-ups a `(node, pane)` whose reads keep up may take in a burst, and
/// per second after that. They never back off: the peer reads what it gets.
const KEEPING_PACE_BURST: f64 = 6.0;
const KEEPING_PACE_PER_SEC: f64 = 1.0;
/// One resume token comes back per this much time, on top of the `Strong`
/// refill. A phone whose pane printed 10 MB while it was away and then went
/// quiet shows no live byte to prove a read; this is its refill. A peer that
/// never reads gets one extra snapshot per pane per 5 minutes from it.
const RESUME_TOKEN_EVERY: Duration = Duration::from_secs(300);

/// Pacing state of one `(remote node id, pane)`. Keyed by the node, not by
/// `client_id`: the id is peer text, a fresh one per connection would reset it.
struct Pace {
    limiter: CatchupLimiter,
    tokens: f64,
    /// Far-behind resumes left before the backoff. A `Strong` claim adds one,
    /// and time adds one per [`RESUME_TOKEN_EVERY`] (at most
    /// [`KEEPING_PACE_BURST`] in all).
    resume_tokens: f64,
    /// Time up to which `resume_tokens` has the slow refill.
    resume_clock: tokio::time::Instant,
    /// Tokens earn from here on. Time spent waiting out a delay earns none.
    refilled: tokio::time::Instant,
    /// Tiny claims since the last strong one.
    tiny_streak: u32,
}

impl Pace {
    /// Add the resume tokens that time has earned since the last call.
    fn top_up_resume_tokens(&mut self, now: tokio::time::Instant) {
        let earned = now.saturating_duration_since(self.resume_clock).as_secs_f64()
            / RESUME_TOKEN_EVERY.as_secs_f64();
        self.resume_tokens = (self.resume_tokens + earned).min(KEEPING_PACE_BURST);
        self.resume_clock = now;
    }
}

impl RpcV2 {
    /// How long `(node, pane)` waits before its next snapshot catch-up.
    /// A peer that claims to keep pace (`Weak` or `Strong`) gets a burst of 6,
    /// then one catch-up per second. `Strong` also ends the backoff its stalls
    /// built up. A peer whose write stalled, or that reads too slowly to tell,
    /// waits 1 s, then 2, 4, 8, 10 s while it keeps needing them.
    fn catchup_delay(&self, node: iroh::EndpointId, pane: PaneId, claim: Claim) -> Duration {
        Self::pace(&mut lock(&self.pacing), node, pane, claim)
    }

    /// How long an attach that resumes far behind waits for its snapshot. A
    /// phone that sat in a pocket while a build printed 10 MB is the normal
    /// case, so the resume spends a resume token first: 6 per `(node, pane)`,
    /// at the 100 ms floor between snapshots. A `Strong` claim (a peer that
    /// reads live bytes at a good rate) earns one, and so does each 5 minutes
    /// of time. With no
    /// token left the resume waits out the doubling backoff (1 s up to 10 s),
    /// so a peer that never reads gets 6 snapshots, then the backoff and one
    /// more per 5 minutes.
    fn resume_delay(&self, node: iroh::EndpointId, pane: PaneId) -> Duration {
        Self::resume(&mut lock(&self.pacing), node, pane)
    }

    fn resume(
        map: &mut HashMap<(iroh::EndpointId, Uuid), Pace>,
        node: iroh::EndpointId,
        pane: PaneId,
    ) -> Duration {
        let now = tokio::time::Instant::now();
        let p = Self::slot(map, node, pane, now);
        p.top_up_resume_tokens(now);
        if p.resume_tokens < 1.0 {
            // No token: the doubling backoff, whatever the time since.
            let w = p.limiter.delay_sticky();
            p.refilled = now + w;
            return w;
        }
        p.resume_tokens -= 1.0;
        // The token pays for the wait for a snapshot, not for the spacing:
        // snapshots of one (node, pane) stay KEEPING_PACE_INTERVAL apart, as
        // in `pace`.
        let w = p.limiter.floor(KEEPING_PACE_INTERVAL);
        p.limiter.mark(now + w);
        p.refilled = now + w;
        w
    }

    /// The record of `(node, pane)`, made if it is new. A flood of nodes gets
    /// the idle ones dropped first, on either path.
    fn slot(
        map: &mut HashMap<(iroh::EndpointId, Uuid), Pace>,
        node: iroh::EndpointId,
        pane: PaneId,
        now: tokio::time::Instant,
    ) -> &mut Pace {
        if map.len() > 4096 {
            // Only a flood of nodes gets here (each is an authorized device).
            map.retain(|_, p| now.saturating_duration_since(p.refilled) < Duration::from_secs(300));
        }
        map.entry((node, pane.0)).or_insert_with(|| Pace {
            limiter: CatchupLimiter::default(),
            tokens: KEEPING_PACE_BURST,
            resume_tokens: KEEPING_PACE_BURST,
            resume_clock: now,
            refilled: now,
            tiny_streak: 0,
        })
    }

    /// A pane that closed has no catch-ups left to pace.
    fn forget_pane_pacing(map: &mut HashMap<(iroh::EndpointId, Uuid), Pace>, pane: PaneId) {
        map.retain(|(_, p), _| *p != pane.0);
    }

    fn pace(
        map: &mut HashMap<(iroh::EndpointId, Uuid), Pace>,
        node: iroh::EndpointId,
        pane: PaneId,
        claim: Claim,
    ) -> Duration {
        let now = tokio::time::Instant::now();
        let p = Self::slot(map, node, pane, now);
        let earned = now.saturating_duration_since(p.refilled).as_secs_f64() * KEEPING_PACE_PER_SEC;
        p.tokens = (p.tokens + earned).min(KEEPING_PACE_BURST);
        match claim {
            Claim::Strong => {
                p.tiny_streak = 0;
                // Proof of a read earns a resume token.
                p.top_up_resume_tokens(now);
                p.resume_tokens = (p.resume_tokens + 1.0).min(KEEPING_PACE_BURST);
            }
            Claim::Tiny => p.tiny_streak += 1,
            Claim::Weak | Claim::Stalled => {}
        }
        // A peer that keeps claiming a few bytes without ever proving a read
        // is treated as one whose write stalls.
        let claim = if claim == Claim::Tiny && p.tiny_streak > KEEPING_PACE_TINY_BURST {
            Claim::Stalled
        } else {
            claim
        };
        let wait = if claim != Claim::Stalled {
            // Out of burst: the wait is the time to the next token.
            let for_token = if p.tokens >= 1.0 {
                p.tokens -= 1.0;
                Duration::ZERO
            } else {
                let w = Duration::from_secs_f64((1.0 - p.tokens) / KEEPING_PACE_PER_SEC);
                p.tokens = 0.0;
                w
            };
            let gap = p.limiter.floor(KEEPING_PACE_INTERVAL);
            let w = for_token.max(gap);
            // A peer that reads what it gets is done with the backoff its
            // stalls built up, or an honest client would wait 2 to 10 s for
            // it. A claim that proves nothing leaves the backoff alone.
            if claim == Claim::Strong {
                p.limiter.reset_backoff();
            }
            p.limiter.mark(now + w);
            w
        } else {
            p.limiter.delay()
        };
        p.refilled = now + wait;
        wait
    }
}

/// Get the snapshot of a pane from the shared hub (one build for all streams,
/// two builds at once at most). Returns the header of the stream that carries
/// it, and the snapshot bytes.
async fn take_snapshot(
    rpc: &Arc<RpcV2>,
    node: iroh::EndpointId,
    pane: PaneId,
    shared: &Arc<LogShared>,
) -> Option<(PaneStreamHeader, Vec<u8>)> {
    let sm = rpc.sm.clone();
    // A finished snapshot is handed out again only while the log has not
    // moved far past it: a stale one would be behind the ring on arrival.
    let fresh = {
        let shared = shared.clone();
        let slack = (shared.ring_capacity() / 8) as u64;
        move |s: &forgetty_session::PaneSnapshot| shared.total().saturating_sub(s.offset) <= slack
    };
    let snap = rpc
        .snapshots
        .get_for(*node.as_bytes(), pane, fresh, move || sm.snapshot_pane(pane))
        .await?;
    rpc.qa.catchups.fetch_add(1, Ordering::SeqCst);
    info!(
        "rpc2: pane {pane} snapshot catch-up at {}: {} bytes (last build {} us)",
        snap.offset,
        snap.bytes.len(),
        rpc.snapshots.stats.last_us.load(Ordering::Relaxed)
    );
    let header = PaneStreamHeader {
        pane_id: pane.0,
        start_offset: snap.offset,
        reset: true,
        rows: snap.rows,
        cols: snap.cols,
        snapshot_len: 0,
        // The snapshot stands at its offset: every pane byte after it is live.
        replay_end: Some(snap.offset),
    };
    Some((header, snap.bytes.clone()))
}

/// Write one live frame. With `catchup`, a write that makes no progress for
/// [`NO_PROGRESS`] ends the stream with [`StreamEnd::Catchup`].
async fn write_live(
    send: &mut iroh::endpoint::SendStream,
    bytes: &[u8],
    catchup: bool,
) -> Result<(), StreamEnd> {
    if !catchup {
        return write_output_frame(send, bytes).await.map_err(|_| StreamEnd::Gone);
    }
    match tokio::time::timeout(NO_PROGRESS, write_output_frame(send, bytes)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(StreamEnd::Gone),
        Err(_) => {
            info!("rpc2: a live write made no progress for {NO_PROGRESS:?}, catching up");
            Err(StreamEnd::Catchup { next: 0, stalled: true })
        }
    }
}

/// Send the pane's bytes from `next` on until the stream ends.
async fn stream_from(
    rpc: &Arc<RpcV2>,
    shared: &Arc<LogShared>,
    send: &mut iroh::endpoint::SendStream,
    pane: PaneId,
    mut next: u64,
    catchup: bool,
    after_snapshot: bool,
    force: &mut watch::Receiver<(Uuid, u64)>,
) -> StreamEnd {
    let lag_limit = (shared.ring_capacity() / 2) as u64;
    // A stream that follows a snapshot writes one frame before the lag test
    // applies again. Each snapshot then makes progress, even while a heavy
    // program keeps the head far ahead of the offset the snapshot stands at.
    let mut lag_check = !after_snapshot;
    loop {
        // Arm both wake-ups before looking at the log, so nothing is missed.
        let wake = shared.wake.notified();
        let progress = shared.disk_progress.notified();
        tokio::pin!(wake, progress);
        wake.as_mut().enable();
        progress.as_mut().enable();

        if catchup {
            let forced = force.has_changed().unwrap_or(false) && {
                let target = force.borrow_and_update().0;
                target == pane.0
            };
            if forced || (lag_check && shared.total().saturating_sub(next) > lag_limit) {
                info!(
                    "rpc2: pane {pane} catch-up ({}): reader at {next}, log at {}, limit {lag_limit}",
                    if forced { "forced by the harness" } else { "reader is over the lag limit" },
                    shared.total()
                );
                return StreamEnd::Catchup { next, stalled: false };
            }
        }

        match rpc.sm.plan_log(pane, next, CHUNK) {
            None => return StreamEnd::Closed,
            Some(LogPlan::Caught) => {
                tokio::select! {
                    _ = &mut wake => {}
                    changed = force.changed(), if catchup => {
                        if changed.is_ok() && force.borrow_and_update().0 == pane.0 {
                            return StreamEnd::Catchup { next, stalled: false };
                        }
                    }
                }
            }
            Some(LogPlan::Ring(bytes)) => {
                if let Err(end) = write_live(send, &bytes, catchup).await {
                    return end.at(next);
                }
                next += bytes.len() as u64;
                lag_check = true;
            }
            Some(LogPlan::Disk { disk, len }) => match read_disk(shared, disk, next, len).await {
                Ok(Some(bytes)) if !bytes.is_empty() => {
                    if let Err(end) = write_live(send, &bytes, catchup).await {
                        return end.at(next);
                    }
                    next += bytes.len() as u64;
                    lag_check = true;
                }
                // The file has not caught up yet: wait for the appender.
                Ok(Some(_)) => progress.await,
                // The file moved under the read: wait for the appender to
                // publish the new extent, then plan again.
                Ok(None) => progress.await,
                Err(e) => {
                    warn!("rpc2: pane {pane} log read failed: {e}");
                    let _ = send.reset(LOG_LOST_CODE.into());
                    return StreamEnd::Gone;
                }
            },
            Some(LogPlan::Lost { first }) => {
                // The reader fell behind everything the log still holds. The
                // stream cannot say "reset" in the middle, so it breaks and the
                // client resumes with a reset (section 8).
                if catchup {
                    // The log moved past the offset while a snapshot was built
                    // or the reader stalled: take a newer snapshot.
                    info!(
                        "rpc2: pane {pane} catch-up (log lost the reader's offset): reader at \
                         {next}, log starts at {first}, ends at {}",
                        shared.total()
                    );
                    return StreamEnd::Catchup { next, stalled: false };
                }
                warn!("rpc2: pane {pane} reader at {next} is behind the log (starts at {first})");
                let _ = send.reset(LOG_LOST_CODE.into());
                return StreamEnd::Gone;
            }
        }
    }
}

/// Drop input state of clients that are not in `known`.
fn prune_unknown_clients(
    inputs: &mut HashMap<(Uuid, Uuid), Arc<InputRec>>,
    known: &std::collections::HashSet<Uuid>,
) {
    inputs.retain(|(c, _), _| known.contains(c));
}

#[cfg(test)]
mod prune_tests {
    use super::*;

    #[test]
    fn the_restored_input_sweep_keeps_clients_with_a_record() {
        let (stayed, left, never) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut inputs: HashMap<(Uuid, Uuid), Arc<InputRec>> = HashMap::new();
        for c in [stayed, left, never] {
            inputs.insert((c, Uuid::new_v4()), Arc::new(InputRec::default()));
        }
        // `left` connected and went away less than 10 minutes ago: it has a
        // client record (conns == 0) and its own timer. `never` has none.
        let known: std::collections::HashSet<Uuid> = [stayed, left].into_iter().collect();
        prune_unknown_clients(&mut inputs, &known);
        let kept: std::collections::HashSet<Uuid> = inputs.keys().map(|(c, _)| *c).collect();
        assert_eq!(kept, known);
    }
}

#[cfg(test)]
mod limiter_tests {
    use super::*;

    #[test]
    fn catchups_back_off_up_to_ten_seconds_and_recover() {
        let mut l = CatchupLimiter::default();
        assert_eq!(l.delay(), Duration::ZERO, "the first catch-up is immediate");
        // Needed again at once, over and over (the caller waits each delay
        // before the next call, so the next "now" is the previous slot).
        let mut waits = Vec::new();
        for _ in 0..6 {
            let w = l.delay();
            waits.push(w.as_secs_f64().round() as u64);
            l.last = Some(tokio::time::Instant::now()); // the caller waited
        }
        assert_eq!(waits, [2, 4, 8, 10, 10, 10]);
        // A stream that was quiet starts over with the minimum interval.
        l.last = Some(tokio::time::Instant::now() - Duration::from_secs(60));
        let w = l.delay();
        assert!(w.is_zero(), "after a quiet minute the next catch-up is immediate: {w:?}");
    }

    #[test]
    fn a_peer_that_reads_gets_a_burst_then_one_per_second_and_no_backoff() {
        let mut l = CatchupLimiter::default();
        let floor = |l: &CatchupLimiter| l.floor(KEEPING_PACE_INTERVAL);
        assert_eq!(floor(&l), Duration::ZERO);
        l.mark(tokio::time::Instant::now());
        let w = floor(&l);
        assert!(w <= KEEPING_PACE_INTERVAL && w > Duration::from_millis(50), "{w:?}");
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    /// Builds only the pacing map, which is all `catchup_delay` needs.
    fn pace_map() -> Mutex<HashMap<(iroh::EndpointId, Uuid), Pace>> {
        Mutex::new(HashMap::new())
    }

    fn node() -> iroh::EndpointId {
        iroh::SecretKey::generate(&mut rand::rng()).public()
    }

    /// The same arithmetic as `RpcV2::catchup_delay`, on a map of its own.
    fn delay(
        map: &Mutex<HashMap<(iroh::EndpointId, Uuid), Pace>>,
        n: iroh::EndpointId,
        pane: Uuid,
        keeping: bool,
    ) -> Duration {
        let claim = if keeping { Claim::Strong } else { Claim::Stalled };
        RpcV2::pace(&mut lock(map), n, PaneId(pane), claim)
    }

    fn weak(
        map: &Mutex<HashMap<(iroh::EndpointId, Uuid), Pace>>,
        n: iroh::EndpointId,
        pane: Uuid,
    ) -> Duration {
        RpcV2::pace(&mut lock(map), n, PaneId(pane), Claim::Weak)
    }

    #[tokio::test(start_paused = true)]
    async fn a_reading_peer_has_a_burst_of_six_then_one_per_second_without_backoff() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        let mut waits = Vec::new();
        for _ in 0..12 {
            let w = delay(&map, n, pane, true);
            waits.push(w);
            tokio::time::sleep(w).await; // the caller waits, then is served
        }
        // The first 6 only wait the 100 ms floor (the first not at all).
        assert!(waits[0].is_zero());
        for w in &waits[1..6] {
            assert!(*w <= KEEPING_PACE_INTERVAL, "{w:?}");
        }
        // After that the wait is the time to the next token: 1 s, always.
        for w in &waits[6..] {
            assert!(
                *w >= Duration::from_millis(900) && *w <= Duration::from_millis(1100),
                "steady state is one per second, no growth: {waits:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_peer_backs_off_to_ten_seconds_whatever_its_burst() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        let mut waits = Vec::new();
        for _ in 0..7 {
            let w = delay(&map, n, pane, false);
            waits.push(w.as_secs());
            tokio::time::sleep(w).await;
        }
        assert_eq!(waits, [0, 2, 4, 8, 10, 10, 10]);
        // A reading claim after that costs a token and ends the backoff: the
        // next stall starts again from the minimum.
        let w = delay(&map, n, pane, true);
        assert!(w <= Duration::from_secs(1), "{w:?}");
        tokio::time::sleep(w).await;
        let w = delay(&map, n, pane, false);
        assert!(w <= Duration::from_secs(2), "a reading claim ends the backoff: {w:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_honest_client_resumes_within_a_second_after_old_stalls() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        // Three stalls: the interval grows.
        let mut waits = Vec::new();
        for _ in 0..3 {
            let w = delay(&map, n, pane, false);
            waits.push(w.as_secs());
            tokio::time::sleep(w).await;
        }
        assert_eq!(waits, [0, 2, 4], "the interval grows with each stall");
        // The peer then keeps pace for 5 s, each catch-up a reading one.
        for _ in 0..5 {
            let w = delay(&map, n, pane, true);
            tokio::time::sleep(w.max(Duration::from_secs(1))).await;
        }
        // It drops and resumes: the attach goes through the token path, not
        // the backoff, and is served at once while tokens are left.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
        assert!(w <= Duration::from_millis(200), "resume waits {w:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_claim_that_proves_nothing_does_not_end_the_backoff() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        for _ in 0..5 {
            let w = delay(&map, n, pane, false);
            tokio::time::sleep(w).await;
        }
        // A peer that never reads can make weak claims all day.
        let w = weak(&map, n, pane);
        tokio::time::sleep(w).await;
        let w = delay(&map, n, pane, false);
        assert!(w >= Duration::from_secs(8), "weak claims reset the backoff: {w:?}");
        // A strong one does end it.
        tokio::time::sleep(w).await;
        let w = delay(&map, n, pane, true);
        tokio::time::sleep(w).await;
        let w = delay(&map, n, pane, false);
        assert!(w <= Duration::from_secs(2), "a strong claim kept the backoff: {w:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_only_ever_claims_a_chunk_is_backed_off_after_four() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        let mut waits = Vec::new();
        for _ in 0..12 {
            let w = RpcV2::pace(&mut lock(&map), n, PaneId(pane), Claim::Tiny);
            waits.push(w.as_secs());
            tokio::time::sleep(w).await;
        }
        // 4 are believed (100 ms apart), then the stall rules apply: 2, 4, 8, 10.
        assert!(waits[..4].iter().all(|w| *w == 0), "{waits:?}");
        assert!(waits[4..].iter().any(|w| *w >= 8), "{waits:?}");
        // One proven read gives the burst back.
        let w = delay(&map, n, pane, true);
        tokio::time::sleep(w).await;
        let w = RpcV2::pace(&mut lock(&map), n, PaneId(pane), Claim::Tiny);
        assert!(w <= Duration::from_secs(2), "{w:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_pane_leaves_no_pacing_entries() {
        let map = pace_map();
        let (n1, n2) = (node(), node());
        let (gone, kept) = (Uuid::new_v4(), Uuid::new_v4());
        for n in [n1, n2] {
            delay(&map, n, gone, true);
            delay(&map, n, kept, true);
        }
        RpcV2::forget_pane_pacing(&mut lock(&map), PaneId(gone));
        let m = lock(&map);
        assert_eq!(m.len(), 2);
        assert!(m.keys().all(|(_, p)| *p == kept));
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_six_far_behind_resumes_of_a_node_wait_for_nothing() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        for i in 0..6 {
            let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
            assert!(w < Duration::from_millis(200), "resume {i} waits {w:?}");
            tokio::time::sleep(w).await; // the caller waits, then is served
        }
        // The seventh, with no token left, waits for the backoff.
        let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
        assert!(w >= Duration::from_millis(900), "{w:?}");
        // A node with its own record is not held up by another's.
        let other = RpcV2::resume(&mut lock(&map), node(), PaneId(pane));
        assert_eq!(other, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_strong_claim_earns_a_resume_token_and_time_one_per_five_minutes() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        let tokens = |map: &Mutex<HashMap<(iroh::EndpointId, Uuid), Pace>>| {
            lock(map).get(&(n, pane)).map(|p| p.resume_tokens).unwrap()
        };
        for _ in 0..6 {
            let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
            tokio::time::sleep(w).await;
        }
        // A quiet minute earns no whole token.
        tokio::time::sleep(Duration::from_secs(60)).await;
        let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
        tokio::time::sleep(w).await;
        assert!(tokens(&map) < 1.0, "a quiet minute earned a token: {}", tokens(&map));
        // One strong claim earns one resume, at the floor.
        let w = delay(&map, n, pane, true);
        tokio::time::sleep(w).await;
        assert!(tokens(&map) >= 1.0 && tokens(&map) < 2.0, "{}", tokens(&map));
        let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
        assert!(w <= KEEPING_PACE_INTERVAL, "{w:?}");
        tokio::time::sleep(w).await;
        // No token again. Five minutes of quiet earn one, and only one.
        tokio::time::sleep(Duration::from_secs(300)).await;
        let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
        assert!(w <= KEEPING_PACE_INTERVAL, "five minutes earned no token: {w:?}");
        tokio::time::sleep(w).await;
        let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
        assert!(w >= Duration::from_millis(900), "five minutes earned two tokens: {w:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_phone_with_an_idle_pane_gets_a_token_back_every_five_minutes() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        // Eight far-behind resumes in a day, a few hours apart, no live byte
        // after any of them (so no strong claim): none waits for the pacing.
        for i in 0..8 {
            let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
            assert!(w <= KEEPING_PACE_INTERVAL, "resume {i} waits {w:?}");
            tokio::time::sleep(w + Duration::from_secs(3 * 3600)).await;
        }
        // The 6 tokens used up in a burst, a phone that comes back every five
        // minutes finds a token each time.
        let map = pace_map();
        for _ in 0..6 {
            let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
            tokio::time::sleep(w).await;
        }
        for i in 0..10 {
            tokio::time::sleep(Duration::from_secs(301)).await;
            let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
            assert!(w <= KEEPING_PACE_INTERVAL, "return {i} after 5 minutes waits {w:?}");
        }
        // A never-reading peer that comes back every 2 s for 15 minutes gets
        // its 6, the early backoff slots and a few more, not one per backoff.
        let map = pace_map();
        let t0 = tokio::time::Instant::now();
        let mut served = 0;
        while t0.elapsed() < Duration::from_secs(15 * 60) {
            let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
            if w <= Duration::from_secs(2) {
                served += 1;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        eprintln!("never-reader, 15 minutes: {served}");
        assert!(served <= 6 + 2 + 3, "{served} snapshots in 15 minutes");
    }

    #[tokio::test(start_paused = true)]
    async fn the_free_resumes_keep_the_floor_between_snapshots() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        // Six resumes in the same instant, nobody waits between them (six
        // attaches that arrive together).
        let mut slots = Vec::new();
        for _ in 0..6 {
            slots.push(RpcV2::resume(&mut lock(&map), n, PaneId(pane)));
        }
        for (i, w) in slots.iter().enumerate() {
            let want = KEEPING_PACE_INTERVAL * i as u32;
            assert!(
                *w + Duration::from_millis(1) >= want && *w <= want + Duration::from_millis(1),
                "slot {i} waits {w:?}, want {want:?}: {slots:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_eviction_runs_on_a_new_record_made_by_a_resume_too() {
        let map = pace_map();
        let old = (node(), Uuid::new_v4());
        {
            let mut m = lock(&map);
            for _ in 0..4100 {
                RpcV2::resume(&mut m, node(), PaneId(Uuid::new_v4()));
            }
            RpcV2::resume(&mut m, old.0, PaneId(old.1));
        }
        tokio::time::sleep(Duration::from_secs(400)).await;
        let mut m = lock(&map);
        RpcV2::resume(&mut m, node(), PaneId(Uuid::new_v4()));
        assert!(m.len() < 100, "the idle records stayed: {}", m.len());
    }

    #[tokio::test(start_paused = true)]
    async fn an_honest_phone_gets_every_snapshot_at_the_floor_ten_times_over() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        for round in 0..10 {
            // Far behind: the resume waits for nothing but the floor.
            let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
            assert!(w <= KEEPING_PACE_INTERVAL, "round {round} waits {w:?}");
            tokio::time::sleep(w).await;
            // It reads live bytes at a good rate (a strong claim), then drops.
            RpcV2::pace(&mut lock(&map), n, PaneId(pane), Claim::Strong);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_never_reads_gets_six_snapshots_then_the_backoff_in_a_minute() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        let t0 = tokio::time::Instant::now();
        let mut served = 0;
        // It attaches again every 2 s. An attach whose wait is longer than
        // that is replaced before its snapshot.
        while t0.elapsed() < Duration::from_secs(60) {
            let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
            if w <= Duration::from_secs(2) {
                served += 1;
                // The snapshot goes out; it never reads, the write stalls and
                // the stream asks for a catch-up (a stalled claim). That wait
                // belongs to the stream, which the next attach replaces.
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert!(served <= 6 + 3, "{served} snapshots in 60 s");
        eprintln!("never-reading peer, resumes served in 60 s: {served}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_flood_of_far_behind_resumes_is_bounded() {
        let map = pace_map();
        let (n, pane) = (node(), Uuid::new_v4());
        let t0 = tokio::time::Instant::now();
        let mut served_in_30_s = 0;
        for _ in 0..30 {
            let w = RpcV2::resume(&mut lock(&map), n, PaneId(pane));
            tokio::time::sleep(w).await;
            if t0.elapsed() <= Duration::from_secs(30) {
                served_in_30_s += 1;
            }
        }
        // 6 free, then the backoff of 2, 4, 8, 10 s.
        assert!(served_in_30_s <= 10, "{served_in_30_s} resumes served in 30 s");
        assert!(t0.elapsed() >= Duration::from_secs(100), "30 resumes took {:?}", t0.elapsed());
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;

    #[test]
    fn an_owner_without_a_stream_on_the_pane_does_not_own_the_auto_replies() {
        let (a, b, ghost) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut ps = PaneSizes { attached: vec![a, b], ..Default::default() };
        // No owner yet: the first attached client answers.
        assert!(ps.auto_allowed(a) && !ps.auto_allowed(b));
        // An owner that is attached answers.
        ps.active = Some(b);
        assert!(ps.auto_allowed(b) && !ps.auto_allowed(a));
        // An owner with no stream here (it focused a tab it never attached)
        // is no owner: the first attached client answers again.
        ps.active = Some(ghost);
        assert!(ps.auto_allowed(a) && !ps.auto_allowed(b) && !ps.auto_allowed(ghost));
    }

    #[test]
    fn the_owner_leaving_hands_the_pane_to_the_most_recent_typist() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut ps = PaneSizes { attached: vec![a, b, c], active: Some(a), ..Default::default() };
        ps.by_client.insert(b, (30, 100));
        ps.by_client.insert(c, (40, 120));
        ps.last_user.insert(b, 5);
        ps.last_user.insert(c, 9);
        assert_eq!(ps.leave(a, false), Some((40, 120)));
        assert_eq!(ps.active, Some(c));
        // Nobody left attached: no owner.
        ps.leave(b, false);
        ps.leave(c, false);
        assert_eq!(ps.active, None);
    }

    #[test]
    fn the_reported_owner_is_the_one_whose_auto_input_counts() {
        let (a, b, ghost) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut ps = PaneSizes::default();
        assert_eq!(ps.owner(), None, "a pane nobody attached has no owner");
        ps.attached = vec![a, b];
        assert_eq!(ps.owner(), Some(a), "the first attached client, until someone is active");
        ps.active = Some(b);
        assert_eq!(ps.owner(), Some(b));
        ps.active = Some(ghost);
        assert_eq!(ps.owner(), Some(a), "an owner without a stream here is no owner");
        for c in [a, b] {
            assert_eq!(ps.owner() == Some(c), ps.auto_allowed(c));
        }
        // The owner detaches: the other attached client owns the pane.
        ps.active = Some(a);
        ps.leave(a, false);
        assert_eq!(ps.owner(), Some(b));
        ps.leave(b, false);
        assert_eq!(ps.owner(), None);
    }

    #[test]
    fn wire_owner_maps_every_kind_and_an_unknown_one_to_unknown() {
        let o = |k: &str| SizeOwner {
            client_id: Uuid::nil(),
            client_kind: k.to_string(),
            client_name: String::new(),
        };
        for (s, k) in [
            ("windows", ClientKind::Windows),
            ("android", ClientKind::Android),
            ("cli", ClientKind::Cli),
            ("gtk", ClientKind::Gtk),
            ("local", ClientKind::Local),
            ("unknown", ClientKind::Unknown),
            ("something new", ClientKind::Unknown),
        ] {
            assert_eq!(wire_owner(&o(s)).client_kind, k, "{s}");
        }
    }
}

#[cfg(test)]
mod sync_slot_tests {
    use super::*;

    fn crash(slots: &SyncSlots, pane: Uuid) {
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = SyncSlotGuard { slots, pane, ended: false };
            panic!("a pass panics");
        }));
        assert!(crashed.is_err());
    }

    #[test]
    fn a_panicking_runner_frees_its_slot() {
        let slots = SyncSlots::default();
        let pane = Uuid::new_v4();
        assert!(slots.claim(pane));
        crash(&slots, pane);
        // No one waits for ever, and the next request claims the slot.
        assert!(!slots.wait_idle(pane));
        assert!(slots.claim(pane));
    }

    #[test]
    fn a_panicking_runner_with_a_dirty_mark_hands_the_final_sync_on() {
        let slots = SyncSlots::default();
        let pane = Uuid::new_v4();
        assert!(slots.claim(pane));
        assert!(!slots.claim(pane)); // a request marks it dirty
        crash(&slots, pane);
        // A waiter does not return as synced: it owns the sync now.
        assert!(slots.wait_idle(pane), "the waiter must run the lost sync");
        // Taken over once: the slot is its, and others mark it dirty again.
        assert!(!slots.claim(pane));
        assert!(slots.pass_done(pane));
        assert!(!slots.pass_done(pane));
        assert!(slots.claim(pane));
    }

    #[test]
    fn the_next_claimer_takes_an_orphaned_slot() {
        let slots = SyncSlots::default();
        let pane = Uuid::new_v4();
        assert!(slots.claim(pane));
        assert!(!slots.claim(pane));
        crash(&slots, pane);
        assert!(slots.claim(pane), "a detach after the crash runs the sync");
    }

    #[test]
    fn a_runner_that_ended_normally_leaves_the_slot_to_pass_done() {
        let slots = SyncSlots::default();
        let pane = Uuid::new_v4();
        assert!(slots.claim(pane));
        assert!(!slots.claim(pane)); // marks it dirty
        {
            let mut slot = SyncSlotGuard { slots: &slots, pane, ended: false };
            assert!(slots.pass_done(pane)); // dirty: go again
            assert!(!slots.pass_done(pane));
            slot.ended = true;
        }
        assert!(slots.claim(pane));
    }
}
