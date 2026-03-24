//! PTY multiplexer for managing multiple terminal sessions.
//!
//! Routes I/O between multiple PTY processes and their corresponding
//! terminal panes, handling concurrent reads and dispatching output
//! to the correct VT parser instance.

// TODO: Phase 3 — implement PtyMultiplexer
//
// use forgetty_core::PaneId;
//
// pub struct PtyMultiplexer {
//     // Map of PaneId -> PtyProcess
//     // Async task handles for each reader
// }
//
// impl PtyMultiplexer {
//     pub fn new() -> Self { ... }
//     pub fn spawn_pane(&mut self, pane_id: PaneId, command: &str) -> Result<()> { ... }
//     pub fn remove_pane(&mut self, pane_id: &PaneId) -> Result<()> { ... }
//     pub fn write_to_pane(&self, pane_id: &PaneId, data: &[u8]) -> Result<()> { ... }
// }
