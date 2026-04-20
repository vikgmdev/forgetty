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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::{endpoint::presets, endpoint::Connection, Endpoint, EndpointId, SecretKey};
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::{
    pairing,
    registry::{DeviceEntry, DeviceRegistry},
};

/// ALPN identifier for Forgetty's pairing protocol.
///
/// Owned by `forgetty-sync`; automatically registered by every
/// `SyncEndpoint` because pairing is the reason this crate exists.
pub const FORGETTY_PAIRING_ALPN: &[u8] = b"forgetty/pair/1";

/// Type-erased handler for a non-pairing ALPN.
///
/// Invoked once per accepted connection on a matching ALPN. Receives the
/// authorized-device registry so the handler can enforce its own
/// authorization policy (the registry is shared across ALPNs; updates made
/// by the pairing handler are visible here without further plumbing).
///
/// The handler is called **synchronously** inside the per-connection tokio
/// task. If it needs to run long-lived work (e.g. a subscribe-forever stream),
/// it must spawn its own task — otherwise it blocks the task that would
/// otherwise return to the accept loop's spawn for the next connection.
pub type AlpnHandler = Arc<dyn Fn(Connection, Arc<Mutex<DeviceRegistry>>) + Send + Sync + 'static>;

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

/// Builder for [`SyncEndpoint`].
///
/// Construct via [`SyncEndpoint::builder`], chain zero or more
/// [`register_alpn`](Self::register_alpn) calls to add non-pairing ALPNs,
/// then call [`build`](Self::build) to perform the async `Endpoint::bind`.
///
/// The pairing ALPN is always registered; callers do not need to add it.
pub struct SyncEndpointBuilder {
    secret_key: SecretKey,
    allow_pairing: bool,
    extra_alpns: HashMap<Vec<u8>, AlpnHandler>,
}

impl SyncEndpointBuilder {
    /// Enable auto-pair mode for unknown devices at bind time.
    ///
    /// Equivalent semantics to the flag previously passed to
    /// `SyncEndpoint::bind`. Call [`SyncEndpoint::enable_pairing`] post-bind
    /// for a time-limited pairing window.
    pub fn allow_pairing(mut self, allow: bool) -> Self {
        self.allow_pairing = allow;
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

    /// Bind the iroh endpoint and construct the [`SyncEndpoint`].
    ///
    /// All registered ALPNs (pairing + extras) are advertised during the
    /// QUIC handshake. The registry is loaded from disk (or created empty).
    pub async fn build(self) -> Result<SyncEndpoint, SyncError> {
        let mut alpn_list: Vec<Vec<u8>> = Vec::with_capacity(self.extra_alpns.len() + 1);
        alpn_list.push(FORGETTY_PAIRING_ALPN.to_vec());
        alpn_list.extend(self.extra_alpns.keys().cloned());

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(self.secret_key)
            .alpns(alpn_list)
            .bind()
            .await
            .map_err(|e| SyncError::Bind(e.to_string()))?;

        let registry = Arc::new(Mutex::new(DeviceRegistry::load().map_err(SyncError::Registry)?));
        let (event_tx, _) = broadcast::channel(64);
        let allow_pairing = Arc::new(AtomicBool::new(self.allow_pairing));

        Ok(SyncEndpoint {
            endpoint,
            registry,
            allow_pairing,
            extra_alpns: Arc::new(self.extra_alpns),
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
    allow_pairing: Arc<AtomicBool>,
    extra_alpns: Arc<HashMap<Vec<u8>, AlpnHandler>>,
    /// Broadcast channel for pairing/connection events. Receivers are vended to
    /// socket RPC handlers via `subscribe()`.
    pub event_tx: broadcast::Sender<SyncEvent>,
}

impl SyncEndpoint {
    /// Start configuring a new `SyncEndpoint`.
    pub fn builder(secret_key: SecretKey) -> SyncEndpointBuilder {
        SyncEndpointBuilder { secret_key, allow_pairing: false, extra_alpns: HashMap::new() }
    }

    /// Returns the local `EndpointId` (iroh 0.97 equivalent of spec's `NodeId`).
    pub fn node_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Returns a clone of the shared device registry.
    pub fn registry(&self) -> Arc<Mutex<DeviceRegistry>> {
        Arc::clone(&self.registry)
    }

    /// Subscribe to sync events.
    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.event_tx.subscribe()
    }

    /// Temporarily open a pairing window for `secs` seconds.
    ///
    /// Sets `allow_pairing` to `true` and spawns a task that resets it to
    /// `false` after the timeout. Safe to call from any thread.
    pub fn enable_pairing(&self, secs: u64) {
        self.allow_pairing.store(true, Ordering::Relaxed);
        let flag = Arc::clone(&self.allow_pairing);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            flag.store(false, Ordering::Relaxed);
        });
    }

    /// Close the iroh endpoint gracefully.
    ///
    /// Must be called before dropping the endpoint to avoid the iroh error:
    /// `ERROR Endpoint dropped without calling Endpoint::close. Aborting ungracefully.`
    pub async fn close(&self) {
        self.endpoint.close().await;
    }

    /// Run the accept loop.
    ///
    /// Accepts incoming iroh connections indefinitely, reads the negotiated
    /// ALPN via `Accepting::alpn()`, and dispatches to the appropriate
    /// handler:
    /// - `forgetty/pair/1` → [`pairing::handle_connection`].
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

            // Read the negotiated ALPN before completing the handshake.
            // `Accepting::alpn()` is async and takes `&mut self`; after it
            // returns the `Accepting` can still be awaited for the Connection.
            let alpn = match accepting.alpn().await {
                Ok(a) => a,
                Err(e) => {
                    warn!("totem-sync: failed to read ALPN: {e}");
                    continue;
                }
            };

            let registry = Arc::clone(&self.registry);
            let allow_pair = self.allow_pairing.load(Ordering::Relaxed);
            let event_tx = self.event_tx.clone();
            let extras = Arc::clone(&self.extra_alpns);

            tokio::spawn(async move {
                match accepting.await {
                    Ok(conn) => {
                        if alpn == FORGETTY_PAIRING_ALPN {
                            pairing::handle_connection(conn, registry, allow_pair, event_tx).await;
                        } else if let Some(handler) = extras.get(alpn.as_slice()) {
                            handler(conn, registry);
                        } else {
                            warn!("totem-sync: unknown ALPN {:?}, closing connection", alpn);
                            conn.close(1u8.into(), b"unknown-alpn");
                        }
                    }
                    Err(e) => {
                        warn!("totem-sync: connection handshake error: {e}");
                    }
                }
            });
        }
    }
}
