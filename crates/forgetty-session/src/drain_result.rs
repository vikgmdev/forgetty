//! `DrainResult` — the return type for `SessionManager::process_pane_bytes()`.

/// Result of processing one chunk of PTY output bytes for a pane.
///
/// Per AD-007 the daemon does not parse VT — the raw bytes are teed into the
/// per-pane `ByteLog` ring and broadcast to clients. Each client owns its
/// own VT (AD-008) and feeds these bytes into it.
#[derive(Debug, Clone)]
pub struct DrainResult {
    /// Whether the PTY reader thread has exited (channel disconnected or
    /// child process is no longer alive).
    pub pty_exited: bool,
}
