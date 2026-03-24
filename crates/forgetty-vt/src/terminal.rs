//! High-level terminal state machine.
//!
//! Wraps the low-level VT parser FFI and provides a safe, ergonomic Rust
//! interface for feeding input data and querying terminal state.

// TODO: Phase 2 — implement Terminal struct
//
// pub struct Terminal {
//     // The underlying VT parser handle
//     // Screen dimensions
//     // Scrollback buffer
// }
//
// impl Terminal {
//     pub fn new(rows: usize, cols: usize) -> Self { ... }
//     pub fn feed(&mut self, data: &[u8]) { ... }
//     pub fn resize(&mut self, rows: usize, cols: usize) { ... }
//     pub fn screen(&self) -> &Screen { ... }
// }
