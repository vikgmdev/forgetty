//! Pseudoterminal (PTY) management for Forgetty.
//!
//! This crate handles spawning shell processes in pseudoterminals,
//! managing their lifecycle, and multiplexing I/O across multiple
//! terminal panes.

pub mod multiplexer;
#[cfg(not(windows))]
pub mod process;
#[cfg(windows)]
#[path = "process_windows.rs"]
pub mod process;

pub use multiplexer::{PtyId, PtyMultiplexer};
pub use process::{PtyProcess, PtySize};
#[cfg(windows)]
pub use process::{default_shell_argv, join_server_job, set_default_shell, set_default_shell_from_config};
