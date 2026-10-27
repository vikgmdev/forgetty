//! Pseudoterminal (PTY) management for Forgetty.
//!
//! This crate handles spawning shell processes in pseudoterminals,
//! managing their lifecycle, and multiplexing I/O across multiple
//! terminal panes.

pub mod multiplexer;
pub mod timer_boost;
#[cfg(not(windows))]
pub mod process;
#[cfg(windows)]
#[path = "process_windows.rs"]
pub mod process;

pub use multiplexer::{PtyId, PtyMultiplexer};
pub use process::{PtyProcess, PtySize};
#[cfg(not(windows))]
pub use process::detect_shell;
#[cfg(windows)]
pub use process::{
    default_shell_argv, join_server_job, opt_out_of_power_throttling, power_throttling_mode,
    set_default_shell, set_default_shell_from_config, PowerThrottling,
};

/// The shell a new pane starts, as a program name. Windows has no `$SHELL`: it is the first
/// element of [`default_shell_argv`] (the configured shell, then `pwsh.exe`, `powershell.exe`,
/// `%COMSPEC%`), and `cmd.exe` when that list is empty.
#[cfg(windows)]
pub fn detect_shell() -> String {
    default_shell_argv().into_iter().next().unwrap_or_else(|| "cmd.exe".to_string())
}
