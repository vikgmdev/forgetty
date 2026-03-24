//! Unix domain socket IPC server for Forgetty.
//!
//! Provides a local socket interface that external tools (CLI, editor
//! plugins, AI agents) can use to communicate with a running Forgetty
//! instance. Supports commands like opening new panes, sending input,
//! and querying terminal state.

pub mod handlers;
pub mod protocol;
pub mod server;

// TODO: Phase 8 — re-export key types once socket server is implemented
