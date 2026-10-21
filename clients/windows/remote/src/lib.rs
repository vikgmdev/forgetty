//! Forgetty Windows client: the remote side. `RemoteBackend` implements
//! `SessionBackend` on `forgetty-client` (protocol v2), `store` keeps the
//! paired machines and the device identity, `acl` restricts those files to the
//! current user. No UI code.

pub mod acl;
mod backend;
mod convert;
mod source;
pub mod store;

pub use backend::{map_conn, map_err, RemoteBackend, RemoteParams};
pub use source::{snapshot_marker, RemotePaneSource, SNAPSHOT_MARKER};
