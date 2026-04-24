//! Unix domain socket IPC server for Forgetty.
//!
//! Provides a JSON-RPC 2.0 interface over a local Unix domain socket that
//! external tools (CLI, editor plugins, AI agents) can use to communicate
//! with a running Forgetty instance. Supports commands like listing tabs,
//! opening new panes, sending input, and querying terminal state.

// Why: `Response` is the JSON-RPC envelope; boxing would heap-allocate on
// every handler-error path on the IPC hot-path. Large-by-design, not
// pre-existing cleanup debt. Handlers short-circuit via `?` and the
// `Response` propagates directly to the wire.
#![allow(clippy::result_large_err)]

pub mod framing;
pub mod handlers;
pub mod protocol;
pub mod server;

pub use handlers::dispatch;
pub use protocol::{Request, Response};
pub use server::SocketServer;
