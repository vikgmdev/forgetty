//! Unix domain socket IPC server for Forgetty.
//!
//! Provides a JSON-RPC 2.0 interface over a local Unix domain socket that
//! external tools (CLI, editor plugins, AI agents) can use to communicate
//! with a running Forgetty instance. Supports commands like listing tabs,
//! opening new panes, sending input, and querying terminal state.

// Pre-existing: `Response` is a large enum used as the error variant of
// several `Result<T, Response>` helpers in handlers/protocol. Boxing is a
// follow-up cleanup, not in V2-003 scope.
#![allow(clippy::result_large_err)]

pub mod framing;
pub mod handlers;
pub mod protocol;
pub mod server;

pub use handlers::dispatch;
pub use handlers::save_all_snapshots;
pub use protocol::{Request, Response};
pub use server::SocketServer;
