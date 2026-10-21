//! iroh `Endpoint` lifecycle wrapper for Forgetty sync.
//!
//! # API deviation from spec (iroh 0.35 → 0.97)
//!
//! | Spec (0.35)                            | Implementation (0.97)                          |
//! |----------------------------------------|------------------------------------------------|
//! | `NodeId`                               | `iroh::EndpointId`                             |
//! | `iroh::NodeId::from_secret_key(&k)`    | `secret_key.public()` (via Endpoint::id())     |
//! | `Endpoint::builder().secret_key().bind()` | `Endpoint::builder(presets::N0).secret_key().alpns().bind()` |
//! | `SecretKey::generate()`                | `SecretKey::generate(&mut rand::rngs::OsRng)`  |
//! | `Connecting::remote_node_id()`         | `Connection::remote_id()` (post-handshake)     |
//!
//! # V2-011 / AD-015: transport-only with pluggable ALPNs
//!
//! `SyncEndpoint` owns **only** the pairing ALPN (`forgetty/pair/1`) because
//! pairing is the reason `forgetty-sync` exists. Any other ALPN (terminal
//! streaming, clipboard sync, file transfer, …) is registered by the binary
//! via [`SyncEndpointBuilder::register_alpn`]. The accept loop dispatches
//! by ALPN into the registered handler map.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iroh::{endpoint::presets, endpoint::Connection, Endpoint, EndpointId, SecretKey};
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::{
    pairing::{self, OpenedWindow, PairStatus, PairingState},
    registry::{DeviceEntry, DeviceRegistry},
};

/// ALPN identifier for the retired pairing protocol (v1).
///
/// Still registered so old clients get a clear "upgrade the app" error
/// instead of a bare QUIC failure. It never pairs a device (AD-021).
pub const FORGETTY_PAIRING_ALPN_V1: &[u8] = b"forgetty/pair/1";

/// ALPN identifier for Forgetty's pairing protocol (v2, AD-021).
///
/// Owned by `forgetty-sync`; automatically registered by every
/// `SyncEndpoint` because pairing is the reason this crate exists.
pub const FORGETTY_PAIRING_ALPN: &[u8] = b"forgetty/pair/2";

/// Type-erased handler for a non-pairing ALPN.
///
/// Invoked once per accepted connection on a matching ALPN. Receives the
/// authorized-device registry so the handler can enforce its own
/// authorization policy (the registry is shared across ALPNs; updates made
/// by the pairing handler are visible here without further plumbing).
///
/// The handler returns a future that lives as long as the connection is in
/// use: the accept task awaits it and then closes the connection, so a handler
/// that returns early (or forgets to close) does not leave it open. A handler
/// that wants to keep the connection past its return must not return.
pub type AlpnHandler = Arc<
    dyn Fn(
            Connection,
            Arc<Mutex<DeviceRegistry>>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync
        + 'static,
>;

/// Events emitted by the sync endpoint to signal pairing and connection state
/// changes. Consumed by the socket RPC handlers for GTK polling.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    /// A new device successfully completed the pairing handshake.
    DevicePaired { entry: DeviceEntry },
    /// A device was revoked via the socket RPC.
    DeviceRevoked { device_id: String },
    /// A known device opened a new connection.
    DeviceConnected { device_id: String },
    /// A device's connection was closed.
    DeviceDisconnected { device_id: String },
}

/// Errors from binding or operating the sync endpoint.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("Failed to bind iroh endpoint: {0}")]
    Bind(String),
    #[error("Registry error: {0}")]
    Registry(#[from] crate::registry::RegistryError),
}

/// What [`SyncEndpoint::revoke_device`] did.
#[derive(Debug)]
pub struct RevokeOutcome {
    /// The device was in the registry (or its removal was started).
    pub found: bool,
    /// Live connections closed.
    pub closed: usize,
    /// Set when the registry file could not be saved. The device is revoked
    /// anyway (denied in memory) and the next successful save persists it.
    pub save_error: Option<crate::registry::RegistryError>,
    /// A restart cannot bring the device back: the revoke record in
    /// `revoked.pending` or the registry file has it on disk. `false` means
    /// the device is denied in memory only.
    pub durable: bool,
}

/// Builder for [`SyncEndpoint`].
///
/// Construct via [`SyncEndpoint::builder`], chain zero or more
/// [`register_alpn`](Self::register_alpn) calls to add non-pairing ALPNs,
/// then call [`build`](Self::build) to perform the async `Endpoint::bind`.
///
/// The pairing ALPN is always registered; callers do not need to add it.
pub struct SyncEndpointBuilder {
    secret_key: SecretKey,
    extra_alpns: HashMap<Vec<u8>, AlpnHandler>,
    bind_port: Option<u16>,
    transport: Option<iroh::endpoint::QuicTransportConfig>,
    relay_urls: Vec<iroh::RelayUrl>,
    n0_discovery: bool,
}

impl SyncEndpointBuilder {
    /// Publish this endpoint to, and resolve peers from, the n0 DNS discovery
    /// service (the `N0` preset does). Default `true`. The server turns it off
    /// when it has relays of its own: the apps get relay and direct address from
    /// the pairing payload and the address cache, so nothing needs to be public.
    pub fn n0_discovery(mut self, on: bool) -> Self {
        self.n0_discovery = on;
        self
    }

    /// Use these relays instead of iroh's default relay map. The endpoint
    /// connects to the first one as its home relay. Empty keeps the default.
    pub fn relay_urls(mut self, urls: Vec<iroh::RelayUrl>) -> Self {
        self.relay_urls = urls;
        self
    }

    /// Register a handler for a non-pairing ALPN.
    ///
    /// The binary that owns the terminal stream handler (or any future
    /// service) calls this once per ALPN it wants to serve. `handler` is
    /// invoked on each accepted connection whose negotiated ALPN matches
    /// `alpn`; `forgetty-sync` does no further protocol work for the
    /// connection beyond ALPN dispatch.
    ///
    /// Duplicate registrations replace the previous handler.
    pub fn register_alpn(mut self, alpn: &[u8], handler: AlpnHandler) -> Self {
        self.extra_alpns.insert(alpn.to_vec(), handler);
        self
    }

    /// Listen on this UDP port (IPv4 and IPv6) instead of random ports, so the
    /// server's direct address stays the same across restarts. `0` picks a
    /// free port. IPv6 is skipped when the host cannot bind it.
    pub fn bind_port(mut self, port: u16) -> Self {
        self.bind_port = Some(port);
        self
    }

    /// QUIC transport settings (keep-alive, idle timeout).
    pub fn transport_config(mut self, config: iroh::endpoint::QuicTransportConfig) -> Self {
        self.transport = Some(config);
        self
    }

    /// Bind the iroh endpoint and construct the [`SyncEndpoint`].
    ///
    /// All registered ALPNs (pairing + extras) are advertised during the
    /// QUIC handshake. The registry is loaded from disk (or created empty).
    pub async fn build(self) -> Result<SyncEndpoint, SyncError> {
        let mut alpn_list: Vec<Vec<u8>> = Vec::with_capacity(self.extra_alpns.len() + 1);
        alpn_list.push(FORGETTY_PAIRING_ALPN.to_vec());
        alpn_list.push(FORGETTY_PAIRING_ALPN_V1.to_vec());
        alpn_list.extend(self.extra_alpns.keys().cloned());

        let make = |dual: bool| -> Result<iroh::endpoint::Builder, SyncError> {
            let mut builder = Endpoint::builder(presets::N0)
                .secret_key(self.secret_key.clone())
                .alpns(alpn_list.clone());
            if !self.relay_urls.is_empty() {
                builder = builder.relay_mode(iroh::RelayMode::custom(self.relay_urls.clone()));
            }
            if !self.n0_discovery {
                builder = builder.clear_address_lookup();
            }
            if let Some(t) = &self.transport {
                builder = builder.transport_config(t.clone());
            }
            if let Some(port) = self.bind_port {
                builder = builder
                    .clear_ip_transports()
                    .bind_addr(format!("0.0.0.0:{port}").as_str())
                    .map_err(|e| SyncError::Bind(e.to_string()))?;
                if dual {
                    builder = builder
                        .bind_addr(format!("[::]:{port}").as_str())
                        .map_err(|e| SyncError::Bind(e.to_string()))?;
                }
            }
            Ok(builder)
        };
        // IPv6 on the same port. A host without IPv6 binds IPv4 only.
        let endpoint = match make(self.bind_port.is_some())?.bind().await {
            Ok(ep) => ep,
            Err(e) if self.bind_port.is_some() => {
                warn!(
                    "totem-sync: dual-stack bind on port {:?} failed ({e}), using IPv4 only",
                    self.bind_port
                );
                make(false)?.bind().await.map_err(|e| SyncError::Bind(e.to_string()))?
            }
            Err(e) => return Err(SyncError::Bind(e.to_string())),
        };

        let registry = Arc::new(Mutex::new(DeviceRegistry::load().map_err(SyncError::Registry)?));
        let (event_tx, _) = broadcast::channel(64);

        Ok(SyncEndpoint {
            endpoint,
            registry,
            pairing: Arc::new(Mutex::new(PairingState::new())),
            extra_alpns: Arc::new(self.extra_alpns),
            relay_urls: self.relay_urls.iter().map(|u| u.to_string()).collect(),
            n0_discovery: self.n0_discovery,
            live: crate::connections::LiveConnections::new(),
            quic_open: Arc::new(AtomicUsize::new(0)),
            handshake_failures: Arc::new(AtomicUsize::new(0)),
            event_tx,
        })
    }
}

/// Wrapper around an iroh `Endpoint` that manages the pairing accept loop,
/// the device registry, and ALPN dispatch.
///
/// Construct via [`SyncEndpoint::builder`].
pub struct SyncEndpoint {
    endpoint: Endpoint,
    registry: Arc<Mutex<DeviceRegistry>>,
    pairing: Arc<Mutex<PairingState>>,
    extra_alpns: Arc<HashMap<Vec<u8>, AlpnHandler>>,
    /// Every open connection by device id, so a revoke can close them all.
    live: crate::connections::LiveConnections,
    /// Incoming QUIC connections, from the first packet until they close: the
    /// handshakes in progress plus the open connections.
    quic_open: Arc<AtomicUsize>,
    /// Incoming QUIC handshakes that failed: the peer went away half way.
    handshake_failures: Arc<AtomicUsize>,
    /// Configured relay URLs (empty: iroh's default relays).
    relay_urls: Vec<String>,
    /// Whether the n0 DNS discovery service is in use.
    n0_discovery: bool,
    /// Broadcast channel for pairing/connection events. Receivers are vended to
    /// socket RPC handlers via `subscribe()`.
    pub event_tx: broadcast::Sender<SyncEvent>,
}

impl SyncEndpoint {
    /// Incoming QUIC connections that are not closed yet, handshakes in
    /// progress included. A client that gave up half way through a handshake
    /// must not leave one behind.
    pub fn quic_open(&self) -> usize {
        self.quic_open.load(Ordering::SeqCst)
    }

    /// Incoming QUIC handshakes that failed so far (the peer gave up half way).
    pub fn handshake_failures(&self) -> usize {
        self.handshake_failures.load(Ordering::SeqCst)
    }

    /// Start configuring a new `SyncEndpoint`.
    pub fn builder(secret_key: SecretKey) -> SyncEndpointBuilder {
        SyncEndpointBuilder {
            secret_key,
            extra_alpns: HashMap::new(),
            bind_port: None,
            transport: None,
            relay_urls: Vec::new(),
            n0_discovery: true,
        }
    }

    /// Returns the local `EndpointId` (iroh 0.97 equivalent of spec's `NodeId`).
    pub fn node_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Relay URLs this endpoint was configured with, as written in the pairing
    /// QR. Empty when it uses iroh's default relays.
    pub fn relay_urls(&self) -> &[String] {
        &self.relay_urls
    }

    /// Whether the endpoint publishes to and resolves from n0 DNS discovery.
    pub fn n0_discovery(&self) -> bool {
        self.n0_discovery
    }

    /// The local UDP sockets the endpoint listens on.
    pub fn bound_sockets(&self) -> Vec<std::net::SocketAddr> {
        self.endpoint.bound_sockets()
    }

    /// The table of open connections.
    pub fn live_connections(&self) -> crate::connections::LiveConnections {
        self.live.clone()
    }

    /// Revoke a device: remove it from the registry AND close every open
    /// connection it has, on every ALPN.
    ///
    /// A revoke fails closed. The connections are closed and the event is sent
    /// whatever the save result is, and a device whose removal could not be
    /// saved stays denied in memory (see [`DeviceRegistry::remove`]). The
    /// connections are closed even when the device was not in the registry
    /// (for example it was removed by hand-editing the file).
    pub fn revoke_device(&self, device_id: &str) -> RevokeOutcome {
        let removed = self.registry.lock().unwrap_or_else(|e| e.into_inner()).revoke(device_id);
        let closed = self.live.revoke(device_id);
        // Removed in memory counts as found even when the save failed.
        let (found, save_error, durable) = (removed.found, removed.save_error, removed.durable);
        if found {
            let _ =
                self.event_tx.send(SyncEvent::DeviceRevoked { device_id: device_id.to_string() });
        }
        if save_error.is_some() {
            // Denied in memory meanwhile. Keep trying to put it on disk.
            crate::registry::spawn_save_retry(
                Arc::clone(&self.registry),
                std::time::Duration::from_secs(5),
            );
        }
        RevokeOutcome { found, closed, save_error, durable }
    }

    /// Returns a clone of the shared device registry.
    pub fn registry(&self) -> Arc<Mutex<DeviceRegistry>> {
        Arc::clone(&self.registry)
    }

    /// Subscribe to sync events.
    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.event_tx.subscribe()
    }

    /// Open a pairing window with a fresh one-time secret (AD-021).
    ///
    /// The window lasts `ttl` and accepts one device. Opening a new window
    /// replaces the old one. Safe to call from any thread.
    pub fn open_pairing(&self, ttl: Duration) -> OpenedWindow {
        self.pairing.lock().unwrap_or_else(|e| e.into_inner()).open(ttl, Instant::now())
    }

    /// Status of pairing window `id`.
    pub fn pairing_status(&self, id: u64) -> PairStatus {
        self.pairing.lock().unwrap_or_else(|e| e.into_inner()).status(id, Instant::now())
    }

    /// Close the iroh endpoint gracefully.
    ///
    /// Must be called before dropping the endpoint to avoid the iroh error:
    /// `ERROR Endpoint dropped without calling Endpoint::close. Aborting ungracefully.`
    ///
    /// Bounded to 2 s: a peer that gave up mid-handshake leaves a half-open
    /// connection that would otherwise hold up shutdown.
    pub async fn close(&self) {
        if tokio::time::timeout(std::time::Duration::from_secs(2), self.endpoint.close())
            .await
            .is_err()
        {
            tracing::warn!("iroh endpoint close timed out after 2 s");
        }
    }

    /// Run the accept loop.
    ///
    /// Accepts incoming iroh connections indefinitely, reads the negotiated
    /// ALPN via `Accepting::alpn()`, and dispatches to the appropriate
    /// handler:
    /// - `forgetty/pair/2` → [`pairing::handle_pair_v2`].
    /// - `forgetty/pair/1` → [`pairing::handle_pair_v1`] (refuses unknown devices).
    /// - Any registered non-pairing ALPN → the closure supplied to
    ///   [`SyncEndpointBuilder::register_alpn`].
    /// - Unknown ALPNs → logged and closed with code 1 (`unknown-alpn`).
    ///
    /// In iroh 0.97 `Endpoint::accept()` returns `Option<Incoming>`: `None`
    /// means the endpoint has been closed, at which point the loop exits.
    pub async fn accept_loop(self: Arc<Self>) {
        info!("totem-sync: accept loop started");
        loop {
            let incoming = match self.endpoint.accept().await {
                Some(i) => i,
                None => {
                    info!("totem-sync: endpoint closed, accept loop exiting");
                    break;
                }
            };

            // Begin QUIC handshake — get the Accepting future.
            let mut accepting = match incoming.accept() {
                Ok(a) => a,
                Err(e) => {
                    warn!("totem-sync: incoming.accept() error: {e}");
                    continue;
                }
            };

            let registry = Arc::clone(&self.registry);
            let pairing_state = Arc::clone(&self.pairing);
            let event_tx = self.event_tx.clone();
            let extras = Arc::clone(&self.extra_alpns);
            let live = self.live.clone();
            let open = OpenGuard::new(&self.quic_open);
            let failures = self.handshake_failures.clone();

            // Everything that waits on the peer happens in the spawned task:
            // a peer that stalls its handshake (up to the 30 s idle timeout)
            // must not hold up the next one.
            tokio::spawn(async move {
                let _open = open;
                // Read the negotiated ALPN before completing the handshake.
                // `Accepting::alpn()` is async and takes `&mut self`; after it
                // returns the `Accepting` can still be awaited for the Connection.
                let alpn = match accepting.alpn().await {
                    Ok(a) => a,
                    Err(e) => {
                        failures.fetch_add(1, Ordering::SeqCst);
                        warn!("totem-sync: failed to read ALPN: {e}");
                        return;
                    }
                };
                match accepting.await {
                    Ok(conn) => {
                        let watch = conn.clone();
                        // Track before dispatch: a revoke that lands while the
                        // handler starts still finds this connection.
                        live.track(conn.remote_id().to_string(), conn.clone());
                        if alpn == FORGETTY_PAIRING_ALPN {
                            pairing::handle_pair_v2(conn, registry, pairing_state, event_tx).await;
                        } else if alpn == FORGETTY_PAIRING_ALPN_V1 {
                            pairing::handle_pair_v1(conn, registry, event_tx).await;
                        } else if let Some(handler) = extras.get(alpn.as_slice()) {
                            handler(conn, registry).await;
                        } else {
                            warn!("totem-sync: unknown ALPN {:?}, closing connection", alpn);
                            conn.close(1u8.into(), b"unknown-alpn");
                        }
                        // The handler is done: close, whatever it did. A
                        // close after the handler's own is a no-op.
                        watch.close(0u8.into(), b"done");
                        watch.closed().await;
                    }
                    Err(e) => {
                        failures.fetch_add(1, Ordering::SeqCst);
                        warn!("totem-sync: connection handshake error: {e}");
                    }
                }
            });
        }
    }
}

/// Counts one incoming QUIC connection while it lives.
struct OpenGuard(Arc<AtomicUsize>);

impl OpenGuard {
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        OpenGuard(counter.clone())
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
