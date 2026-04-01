//! `SessionManager` — the platform-agnostic owner of all PTY processes and
//! VT instances.
//!
//! `SessionManager` is `Clone + Send + Sync`. Cloning it gives a second handle
//! to the same internal state (backed by `Arc<Mutex<>>`). All public methods
//! acquire the mutex, do their work, and release.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use libc;

use forgetty_core::{PaneId, Result};
use forgetty_pty::PtySize;
use forgetty_workspace::WorkspaceState;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::drain_result::DrainResult;
use crate::events::{scan_osc_notification, SessionEvent};
use crate::pane::{PaneInfo, PaneState};
use crate::pty_bridge::PtyBridge;
use crate::vt_instance::VtInstance;
use crate::workspace::{build_workspace_state, WorkspaceLayout};

/// Maximum bytes drained from the PTY channel per `drain_output()` call.
/// Matches the GTK terminal's `MAX_DRAIN_BYTES` cap (128 KiB per tick).
const MAX_DRAIN_BYTES: usize = 128 * 1024;

/// Capacity of the broadcast event channel.
const BROADCAST_CAPACITY: usize = 1024;

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct SessionManagerInner {
    panes: HashMap<PaneId, PaneState>,
    event_tx: broadcast::Sender<SessionEvent>,
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// Platform-agnostic session manager.
///
/// Owns all PTY processes and session-side VT instances. Compiles with zero
/// GTK dependencies. Clone to share ownership across threads or callbacks.
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Mutex<SessionManagerInner>>,
}

impl SessionManager {
    /// Create a new, empty session manager.
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(SessionManagerInner { panes: HashMap::new(), event_tx })),
        }
    }

    // -----------------------------------------------------------------------
    // Pane lifecycle
    // -----------------------------------------------------------------------

    /// Spawn a new pane (PTY + VT). Returns the assigned `PaneId`.
    ///
    /// - `size` — initial terminal dimensions.
    /// - `cwd` — override the working directory (`None` → use session default).
    /// - `command` — explicit argv to run (`None` → detected shell).
    /// - `shell` — config shell override (`None` → auto-detect).
    /// - `login_shell` — whether to invoke as a login shell.
    pub fn create_pane(
        &self,
        size: PtySize,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        shell: Option<String>,
        login_shell: bool,
    ) -> Result<PaneId> {
        let id = PaneId::new();

        let pty_bridge = PtyBridge::spawn(
            size,
            cwd.as_deref(),
            command.as_deref(),
            shell.as_deref(),
            login_shell,
        )
        .map_err(|e| forgetty_core::ForgettyError::Pty(e))?;

        let vt = VtInstance::new(size.rows as usize, size.cols as usize);

        let initial_cwd = cwd.unwrap_or_else(home_dir_fallback);

        let pane = PaneState {
            id,
            pty_bridge,
            vt,
            cwd: initial_cwd,
            title: String::new(),
            rows: size.rows,
            cols: size.cols,
        };

        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.panes.insert(id, pane);
            let _ = inner.event_tx.send(SessionEvent::PaneCreated { pane_id: id });
        }

        debug!(%id, rows = size.rows, cols = size.cols, "session pane created");
        Ok(id)
    }

    /// Kill a pane's PTY and remove it from the registry.
    ///
    /// After this call, `pane_info(id)` returns `None`.
    pub fn close_pane(&self, id: PaneId) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut pane) = inner.panes.remove(&id) {
            if let Err(e) = pane.pty_bridge.pty.kill() {
                warn!(%id, "failed to kill PTY on close_pane: {e}");
            }
            let _ = inner.event_tx.send(SessionEvent::PaneClosed { pane_id: id });
            debug!(%id, "session pane closed");
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // I/O
    // -----------------------------------------------------------------------

    /// Write raw bytes to the PTY master for the given pane.
    pub fn write_pty(&self, id: PaneId, data: &[u8]) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let pane = inner
            .panes
            .get_mut(&id)
            .ok_or_else(|| forgetty_core::ForgettyError::Pty(format!("pane {id} not found")))?;
        pane.pty_bridge.pty.write(data)
    }

    /// Resize a pane's PTY and VT to new dimensions.
    pub fn resize_pane(&self, id: PaneId, size: PtySize) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let pane = inner
            .panes
            .get_mut(&id)
            .ok_or_else(|| forgetty_core::ForgettyError::Pty(format!("pane {id} not found")))?;
        pane.pty_bridge.pty.resize(size)?;
        pane.vt.resize(size.rows as usize, size.cols as usize);
        pane.rows = size.rows;
        pane.cols = size.cols;
        Ok(())
    }

    /// Drain pending PTY output for a pane.
    ///
    /// - Reads up to `MAX_DRAIN_BYTES` from the mpsc channel.
    /// - Scans for OSC notifications.
    /// - Feeds bytes to the session-side VT.
    /// - Returns `DrainResult` with `raw_bytes` so the GTK layer can feed the
    ///   same data to its own `Terminal` instance (T-048 dual-VT approach).
    ///
    /// Uses `try_lock()` so the GTK main thread never blocks if a future
    /// background thread holds the lock.
    pub fn drain_output(&self, id: PaneId) -> Result<DrainResult> {
        let Ok(mut inner) = self.inner.try_lock() else {
            // Another holder has the lock — return empty result and retry next tick.
            return Ok(DrainResult {
                had_data: false,
                pty_exited: false,
                notification: None,
                raw_bytes: Vec::new(),
            });
        };

        let pane = inner
            .panes
            .get_mut(&id)
            .ok_or_else(|| forgetty_core::ForgettyError::Pty(format!("pane {id} not found")))?;

        let mut had_data = false;
        let mut disconnected = false;
        let mut bytes_drained: usize = 0;
        let mut notification = None;
        let mut raw_bytes: Vec<Vec<u8>> = Vec::new();

        loop {
            if bytes_drained >= MAX_DRAIN_BYTES {
                break;
            }

            match pane.pty_bridge.pty_rx.try_recv() {
                Ok(data) => {
                    bytes_drained += data.len();
                    had_data = true;

                    // Scan for OSC notification sequences BEFORE feeding to VT.
                    if notification.is_none() {
                        notification = scan_osc_notification(&data);
                    }

                    // Feed to session-side VT, draining write-PTY responses.
                    {
                        let pty = &mut pane.pty_bridge.pty;
                        pane.vt.feed_and_respond(&data, |resp| {
                            if let Err(e) = pty.write(resp) {
                                warn!(%id, "failed to write PTY response: {e}");
                            }
                        });
                    }

                    raw_bytes.push(data);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Check if child exited externally (orphan slave fd may delay EOF).
                    if !pane.pty_bridge.pty.is_alive() {
                        disconnected = true;
                    }
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        // Update cached CWD if we have a PID (lazy refresh on each drain).
        if had_data {
            if let Some(pid) = pane.pty_bridge.pty.pid() {
                if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                    pane.cwd = cwd;
                }
            }
        }

        // Broadcast raw output events now that we are done mutably borrowing `pane`.
        // We collect a clone of the bytes for the broadcast channel separately
        // from `raw_bytes` (which stays owned for the caller).
        for data in &raw_bytes {
            let _ = inner.event_tx.send(SessionEvent::PtyOutput {
                pane_id: id,
                data: bytes::Bytes::copy_from_slice(data),
            });
        }

        Ok(DrainResult { had_data, pty_exited: disconnected, notification, raw_bytes })
    }

    // -----------------------------------------------------------------------
    // VT access
    // -----------------------------------------------------------------------

    /// Read-only access to a pane's session-side VT.
    pub fn with_vt<F, R>(&self, id: PaneId, f: F) -> Result<R>
    where
        F: FnOnce(&forgetty_vt::Terminal) -> R,
    {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let pane = inner
            .panes
            .get(&id)
            .ok_or_else(|| forgetty_core::ForgettyError::Pty(format!("pane {id} not found")))?;
        Ok(f(&pane.vt.terminal))
    }

    /// Mutable access to a pane's session-side VT.
    pub fn with_vt_mut<F, R>(&self, id: PaneId, f: F) -> Result<R>
    where
        F: FnOnce(&mut forgetty_vt::Terminal) -> R,
    {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let pane = inner
            .panes
            .get_mut(&id)
            .ok_or_else(|| forgetty_core::ForgettyError::Pty(format!("pane {id} not found")))?;
        Ok(f(&mut pane.vt.terminal))
    }

    // -----------------------------------------------------------------------
    // Metadata
    // -----------------------------------------------------------------------

    /// Return a snapshot of pane metadata, or `None` if the pane is not found.
    pub fn pane_info(&self, id: PaneId) -> Option<PaneInfo> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.panes.get(&id).map(|pane| PaneInfo {
            id: pane.id,
            pid: pane.pty_bridge.pty.pid(),
            rows: pane.rows,
            cols: pane.cols,
            cwd: pane.cwd.clone(),
            title: pane.title.clone(),
        })
    }

    /// List all live pane IDs.
    pub fn list_panes(&self) -> Vec<PaneId> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.panes.keys().copied().collect()
    }

    // -----------------------------------------------------------------------
    // Broadcast channel
    // -----------------------------------------------------------------------

    /// Subscribe to session events.
    ///
    /// Returns a `broadcast::Receiver`. Slow consumers that fall behind by
    /// more than `BROADCAST_CAPACITY` events receive `RecvError::Lagged` and
    /// should request a fresh snapshot.
    pub fn subscribe_output(&self) -> broadcast::Receiver<SessionEvent> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.event_tx.subscribe()
    }

    // -----------------------------------------------------------------------
    // Workspace
    // -----------------------------------------------------------------------

    /// Build a `WorkspaceState` from a `WorkspaceLayout` (produced by the GTK
    /// widget-tree walker) by resolving live CWD from each pane's session record.
    pub fn snapshot_workspace(&self, layout: &WorkspaceLayout) -> WorkspaceState {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        build_workspace_state(layout, |pane_id| {
            inner.panes.get(&pane_id).map(|p| p.cwd.clone()).unwrap_or_else(home_dir_fallback)
        })
    }

    // -----------------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------------

    /// Send SIGINT to the foreground process group of a pane.
    ///
    /// This is the daemon-side implementation of the Ctrl+C signal path described
    /// in . It does two things:
    /// 1. The caller (handle_send_sigint) already wrote 0x03 via write_pty.
    /// 2. This method calls kill(-pgid, SIGINT) via tcgetpgrp on the master PTY fd.
    ///    This is necessary when the child has disabled ISIG (e.g. Node.js, pm2).
    pub fn send_sigint(&self, id: PaneId) {
        #[cfg(target_os = "linux")]
        {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pane) = inner.panes.get(&id) {
                if let Some(pgid) = pane.pty_bridge.pty.foreground_pgrp() {
                    let my_pid = std::process::id() as libc::pid_t;
                    if pgid > 0 && pgid != my_pid {
                        unsafe { libc::kill(-(pgid as libc::c_int), libc::SIGINT) };
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = id;
    }

    /// Kill every live PTY process (for clean shutdown).
    pub fn kill_all(&self) {
        let Ok(mut inner) = self.inner.try_lock() else {
            warn!("kill_all: mutex contention — will retry via idle callback");
            return;
        };
        let count = inner.panes.len();
        if count > 0 {
            debug!("kill_all: killing {count} PTY process(es)");
        }
        for (id, pane) in inner.panes.iter_mut() {
            if let Err(e) = pane.pty_bridge.pty.kill() {
                warn!(%id, "kill_all: failed to kill PTY: {e}");
            }
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn home_dir_fallback() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

// ---------------------------------------------------------------------------
// Thread-safety assertion
// ---------------------------------------------------------------------------

fn _assert_send_sync() {
    fn _is_send<T: Send>() {}
    fn _is_sync<T: Sync>() {}
    _is_send::<SessionManager>();
    _is_sync::<SessionManager>();
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// AC-3: SessionManager must be Send + Sync; clone must be movable into a thread.
    #[test]
    fn test_send_sync_clone_into_thread() {
        let session = SessionManager::new();
        let session2 = session.clone();
        let handle = std::thread::spawn(move || {
            // Call at least one method on the clone from a different thread.
            let panes = session2.list_panes();
            assert!(panes.is_empty());
        });
        handle.join().expect("thread should not panic");
    }

    /// AC-4: create_pane spawns a real PTY, write_pty + drain_output deliver output.
    #[test]
    fn test_create_pane_write_drain() {
        let session = SessionManager::new();

        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let id =
            session.create_pane(size, None, None, None, true).expect("create_pane should succeed");

        // Give the shell a moment to start.
        std::thread::sleep(Duration::from_millis(300));

        // Write a command that produces a known output.
        session.write_pty(id, b"echo hello_session_test\n").expect("write_pty should succeed");

        // Poll for the output for up to 2 seconds.
        let mut got_hello = false;
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(10));
            let result = session.drain_output(id).expect("drain_output should succeed");
            for chunk in &result.raw_bytes {
                if chunk.windows(b"hello_session_test".len()).any(|w| w == b"hello_session_test") {
                    got_hello = true;
                }
            }
            if got_hello {
                break;
            }
        }

        assert!(got_hello, "drain_output should contain 'hello_session_test'");

        // AC-5: close_pane removes the pane.
        session.close_pane(id).expect("close_pane should succeed");
        assert!(session.pane_info(id).is_none(), "pane_info should return None after close_pane");
    }

    /// AC-5: close_pane removes the pane from the registry.
    #[test]
    fn test_close_pane_removes_from_registry() {
        let session = SessionManager::new();
        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let id = session.create_pane(size, None, None, None, true).expect("create pane");
        assert!(session.pane_info(id).is_some());
        session.close_pane(id).expect("close pane");
        assert!(session.pane_info(id).is_none());
    }

    /// AC-7: subscribe_output receives PtyOutput events within 200ms.
    #[test]
    fn test_subscribe_output_receives_events() {
        let session = SessionManager::new();
        let mut rx = session.subscribe_output();

        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let id = session.create_pane(size, None, None, None, true).expect("create pane");

        // Give the shell a moment to start.
        std::thread::sleep(Duration::from_millis(300));

        // Write something to trigger PTY output.
        session.write_pty(id, b"echo subscribe_test\n").expect("write_pty");

        // Poll drain_output to drive the session VT and broadcast events.
        let mut got_event = false;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(10));
            let _ = session.drain_output(id);

            // Check if a PtyOutput event appeared.
            loop {
                match rx.try_recv() {
                    Ok(SessionEvent::PtyOutput { .. }) => {
                        got_event = true;
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            if got_event {
                break;
            }
        }

        assert!(got_event, "subscribe_output should receive PtyOutput event");
        session.close_pane(id).ok();
    }

    /// AC-11: Tests run without GTK initialization or display server.
    #[test]
    fn test_no_gtk_required() {
        // If this test compiles and runs, GTK is not required.
        let session = SessionManager::new();
        assert!(session.list_panes().is_empty());
    }
}
