//! Unix domain socket server.
//!
//! Listens on a Unix domain socket for incoming connections and
//! dispatches requests to the appropriate handlers.

// TODO: Phase 8 — implement SocketServer
//
// use tokio::net::UnixListener;
//
// pub struct SocketServer {
//     listener: UnixListener,
// }
//
// impl SocketServer {
//     pub async fn bind(path: &Path) -> Result<Self> { ... }
//     pub async fn run(&self) -> Result<()> { ... }
// }
