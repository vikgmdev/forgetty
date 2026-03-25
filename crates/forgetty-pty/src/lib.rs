//! Pseudoterminal (PTY) management for Forgetty.
//!
//! This crate handles spawning shell processes in pseudoterminals,
//! managing their lifecycle, and multiplexing I/O across multiple
//! terminal panes.

pub mod multiplexer;
pub mod process;

pub use multiplexer::{PtyId, PtyMultiplexer};
pub use process::{PtyProcess, PtySize};
