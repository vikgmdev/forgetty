//! iroh `Endpoint` lifecycle wrapper for Forgetty sync.
//!
//! # API deviation from spec (iroh 0.35 → 0.97)
//!
//! | Spec (0.35)                            | Implementation (0.97)                          |
//! |----------------------------------------|------------------------------------------------|
//! | `NodeId`                               | `iroh::EndpointId`                             |
//! | `iroh::NodeId::from_secret_key(&k)`    | `secret_key.public()` (via Endpoint::id())     |
//! | `Endpoint::builder().secret_key().bind()` | `Endpoint::builder(presets::N0).secret_key().alpns().bind()` |
//! | `SecretKey::generate()`                | `SecretKey::generate(&mut OsRng)`              |
//! | `Connecting::remote_node_id()`         | `Connection::remote_id()` (post-handshake)     |

use std::sync::{Arc, Mutex};

use iroh::{Endpoint, EndpointId, SecretKey, endpoint::presets};
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::{
    pairing,
    registry::{DeviceEntry, DeviceRegistry},
};

/// ALPN identifier for Forgetty's pairing protocol.
const FORGETTY_PAIRING_ALPN: &[u8] = b"forgetty/pair/1";

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

/// Wrapper around an iroh `Endpoint` that manages the pairing accept loop and
/// the device registry.
pub struct SyncEndpoint {
    endpoint: Endpoint,
    registry: Arc<Mutex<DeviceRegistry>>,
    allow_pairing: bool,
    /// Broadcast channel for pairing/connection events. Receivers are vended to
    /// socket RPC handlers via `subscribe()`.
    pub event_tx: broadcast::Sender<SyncEvent>,
}

impl SyncEndpoint {
    /// Bind a new iroh endpoint with the given secret key.
    ///
    /// The registry is loaded from disk (or created empty) on bind.
    pub async fn bind(
        secret_key: SecretKey,
        allow_pairing: bool,
    ) -> Result<Self, SyncError> {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(vec![FORGETTY_PAIRING_ALPN.to_vec()])
            .bind()
            .await
            .map_err(|e| SyncError::Bind(e.to_string()))?;

        let registry = Arc::new(Mutex::new(
            DeviceRegistry::load().map_err(SyncError::Registry)?,
        ));
        let (event_tx, _) = broadcast::channel(64);

        Ok(Self { endpoint, registry, allow_pairing, event_tx })
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

    /// Close the iroh endpoint gracefully.
    ///
    /// Must be called before dropping the endpoint to avoid the iroh error:
    /// `ERROR Endpoint dropped without calling Endpoint::close. Aborting ungracefully.`
    pub async fn close(&self) {
        self.endpoint.close().await;
    }

    /// Run the accept loop.
    ///
    /// Accepts incoming iroh connections indefinitely and dispatches each to the
    /// pairing handler. Intended to run as a `tokio::spawn`ed task alongside the
    /// existing socket server task.
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

            // Accept the QUIC handshake (iroh 0.97: Incoming → Accepting → Connection).
            let accepting = match incoming.accept() {
                Ok(a) => a,
                Err(e) => {
                    warn!("totem-sync: incoming.accept() error: {e}");
                    continue;
                }
            };

            let registry = Arc::clone(&self.registry);
            let allow_pairing = self.allow_pairing;
            let event_tx = self.event_tx.clone();

            tokio::spawn(async move {
                match accepting.await {
                    Ok(conn) => {
                        pairing::handle_connection(conn, registry, allow_pairing, event_tx).await;
                    }
                    Err(e) => {
                        warn!("totem-sync: connection handshake error: {e}");
                    }
                }
            });
        }
    }
}
