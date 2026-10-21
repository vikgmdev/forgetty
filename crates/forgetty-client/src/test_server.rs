//! In-process protocol v2 server for tests (feature `test-util`).
//!
//! It speaks exactly PROTOCOL_V2 with `forgetty-proto`: handshake, the
//! `attach_pane` / `detach_pane` / `subscribe_events` / `resize_pane` /
//! `list_clients` methods, offset resume over an in-memory byte log, input
//! echo and pairing. Panes are plain byte logs. A flat layout model answers
//! `get_layout`, `list_tabs`, `new_tab`, `split_pane`, `close_pane`,
//! `close_tab`, `focus_tab`, `rename_workspace` and `get_pane_info`, so a UI
//! can be driven against it. The server is not a model of the real one: it
//! exists to drive the client through resume, reset and drop scenarios on
//! real iroh endpoints.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use forgetty_proto::{
    cap, close_code, error_code, from_value, method, read_input_frame, read_msg, to_value,
    write_msg, write_output_frame, AttachPaneParams, ClientEntry, ControlMsg, DetachPaneParams,
    ErrorMsg, HandshakeReply, Hello, OkResult, PairRequest, PairResult, PaneStreamHeader,
    ResizePaneParams, ServerEvent, SubscribeResult, Value, Welcome, ALPN_PAIR, ALPN_RPC,
    PROTO_MAJOR, PROTO_MINOR,
};
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use tokio::sync::{broadcast, mpsc, Notify};
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::config::transport_config;

/// Application close code used by [`TestServer::drop_connections`].
pub const TEST_DROP_CODE: u32 = 0x10;

/// Test server configuration.
#[derive(Debug, Clone)]
pub struct TestServerConfig {
    /// `Some(set)`: only these node ids may connect (others get close code
    /// 0x01). `None`: everyone.
    pub authorized: Option<HashSet<EndpointId>>,
    /// `proto_major` the server answers with (to simulate a newer server).
    pub proto_major: u16,
    /// Capabilities in `Welcome`.
    pub capabilities: Vec<String>,
    /// Secret accepted on `forgetty/pair/2`. `None`: pairing disabled.
    pub pair_secret: Option<String>,
    /// Append received input to the pane log, like a PTY echo.
    pub echo_input: bool,
    /// Bytes per output frame.
    pub chunk: usize,
    /// Fixed UDP port on 127.0.0.1 (a restarted server reuses it).
    pub port: Option<u16>,
    /// Fixed identity (a restarted server keeps its node id).
    pub secret_key: Option<SecretKey>,
    /// Panes that exist from the start, with their initial log.
    pub panes: Vec<(Uuid, Vec<u8>)>,
    /// Run the server on its own 2-thread runtime, so [`TestServer::freeze`]
    /// can stop it without closing anything.
    pub own_runtime: bool,
}

impl Default for TestServerConfig {
    fn default() -> Self {
        TestServerConfig {
            authorized: None,
            proto_major: PROTO_MAJOR,
            capabilities: vec![
                cap::RESUME.to_string(),
                cap::LAYOUT_EVENTS.to_string(),
                cap::SIZE_POLICY_LATEST.to_string(),
            ],
            pair_secret: None,
            echo_input: true,
            chunk: 64 * 1024,
            port: None,
            secret_key: None,
            panes: Vec::new(),
            own_runtime: false,
        }
    }
}

/// Pane tree of one tab, shaped like the real `PaneTreeLayout`.
#[derive(Debug, Clone)]
enum Tree {
    Leaf(Uuid),
    Split { direction: String, first: Box<Tree>, second: Box<Tree> },
}

impl Tree {
    fn contains(&self, pane: Uuid) -> bool {
        match self {
            Tree::Leaf(id) => *id == pane,
            Tree::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    /// Replace the leaf `pane` with a split holding it and `new_pane`.
    fn split(&mut self, pane: Uuid, new_pane: Uuid, direction: &str) -> bool {
        match self {
            Tree::Leaf(id) if *id == pane => {
                *self = Tree::Split {
                    direction: direction.to_string(),
                    first: Box::new(Tree::Leaf(pane)),
                    second: Box::new(Tree::Leaf(new_pane)),
                };
                true
            }
            Tree::Leaf(_) => false,
            Tree::Split { first, second, .. } => {
                first.split(pane, new_pane, direction) || second.split(pane, new_pane, direction)
            }
        }
    }

    /// Remove the leaf `pane`; the sibling takes its place. `None` when the
    /// tree was only that leaf.
    fn remove(self, pane: Uuid) -> Option<Tree> {
        match self {
            Tree::Leaf(id) if id == pane => None,
            Tree::Leaf(_) => Some(self),
            Tree::Split { direction, first, second } => {
                match (first.remove(pane), second.remove(pane)) {
                    (Some(a), Some(b)) => {
                        Some(Tree::Split { direction, first: Box::new(a), second: Box::new(b) })
                    }
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }

    fn panes(&self, out: &mut Vec<Uuid>) {
        match self {
            Tree::Leaf(id) => out.push(*id),
            Tree::Split { first, second, .. } => {
                first.panes(out);
                second.panes(out);
            }
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Tree::Leaf(id) => map(&[("Leaf", map(&[("pane_id", str_val(&id.to_string()))]))]),
            Tree::Split { direction, first, second } => map(&[(
                "Split",
                map(&[
                    ("direction", str_val(direction)),
                    ("ratio", Value::F32(0.5)),
                    ("first", first.to_value()),
                    ("second", second.to_value()),
                ]),
            )]),
        }
    }
}

struct TabNode {
    id: Uuid,
    title: String,
    tree: Tree,
}

struct Layout {
    workspace: String,
    tabs: Vec<TabNode>,
}

fn str_val(s: &str) -> Value {
    Value::String(s.into())
}

fn map(entries: &[(&str, Value)]) -> Value {
    Value::Map(entries.iter().map(|(k, v)| (str_val(k), v.clone())).collect())
}

struct PaneLog {
    /// Absolute offset of `data[0]`.
    first: u64,
    data: Vec<u8>,
    notify: Arc<Notify>,
    rows: u16,
    cols: u16,
    /// Everything the clients typed into this pane, in arrival order.
    input: Vec<u8>,
}

struct State {
    cfg: TestServerConfig,
    /// Last input `seq` written per `client_id` (exactly-once input).
    input_seq: Mutex<HashMap<(Uuid, Uuid), u64>>,
    dedup_drops: AtomicU64,
    hold_acks: AtomicBool,
    server_id: Uuid,
    panes: Mutex<HashMap<Uuid, PaneLog>>,
    conns: Mutex<Vec<Connection>>,
    events: broadcast::Sender<(u64, ServerEvent)>,
    event_seq: AtomicU64,
    authorized: Mutex<Option<HashSet<EndpointId>>>,
    pair_secret: Mutex<Option<String>>,
    clients: Mutex<Vec<ClientEntry>>,
    /// `(pane, from_offset)` of every `attach_pane`, in order.
    attaches: Mutex<Vec<(Uuid, Option<u64>)>>,
    handshakes: AtomicU64,
    paused: AtomicBool,
    layout: Mutex<Layout>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A running test server.
pub struct TestServer {
    rt: Option<Arc<tokio::runtime::Runtime>>,
    endpoint: Endpoint,
    state: Arc<State>,
    addr: EndpointAddr,
    accept: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// Bind on localhost without relay and start accepting.
    pub async fn start(cfg: TestServerConfig) -> TestServer {
        let key = cfg.secret_key.clone().unwrap_or_else(|| SecretKey::generate(&mut rand::rng()));
        if !cfg.own_runtime {
            return Self::start_inner(cfg, key).await;
        }
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("server runtime"),
        );
        let mut server = rt.spawn(Self::start_inner(cfg, key)).await.expect("server start");
        server.rt = Some(rt);
        server
    }

    async fn start_inner(cfg: TestServerConfig, key: SecretKey) -> TestServer {
        let mut builder = Endpoint::empty_builder()
            .secret_key(key)
            .alpns(vec![ALPN_RPC.to_vec(), ALPN_PAIR.to_vec()])
            .transport_config(transport_config());
        if let Some(port) = cfg.port {
            builder = builder
                .clear_ip_transports()
                .bind_addr(format!("127.0.0.1:{port}").as_str())
                .expect("valid bind address");
        }
        let endpoint = builder.bind().await.expect("bind test server");
        let port = endpoint
            .bound_sockets()
            .into_iter()
            .find(|s| s.is_ipv4())
            .expect("an ipv4 socket")
            .port();
        let addr = EndpointAddr::new(endpoint.id())
            .with_ip_addr(std::net::SocketAddr::from(([127, 0, 0, 1], port)));
        let (events, _) = broadcast::channel(256);
        let state = Arc::new(State {
            input_seq: Mutex::new(HashMap::new()),
            dedup_drops: AtomicU64::new(0),
            hold_acks: AtomicBool::new(false),
            server_id: Uuid::new_v4(),
            panes: Mutex::new(HashMap::new()),
            conns: Mutex::new(Vec::new()),
            events,
            event_seq: AtomicU64::new(0),
            authorized: Mutex::new(cfg.authorized.clone()),
            pair_secret: Mutex::new(cfg.pair_secret.clone()),
            clients: Mutex::new(Vec::new()),
            attaches: Mutex::new(Vec::new()),
            handshakes: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            layout: Mutex::new(Layout { workspace: "Default".into(), tabs: Vec::new() }),
            cfg,
        });
        for (id, log) in &state.cfg.panes {
            lock(&state.panes).insert(
                *id,
                PaneLog {
                    first: 0,
                    data: log.clone(),
                    notify: Arc::new(Notify::new()),
                    rows: 24,
                    cols: 80,
                    input: Vec::new(),
                },
            );
            lock(&state.layout).tabs.push(TabNode {
                id: Uuid::new_v4(),
                title: String::new(),
                tree: Tree::Leaf(*id),
            });
        }
        let accept = tokio::spawn(accept_loop(endpoint.clone(), state.clone()));
        TestServer { rt: None, endpoint, state, addr, accept }
    }

    /// Address clients dial (direct localhost address, no relay).
    pub fn addr(&self) -> EndpointAddr {
        self.addr.clone()
    }

    /// The raw `127.0.0.1:port` this server listens on.
    pub fn udp_addr(&self) -> std::net::SocketAddr {
        *self.addr.ip_addrs().next().expect("direct address")
    }

    /// Frames dropped as duplicates (`seq <=` last written) so far.
    pub fn dedup_drops(&self) -> u64 {
        self.state.dedup_drops.load(Ordering::SeqCst)
    }

    /// While held, `Pong.input_acked` stays 0, so clients keep (and after a
    /// reconnect resend) all their input.
    pub fn hold_acks(&self, hold: bool) {
        self.state.hold_acks.store(hold, Ordering::SeqCst);
    }

    /// Create an empty pane.
    pub fn add_pane(&self) -> Uuid {
        let id = Uuid::new_v4();
        lock(&self.state.panes).insert(
            id,
            PaneLog {
                first: 0,
                data: Vec::new(),
                notify: Arc::new(Notify::new()),
                rows: 24,
                cols: 80,
                input: Vec::new(),
            },
        );
        id
    }

    /// Append PTY output to a pane log.
    pub fn append(&self, pane: Uuid, bytes: &[u8]) {
        let notify = {
            let mut panes = lock(&self.state.panes);
            let p = panes.get_mut(&pane).expect("pane exists");
            p.data.extend_from_slice(bytes);
            p.notify.clone()
        };
        notify.notify_waiters();
    }

    /// Close a pane: its output streams finish and it can no longer be
    /// attached.
    pub fn close_pane(&self, pane: Uuid) {
        remove_pane(&self.state, pane);
    }

    /// Send a `StatusChanged` event to every subscribed client.
    pub fn push_status(&self, pane: Uuid, status: &str, message: &str) {
        self.push_event(ServerEvent::StatusChanged {
            pane_id: pane,
            status: status.to_string(),
            message: message.to_string(),
        });
    }

    /// Forget log bytes before `offset` (the log now starts there).
    pub fn truncate_log_before(&self, pane: Uuid, offset: u64) {
        let mut panes = lock(&self.state.panes);
        let p = panes.get_mut(&pane).expect("pane exists");
        if offset > p.first {
            let n = ((offset - p.first) as usize).min(p.data.len());
            p.data.drain(..n);
            p.first += n as u64;
        }
    }

    /// Total bytes ever written to the pane.
    pub fn total_written(&self, pane: Uuid) -> u64 {
        let panes = lock(&self.state.panes);
        let p = panes.get(&pane).expect("pane exists");
        p.first + p.data.len() as u64
    }

    /// The pane log from its first retained byte.
    pub fn log(&self, pane: Uuid) -> Vec<u8> {
        lock(&self.state.panes).get(&pane).expect("pane exists").data.clone()
    }

    /// Everything clients typed into the pane, in arrival order.
    pub fn received_input(&self, pane: Uuid) -> Vec<u8> {
        lock(&self.state.panes).get(&pane).expect("pane exists").input.clone()
    }

    /// Last size set with `resize_pane`.
    pub fn pane_size(&self, pane: Uuid) -> (u16, u16) {
        let panes = lock(&self.state.panes);
        let p = panes.get(&pane).expect("pane exists");
        (p.rows, p.cols)
    }

    /// `(pane, from_offset)` of every `attach_pane` seen so far.
    pub fn attaches(&self) -> Vec<(Uuid, Option<u64>)> {
        lock(&self.state.attaches).clone()
    }

    /// Number of completed handshakes.
    pub fn handshakes(&self) -> u64 {
        self.state.handshakes.load(Ordering::SeqCst)
    }

    /// While paused, new connections stall before the handshake reply, as if
    /// the server were unreachable. Existing connections are not touched.
    pub fn pause_handshakes(&self, paused: bool) {
        self.state.paused.store(paused, Ordering::SeqCst);
    }

    /// Close every client connection with [`TEST_DROP_CODE`], mid-stream.
    pub fn drop_connections(&self) {
        let conns: Vec<_> = lock(&self.state.conns).drain(..).collect();
        for c in conns {
            c.close(TEST_DROP_CODE.into(), b"test drop");
        }
    }

    /// Send an event to every subscribed client.
    pub fn push_event(&self, event: ServerEvent) {
        let seq = self.state.event_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.state.events.send((seq, event));
    }

    /// Allow a node id to connect (when `authorized` is `Some`).
    pub fn authorize(&self, id: EndpointId) {
        if let Some(set) = lock(&self.state.authorized).as_mut() {
            set.insert(id);
        }
    }

    /// Stop the server.
    pub async fn shutdown(self) {
        self.accept.abort();
        self.drop_connections();
        self.endpoint.close().await;
        if let Some(rt) = self.rt {
            if let Ok(rt) = Arc::try_unwrap(rt) {
                rt.shutdown_background();
            }
        }
    }

    /// Stop the server for `duration` without closing anything: it neither
    /// reads nor writes a packet, like a dead network or a hung process. Needs
    /// `own_runtime`. Queued packets are handled when it thaws.
    pub fn freeze(&self, duration: Duration) {
        let rt = self.rt.as_ref().expect("freeze needs TestServerConfig::own_runtime");
        for _ in 0..2 {
            rt.spawn(async move { std::thread::sleep(duration) });
        }
    }
}

async fn accept_loop(endpoint: Endpoint, state: Arc<State>) {
    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            let alpn = conn.alpn().to_vec();
            if alpn == ALPN_RPC {
                handle_rpc(conn, state).await;
            } else if alpn == ALPN_PAIR {
                handle_pair(conn, state).await;
            } else {
                conn.close(1u8.into(), b"unknown-alpn");
            }
        });
    }
}

async fn handle_pair(conn: Connection, state: Arc<State>) {
    let remote = conn.remote_id();
    let Ok((mut send, mut recv)) = conn.accept_bi().await else { return };
    let req: Option<PairRequest> = read_msg(&mut recv).await.ok().flatten();
    let result = match req {
        None => PairResult { ok: false, reason: Some("bad_request".into()) },
        Some(req) => {
            let mut secret = lock(&state.pair_secret);
            match secret.as_deref() {
                None => PairResult { ok: false, reason: Some("expired".into()) },
                Some(s) if s.eq_ignore_ascii_case(&req.secret) => {
                    *secret = None; // one device per window
                    if let Some(set) = lock(&state.authorized).as_mut() {
                        set.insert(remote);
                    }
                    PairResult { ok: true, reason: None }
                }
                Some(_) => PairResult { ok: false, reason: Some("bad_secret".into()) },
            }
        }
    };
    let _ = write_msg(&mut send, &result).await;
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    conn.close(0u8.into(), b"pair-done");
}

async fn handle_rpc(conn: Connection, state: Arc<State>) {
    let remote = conn.remote_id();
    let allowed = lock(&state.authorized).as_ref().is_none_or(|set| set.contains(&remote));
    if !allowed {
        conn.close(close_code::NOT_AUTHORIZED.into(), b"not_authorized");
        return;
    }
    let Ok((mut send, mut recv)) = conn.accept_bi().await else { return };
    let hello: Hello = match read_msg(&mut recv).await {
        Ok(Some(h)) => h,
        _ => {
            conn.close(0u8.into(), b"bad hello");
            return;
        }
    };
    while state.paused.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
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
        conn.close(0u8.into(), b"proto_mismatch");
        return;
    }
    let welcome = Welcome {
        proto_major: state.cfg.proto_major,
        proto_minor: PROTO_MINOR,
        server_id: state.server_id,
        server_version: "test".into(),
        hostname: "test-host".into(),
        capabilities: state.cfg.capabilities.clone(),
    };
    if write_msg(&mut send, &HandshakeReply::Welcome(welcome)).await.is_err() {
        return;
    }
    state.handshakes.fetch_add(1, Ordering::SeqCst);
    lock(&state.conns).push(conn.clone());
    let entry = ClientEntry {
        client_id: hello.client_id,
        client_kind: hello.client_kind,
        client_name: hello.client_name.clone(),
        connected_at: "1970-01-01T00:00:00Z".into(),
    };
    lock(&state.clients).push(entry);

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ControlMsg>();
    let mut tasks: JoinSet<()> = JoinSet::new();

    // Control writer.
    tasks.spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });

    // Input reader: the client's first uni stream.
    {
        let conn = conn.clone();
        let state = state.clone();
        let client_id = hello.client_id;
        let input_epoch = hello.input_epoch;
        tasks.spawn(async move {
            let Ok(mut input) = conn.accept_uni().await else { return };
            while let Ok(Some((pane, seq, bytes))) = read_input_frame(&mut input).await {
                // Dedup and write under one lock, so a resend on a new
                // connection can never overtake or double a frame.
                let mut seqs = lock(&state.input_seq);
                let last = seqs.entry((client_id, input_epoch)).or_insert(0);
                if seq <= *last {
                    state.dedup_drops.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
                *last = seq;
                let notify = {
                    let mut panes = lock(&state.panes);
                    let Some(p) = panes.get_mut(&pane) else { continue };
                    p.input.extend_from_slice(&bytes);
                    if state.cfg.echo_input {
                        p.data.extend_from_slice(&bytes);
                    }
                    p.notify.clone()
                };
                drop(seqs);
                notify.notify_waiters();
            }
        });
    }

    // Control request loop, in this task.
    let mut streams: HashMap<Uuid, tokio::task::AbortHandle> = HashMap::new();
    let mut events_subscribed = false;
    let mut event_rx = state.events.subscribe();
    loop {
        tokio::select! {
            msg = read_msg::<_, ControlMsg>(&mut recv) => {
                let Ok(Some(msg)) = msg else { break };
                if let ControlMsg::Ping { nonce } = msg {
                    let input_acked = if state.hold_acks.load(Ordering::SeqCst) {
                        0
                    } else {
                        lock(&state.input_seq)
                            .get(&(hello.client_id, hello.input_epoch))
                            .copied()
                            .unwrap_or(0)
                    };
                    let _ = out_tx.send(ControlMsg::Pong { nonce, input_acked });
                    continue;
                }
                let ControlMsg::Request { id, method: m, params } = msg else { continue };
                let reply = handle_request(
                    &conn, &state, &mut tasks, &mut streams, &mut events_subscribed, &m, params,
                );
                let _ = out_tx.send(match reply {
                    Ok(result) => ControlMsg::Response { id, result },
                    Err((code, message)) => ControlMsg::Failure { id, code: code.into(), message },
                });
            }
            ev = event_rx.recv(), if events_subscribed => {
                if let Ok((seq, event)) = ev {
                    let _ = out_tx.send(ControlMsg::Event { seq, event });
                }
            }
        }
    }
    tasks.abort_all();
    for (_, h) in streams {
        h.abort();
    }
    lock(&state.clients).retain(|c| c.client_id != hello.client_id);
    conn.close(0u8.into(), b"bye");
}

type Reply = Result<Value, (&'static str, String)>;

fn handle_request(
    conn: &Connection,
    state: &Arc<State>,
    tasks: &mut JoinSet<()>,
    streams: &mut HashMap<Uuid, tokio::task::AbortHandle>,
    events_subscribed: &mut bool,
    m: &str,
    params: Value,
) -> Reply {
    let bad = |e: forgetty_proto::FrameError| (error_code::BAD_REQUEST, e.to_string());
    match m {
        method::SUBSCRIBE_EVENTS => {
            *events_subscribed = true;
            let seq = state.event_seq.load(Ordering::SeqCst);
            to_value(&SubscribeResult { seq }).map_err(bad)
        }
        method::ATTACH_PANE => {
            let p: AttachPaneParams = from_value(params).map_err(bad)?;
            let (start, reset, rows, cols, notify) = {
                let panes = lock(&state.panes);
                let Some(log) = panes.get(&p.pane_id) else {
                    return Err((error_code::NOT_FOUND, "no such pane".into()));
                };
                let total = log.first + log.data.len() as u64;
                // PROTOCOL_V2 section 8.
                let (start, reset) = match p.from_offset {
                    None => (log.first, true),
                    Some(o) if o > total => (log.first, true),
                    Some(o) if o < log.first => (log.first, true),
                    Some(o) => (o, false),
                };
                (start, reset, log.rows, log.cols, log.notify.clone())
            };
            lock(&state.attaches).push((p.pane_id, p.from_offset));
            if let Some(old) = streams.remove(&p.pane_id) {
                old.abort();
            }
            let handle = tasks.spawn(pane_stream(
                conn.clone(),
                state.clone(),
                notify,
                PaneStreamHeader {
                    pane_id: p.pane_id,
                    start_offset: start,
                    reset,
                    rows,
                    cols,
                    snapshot_len: 0,
                },
            ));
            streams.insert(p.pane_id, handle);
            to_value(&OkResult { ok: true }).map_err(bad)
        }
        method::DETACH_PANE => {
            let p: DetachPaneParams = from_value(params).map_err(bad)?;
            if let Some(h) = streams.remove(&p.pane_id) {
                h.abort();
            }
            to_value(&OkResult { ok: true }).map_err(bad)
        }
        method::RESIZE_PANE => {
            let p: ResizePaneParams = from_value(params).map_err(bad)?;
            let mut panes = lock(&state.panes);
            let Some(log) = panes.get_mut(&p.pane_id) else {
                return Err((error_code::NOT_FOUND, "no such pane".into()));
            };
            log.rows = p.rows;
            log.cols = p.cols;
            drop(panes);
            // AD-020: the latest client's size is the PTY size.
            let seq = state.event_seq.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = state.events.send((
                seq,
                ServerEvent::PaneResized { pane_id: p.pane_id, rows: p.rows, cols: p.cols },
            ));
            to_value(&OkResult { ok: true }).map_err(bad)
        }
        method::LIST_CLIENTS => {
            let list: Vec<Value> =
                lock(&state.clients).iter().map(|c| to_value(c).unwrap_or(Value::Nil)).collect();
            Ok(Value::Array(list))
        }
        "get_layout" => Ok(layout_value(state)),
        "list_tabs" => {
            let layout = lock(&state.layout);
            let panes = lock(&state.panes);
            let mut ids = Vec::new();
            for t in &layout.tabs {
                t.tree.panes(&mut ids);
            }
            let tabs: Vec<Value> = ids
                .iter()
                .filter_map(|id| panes.get(id).map(|p| (id, p)))
                .map(|(id, p)| {
                    map(&[
                        ("pane_id", str_val(&id.to_string())),
                        ("pid", Value::from(1u32)),
                        ("rows", Value::from(p.rows)),
                        ("cols", Value::from(p.cols)),
                        ("cwd", str_val("/home/test")),
                        ("title", str_val(&format!("pane {}", &id.to_string()[..8]))),
                    ])
                })
                .collect();
            Ok(map(&[("tabs", Value::Array(tabs))]))
        }
        "new_tab" => {
            let pane = new_pane_log(state, 24, 80);
            let tab_id = Uuid::new_v4();
            lock(&state.layout).tabs.push(TabNode {
                id: tab_id,
                title: String::new(),
                tree: Tree::Leaf(pane),
            });
            layout_changed(state);
            Ok(map(&[
                ("tab_id", str_val(&tab_id.to_string())),
                ("pane_id", str_val(&pane.to_string())),
            ]))
        }
        "split_pane" => {
            let pane = param_uuid(&params, "pane_id")?;
            let direction = param_str(&params, "direction").unwrap_or("horizontal");
            if !lock(&state.panes).contains_key(&pane) {
                return Err((error_code::NOT_FOUND, "no such pane".into()));
            }
            let new_pane = new_pane_log(state, 24, 80);
            let split = lock(&state.layout)
                .tabs
                .iter_mut()
                .any(|t| t.tree.contains(pane) && t.tree.split(pane, new_pane, direction));
            if !split {
                remove_pane(state, new_pane);
                return Err((error_code::NOT_FOUND, "pane is in no tab".into()));
            }
            layout_changed(state);
            Ok(map(&[("pane_id", str_val(&new_pane.to_string()))]))
        }
        "close_pane" => {
            let pane = param_uuid(&params, "pane_id")?;
            if !lock(&state.panes).contains_key(&pane) {
                return Err((error_code::NOT_FOUND, "no such pane".into()));
            }
            remove_pane(state, pane);
            Ok(map(&[("ok", Value::Boolean(true))]))
        }
        "close_tab" => {
            let tab = param_uuid(&params, "tab_id")?;
            let mut ids = Vec::new();
            {
                let layout = lock(&state.layout);
                match layout.tabs.iter().find(|t| t.id == tab) {
                    Some(t) => t.tree.panes(&mut ids),
                    None => return Err((error_code::NOT_FOUND, "no such tab".into())),
                }
            }
            for id in ids {
                remove_pane(state, id);
            }
            Ok(map(&[("ok", Value::Boolean(true))]))
        }
        "focus_tab" => {
            param_uuid(&params, "tab_id")?;
            Ok(map(&[("ok", Value::Boolean(true))]))
        }
        "rename_workspace" => {
            let name = param_str(&params, "name")
                .ok_or((error_code::BAD_REQUEST, "name is required".to_string()))?
                .to_string();
            lock(&state.layout).workspace = name;
            layout_changed(state);
            Ok(map(&[("ok", Value::Boolean(true))]))
        }
        "get_pane_info" => {
            let pane = param_uuid(&params, "pane_id")?;
            let panes = lock(&state.panes);
            let Some(p) = panes.get(&pane) else {
                return Err((error_code::NOT_FOUND, "no such pane".into()));
            };
            Ok(map(&[
                ("pane_id", str_val(&pane.to_string())),
                ("rows", Value::from(p.rows)),
                ("cols", Value::from(p.cols)),
                ("title", str_val(&format!("pane {}", &pane.to_string()[..8]))),
                ("cwd", str_val("/home/test")),
                ("pid", Value::from(1u32)),
            ]))
        }
        _ => Err((error_code::UNKNOWN_METHOD, format!("unknown method {m}"))),
    }
}

fn param_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.as_map()?.iter().find(|(k, _)| k.as_str() == Some(key))?.1.as_str()
}

fn param_uuid(params: &Value, key: &str) -> Result<Uuid, (&'static str, String)> {
    param_str(params, key)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or((error_code::BAD_REQUEST, format!("{key} must be a uuid string")))
}

fn new_pane_log(state: &Arc<State>, rows: u16, cols: u16) -> Uuid {
    let id = Uuid::new_v4();
    lock(&state.panes).insert(
        id,
        PaneLog {
            first: 0,
            data: Vec::new(),
            notify: Arc::new(Notify::new()),
            rows,
            cols,
            input: Vec::new(),
        },
    );
    id
}

/// Drop a pane from the byte logs and the layout, and tell subscribers.
fn remove_pane(state: &Arc<State>, pane: Uuid) {
    let notify = lock(&state.panes).remove(&pane).map(|p| p.notify);
    if let Some(n) = notify {
        n.notify_waiters();
    }
    {
        let mut layout = lock(&state.layout);
        let tabs = std::mem::take(&mut layout.tabs);
        layout.tabs = tabs
            .into_iter()
            .filter_map(|t| {
                let TabNode { id, title, tree } = t;
                tree.remove(pane).map(|tree| TabNode { id, title, tree })
            })
            .collect();
    }
    let seq = state.event_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = state.events.send((seq, ServerEvent::PaneClosed { pane_id: pane }));
    layout_changed(state);
}

fn layout_value(state: &Arc<State>) -> Value {
    let layout = lock(&state.layout);
    let tabs: Vec<Value> = layout
        .tabs
        .iter()
        .map(|t| {
            let mut ids = Vec::new();
            t.tree.panes(&mut ids);
            let active = ids.first().map(|id| str_val(&id.to_string())).unwrap_or(Value::Nil);
            map(&[
                ("id", str_val(&t.id.to_string())),
                ("title", str_val(&t.title)),
                ("pane_tree", t.tree.to_value()),
                ("active_pane_id", active),
            ])
        })
        .collect();
    map(&[
        (
            "workspaces",
            Value::Array(vec![map(&[
                ("id", str_val("00000000-0000-0000-0000-000000000001")),
                ("name", str_val(&layout.workspace)),
                ("tabs", Value::Array(tabs)),
                ("active_tab", Value::from(0u32)),
                ("color", Value::Nil),
            ])]),
        ),
        ("active_workspace", Value::from(0u32)),
    ])
}

fn layout_changed(state: &Arc<State>) {
    let layout = layout_value(state);
    let seq = state.event_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = state.events.send((seq, ServerEvent::LayoutChanged { layout }));
}

/// One Pane output stream: header, then raw frames from the log.
async fn pane_stream(
    conn: Connection,
    state: Arc<State>,
    notify: Arc<Notify>,
    header: PaneStreamHeader,
) {
    let Ok(mut send) = conn.open_uni().await else { return };
    if write_msg(&mut send, &header).await.is_err() {
        return;
    }
    let mut next = header.start_offset;
    loop {
        let wake = notify.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        // `None`: pane closed. `Some(empty)`: nothing new yet.
        let chunk: Option<Vec<u8>> = {
            let panes = lock(&state.panes);
            match panes.get(&header.pane_id) {
                None => None,
                Some(log) => {
                    if next < log.first {
                        // The log moved past us: skip ahead (a real server
                        // rewinds from disk; the tests do not truncate under
                        // a live stream).
                        next = log.first;
                    }
                    let total = log.first + log.data.len() as u64;
                    if next < total {
                        let from = (next - log.first) as usize;
                        let to = (from + state.cfg.chunk).min(log.data.len());
                        Some(log.data[from..to].to_vec())
                    } else {
                        Some(Vec::new())
                    }
                }
            }
        };
        match chunk {
            None => {
                // Pane closed: finish the stream cleanly (FIN).
                let _ = send.finish();
                let _ = send.stopped().await;
                return;
            }
            Some(bytes) if !bytes.is_empty() => {
                if write_output_frame(&mut send, &bytes).await.is_err() {
                    return;
                }
                next += bytes.len() as u64;
            }
            Some(_) => wake.await,
        }
    }
}
