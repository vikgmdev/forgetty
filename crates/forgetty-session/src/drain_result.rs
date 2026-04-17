//! `DrainResult` — the return type for `SessionManager::drain_output()`.

/// Result of draining the PTY output channel for one pane.
#[derive(Debug, Clone)]
pub struct DrainResult {
    /// Whether any data was fed to the VT parser this tick.
    pub had_data: bool,
    /// Whether the PTY reader thread has exited (channel disconnected or
    /// child process is no longer alive).
    pub pty_exited: bool,
    /// The raw byte chunks that were drained, in order.
    ///
    /// The GTK layer feeds these same bytes to its own `Terminal` instance
    /// so both VTs stay in sync without re-reading the channel.
    pub raw_bytes: Vec<Vec<u8>>,
}
