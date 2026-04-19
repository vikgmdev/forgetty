//! `PaneState` — the private per-pane record inside the session manager, and
//! `PaneInfo` — the public snapshot of pane metadata returned by `pane_info()`.

use std::path::PathBuf;

use forgetty_core::PaneId;

use crate::pty_bridge::PtyBridge;

/// Full per-pane state, owned behind the `Arc<Mutex<SessionManagerInner>>`.
///
/// Per AD-007 the daemon is a byte pipe — there is no VT parser here. Raw
/// PTY output is teed into the per-pane `ByteLog` (in `SessionManagerInner`)
/// and broadcast to clients that own the VT (AD-008).
///
/// None of these fields reference any GTK type.
pub struct PaneState {
    /// Unique identifier for this pane.
    pub id: PaneId,
    /// PTY bridge (process + background reader thread + output channel).
    pub pty_bridge: PtyBridge,
    /// Last known working directory (updated lazily from `/proc/{pid}/cwd`).
    pub cwd: PathBuf,
    /// Last known title (reported by the client, or CWD basename fallback).
    pub title: String,
    /// Current PTY rows.
    pub rows: u16,
    /// Current PTY columns.
    pub cols: u16,
}

/// Public snapshot of pane metadata (returned by `SessionManager::pane_info()`).
#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub id: PaneId,
    /// Child process ID, if available.
    pub pid: Option<u32>,
    pub rows: u16,
    pub cols: u16,
    /// Last known working directory.
    pub cwd: PathBuf,
    /// Last known title.
    pub title: String,
}
