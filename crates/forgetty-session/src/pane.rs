//! `PaneState` — the private per-pane record inside the session manager, and
//! `PaneInfo` — the public snapshot of pane metadata returned by `pane_info()`.

use std::path::PathBuf;

use forgetty_core::PaneId;
use uuid::Uuid;

use crate::osc::OscScanner;
use crate::pty_bridge::PtyBridge;
use crate::status::{StatusSnapshot, StatusTracker};

/// The last desktop notification (OSC 9, 99 or 777) a pane sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationInfo {
    pub title: Option<String>,
    pub body: String,
    /// Unix time in milliseconds.
    pub at_ms: u64,
}

/// What the OSC scanner learns about a pane (AD-019). One per pane.
#[derive(Debug, Default)]
pub struct PaneOsc {
    pub scanner: OscScanner,
    pub status: StatusTracker,
    /// Status last published, to emit `StatusChanged` only on a real change.
    pub published: StatusSnapshot,
    pub last_notification: Option<NotificationInfo>,
    /// The shell reported its directory with OSC 7. From then on that wins
    /// over the process cwd from `/proc` (the shell may be on another host).
    pub cwd_from_osc: bool,
    /// Notification rate limit window.
    pub notify_window_start_ms: u64,
    pub notify_in_window: u32,
}

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
    /// Keep the pane listed after its process exits (`forgetty new --keep`).
    pub remain_on_exit: bool,
    /// `true` once the process has exited and the pane was kept.
    pub exited: bool,
    /// Exit code of the process, once known.
    pub exit_code: Option<i32>,
    /// Title, cwd, status and notifications found in the output (AD-019).
    pub osc: PaneOsc,
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
    /// Kept after exit (opt-in).
    pub remain_on_exit: bool,
    /// The process has exited and the pane was kept.
    pub exited: bool,
    /// Exit code, once the process has exited.
    pub exit_code: Option<i32>,
    /// Program status (OSC 7501), `idle` when the program reports nothing.
    pub status: StatusSnapshot,
    /// Last desktop notification the pane sent, if any.
    pub last_notification: Option<NotificationInfo>,
}

/// Returned per-tab metadata from `SessionManager::duplicate_workspace` (FIX-007).
///
/// The caller (GTK socket handler) serialises this into the JSON-RPC response
/// so the client can build widgets for each duplicated tab without an extra
/// `get_layout` round-trip.
#[derive(Debug, Clone)]
pub struct DuplicatedTab {
    pub tab_id: Uuid,
    pub pane_id: PaneId,
}
