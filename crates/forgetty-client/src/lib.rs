//! Forgetty protocol v2 client (`forgetty/rpc/2`, `forgetty/pair/2`).
//!
//! No UI, no JNI, no platform specific code. The Android and Windows apps
//! wrap this crate.
//!
//! - [`Client::connect`] dials, runs the handshake and starts the reconnect
//!   supervisor (PROTOCOL_V2 section 9).
//! - [`ClientHandle::request`], [`ClientHandle::events`],
//!   [`ClientHandle::attach_pane`], [`ClientHandle::send_input`],
//!   [`ClientHandle::resize`].
//! - [`pair`] pairs a new device from a QR payload.
//! - [`ConnectionState`] is observable through [`ClientHandle::watch_state`].

mod addr_cache;
mod client;
mod config;
mod error;
pub mod identity;
mod pair;
mod state;
#[cfg(feature = "test-util")]
pub mod test_server;

pub use addr_cache::{AddrCache, AddrCacheSnapshot, CachedAddr};
pub use client::{Client, ClientHandle, EventItem, EventStream, PaneItem, PaneStream};
pub use config::{transport_config, ClientInfo, ClientOptions, RelayChoice};
pub use error::ClientError;
pub use identity::{load_or_generate_client_id, load_or_generate_identity};
pub use pair::{pair, pair_with_addr, qr_to_addr, server_addr, PairedServer};
pub use state::ConnectionState;

pub use forgetty_proto as proto;
pub use iroh::{EndpointAddr, EndpointId, SecretKey};
