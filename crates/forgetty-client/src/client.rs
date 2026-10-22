//! Connection supervisor, request correlation, input queue and handle.
//!
//! Layout of one connection (a "session"):
//!
//! - Control bi stream: a writer task (frames from `Link::control_tx`) and a
//!   reader task (Response / Failure / Event).
//! - Input uni stream: a writer task draining the shared input queue.
//! - Pane output uni streams: an acceptor task hands each stream to a
//!   `pane_reader` task, which feeds the `PaneStream` channel and tracks the
//!   absolute offset.
//! - An init task re-subscribes events and re-attaches every live pane with
//!   its last offset, then releases the input writer.
//!
//! The supervisor owns the sessions. When one ends it aborts and joins every
//! task of that session before it redials, so no stale reader can touch pane
//! offsets afterwards.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use forgetty_proto::{
    cap, close_code, decode_msg, encode_input_frame, encode_input_frame_flags, encode_msg,
    error_code, from_value, method, read_frame_into, read_msg, read_output_frame, to_value,
    write_frame, write_msg, AttachPaneParams, ControlMsg, DetachPaneParams, HandshakeReply, Hello,
    PaneStreamHeader, ResizePaneParams, ServerEvent, Value, Welcome, ALPN_RPC, INPUT_BUFFER_CAP,
    INPUT_FLAG_AUTO, NETWORK_CHANGE_PONG_MS, PING_INTERVAL_MS, PONG_DEAD_AFTER_MS, PROTO_MAJOR,
    RECONNECT_BACKOFF_MS,
};
use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use tokio::sync::{broadcast, mpsc, oneshot, watch, Notify};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::addr_cache::AddrCache;
use crate::config::{transport_config, ClientInfo, ClientOptions};
use crate::error::ClientError;
use crate::state::ConnectionState;

/// QUIC send priority of the control and input streams. Pane output uses the
/// default 0, so a keystroke or a request is never queued behind output.
const HIGH_PRIORITY: i32 = 100;

/// Largest input slice per frame. With the 64 KiB cap that is exactly four
/// frames.
const INPUT_CHUNK: usize = 16 * 1024;

/// A session shorter than this counts as a failed dial for backoff purposes.
const MIN_HEALTHY_SESSION: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Public item types
// ---------------------------------------------------------------------------

/// One item of a [`PaneStream`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneItem {
    /// The server asked for a VT reset: first attach, or the offset was older
    /// than the log. Reset the terminal state, then apply the bytes that
    /// follow. `rows` and `cols` are the PTY size now.
    Reset {
        /// PTY rows.
        rows: u16,
        /// PTY columns.
        cols: u16,
    },
    /// Snapshot catch-up (AD-023). The client fell behind and the server sent
    /// the current screen as VT bytes instead of the bytes it missed. It
    /// always follows a `Reset`. Apply `bytes` to the reset terminal. They are
    /// not pane bytes: `offset` is where the pane stood, and the next `Data`
    /// starts there.
    Snapshot {
        /// Pane offset the snapshot stands at.
        offset: u64,
        /// VT bytes.
        bytes: Vec<u8>,
    },
    /// PTY bytes. `offset` is the absolute pane offset of `bytes[0]`.
    Data {
        /// Absolute offset of the first byte.
        offset: u64,
        /// The bytes.
        bytes: Vec<u8>,
    },
    /// The server finished the stream: the pane closed (or no longer exists).
    Ended,
}

/// One item of an [`EventStream`].
#[derive(Debug, Clone, PartialEq)]
pub enum EventItem {
    /// An event from the server.
    Server {
        /// Event sequence number.
        seq: u64,
        /// The event.
        event: ServerEvent,
    },
    /// Events may have been missed (reconnect or lag). Refetch `get_layout`.
    Resynced,
    /// The server restarted (its `Welcome.boot_id` changed) while this client
    /// held input it had not seen acked. The programs that input was for are
    /// gone, so the client dropped it instead of typing it into a fresh shell.
    /// The user should be told that `bytes` bytes were not sent.
    InputLost {
        /// Bytes of input dropped.
        bytes: u64,
    },
}

/// Stream of server events. Created by [`ClientHandle::events`].
pub struct EventStream {
    rx: broadcast::Receiver<EventItem>,
}

impl EventStream {
    /// Next event. `None` after the client is closed.
    pub async fn next(&mut self) -> Option<EventItem> {
        match self.rx.recv().await {
            Ok(item) => Some(item),
            Err(broadcast::error::RecvError::Lagged(_)) => Some(EventItem::Resynced),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

type Pending = Result<Value, ClientError>;

/// One live connection, as seen by request callers.
pub(crate) struct Link {
    gen: u64,
    control_tx: mpsc::UnboundedSender<Vec<u8>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Pending>>>,
    caps: HashSet<String>,
    conn: Connection,
    /// Set when `session_init` finished: events and panes are resumed.
    resumed: AtomicBool,
    /// When the last `Pong` arrived (session start counts as one).
    last_pong: Mutex<Instant>,
    /// Highest `Ping` nonce answered so far.
    pong_tx: watch::Sender<u64>,
}

impl Link {
    fn has_cap(&self, c: &str) -> bool {
        self.caps.contains(c)
    }

    fn complete(&self, id: u64, result: Pending) {
        if let Some(tx) = lock(&self.pending).remove(&id) {
            let _ = tx.send(result);
        }
    }

    fn fail_all(&self, err: ClientError) {
        let drained: Vec<_> = lock(&self.pending).drain().collect();
        for (_, tx) in drained {
            let _ = tx.send(Err(err.clone()));
        }
    }
}

pub(crate) struct PaneState {
    /// Session generation that owns the stream now.
    gen: u64,
    /// Next expected absolute offset. `None` until the first header.
    offset: Option<u64>,
    /// Number of the newest stream that opened with a reset. Items of older
    /// streams still queued for the consumer are stale and are dropped.
    reset_stream: u64,
    /// Counts the pane streams read so far. A newer stream (snapshot
    /// catch-up, re-attach) takes the pane over from an older one.
    stream: u64,
    /// `PaneStreamHeader.replay_end` of the newest stream: the pane offset where the replay
    /// ends and live bytes start. `None` from a server that does not send it.
    replay_end: Option<u64>,
}

/// A queued item with the number of the stream it came from.
pub(crate) struct Tagged {
    stream: u64,
    item: PaneItem,
}

/// Tag of items that are never stale (`Ended`).
const ALWAYS: u64 = u64::MAX;

pub(crate) struct PaneShared {
    id: Uuid,
    tx: mpsc::Sender<Tagged>,
    state: Mutex<PaneState>,
    /// Generation of the link this pane is attached on (0 = none). Held across
    /// the attach request so two paths never attach twice.
    attached_gen: Arc<tokio::sync::Mutex<u64>>,
}

/// One input frame kept until the server acks it.
struct InFrame {
    seq: u64,
    pane: Uuid,
    /// The input bytes. The wire payload is built when the frame is sent, because a reconnect
    /// can land on a server with a different `input_flags` answer.
    bytes: Arc<[u8]>,
    /// AD-025: a terminal reply or focus report, not typing. The wire flag is sent only
    /// when both sides list `input_flags`.
    auto: bool,
}

impl InFrame {
    /// `len` counts toward the 64 KiB buffer.
    fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Wire payload for a link that did (`flags`) or did not negotiate `input_flags`.
    fn payload(&self, flags: bool) -> Vec<u8> {
        if flags {
            encode_input_frame_flags(
                self.pane,
                self.seq,
                if self.auto { INPUT_FLAG_AUTO } else { 0 },
                &self.bytes,
            )
        } else {
            encode_input_frame(self.pane, self.seq, &self.bytes)
        }
    }
}

/// Where to cut `b` so that a piece of at most `max` bytes never ends inside a UTF-8 character
/// or inside an escape sequence (CSI, SS3, OSC). A piece is queued, sent and acked whole, so
/// the PTY never sees half a sequence between two frames.
fn cut_point(b: &[u8], max: usize) -> usize {
    if b.len() <= max {
        return b.len();
    }
    let mut n = max;
    while n > 0 && (b[n] & 0xC0) == 0x80 {
        n -= 1;
    }
    let lo = n.saturating_sub(64);
    if let Some(i) = b[lo..n].iter().rposition(|&c| c == 0x1b).map(|i| lo + i) {
        let seq = &b[i..n];
        let open = match seq.get(1) {
            None => true,
            Some(b'[') => !seq[2..].iter().any(|c| (0x40..=0x7e).contains(c)),
            Some(b'O') => seq.len() < 3,
            Some(b']') => {
                !seq[2..].contains(&0x07) && !seq.windows(2).any(|w| w == [0x1b, b'\\'])
            }
            Some(_) => false,
        };
        if open && i > 0 {
            return i;
        }
    }
    if n == 0 {
        max
    } else {
        n
    }
}

fn split_input(mut b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let n = cut_point(b, INPUT_CHUNK);
        let (head, tail) = b.split_at(n);
        out.push(head);
        b = tail;
    }
    out
}

struct InputState {
    /// Unacked frames in `seq` order.
    frames: VecDeque<InFrame>,
    /// Sum of `len` over `frames`.
    bytes: usize,
    next_seq: u64,
}

struct InputQueue {
    state: Mutex<InputState>,
    ready: Notify,
    space: Notify,
}

impl InputQueue {
    /// Drop every frame the server wrote to a PTY (`seq <= acked`).
    fn ack(&self, acked: u64) {
        let mut st = lock(&self.state);
        let mut freed = false;
        while st.frames.front().is_some_and(|f| f.seq <= acked) {
            let f = st.frames.pop_front().expect("front checked");
            st.bytes -= f.len();
            freed = true;
        }
        drop(st);
        if freed {
            self.space.notify_waiters();
        }
    }
}

/// An iroh endpoint and the number of the bind that made it. Session state
/// (pane offsets, unacked input, subscriptions, `boot_id`) lives in [`Shared`],
/// above the endpoint, so a new endpoint loses none of it.
#[derive(Clone)]
struct Ep {
    /// Shared by the dials, the connect attempts and the network probe. The
    /// count of clones is how a replace knows that none is left.
    ep: Arc<Endpoint>,
    gen: u64,
}

/// The tasks that may hold a clone of the endpoint after the dial that made
/// them is gone: retired connect attempts and the network probe. A replace
/// aborts them all before it closes the endpoint.
#[derive(Default)]
struct HolderBin(Mutex<Vec<tokio::task::AbortHandle>>);

impl HolderBin {
    fn add(&self, h: tokio::task::AbortHandle) {
        let mut v = lock(&self.0);
        v.retain(|h| !h.is_finished());
        v.push(h);
    }

    fn abort_all(&self) {
        for h in lock(&self.0).drain(..) {
            h.abort();
        }
    }
}

/// A network change that leaves no connection this long afterwards makes the
/// client replace its endpoint: close the old one, bind a fresh one with the
/// same key and dial with that. Never two at once, so the endpoint id is
/// registered once at the relay. iroh rebinds its UDP socket only after its own
/// network probe, which can take 4 to 52 s.
///
/// Longer than the Pong limit of `network_changed` (1000 ms), so a Pong that
/// lands right at the limit does not start a needless replace.
const NETWORK_FALLBACK: Duration = Duration::from_millis(1500);

async fn bind_endpoint(
    opts: &ClientOptions,
    server: &EndpointAddr,
    identity: SecretKey,
) -> Result<Endpoint, String> {
    // An empty builder: no n0 discovery service. The client dials with the
    // relay URLs of the pairing payload and the cached direct address.
    opts.shape(Endpoint::empty_builder())
        .secret_key(identity)
        .relay_mode(opts.relay.mode(server))
        .transport_config(transport_config())
        .bind()
        .await
        .map_err(|e| e.to_string())
}

pub(crate) struct Shared {
    /// The endpoint dials use now. Swapped when a fresh one wins the race.
    endpoint: Mutex<Option<Ep>>,
    /// The key every endpoint of this client is bound with.
    identity: SecretKey,
    /// Set by the network-change fallback to the value of `connected_count` it
    /// saw (`u64::MAX` when nothing is asked): the supervisor replaces the
    /// endpoint before its next dial, if no connection was made since.
    replace_for: AtomicU64,
    next_ep_gen: AtomicU64,
    /// Endpoints bound and not yet closed, and the most there ever were. One,
    /// always (checked by the tests).
    live_eps: AtomicUsize,
    max_live_eps: AtomicUsize,
    /// Tasks that hold clones of the endpoint (see [`HolderBin`]).
    holders: Arc<HolderBin>,
    /// The id of the endpoint: the same for every endpoint of this client.
    endpoint_id: iroh::EndpointId,
    /// Test hook: clones of the old endpoint still alive when the new one was
    /// bound (`usize::MAX` before any replace). Must be 0.
    clones_at_bind: AtomicUsize,
    /// Connections established so far.
    connected_count: AtomicU64,
    /// Test hook: dials on the endpoint of this bind number never connect.
    stuck_gen: AtomicU64,
    /// Test hook: the next this many binds fail, and how many did.
    fail_binds: AtomicU32,
    injected_bind_failures: AtomicU32,
    server: EndpointAddr,
    info: ClientInfo,
    opts: ClientOptions,
    state_tx: watch::Sender<ConnectionState>,
    link_tx: watch::Sender<Option<Arc<Link>>>,
    next_req: AtomicU64,
    next_gen: AtomicU64,
    panes: Mutex<HashMap<Uuid, Arc<PaneShared>>>,
    input: InputQueue,
    events_tx: broadcast::Sender<EventItem>,
    events_wanted: AtomicBool,
    events_gen: Arc<tokio::sync::Mutex<u64>>,
    ever_connected: AtomicBool,
    welcome: Mutex<Option<Welcome>>,
    /// `Welcome.boot_id` of the last connection.
    boot_id: Mutex<Option<Uuid>>,
    /// Input bytes dropped because the server restarted, in total.
    input_lost: AtomicU64,
    shutdown_tx: watch::Sender<bool>,
    /// Wakes the supervisor: cancels an in-flight dial or a backoff sleep and
    /// dials at once.
    kick: Notify,
    /// Wakes the supervisor from a backoff sleep only. Never aborts a dial in
    /// flight: an app that calls `reconnect_now` every few hundred ms while
    /// Reconnecting must not cancel a handshake that takes longer than that.
    wake_backoff: Notify,
    /// Set by `network_changed`: the next redial skips the backoff and the
    /// failure count starts again at 0.
    reset_backoff: AtomicBool,
    /// Wakes the liveness task for an immediate `Ping`.
    ping_now: Notify,
    next_nonce: AtomicU64,
    /// Random per `spawn`: the `Hello.input_epoch`.
    input_epoch: Uuid,
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// Entry points of the crate.
pub struct Client;

impl Client {
    /// Connect with default options and wait for the first handshake.
    ///
    /// Returns `Err` when the connection fails for good (`proto_mismatch`,
    /// `not_authorized`). A server that is merely unreachable keeps the call
    /// waiting while the supervisor retries: wrap the call in a timeout, or
    /// use [`Client::spawn`] to get the handle at once.
    pub async fn connect(
        server: EndpointAddr,
        identity: SecretKey,
        info: ClientInfo,
    ) -> Result<ClientHandle, ClientError> {
        Self::connect_with(server, identity, info, ClientOptions::default()).await
    }

    /// [`Client::connect`] with explicit options.
    pub async fn connect_with(
        server: EndpointAddr,
        identity: SecretKey,
        info: ClientInfo,
        opts: ClientOptions,
    ) -> Result<ClientHandle, ClientError> {
        let handle = Self::spawn(server, identity, info, opts).await?;
        let mut rx = handle.watch_state();
        loop {
            let state = rx.borrow_and_update().clone();
            match state {
                ConnectionState::Connected => return Ok(handle),
                ConnectionState::Failed(reason) => return Err(ClientError::Failed(reason)),
                ConnectionState::Closed => return Err(ClientError::Closed),
                ConnectionState::Reconnecting { .. } => {}
            }
            if rx.changed().await.is_err() {
                return Err(ClientError::Closed);
            }
        }
    }

    /// Bind the endpoint, start the supervisor and return at once. The state
    /// starts as `Reconnecting`; watch it with [`ClientHandle::watch_state`].
    pub async fn spawn(
        server: EndpointAddr,
        identity: SecretKey,
        info: ClientInfo,
        opts: ClientOptions,
    ) -> Result<ClientHandle, ClientError> {
        let endpoint =
            bind_endpoint(&opts, &server, identity.clone()).await.map_err(ClientError::Setup)?;

        let endpoint_id = endpoint.id();
        let (state_tx, _) = watch::channel(ConnectionState::Reconnecting { since: Instant::now() });
        let (link_tx, _) = watch::channel(None);
        let (events_tx, _) = broadcast::channel(opts.event_buffer.max(16));
        let (shutdown_tx, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            endpoint: Mutex::new(Some(Ep { ep: Arc::new(endpoint), gen: 0 })),
            identity,
            replace_for: AtomicU64::new(u64::MAX),
            next_ep_gen: AtomicU64::new(1),
            live_eps: AtomicUsize::new(1),
            max_live_eps: AtomicUsize::new(1),
            holders: Arc::new(HolderBin::default()),
            endpoint_id,
            clones_at_bind: AtomicUsize::new(usize::MAX),
            connected_count: AtomicU64::new(0),
            stuck_gen: AtomicU64::new(u64::MAX),
            fail_binds: AtomicU32::new(0),
            injected_bind_failures: AtomicU32::new(0),
            server,
            info,
            opts,
            state_tx,
            link_tx,
            next_req: AtomicU64::new(1),
            next_gen: AtomicU64::new(0),
            panes: Mutex::new(HashMap::new()),
            input: InputQueue {
                state: Mutex::new(InputState { frames: VecDeque::new(), bytes: 0, next_seq: 1 }),
                ready: Notify::new(),
                space: Notify::new(),
            },
            events_tx,
            events_wanted: AtomicBool::new(false),
            events_gen: Arc::new(tokio::sync::Mutex::new(0)),
            ever_connected: AtomicBool::new(false),
            welcome: Mutex::new(None),
            boot_id: Mutex::new(None),
            input_lost: AtomicU64::new(0),
            shutdown_tx,
            kick: Notify::new(),
            wake_backoff: Notify::new(),
            reset_backoff: AtomicBool::new(false),
            ping_now: Notify::new(),
            next_nonce: AtomicU64::new(1),
            input_epoch: Uuid::new_v4(),
        });
        let supervisor = tokio::spawn(supervise(shared.clone()));
        Ok(ClientHandle {
            inner: Arc::new(HandleInner { shared, supervisor: Mutex::new(Some(supervisor)) }),
        })
    }
}

impl Shared {
    /// The endpoint dials use. `None` only while it is being replaced.
    fn ep(&self) -> Option<Ep> {
        lock(&self.endpoint).clone()
    }
}

/// After [`NETWORK_FALLBACK`], and unless the network change was answered or a
/// connection came up since, ask the supervisor to replace the endpoint.
fn spawn_fallback(shared: Arc<Shared>, seen: u64, pong_ok: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut shutdown = shared.shutdown_tx.subscribe();
        tokio::select! {
            () = tokio::time::sleep(NETWORK_FALLBACK) => {}
            _ = shutdown.changed() => return,
        }
        let connected_again = shared.link_tx.borrow().is_some()
            && shared.connected_count.load(Ordering::SeqCst) != seen;
        if pong_ok.load(Ordering::SeqCst) || connected_again || *shutdown.borrow() {
            return;
        }
        info!("client: no connection {NETWORK_FALLBACK:?} after a network change, replacing the endpoint");
        shared.replace_for.store(seen, Ordering::SeqCst);
        // Ends the dial in flight (it is on the endpoint that is going) or the
        // backoff sleep. A live but silent link is closed by `network_changed`.
        shared.kick.notify_one();
    });
}

/// How long replacing the endpoint waits for the old one to drain. Short: the
/// reason for the replace is that its socket does not work.
const REPLACE_CLOSE_GRACE: Duration = Duration::from_millis(250);

/// How long a replace waits for the clones of the old endpoint to go.
const CLONES_WAIT: Duration = Duration::from_secs(2);

/// Wait before binding again after a failed bind, and after 10 failures.
const BIND_RETRY: Duration = Duration::from_millis(500);
const BIND_RETRY_SLOW: Duration = Duration::from_secs(2);

impl Shared {
    /// Close the endpoint and bind a new one with the same key, in that order:
    /// only one exists at a time. An `Err` means the bind failed and the
    /// endpoint stays closed: call again to bind (the old one is not closed
    /// twice).
    async fn replace_endpoint(&self) -> Result<(), String> {
        let old = lock(&self.endpoint).take();
        if let Some(old) = old {
            // Retired connect attempts and the network probe hold clones of the
            // endpoint, some for seconds. End them first.
            self.holders.abort_all();
            let weak = Arc::downgrade(&old.ep);
            // The socket may be dead, so the peers will not ack: wait briefly.
            let _ = tokio::time::timeout(REPLACE_CLOSE_GRACE, old.ep.close()).await;
            self.live_eps.fetch_sub(1, Ordering::SeqCst);
            drop(old);
            // Bind only when every clone is gone. The aborted tasks drop theirs
            // as soon as the runtime runs them.
            let waited = Instant::now();
            let mut shutdown = self.shutdown_tx.subscribe();
            while weak.strong_count() > 0 && waited.elapsed() < CLONES_WAIT {
                if *shutdown.borrow() {
                    return Err("shutting down".into());
                }
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(5)) => {}
                    // `close()` must not wait for this.
                    _ = shutdown.changed() => return Err("shutting down".into()),
                }
            }
            let left = weak.strong_count();
            if left > 0 {
                warn!("client: {left} clones of the old endpoint are still alive, binding anyway");
            }
            self.clones_at_bind.store(left, Ordering::SeqCst);
        }
        if self.fail_binds.load(Ordering::SeqCst) > 0 {
            self.fail_binds.fetch_sub(1, Ordering::SeqCst);
            self.injected_bind_failures.fetch_add(1, Ordering::SeqCst);
            return Err("test: bind refused".into());
        }
        let ep = bind_endpoint(&self.opts, &self.server, self.identity.clone()).await?;
        let now = self.live_eps.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_live_eps.fetch_max(now, Ordering::SeqCst);
        let gen = self.next_ep_gen.fetch_add(1, Ordering::SeqCst);
        *lock(&self.endpoint) = Some(Ep { ep: Arc::new(ep), gen });
        Ok(())
    }
}

struct HandleInner {
    shared: Arc<Shared>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        // Last handle gone: stop the supervisor, it closes the endpoint.
        self.shared.shutdown_tx.send_replace(true);
    }
}

/// Cheap, cloneable handle to a running client.
#[derive(Clone)]
pub struct ClientHandle {
    inner: Arc<HandleInner>,
}

impl ClientHandle {
    fn shared(&self) -> &Arc<Shared> {
        &self.inner.shared
    }

    /// Current connection state.
    pub fn state(&self) -> ConnectionState {
        self.shared().state_tx.borrow().clone()
    }

    /// Observe connection state changes.
    pub fn watch_state(&self) -> watch::Receiver<ConnectionState> {
        self.shared().state_tx.subscribe()
    }

    /// `Welcome` of the latest handshake.
    pub fn welcome(&self) -> Option<Welcome> {
        lock(&self.shared().welcome).clone()
    }

    /// `true` when both sides listed the capability in the current session.
    pub fn has_capability(&self, name: &str) -> bool {
        self.shared().link_tx.borrow().as_ref().is_some_and(|l| l.has_cap(name))
    }

    /// The direct addresses learned so far. Persist the snapshot and pass it
    /// back in `ClientOptions::addr_cache` on the next start.
    pub fn addr_cache(&self) -> AddrCache {
        self.shared().opts.addr_cache.clone()
    }

    /// The local iroh endpoint id (the device id the server registers).
    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.shared().endpoint_id
    }

    /// Call a method and wait for the result, with the default timeout. Waits
    /// for a connection if there is none, within the same timeout.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        self.request_with_timeout(method, params, self.shared().opts.request_timeout).await
    }

    /// [`ClientHandle::request`] with an explicit timeout.
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let shared = self.shared();
        let link = wait_link(shared, deadline).await?;
        call(shared, &link, method, params, deadline).await
    }

    /// Typed variant of [`ClientHandle::request`].
    pub async fn request_typed<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, ClientError> {
        let value = self.request(method, to_value(params)?).await?;
        Ok(from_value(value)?)
    }

    /// Subscribe to server events. The first call sends `subscribe_events`.
    /// After every reconnect the client subscribes again and yields
    /// [`EventItem::Resynced`].
    pub async fn events(&self) -> Result<EventStream, ClientError> {
        let shared = self.shared();
        let rx = shared.events_tx.subscribe();
        shared.events_wanted.store(true, Ordering::SeqCst);
        let link = shared.link_tx.borrow().clone();
        if let Some(link) = link {
            let deadline = tokio::time::Instant::now() + shared.opts.request_timeout;
            match ensure_subscribed(shared, &link, deadline).await {
                Ok(()) => {}
                // The supervisor subscribes again on the next connection.
                Err(ClientError::ConnectionLost | ClientError::Timeout) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(EventStream { rx })
    }

    /// Attach a pane output stream. The stream survives reconnects: the
    /// client re-attaches it with the last offset.
    ///
    /// If there is no connection right now the call still succeeds and the
    /// attach happens on the next connection.
    pub async fn attach_pane(&self, pane_id: Uuid) -> Result<PaneStream, ClientError> {
        let shared = self.shared();
        let (tx, rx) = mpsc::channel(shared.opts.pane_buffer.max(2));
        let pane = Arc::new(PaneShared {
            id: pane_id,
            tx,
            state: Mutex::new(PaneState {
                gen: 0,
                offset: None,
                stream: 0,
                reset_stream: 0,
                replay_end: None,
            }),
            attached_gen: Arc::new(tokio::sync::Mutex::new(0)),
        });
        {
            let mut panes = lock(&shared.panes);
            if panes.contains_key(&pane_id) {
                return Err(ClientError::PaneAlreadyAttached);
            }
            panes.insert(pane_id, pane.clone());
        }
        let stream = PaneStream { pane: pane.clone(), rx, client: self.clone(), ended: false };
        let link = shared.link_tx.borrow().clone();
        if let Some(link) = link {
            let deadline = tokio::time::Instant::now() + shared.opts.request_timeout;
            match ensure_attached(shared, &link, &pane, deadline).await {
                Ok(()) => {}
                Err(ClientError::ConnectionLost | ClientError::Timeout) => {}
                Err(e) => return Err(e), // `stream` drops and unregisters the pane
            }
        }
        Ok(stream)
    }

    /// Send keyboard input to a pane on the dedicated input stream.
    ///
    /// While disconnected the bytes are buffered (64 KiB cap) and sent in
    /// order after the connection resumes. Returns `InputBufferFull` when the
    /// cap would be exceeded while disconnected. While connected the call
    /// waits for room instead.
    pub async fn send_input(&self, pane_id: Uuid, bytes: &[u8]) -> Result<(), ClientError> {
        self.send_input_flagged(pane_id, bytes, false).await
    }

    /// Like [`send_input`](Self::send_input) with the AD-025 flag: `auto` marks terminal
    /// replies (DA, CPR, kitty answers) and focus reports. An `auto` frame never counts as
    /// typing. The flag goes on the wire only when the server lists `input_flags`.
    ///
    /// A buffer is accepted whole or refused whole (`InputBufferFull`), never in part. It is cut
    /// into frames only at character and escape sequence boundaries.
    pub async fn send_input_flagged(
        &self,
        pane_id: Uuid,
        bytes: &[u8],
        auto: bool,
    ) -> Result<(), ClientError> {
        let shared = self.shared();
        if self.state() == ConnectionState::Closed {
            return Err(ClientError::Closed);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let pieces = split_input(bytes);
        loop {
            let space = shared.input.space.notified();
            tokio::pin!(space);
            space.as_mut().enable();
            {
                // All pieces go in under one lock, or none do: a buffer is never cut by a full
                // queue or by a connection that drops while this call waits.
                let mut st = lock(&shared.input.state);
                if st.bytes + bytes.len() <= INPUT_BUFFER_CAP || st.frames.is_empty() {
                    for piece in &pieces {
                        let seq = st.next_seq;
                        st.next_seq += 1;
                        st.bytes += piece.len();
                        st.frames.push_back(InFrame {
                            seq,
                            pane: pane_id,
                            bytes: Arc::from(*piece),
                            auto,
                        });
                    }
                    drop(st);
                    shared.input.ready.notify_one();
                    return Ok(());
                }
            }
            if !self.state().is_connected() {
                return Err(ClientError::InputBufferFull);
            }
            // Full of unacked frames: ask for an ack now, then wait.
            shared.ping_now.notify_one();
            space.await;
        }
    }

    /// Input bytes dropped so far because the server restarted before they
    /// were acked (PROTOCOL_V2 section 9). Also reported once per restart as
    /// [`EventItem::InputLost`].
    pub fn lost_input_bytes(&self) -> u64 {
        self.shared().input_lost.load(Ordering::SeqCst)
    }

    /// Input bytes sent or buffered but not yet acked by the server.
    pub fn unacked_input_bytes(&self) -> usize {
        lock(&self.shared().input.state).bytes
    }

    /// Tell the server the size this client wants for a pane (`resize_pane`).
    pub async fn resize(&self, pane_id: Uuid, rows: u16, cols: u16) -> Result<(), ClientError> {
        let params = to_value(&ResizePaneParams { pane_id, rows, cols })?;
        self.request(method::RESIZE_PANE, params).await.map(|_| ())
    }

    /// Detach a pane (same as dropping its `PaneStream`).
    pub async fn detach_pane(&self, pane_id: Uuid) -> Result<(), ClientError> {
        let shared = self.shared();
        lock(&shared.panes).remove(&pane_id);
        let link = shared.link_tx.borrow().clone();
        if let Some(link) = link {
            let deadline = tokio::time::Instant::now() + shared.opts.request_timeout;
            let params = to_value(&DetachPaneParams { pane_id })?;
            call(shared, &link, method::DETACH_PANE, params, deadline).await?;
        }
        Ok(())
    }

    /// Call when the OS reports a network change (Wi-Fi to LTE). Tells iroh to
    /// re-probe paths, sends a `Ping` at once and redials if no `Pong` comes
    /// back within 1 s.
    pub async fn network_changed(&self) {
        let shared = self.shared();
        // Probe first: `network_change` can take long on a dead network, and
        // must not delay the Ping.
        let link = shared.link_tx.borrow().clone();
        {
            // The probe holds a clone of the endpoint. Register it under the
            // endpoint lock: a replace takes the endpoint under the same lock
            // and aborts the holders after, so either the probe is registered
            // before the take (and aborted) or there is no endpoint and no
            // probe. No holder appears after the abort.
            let slot = lock(&shared.endpoint);
            if let Some(ep) = slot.clone() {
                let probe = tokio::spawn(async move { ep.ep.network_change().await });
                shared.holders.add(probe.abort_handle());
            }
        }
        // iroh may need many seconds to rebind a socket that went away. If
        // there is no connection after a second, dial with a fresh endpoint.
        let pong_ok = Arc::new(AtomicBool::new(false));
        spawn_fallback(
            shared.clone(),
            shared.connected_count.load(Ordering::SeqCst),
            pong_ok.clone(),
        );
        let Some(link) = link else {
            // No connection: a dial is in flight (it may hang on a dead
            // address) or the supervisor sleeps in a backoff. Abort it, forget
            // the failures and dial again right away.
            shared.reset_backoff.store(true, Ordering::SeqCst);
            shared.kick.notify_one();
            return;
        };
        let nonce = shared.next_nonce.fetch_add(1, Ordering::Relaxed);
        let mut pong_rx = link.pong_tx.subscribe();
        if send_ping(&link, nonce).is_err() {
            return;
        }
        let wait = pong_rx.wait_for(|n| *n >= nonce);
        let limit = Duration::from_millis(NETWORK_CHANGE_PONG_MS);
        if matches!(tokio::time::timeout(limit, wait).await, Ok(Ok(_))) {
            pong_ok.store(true, Ordering::SeqCst);
        } else {
            info!("client: no Pong after network change, redialing");
            shared.reset_backoff.store(true, Ordering::SeqCst);
            link.conn.close(close_code::OK.into(), b"network changed");
        }
    }

    /// Test hook: endpoints bound and not closed now, and the most at once.
    #[doc(hidden)]
    pub fn live_endpoints_for_tests(&self) -> (usize, usize) {
        let s = self.shared();
        (s.live_eps.load(Ordering::SeqCst), s.max_live_eps.load(Ordering::SeqCst))
    }

    /// Test hook: clones of the old endpoint alive when the last replacement
    /// was bound (`usize::MAX` if there was none).
    #[doc(hidden)]
    pub fn old_endpoint_clones_at_bind_for_tests(&self) -> usize {
        self.shared().clones_at_bind.load(Ordering::SeqCst)
    }

    /// Test hook: the next `n` endpoint binds fail.
    #[doc(hidden)]
    pub fn fail_binds_for_tests(&self, n: u32) {
        self.shared().fail_binds.store(n, Ordering::SeqCst);
    }

    /// Test hook: how many binds `fail_binds_for_tests` made fail so far.
    #[doc(hidden)]
    pub fn injected_bind_failures_for_tests(&self) -> u32 {
        self.shared().injected_bind_failures.load(Ordering::SeqCst)
    }

    /// Test hook: the socket of the current endpoint went away. The current
    /// connection ends and no dial on this endpoint connects again.
    #[doc(hidden)]
    pub fn blackhole_endpoint_for_tests(&self) {
        let shared = self.shared();
        if let Some(ep) = shared.ep() {
            shared.stuck_gen.store(ep.gen, Ordering::SeqCst);
        }
        if let Some(link) = shared.link_tx.borrow().clone() {
            link.conn.close(close_code::OK.into(), b"test blackhole");
        }
    }

    /// With a live connection: drop it and redial at once. Without one: cut
    /// the backoff sleep short and dial now. A dial or probe burst already in
    /// flight is left alone (call it as often as you like while Reconnecting;
    /// [`ClientHandle::network_changed`] is what aborts a dial).
    pub fn reconnect_now(&self) {
        let shared = self.shared();
        let link = shared.link_tx.borrow().clone();
        match link {
            Some(link) => link.conn.close(close_code::OK.into(), b"reconnect"),
            None => shared.wake_backoff.notify_one(),
        }
    }

    /// Stop the client and close the endpoint.
    pub async fn close(&self) {
        let shared = self.shared();
        shared.shutdown_tx.send_replace(true);
        let join = lock(&self.inner.supervisor).take();
        if let Some(join) = join {
            let _ = join.await;
        }
        shared.state_tx.send_replace(ConnectionState::Closed);
    }
}

// ---------------------------------------------------------------------------
// PaneStream
// ---------------------------------------------------------------------------

/// Output of one pane, with resume. Created by [`ClientHandle::attach_pane`].
///
/// Dropping it detaches the pane.
pub struct PaneStream {
    pane: Arc<PaneShared>,
    rx: mpsc::Receiver<Tagged>,
    client: ClientHandle,
    ended: bool,
}

impl PaneStream {
    /// The pane id.
    pub fn pane_id(&self) -> Uuid {
        self.pane.id
    }

    /// Next item. `None` after [`PaneItem::Ended`] or when the client closed.
    pub async fn next(&mut self) -> Option<PaneItem> {
        if self.ended {
            return None;
        }
        loop {
            let tagged = self.rx.recv().await?;
            // A newer stream started with a reset (snapshot catch-up): what an
            // older stream queued would only be wiped by that reset.
            if tagged.stream != ALWAYS && tagged.stream < lock(&self.pane.state).reset_stream {
                continue;
            }
            if matches!(tagged.item, PaneItem::Ended) {
                self.ended = true;
            }
            return Some(tagged.item);
        }
    }

    /// Absolute offset of the next byte this stream will yield. `None` before
    /// the first stream header arrived.
    pub fn offset(&self) -> Option<u64> {
        lock(&self.pane.state).offset
    }

    /// Pane offset where the replay of the newest stream ends and live bytes start
    /// (`PaneStreamHeader.replay_end`). `None` before the first header, or from a server that
    /// does not send it.
    pub fn replay_end(&self) -> Option<u64> {
        lock(&self.pane.state).replay_end
    }
}

impl Drop for PaneStream {
    fn drop(&mut self) {
        let shared = self.client.shared().clone();
        let removed = {
            let mut panes = lock(&shared.panes);
            match panes.get(&self.pane.id) {
                Some(p) if Arc::ptr_eq(p, &self.pane) => {
                    panes.remove(&self.pane.id);
                    true
                }
                _ => false,
            }
        };
        if !removed || self.ended {
            return;
        }
        let link = shared.link_tx.borrow().clone();
        if let (Some(link), Ok(rt)) = (link, tokio::runtime::Handle::try_current()) {
            let id = self.pane.id;
            rt.spawn(async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                if let Ok(params) = to_value(&DetachPaneParams { pane_id: id }) {
                    let _ = call(&shared, &link, method::DETACH_PANE, params, deadline).await;
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// Wait until a link exists, the client failed, or the deadline passes.
async fn wait_link(
    shared: &Shared,
    deadline: tokio::time::Instant,
) -> Result<Arc<Link>, ClientError> {
    let mut link_rx = shared.link_tx.subscribe();
    let mut state_rx = shared.state_tx.subscribe();
    loop {
        let link = link_rx.borrow_and_update().clone();
        if let Some(link) = link {
            return Ok(link);
        }
        let state = state_rx.borrow_and_update().clone();
        match state {
            ConnectionState::Failed(reason) => return Err(ClientError::Failed(reason)),
            ConnectionState::Closed => return Err(ClientError::Closed),
            _ => {}
        }
        let changed = async {
            tokio::select! {
                _ = link_rx.changed() => {}
                _ = state_rx.changed() => {}
            }
        };
        if tokio::time::timeout_at(deadline, changed).await.is_err() {
            return Err(ClientError::NotConnected);
        }
    }
}

/// Queue a request on `link`. The receiver resolves with the answer.
fn send_request(
    shared: &Shared,
    link: &Link,
    method: &str,
    params: Value,
) -> Result<(u64, oneshot::Receiver<Pending>), ClientError> {
    let id = shared.next_req.fetch_add(1, Ordering::Relaxed);
    let frame = encode_msg(&ControlMsg::Request { id, method: method.to_string(), params })?;
    let (tx, rx) = oneshot::channel();
    lock(&link.pending).insert(id, tx);
    if link.control_tx.send(frame).is_err() {
        lock(&link.pending).remove(&id);
        return Err(ClientError::ConnectionLost);
    }
    Ok((id, rx))
}

async fn await_answer(
    link: &Link,
    id: u64,
    rx: oneshot::Receiver<Pending>,
    deadline: tokio::time::Instant,
) -> Result<Value, ClientError> {
    match tokio::time::timeout_at(deadline, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(ClientError::ConnectionLost),
        Err(_) => {
            lock(&link.pending).remove(&id);
            Err(ClientError::Timeout)
        }
    }
}

async fn call(
    shared: &Shared,
    link: &Link,
    method: &str,
    params: Value,
    deadline: tokio::time::Instant,
) -> Result<Value, ClientError> {
    let (id, rx) = send_request(shared, link, method, params)?;
    await_answer(link, id, rx, deadline).await
}

async fn ensure_subscribed(
    shared: &Shared,
    link: &Arc<Link>,
    deadline: tokio::time::Instant,
) -> Result<(), ClientError> {
    let mut guard = shared.events_gen.lock().await;
    if *guard == link.gen {
        return Ok(());
    }
    call(shared, link, method::SUBSCRIBE_EVENTS, Value::Map(vec![]), deadline).await?;
    *guard = link.gen;
    Ok(())
}

async fn ensure_attached(
    shared: &Shared,
    link: &Arc<Link>,
    pane: &Arc<PaneShared>,
    deadline: tokio::time::Instant,
) -> Result<(), ClientError> {
    let mut guard = pane.attached_gen.lock().await;
    if *guard == link.gen {
        return Ok(());
    }
    let params = attach_params(link, pane)?;
    call(shared, link, method::ATTACH_PANE, params, deadline).await?;
    *guard = link.gen;
    Ok(())
}

fn attach_params(link: &Link, pane: &PaneShared) -> Result<Value, ClientError> {
    let from_offset = if link.has_cap(cap::RESUME) { lock(&pane.state).offset } else { None };
    Ok(to_value(&AttachPaneParams { pane_id: pane.id, from_offset })?)
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

enum DialError {
    Transient(String),
    Fatal(String),
}

struct Established {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    welcome: Welcome,
}

enum SessionEnd {
    Lost(String),
    Fatal(String),
    Shutdown,
}

enum TaskEnd {
    Done,
    Lost(String),
}

fn backoff(failures: usize) -> Duration {
    Duration::from_millis(RECONNECT_BACKOFF_MS[failures.min(RECONNECT_BACKOFF_MS.len() - 1)])
}

/// Take a stored `notify_one` permit, if there is one.
async fn drain_permit(n: &Notify) {
    tokio::select! {
        biased;
        () = n.notified() => {}
        () = std::future::ready(()) => {}
    }
}

async fn supervise(shared: Arc<Shared>) {
    let mut shutdown = shared.shutdown_tx.subscribe();
    let mut failures = 0usize;
    let mut final_state = ConnectionState::Closed;
    let mut attempt_no = 0u64;
    let mut bind_failures = 0u32;
    loop {
        if *shutdown.borrow() {
            break;
        }
        // A network change left no connection: replace the endpoint before
        // dialing again. Only if nothing connected since, and the old endpoint
        // is closed first. A failed bind is tried again after a short wait.
        let want = shared.replace_for.load(Ordering::SeqCst);
        if want == shared.connected_count.load(Ordering::SeqCst) {
            match shared.replace_endpoint().await {
                Ok(()) => {
                    shared.replace_for.store(u64::MAX, Ordering::SeqCst);
                    bind_failures = 0;
                }
                Err(_) if *shutdown.borrow() => continue, // the loop top ends it
                Err(e) => {
                    bind_failures += 1;
                    // The first failure, then every 20th: a bind that keeps
                    // failing must not fill the log.
                    if bind_failures % 20 == 1 {
                        warn!(
                            "client: fresh endpoint failed to bind ({bind_failures} so far): {e}"
                        );
                    }
                    let wait = if bind_failures > 10 { BIND_RETRY_SLOW } else { BIND_RETRY };
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        _ = shutdown.changed() => {}
                        // A reconnect request ends the wait.
                        _ = shared.wake_backoff.notified() => {}
                    }
                    continue;
                }
            }
        }
        attempt_no += 1;
        let attempt_started = Instant::now();
        debug!("client: dial #{attempt_no} starts (failures so far {failures})");
        // A `network_changed` while a dial is in flight aborts it and starts
        // a new one at once, with no backoff.
        if shared.reset_backoff.swap(false, Ordering::SeqCst) {
            failures = 0;
        }
        let Some(ep) = shared.ep() else {
            // Replaced just now and the bind failed: the loop above retries.
            continue;
        };
        // A `reconnect_now` from before this dial wanted this dial, so it is
        // spent. One that comes during or after it stays stored: if the link
        // dies at once, it cuts the backoff that follows.
        drain_permit(&shared.wake_backoff).await;
        let attempt = tokio::time::timeout(shared.opts.dial_timeout, dial(&shared, ep));
        let result = tokio::select! {
            r = attempt => r,
            _ = shutdown.changed() => continue,
            _ = shared.kick.notified() => {
                debug!("client: dial aborted, redialing");
                continue;
            }
        };
        let delay = match result {
            Ok(Ok(est)) => {
                debug!(
                    "client: dial #{attempt_no} connected after {:?}",
                    attempt_started.elapsed()
                );
                let started = Instant::now();
                shared.connected_count.fetch_add(1, Ordering::SeqCst);
                shared.replace_for.store(u64::MAX, Ordering::SeqCst);
                let (end, resumed) = run_session(&shared, est, &mut shutdown).await;
                match end {
                    SessionEnd::Shutdown => break,
                    SessionEnd::Fatal(reason) => {
                        warn!("client: connection failed for good: {reason}");
                        final_state = ConnectionState::Failed(reason);
                        break;
                    }
                    SessionEnd::Lost(reason) => {
                        info!("client: connection lost ({reason}), redialing");
                    }
                }
                shared
                    .state_tx
                    .send_replace(ConnectionState::Reconnecting { since: Instant::now() });
                // A session that resumed (or lasted) counts as healthy, so the
                // redial is immediate. One that died before it resumed backs
                // off, so a broken server is not hammered.
                if resumed || started.elapsed() >= MIN_HEALTHY_SESSION {
                    failures = 0;
                    None
                } else {
                    failures += 1;
                    Some(backoff(failures - 1))
                }
            }
            Ok(Err(DialError::Fatal(reason))) => {
                warn!("client: dial failed for good: {reason}");
                final_state = ConnectionState::Failed(reason);
                break;
            }
            Ok(Err(DialError::Transient(reason))) => {
                debug!(
                    "client: dial #{attempt_no} failed after {:?}: {reason}",
                    attempt_started.elapsed()
                );
                let d = backoff(failures);
                failures += 1;
                Some(d)
            }
            Err(_) => {
                debug!(
                    "client: dial #{attempt_no} timed out after {:?}",
                    attempt_started.elapsed()
                );
                let d = backoff(failures);
                failures += 1;
                Some(d)
            }
        };
        let delay = if shared.reset_backoff.swap(false, Ordering::SeqCst) {
            failures = 0;
            None
        } else {
            delay
        };
        if let Some(delay) = delay {
            debug!("client: backoff {delay:?} before dial #{}", attempt_no + 1);
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = shutdown.changed() => {}
                _ = shared.kick.notified() => {}
                _ = shared.wake_backoff.notified() => {}
            }
        }
    }
    shared.link_tx.send_replace(None);
    // The close frames are out when `close` starts. What it waits for after
    // that is the peers' acks, and connections that were retired (or lost
    // across an outage) can take seconds to drain: an app that quits must not
    // wait for them.
    shared.holders.abort_all();
    // Only an endpoint that exists is closed: after a failed bind there is none
    // and the live count was already lowered.
    let last = lock(&shared.endpoint).take();
    if let Some(ep) = last {
        let _ = tokio::time::timeout(CLOSE_GRACE, ep.ep.close()).await;
        shared.live_eps.fetch_sub(1, Ordering::SeqCst);
    }
    shared.state_tx.send_replace(final_state);
}

/// Gap before each further QUIC connect attempt, in milliseconds. The last
/// one repeats.
///
/// One attempt is not enough: QUIC repeats its first packet only after a
/// timeout that starts near 1 s and doubles. A dial that starts while the
/// server is down (a restart, a network that is still coming back) then
/// notices the server up to 1 s, 2 s, ... late. So while no attempt has
/// connected, a new one sends a fresh first packet every gap. The earlier ones
/// stay alive and the first to connect wins, so a slow link still finishes.
const CONNECT_PROBES_MS: [u64; 3] = [400, 400, 800];

/// A dial to the cached direct address gives up after this long.
const DIRECT_DIAL_TIMEOUT: Duration = Duration::from_secs(1);

/// How long closing the client waits for its connections to drain.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// How long connect attempts that lost the race, or were abandoned, get to
/// finish their handshake so they can be closed. Past it they are dropped.
const RETIRE_GRACE: Duration = Duration::from_secs(2);

/// The connect attempts of one [`connect_probing`] call. When it goes out of
/// scope, by a win, the 1 s direct limit, the dial timeout, a kick or the
/// other leg winning, the attempts still in flight are retired, not dropped.
/// Dropped half way through the handshake an attempt leaves a half-open
/// connection on the server, which holds up its shutdown for seconds. Each one
/// gets [`RETIRE_GRACE`] to finish and is then closed.
struct Attempts {
    set: Option<tokio::task::JoinSet<Result<Connection, String>>>,
    holders: Arc<HolderBin>,
}

impl Drop for Attempts {
    fn drop(&mut self) {
        let Some(mut set) = self.set.take() else { return };
        if set.is_empty() {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let retire = rt.spawn(async move {
            let grace = tokio::time::sleep(RETIRE_GRACE);
            tokio::pin!(grace);
            loop {
                tokio::select! {
                    done = set.join_next() => match done {
                        Some(Ok(Ok(conn))) => conn.close(0u32.into(), b"redundant dial"),
                        Some(_) => {}
                        None => return,
                    },
                    () = &mut grace => return,
                }
            }
        });
        // The attempts hold clones of the endpoint: a replace aborts them.
        self.holders.add(retire.abort_handle());
    }
}

/// QUIC connect to `target`: attempts overlap, one more every gap
/// ([`CONNECT_PROBES_MS`]), and the first to connect wins. Fails at once on a
/// real error, never on a timeout: the caller's dial timeout ends it.
async fn connect_probing(
    shared: &Shared,
    ep: &Ep,
    target: EndpointAddr,
) -> Result<Connection, String> {
    let mut attempts =
        Attempts { set: Some(tokio::task::JoinSet::new()), holders: shared.holders.clone() };
    let mut n = 0usize;
    loop {
        let endpoint = ep.ep.clone();
        let to = target.clone();
        let stuck = shared.stuck_gen.load(Ordering::SeqCst) == ep.gen;
        let set = attempts.set.as_mut().expect("set is taken only on drop");
        set.spawn(async move {
            if stuck {
                // Test hook: the socket of this endpoint is gone.
                std::future::pending::<()>().await;
            }
            endpoint.connect(to, ALPN_RPC).await.map_err(|e| e.to_string())
        });
        let gap = Duration::from_millis(CONNECT_PROBES_MS[n.min(CONNECT_PROBES_MS.len() - 1)]);
        n += 1;
        let set = attempts.set.as_mut().expect("set is taken only on drop");
        tokio::select! {
            Some(done) = set.join_next() => {
                return match done {
                    Ok(result) => result,
                    Err(e) => Err(format!("connect attempt failed: {e}")),
                };
            },
            () = tokio::time::sleep(gap) => {
                debug!("client: connect attempt {n} unanswered after {gap:?}, another");
            }
        }
    }
}

/// Connect to `target` and run the handshake. `connect_limit` bounds the QUIC
/// connect only: once the path is up the handshake can take as long as the
/// overall dial timeout, because a slow server (a busy `Welcome`) is not a
/// dead address.
async fn dial_to(
    shared: &Shared,
    ep: &Ep,
    target: EndpointAddr,
    connect_limit: Option<Duration>,
) -> Result<Established, DialError> {
    let probing = connect_probing(shared, ep, target);
    let conn = match connect_limit {
        Some(limit) => match tokio::time::timeout(limit, probing).await {
            Ok(r) => r,
            Err(_) => return Err(DialError::Transient("direct connect timed out".into())),
        },
        None => probing.await,
    }
    .map_err(DialError::Transient)?;
    match handshake(shared, &conn).await {
        Ok((send, recv, welcome)) => Ok(Established { conn, send, recv, welcome }),
        Err(e) => {
            conn.close(close_code::OK.into(), b"handshake failed");
            Err(e)
        }
    }
}

/// The cached direct address and the addresses of the pairing payload (relay
/// URLs, direct addresses) are dialed in parallel, not one after the other
/// (happy eyeballs). The direct connect gets 1 s: a dead cached address (the
/// network changed) must not hold up the relay. The 1 s covers the QUIC
/// connect, not the handshake after it. The first handshake wins and
/// the other dial is dropped. A fatal error ends the race at once.
async fn dial(shared: &Shared, ep: Ep) -> Result<Established, DialError> {
    let ep = &ep;
    let Some(ip) = shared.opts.addr_cache.get(shared.server.id) else {
        return dial_to(shared, ep, shared.server.clone(), None).await;
    };
    let direct = EndpointAddr::new(shared.server.id).with_ip_addr(ip);
    if shared.server.is_empty() {
        // Nothing to fall back to: the direct dial has the whole dial timeout.
        return dial_to(shared, ep, direct, None).await;
    }
    let a = dial_to(shared, ep, direct, Some(DIRECT_DIAL_TIMEOUT));
    let b = dial_to(shared, ep, shared.server.clone(), None);
    tokio::pin!(a, b);
    let (mut a_done, mut b_done) = (false, false);
    let mut last = DialError::Transient("no address".into());
    while !(a_done && b_done) {
        let r = tokio::select! {
            r = &mut a, if !a_done => { a_done = true; r }
            r = &mut b, if !b_done => { b_done = true; r }
        };
        match r {
            Ok(est) => return Ok(est),
            Err(e @ DialError::Fatal(_)) => return Err(e),
            Err(DialError::Transient(m)) => {
                debug!("client: a dial leg failed: {m}");
                last = DialError::Transient(m);
            }
        }
    }
    Err(last)
}

/// Did the peer close the connection with `not_authorized`?
async fn closed_not_authorized(conn: &Connection) -> bool {
    let reason = match conn.close_reason() {
        Some(r) => Some(r),
        None => tokio::time::timeout(Duration::from_millis(500), conn.closed()).await.ok(),
    };
    matches!(
        reason,
        Some(ConnectionError::ApplicationClosed(c))
            if u64::from(c.error_code) == u64::from(close_code::NOT_AUTHORIZED)
    )
}

async fn handshake(
    shared: &Shared,
    conn: &Connection,
) -> Result<(SendStream, RecvStream, Welcome), DialError> {
    let transient = |e: String| DialError::Transient(e);
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| transient(e.to_string()))?;
    let _ = send.set_priority(HIGH_PRIORITY);
    let info = &shared.info;
    let hello = Hello::new(
        info.client_id,
        shared.input_epoch,
        info.kind,
        info.name.clone(),
        info.version.clone(),
        info.capabilities.clone(),
    );
    if let Err(e) = write_msg(&mut send, &hello).await {
        return Err(if closed_not_authorized(conn).await {
            DialError::Fatal("not_authorized: pair this device first".into())
        } else {
            transient(e.to_string())
        });
    }
    match read_msg::<_, HandshakeReply>(&mut recv).await {
        Ok(Some(HandshakeReply::Welcome(w))) => {
            if w.proto_major != PROTO_MAJOR {
                return Err(DialError::Fatal(format!(
                    "{}: server speaks protocol {}",
                    error_code::PROTO_MISMATCH,
                    w.proto_major
                )));
            }
            Ok((send, recv, w))
        }
        // A stopping server will be back: retry with the normal backoff.
        Ok(Some(HandshakeReply::Error(e))) if e.code == error_code::SERVER_STOPPING => {
            Err(transient(format!("{}: {}", e.code, e.message)))
        }
        Ok(Some(HandshakeReply::Error(e))) => {
            Err(DialError::Fatal(format!("{}: {}", e.code, e.message)))
        }
        Ok(None) | Err(_) => Err(if closed_not_authorized(conn).await {
            DialError::Fatal("not_authorized: pair this device first".into())
        } else {
            transient("handshake stream closed".into())
        }),
    }
}

async fn run_session(
    shared: &Arc<Shared>,
    est: Established,
    shutdown: &mut watch::Receiver<bool>,
) -> (SessionEnd, bool) {
    let Established { conn, send, recv, welcome } = est;
    let gen = shared.next_gen.fetch_add(1, Ordering::SeqCst) + 1;
    note_boot(shared, welcome.boot_id);

    let caps: HashSet<String> = welcome
        .capabilities
        .iter()
        .filter(|c| shared.info.capabilities.contains(c))
        .cloned()
        .collect();
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let link = Arc::new(Link {
        gen,
        control_tx,
        pending: Mutex::new(HashMap::new()),
        caps,
        conn: conn.clone(),
        resumed: AtomicBool::new(false),
        last_pong: Mutex::new(Instant::now()),
        pong_tx: watch::channel(0).0,
    });

    let input_send = match conn.open_uni().await {
        Ok(s) => {
            let _ = s.set_priority(HIGH_PRIORITY);
            s
        }
        Err(e) => {
            conn.close(close_code::OK.into(), b"no input stream");
            return (SessionEnd::Lost(e.to_string()), false);
        }
    };

    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<RecvStream>();
    let (resumed_tx, resumed_rx) = oneshot::channel();
    let mut tasks: JoinSet<TaskEnd> = JoinSet::new();
    tasks.spawn(control_writer(send, control_rx));
    tasks.spawn(control_reader(link.clone(), shared.clone(), recv));
    tasks.spawn(input_writer(
        shared.clone(),
        input_send,
        resumed_rx,
        link.has_cap(cap::INPUT_FLAGS),
    ));
    tasks.spawn(accept_streams(conn.clone(), stream_tx));
    tasks.spawn(session_init(shared.clone(), link.clone(), resumed_tx));
    tasks.spawn(liveness(shared.clone(), link.clone()));

    *lock(&shared.welcome) = Some(welcome);
    shared.link_tx.send_replace(Some(link.clone()));
    shared.state_tx.send_replace(ConnectionState::Connected);
    shared.ever_connected.store(true, Ordering::SeqCst);

    let end = loop {
        tokio::select! {
            Some(recv) = stream_rx.recv() => {
                tasks.spawn(pane_reader(shared.clone(), gen, recv));
            }
            res = tasks.join_next() => match res {
                Some(Ok(TaskEnd::Done)) => {}
                Some(Ok(TaskEnd::Lost(reason))) => break SessionEnd::Lost(reason),
                Some(Err(e)) => break SessionEnd::Lost(format!("task failed: {e}")),
                None => break SessionEnd::Lost("no tasks left".into()),
            },
            reason = conn.closed() => break SessionEnd::Lost(reason.to_string()),
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    // Input the user typed just before closing is still on its
                    // way: the session tasks keep running while this waits.
                    drain_input(shared, &link).await;
                    break SessionEnd::Shutdown;
                }
            }
        }
    };

    // Tear down: nothing of this session may run after this point.
    shared.link_tx.send_replace(None);
    link.fail_all(ClientError::ConnectionLost);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let resumed = link.resumed.load(Ordering::SeqCst);
    let not_authorized = matches!(
        conn.close_reason(),
        Some(ConnectionError::ApplicationClosed(c))
            if u64::from(c.error_code) == u64::from(close_code::NOT_AUTHORIZED)
    );
    conn.close(close_code::OK.into(), b"bye");
    if not_authorized {
        let reason = "not_authorized: this device was revoked or not paired".to_string();
        return (SessionEnd::Fatal(reason), resumed);
    }
    (end, resumed)
}

/// How long a closing client waits for the server to ack its input.
const INPUT_DRAIN: Duration = Duration::from_millis(300);

/// Wait up to [`INPUT_DRAIN`] until the server has acked all the input sent
/// (`input_acked` in a `Pong` covers every frame), asking with a `Ping` so the
/// ack does not wait for the next liveness one. Input that is not acked by
/// then is dropped with the connection, as before.
async fn drain_input(shared: &Arc<Shared>, link: &Arc<Link>) {
    let deadline = tokio::time::Instant::now() + INPUT_DRAIN;
    while !lock(&shared.input.state).frames.is_empty() {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            debug!(
                "client: closing with {} bytes of input not acked",
                lock(&shared.input.state).bytes
            );
            return;
        }
        let nonce = shared.next_nonce.fetch_add(1, Ordering::Relaxed);
        let mut pong = link.pong_tx.subscribe();
        if send_ping(link, nonce).is_err() {
            return;
        }
        let _ = tokio::time::timeout(deadline - now, pong.wait_for(|n| *n >= nonce)).await;
        // Frames the writer had not sent when the Ping went out are still
        // open: ask again shortly.
        if !lock(&shared.input.state).frames.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Session tasks
// ---------------------------------------------------------------------------

async fn control_writer(mut send: SendStream, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) -> TaskEnd {
    while let Some(frame) = rx.recv().await {
        if let Err(e) = write_frame(&mut send, &frame).await {
            return TaskEnd::Lost(format!("control write: {e}"));
        }
    }
    TaskEnd::Done
}

async fn control_reader(link: Arc<Link>, shared: Arc<Shared>, mut recv: RecvStream) -> TaskEnd {
    let mut buf = Vec::new();
    loop {
        match read_frame_into(&mut recv, &mut buf).await {
            Ok(true) => match decode_msg::<ControlMsg>(&buf) {
                Ok(ControlMsg::Response { id, result }) => link.complete(id, Ok(result)),
                Ok(ControlMsg::Failure { id, code, message }) => {
                    link.complete(id, Err(ClientError::Server { code, message }))
                }
                Ok(ControlMsg::Event { seq, event }) => {
                    let _ = shared.events_tx.send(EventItem::Server { seq, event });
                }
                Ok(ControlMsg::Pong { nonce, input_acked }) => {
                    *lock(&link.last_pong) = Instant::now();
                    link.pong_tx.send_modify(|n| *n = (*n).max(nonce));
                    shared.input.ack(input_acked);
                }
                Ok(ControlMsg::Request { .. } | ControlMsg::Ping { .. }) => {
                    warn!("client: server sent a client-only message, ignored")
                }
                // An event type this version does not know. The frame is
                // length delimited, so skipping it is safe.
                Err(e) => warn!("client: undecodable control frame skipped: {e}"),
            },
            Ok(false) => return TaskEnd::Lost("control stream closed".into()),
            Err(e) => return TaskEnd::Lost(format!("control read: {e}")),
        }
    }
}

async fn accept_streams(conn: Connection, tx: mpsc::UnboundedSender<RecvStream>) -> TaskEnd {
    loop {
        match conn.accept_uni().await {
            Ok(stream) => {
                if tx.send(stream).is_err() {
                    return TaskEnd::Done;
                }
            }
            Err(e) => return TaskEnd::Lost(format!("accept_uni: {e}")),
        }
    }
}

/// Resume after a (re)connect: events first, then every live pane with its
/// last offset, then release the input writer (PROTOCOL_V2 section 9).
async fn session_init(
    shared: Arc<Shared>,
    link: Arc<Link>,
    resumed: oneshot::Sender<()>,
) -> TaskEnd {
    let deadline = tokio::time::Instant::now() + shared.opts.request_timeout;
    let first = link.gen == 1;

    if shared.events_wanted.load(Ordering::SeqCst) {
        match ensure_subscribed(&shared, &link, deadline).await {
            Ok(()) => {
                if !first {
                    let _ = shared.events_tx.send(EventItem::Resynced);
                }
            }
            Err(e) => return TaskEnd::Lost(format!("subscribe_events: {e}")),
        }
    }

    let panes: Vec<Arc<PaneShared>> = lock(&shared.panes).values().cloned().collect();
    let mut inflight = Vec::new();
    for pane in panes {
        let guard = pane.attached_gen.clone().lock_owned().await;
        if *guard == link.gen {
            continue;
        }
        let sent = attach_params(&link, &pane)
            .and_then(|params| send_request(&shared, &link, method::ATTACH_PANE, params));
        match sent {
            Ok((id, rx)) => inflight.push((pane, guard, id, rx)),
            Err(e) => return TaskEnd::Lost(format!("attach_pane: {e}")),
        }
    }
    for (pane, mut guard, id, rx) in inflight {
        match await_answer(&link, id, rx, deadline).await {
            Ok(_) => *guard = link.gen,
            Err(ClientError::Server { code, message }) => {
                warn!("client: pane {} cannot be re-attached: {code} {message}", pane.id);
                lock(&shared.panes).remove(&pane.id);
                let tx = pane.tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Tagged { stream: ALWAYS, item: PaneItem::Ended }).await;
                });
            }
            Err(e) => return TaskEnd::Lost(format!("attach_pane: {e}")),
        }
    }
    link.resumed.store(true, Ordering::SeqCst);
    let _ = resumed.send(());
    TaskEnd::Done
}

/// A server run with another `boot_id` than the last connection's is a
/// restart: its programs are gone, so the input held here, which the old run
/// never acked, must not be typed into the fresh shells (AD-017). Drop it and
/// report how much.
fn note_boot(shared: &Shared, boot: Option<Uuid>) {
    let prev = std::mem::replace(&mut *lock(&shared.boot_id), boot);
    let (Some(prev), Some(boot)) = (prev, boot) else { return };
    if prev == boot {
        return;
    }
    let bytes = {
        let mut st = lock(&shared.input.state);
        let bytes = st.bytes as u64;
        st.frames.clear();
        st.bytes = 0;
        bytes
    };
    shared.input.space.notify_waiters();
    if bytes > 0 {
        warn!("client: the server restarted, {bytes} bytes of unacked input were not sent");
        shared.input_lost.fetch_add(bytes, Ordering::SeqCst);
        let _ = shared.events_tx.send(EventItem::InputLost { bytes });
    }
}

/// Send every unacked frame in `seq` order. After a reconnect `sent` starts
/// at 0, so all unacked frames go out again; the server drops the ones it
/// already wrote (exactly-once input, PROTOCOL_V2 section 5).
async fn input_writer(
    shared: Arc<Shared>,
    mut send: SendStream,
    resumed: oneshot::Receiver<()>,
    flags: bool,
) -> TaskEnd {
    if resumed.await.is_err() {
        return TaskEnd::Done; // init failed, the session is ending
    }
    let q = &shared.input;
    let mut sent = 0u64;
    loop {
        let ready = q.ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();
        let next = {
            let st = lock(&q.state);
            let i = st.frames.partition_point(|f| f.seq <= sent);
            st.frames.get(i).map(|f| (f.seq, f.payload(flags)))
        };
        let Some((seq, payload)) = next else {
            ready.await;
            continue;
        };
        if let Err(e) = write_frame(&mut send, &payload).await {
            return TaskEnd::Lost(format!("input write: {e}"));
        }
        sent = seq;
    }
}

/// Ping every second; declare the connection dead after 3 s without a Pong.
async fn liveness(shared: Arc<Shared>, link: Arc<Link>) -> TaskEnd {
    let interval = Duration::from_millis(PING_INTERVAL_MS);
    let dead_after = Duration::from_millis(PONG_DEAD_AFTER_MS);
    let mut last_ping: Option<Instant> = None;
    loop {
        let due = last_ping.is_none_or(|t| t.elapsed() >= interval);
        if due {
            let nonce = shared.next_nonce.fetch_add(1, Ordering::Relaxed);
            if send_ping(&link, nonce).is_err() {
                return TaskEnd::Lost("control closed".into());
            }
            last_ping = Some(Instant::now());
            note_direct_addr(&shared, &link.conn);
        }
        if lock(&link.last_pong).elapsed() > dead_after {
            return TaskEnd::Lost("no Pong in 3 s".into());
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            _ = shared.ping_now.notified() => { last_ping = None; }
        }
    }
}

/// Remember the direct address of the selected path, for the next dial.
fn note_direct_addr(shared: &Shared, conn: &Connection) {
    let direct = conn.to_info().paths().into_iter().find_map(|p| match p.remote_addr() {
        iroh::TransportAddr::Ip(addr) if p.is_selected() => Some(*addr),
        _ => None,
    });
    if let Some(addr) = direct {
        shared.opts.addr_cache.insert(shared.server.id, addr);
    }
}

fn send_ping(link: &Link, nonce: u64) -> Result<(), ClientError> {
    let frame = encode_msg(&ControlMsg::Ping { nonce })?;
    link.control_tx.send(frame).map_err(|_| ClientError::ConnectionLost)
}

/// Feed one pane output stream into its `PaneStream`.
async fn pane_reader(shared: Arc<Shared>, gen: u64, mut recv: RecvStream) -> TaskEnd {
    let header: PaneStreamHeader = match read_msg(&mut recv).await {
        Ok(Some(h)) => h,
        Ok(None) => return TaskEnd::Done,
        // Replaced by a snapshot catch-up before its header was read.
        Err(e) if is_superseded(&e) => return TaskEnd::Done,
        Err(e) => return TaskEnd::Lost(format!("pane header: {e}")),
    };
    let pane = lock(&shared.panes).get(&header.pane_id).cloned();
    let Some(pane) = pane else {
        debug!("client: stream for unknown pane {}, stopped", header.pane_id);
        let _ = recv.stop(0u8.into());
        return TaskEnd::Done;
    };

    // Claim the pane before reserving a slot: from here an older stream of
    // this pane sends nothing more, so its bytes cannot land after a reset.
    let mine = {
        let mut st = lock(&pane.state);
        st.stream += 1;
        if header.reset {
            // From here the consumer drops what older streams queued.
            st.reset_stream = st.stream;
        }
        st.stream
    };
    debug!(
        "client: pane {} stream {mine}: reset={} start={} snapshot_len={}",
        pane.id, header.reset, header.start_offset, header.snapshot_len
    );
    // A reset item must reach the consumer before any byte, so reserve the
    // channel slot first.
    let Ok(permit) = pane.tx.reserve().await else {
        return TaskEnd::Done; // PaneStream dropped
    };
    {
        let mut st = lock(&pane.state);
        if st.stream != mine {
            return TaskEnd::Done; // an even newer stream already took over
        }
        if header.reset {
            // A snapshot is not complete until its last byte arrived: a drop before that
            // must reattach from scratch, not resume at an offset the VT never reached.
            st.offset = if header.snapshot_len > 0 { None } else { Some(header.start_offset) };
        } else if st.offset.is_some_and(|o| o != header.start_offset) {
            return TaskEnd::Lost(format!(
                "pane {} resumed at {} but expected {:?}",
                pane.id, header.start_offset, st.offset
            ));
        } else {
            st.offset = Some(header.start_offset);
        }
        st.gen = gen;
        st.replay_end = header.replay_end;
        if header.reset {
            permit.send(Tagged {
                stream: mine,
                item: PaneItem::Reset { rows: header.rows, cols: header.cols },
            });
        }
    }

    let mut snapshot_left = header.snapshot_len as usize;
    let mut buf = Vec::new();
    loop {
        match read_output_frame(&mut recv, &mut buf).await {
            Ok(true) => {
                let Ok(permit) = pane.tx.reserve().await else {
                    return TaskEnd::Done;
                };
                let mut st = lock(&pane.state);
                if st.gen != gen || st.stream != mine {
                    return TaskEnd::Done; // a newer stream owns the pane
                }
                let offset = st.offset.unwrap_or(header.start_offset);
                if snapshot_left > 0 {
                    if buf.len() > snapshot_left || !header.reset {
                        return TaskEnd::Lost(format!(
                            "pane {} snapshot frame does not fit the header",
                            pane.id
                        ));
                    }
                    snapshot_left -= buf.len();
                    if snapshot_left == 0 {
                        // Complete: from here a reconnect may resume at the pane offset.
                        st.offset = Some(header.start_offset);
                    }
                    permit.send(Tagged {
                        stream: mine,
                        item: PaneItem::Snapshot { offset, bytes: std::mem::take(&mut buf) },
                    });
                } else {
                    st.offset = Some(offset + buf.len() as u64);
                    permit.send(Tagged {
                        stream: mine,
                        item: PaneItem::Data { offset, bytes: std::mem::take(&mut buf) },
                    });
                }
            }
            Ok(false) => {
                // Clean FIN: the pane closed.
                let current = {
                    let st = lock(&pane.state);
                    st.gen == gen && st.stream == mine
                };
                if current {
                    lock(&shared.panes).remove(&pane.id);
                    let _ = pane.tx.send(Tagged { stream: ALWAYS, item: PaneItem::Ended }).await;
                }
                return TaskEnd::Done;
            }
            Err(e) => {
                if is_superseded(&e) || lock(&pane.state).stream != mine {
                    // The server replaced this stream (snapshot catch-up, new attach).
                    // The new stream is on its way, this one is over.
                    return TaskEnd::Done;
                }
                return TaskEnd::Lost(format!("pane {} read: {e}", pane.id));
            }
        }
    }
}

/// `true` when a pane stream was reset with `stream_code::SUPERSEDED`.
fn is_superseded(e: &forgetty_proto::FrameError) -> bool {
    let forgetty_proto::FrameError::Io(io) = e else { return false };
    io.get_ref().and_then(|inner| inner.downcast_ref::<iroh::endpoint::ReadError>()).is_some_and(
        |r| {
            matches!(r, iroh::endpoint::ReadError::Reset(code)
                if code.into_inner() == u64::from(forgetty_proto::stream_code::SUPERSEDED))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stored_wake_permit_is_drained() {
        let n = Notify::new();
        n.notify_one();
        drain_permit(&n).await;
        let woke = tokio::time::timeout(Duration::from_millis(30), n.notified()).await;
        assert!(woke.is_err(), "the permit survived the drain");
        // With no permit the drain returns at once and does not store one.
        drain_permit(&n).await;
        assert!(tokio::time::timeout(Duration::from_millis(30), n.notified()).await.is_err());
    }

    #[test]
    fn input_is_cut_only_at_character_and_escape_boundaries() {
        // 3-byte characters: no cut may fall inside one.
        let text = "\u{20ac}".repeat(INPUT_CHUNK);
        let pieces = split_input(text.as_bytes());
        assert!(pieces.len() > 1);
        for p in &pieces {
            assert!(std::str::from_utf8(p).is_ok(), "a piece ends inside a UTF-8 character");
            assert!(p.len() <= INPUT_CHUNK);
        }
        assert_eq!(pieces.concat(), text.as_bytes());
        // A CSI that would straddle the cut moves whole into the next piece.
        let mut b = vec![b'a'; INPUT_CHUNK - 3];
        b.extend_from_slice(b"\x1b[200~");
        b.extend_from_slice(&[b'b'; 100]);
        let pieces = split_input(&b);
        assert_eq!(pieces[0].len(), INPUT_CHUNK - 3);
        assert!(pieces[1].starts_with(b"\x1b[200~"));
        assert_eq!(pieces.concat(), b);
        // A finished sequence at the edge stays where it is.
        let mut b = vec![b'a'; INPUT_CHUNK - 4];
        b.extend_from_slice(b"\x1b[1m");
        b.extend_from_slice(&[b'b'; 10]);
        assert_eq!(split_input(&b)[0].len(), INPUT_CHUNK);
        // Short input is one piece, and a lone ESC run cannot loop forever.
        assert_eq!(split_input(b"ls\r").len(), 1);
        let esc = vec![0x1b; INPUT_CHUNK * 2 + 5];
        assert_eq!(split_input(&esc).concat(), esc);
    }

    #[test]
    fn backoff_follows_the_contract() {
        let ms: Vec<u128> = (0..9).map(|i| backoff(i).as_millis()).collect();
        assert_eq!(ms, [100, 250, 500, 1000, 2000, 2000, 2000, 2000, 2000]);
    }
}
