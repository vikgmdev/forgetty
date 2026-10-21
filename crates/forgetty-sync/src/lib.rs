//! `forgetty-sync` — iroh-based identity and QR pairing for Forgetty.
//!
//! This crate has no GTK dependency. It exposes a clean async API that the
//! daemon binary uses. GTK learns about pairing events only through the
//! Unix-socket JSON-RPC methods that `forgetty-socket` exposes.
//!
//! # API deviation from spec
//!
//! The spec was written against `iroh 0.35`. The implementation uses `iroh 0.97`,
//! which renamed several types:
//! - `NodeId`      → `iroh::EndpointId`
//! - `Endpoint::builder().secret_key().bind()` → `Endpoint::builder(presets::N0).secret_key().bind()`
//! - `SecretKey::generate()` → `SecretKey::generate(&mut rand::rngs::OsRng)`
//! - `connecting.remote_node_id()` → `connection.remote_id()` (post-handshake)

pub mod connections;
pub mod endpoint;
pub mod identity;
pub mod pairing;
pub mod qr;
pub mod registry;
/// Sanitising of untrusted text. Lives in `forgetty-core`; re-exported here.
pub use forgetty_core::sanitize;

pub use endpoint::{
    AlpnHandler, SyncEndpoint, SyncEndpointBuilder, SyncEvent, FORGETTY_PAIRING_ALPN,
    FORGETTY_PAIRING_ALPN_V1,
};
pub use identity::load_or_generate;
pub use pairing::{
    OpenedWindow, PairFailure, PairRequest, PairResult, PairStatus, PairingState,
    PAIRING_WINDOW_SECS,
};
pub use qr::QrPayload;
pub use registry::{DeviceEntry, DeviceRegistry};
