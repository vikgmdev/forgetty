//! `forgetty-sync` — iroh-based identity and QR pairing for Forgetty.
//!
//! This crate has no GTK dependency. It exposes a clean async API that the
//! daemon binary uses. GTK learns about pairing events only through the
//! Unix-socket JSON-RPC methods that `forgetty-socket` exposes.

// Pre-existing lints surfaced by clippy 1.94.0; Android wire is frozen per
// V2-003 SPEC §7, so cleanup waits on a separate P-xxx chore.
#![allow(clippy::implicit_saturating_sub, clippy::manual_is_multiple_of, clippy::while_let_loop)]
//!
//! # API deviation from spec
//!
//! The spec was written against `iroh 0.35`. The implementation uses `iroh 0.97`,
//! which renamed several types:
//! - `NodeId`      → `iroh::EndpointId`
//! - `Endpoint::builder().secret_key().bind()` → `Endpoint::builder(presets::N0).secret_key().bind()`
//! - `SecretKey::generate()` → `SecretKey::generate(&mut rand::rngs::OsRng)`
//! - `connecting.remote_node_id()` → `connection.remote_id()` (post-handshake)

pub mod endpoint;
pub mod identity;
pub mod pairing;
pub mod qr;
pub mod registry;

pub use endpoint::{
    AlpnHandler, SyncEndpoint, SyncEndpointBuilder, SyncEvent, FORGETTY_PAIRING_ALPN,
};
pub use identity::load_or_generate;
pub use qr::QrPayload;
pub use registry::{DeviceEntry, DeviceRegistry};
