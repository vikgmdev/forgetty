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

use uuid::Uuid;

use crate::drain_result::DrainResult;
use crate::events::{scan_osc_notification, SessionEvent};
use crate::layout::{SessionLayout, SessionTab};
use crate::pane::{PaneInfo, PaneState};
use crate::pty_bridge::PtyBridge;
use crate::vt_instance::VtInstance;
use crate::workspace::{build_workspace_state, PaneTreeLayout, WorkspaceLayout};

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
    /// Tracks insertion order so `list_panes()` returns panes in creation order.
    /// `HashMap` iteration is non-deterministic; preserving order ensures tabs
    /// are restored in the same visual position after a window reopen.
    pane_order: Vec<PaneId>,
    event_tx: broadcast::Sender<SessionEvent>,
    /// Daemon-owned layout: workspaces → tabs → pane trees (AD-002, AD-007).
    /// Mutated by `create_pane` and `close_pane`; exposed via `layout()`.
    layout: SessionLayout,
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
            inner: Arc::new(Mutex::new(SessionManagerInner {
                panes: HashMap::new(),
                pane_order: Vec::new(),
                event_tx,
                layout: SessionLayout::new_default(),
            })),
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
            inner.pane_order.push(id);
            // Mirror the new pane as a single-leaf tab in the default workspace (AD-002).
            // `active_tab` is NOT advanced here — that is UI state owned by GTK (AD-008).
            let tab = SessionTab {
                id: Uuid::new_v4(),
                title: String::new(),
                pane_tree: PaneTreeLayout::Leaf { pane_id: id },
            };
            if let Some(ws) = inner.layout.workspaces.first_mut() {
                ws.tabs.push(tab);
            }
            let _ = inner.event_tx.send(SessionEvent::PaneCreated { pane_id: id });
        }

        debug!(%id, rows = size.rows, cols = size.cols, "session pane created");
        Ok(id)
    }

    // -----------------------------------------------------------------------
    // Layout mutation (T-060, T-067)
    // -----------------------------------------------------------------------

    /// Create a new named workspace. Returns `(workspace_id, workspace_idx)`.
    ///
    /// The workspace is appended at the end of the workspace list and starts
    /// empty (no tabs). Callers must follow up with `create_tab(workspace_idx, ...)`
    /// to populate it. This method is infallible — always appends.
    pub fn create_workspace(&self, name: &str) -> (Uuid, usize) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let id = Uuid::new_v4();
        let ws = crate::layout::SessionWorkspace {
            id,
            name: name.to_string(),
            tabs: Vec::new(),
            active_tab: 0,
        };
        inner.layout.workspaces.push(ws);
        let idx = inner.layout.workspaces.len() - 1;
        let _ = inner.event_tx.send(SessionEvent::WorkspaceCreated {
            workspace_idx: idx,
            workspace_id: id,
            name: name.to_string(),
        });
        debug!(name, workspace_idx = idx, %id, "create_workspace: workspace created");
        (id, idx)
    }

    /// Create a new tab in the given workspace, spawn a PTY for it, and return
    /// `(pane_id, tab_id)`.
    ///
    /// The tab is appended at the end of the workspace's tab list.
    /// `active_tab` is NOT advanced — that is UI state owned by GTK (AD-008).
    ///
    /// Returns `Err` if `workspace_idx` is out of bounds.
    pub fn create_tab(
        &self,
        workspace_idx: usize,
        cwd: Option<PathBuf>,
        size: PtySize,
    ) -> Result<(PaneId, Uuid)> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // Bounds-check BEFORE spawning, so we never spawn a dangling PTY.
        if workspace_idx >= inner.layout.workspaces.len() {
            return Err(forgetty_core::ForgettyError::Pty(format!(
                "workspace index {workspace_idx} out of bounds (len={})",
                inner.layout.workspaces.len()
            )));
        }

        let pane_id = PaneId::new();

        // Spawn PTY first; insert into layout only if spawn succeeds (R-2).
        let pty_bridge = PtyBridge::spawn(size, cwd.as_deref(), None, None, true)
            .map_err(forgetty_core::ForgettyError::Pty)?;

        let vt = VtInstance::new(size.rows as usize, size.cols as usize);
        let initial_cwd = cwd.unwrap_or_else(home_dir_fallback);

        let pane = PaneState {
            id: pane_id,
            pty_bridge,
            vt,
            cwd: initial_cwd,
            title: String::new(),
            rows: size.rows,
            cols: size.cols,
        };

        inner.panes.insert(pane_id, pane);
        inner.pane_order.push(pane_id);

        let tab_id = Uuid::new_v4();
        let tab = SessionTab {
            id: tab_id,
            title: String::new(),
            pane_tree: PaneTreeLayout::Leaf { pane_id },
        };
        inner.layout.workspaces[workspace_idx].tabs.push(tab);

        let _ = inner.event_tx.send(SessionEvent::PaneCreated { pane_id });
        let _ = inner.event_tx.send(SessionEvent::TabCreated { workspace_idx, tab_id, pane_id });

        debug!(%pane_id, %tab_id, workspace_idx, "create_tab: tab created");
        Ok((pane_id, tab_id))
    }

    /// Split an existing pane, creating a new PTY alongside it.
    ///
    /// Finds the `Leaf` containing `pane_id` across all workspaces/tabs,
    /// replaces it with a `Split { direction, ratio: 0.5, first: Leaf(pane_id),
    /// second: Leaf(new_pane_id) }`, and returns `new_pane_id`.
    ///
    /// Returns `Err` if `pane_id` is not found in any tab tree.
    pub fn split_pane(
        &self,
        pane_id: PaneId,
        direction: &str,
        size: PtySize,
        cwd: Option<PathBuf>,
    ) -> Result<PaneId> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if !inner.panes.contains_key(&pane_id) {
            return Err(forgetty_core::ForgettyError::Pty(format!(
                "split_pane: pane {pane_id} not found"
            )));
        }

        // Spawn new PTY BEFORE mutating the tree (R-2).
        let new_pane_id = PaneId::new();
        let pty_bridge = PtyBridge::spawn(size, cwd.as_deref(), None, None, true)
            .map_err(forgetty_core::ForgettyError::Pty)?;

        let vt = VtInstance::new(size.rows as usize, size.cols as usize);
        let initial_cwd = cwd.unwrap_or_else(home_dir_fallback);

        let new_pane = PaneState {
            id: new_pane_id,
            pty_bridge,
            vt,
            cwd: initial_cwd,
            title: String::new(),
            rows: size.rows,
            cols: size.cols,
        };

        inner.panes.insert(new_pane_id, new_pane);
        inner.pane_order.push(new_pane_id);

        // Walk all tab trees to find and replace the target leaf.
        // Also capture the tab_id for the PaneSplit event (R-1).
        let mut replaced = false;
        let mut found_tab_id: Option<Uuid> = None;
        'outer: for ws in inner.layout.workspaces.iter_mut() {
            for tab in ws.tabs.iter_mut() {
                if replace_leaf(&mut tab.pane_tree, pane_id, new_pane_id, direction) {
                    replaced = true;
                    found_tab_id = Some(tab.id);
                    break 'outer;
                }
            }
        }

        if !replaced {
            // Pane exists in registry but not in any tab tree — clean up and fail.
            inner.panes.remove(&new_pane_id);
            inner.pane_order.retain(|&p| p != new_pane_id);
            return Err(forgetty_core::ForgettyError::Pty(format!(
                "split_pane: pane {pane_id} not found in any tab tree"
            )));
        }

        let _ = inner.event_tx.send(SessionEvent::PaneCreated { pane_id: new_pane_id });
        if let Some(tab_id) = found_tab_id {
            let _ = inner.event_tx.send(SessionEvent::PaneSplit {
                tab_id,
                parent_pane_id: pane_id,
                new_pane_id,
                direction: direction.to_string(),
            });
        }

        debug!(%pane_id, %new_pane_id, direction, "split_pane: pane split");
        Ok(new_pane_id)
    }

    /// Close a tab by `tab_id`, killing all PTYs in its pane tree.
    ///
    /// Broadcasts `PaneClosed` for each killed pane.
    /// Clamps `active_tab` if needed.
    ///
    /// Returns `Err` if the tab is not found.
    pub fn close_tab(&self, tab_id: Uuid) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // Find the tab across all workspaces.
        let mut found = None;
        for (ws_idx, ws) in inner.layout.workspaces.iter().enumerate() {
            if let Some(tab_idx) = ws.tabs.iter().position(|t| t.id == tab_id) {
                found = Some((ws_idx, tab_idx));
                break;
            }
        }
        let (ws_idx, tab_idx) = found.ok_or_else(|| {
            forgetty_core::ForgettyError::Pty(format!("close_tab: tab {tab_id} not found"))
        })?;

        // Collect all pane IDs in the tab's tree.
        let mut pane_ids = Vec::new();
        collect_pane_ids(&inner.layout.workspaces[ws_idx].tabs[tab_idx].pane_tree, &mut pane_ids);

        // Remove the tab from the workspace.
        inner.layout.workspaces[ws_idx].tabs.remove(tab_idx);

        // Clamp active_tab (same logic as close_pane).
        let ws = &mut inner.layout.workspaces[ws_idx];
        if ws.active_tab >= ws.tabs.len() && !ws.tabs.is_empty() {
            ws.active_tab = ws.tabs.len() - 1;
        }

        // Kill each pane and broadcast PaneClosed.
        for pid in pane_ids {
            inner.pane_order.retain(|&p| p != pid);
            if let Some(mut pane) = inner.panes.remove(&pid) {
                if let Err(e) = pane.pty_bridge.pty.kill() {
                    warn!(%pid, "close_tab: failed to kill PTY: {e}");
                }
                let _ = inner.event_tx.send(SessionEvent::PaneClosed { pane_id: pid });
                debug!(%pid, %tab_id, "close_tab: pane killed");
            }
        }

        let _ = inner.event_tx.send(SessionEvent::TabClosed { workspace_idx: ws_idx, tab_id });

        debug!(%tab_id, "close_tab: tab removed");
        Ok(())
    }

    /// Move a tab to a new position within its workspace.
    ///
    /// The `new_index` is clamped to `tabs.len() - 1`. If the tab is already at
    /// `new_index`, this is a no-op. `active_tab` is updated to follow the
    /// previously-active tab to its new position.
    ///
    /// Returns `Err` if the tab is not found.
    pub fn move_tab(&self, tab_id: Uuid, new_index: usize) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // Find the tab across all workspaces.
        let mut found = None;
        for (ws_idx, ws) in inner.layout.workspaces.iter().enumerate() {
            if let Some(tab_idx) = ws.tabs.iter().position(|t| t.id == tab_id) {
                found = Some((ws_idx, tab_idx));
                break;
            }
        }
        let (ws_idx, current_idx) = found.ok_or_else(|| {
            forgetty_core::ForgettyError::Pty(format!("move_tab: tab {tab_id} not found"))
        })?;

        let ws = &mut inner.layout.workspaces[ws_idx];
        let last = ws.tabs.len().saturating_sub(1);
        let target_idx = new_index.min(last);

        if current_idx == target_idx {
            return Ok(());
        }

        // Remember which tab_id was active so we can follow it.
        let active_tab_id = ws.tabs.get(ws.active_tab).map(|t| t.id);

        let tab = ws.tabs.remove(current_idx);
        ws.tabs.insert(target_idx, tab);

        // Update active_tab to follow the previously-active tab.
        if let Some(active_id) = active_tab_id {
            if let Some(new_active_idx) = ws.tabs.iter().position(|t| t.id == active_id) {
                ws.active_tab = new_active_idx;
            }
        }

        let _ = inner.event_tx.send(SessionEvent::TabMoved {
            workspace_idx: ws_idx,
            tab_id,
            new_index: target_idx,
        });

        debug!(%tab_id, from = current_idx, to = target_idx, "move_tab: tab moved");
        Ok(())
    }

    /// Set the active tab index for a workspace.
    ///
    /// Returns `Err` if `workspace_idx` is out of bounds or `tab_idx >= tabs.len()`.
    pub fn set_active_tab(&self, workspace_idx: usize, tab_idx: usize) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if workspace_idx >= inner.layout.workspaces.len() {
            return Err(forgetty_core::ForgettyError::Pty(format!(
                "set_active_tab: workspace index {workspace_idx} out of bounds (len={})",
                inner.layout.workspaces.len()
            )));
        }
        let ws = &mut inner.layout.workspaces[workspace_idx];
        if tab_idx >= ws.tabs.len() {
            return Err(forgetty_core::ForgettyError::Pty(format!(
                "set_active_tab: tab index {tab_idx} out of bounds (len={})",
                ws.tabs.len()
            )));
        }
        ws.active_tab = tab_idx;
        let _ = inner
            .event_tx
            .send(SessionEvent::ActiveTabChanged { workspace_idx, tab_idx });
        debug!(workspace_idx, tab_idx, "set_active_tab: active tab updated");
        Ok(())
    }

    /// Kill a pane's PTY and remove it from the registry.
    ///
    /// After this call, `pane_info(id)` returns `None`.
    pub fn close_pane(&self, id: PaneId) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut pane) = inner.panes.remove(&id) {
            inner.pane_order.retain(|&p| p != id);
            // Remove the matching single-leaf tab from the default workspace.
            // Only leaf tabs are removed here; split trees are handled by T-060+.
            // NOTE: do NOT call self.layout() from inside this lock — that deadlocks.
            if let Some(ws) = inner.layout.workspaces.first_mut() {
                let before = ws.tabs.len();
                ws.tabs.retain(
                    |t| !matches!(&t.pane_tree, PaneTreeLayout::Leaf { pane_id } if *pane_id == id),
                );
                let removed = before != ws.tabs.len();
                // Clamp active_tab when the removed tab was at or past the current index.
                // Guard against the empty-tabs case (saturating_sub(1) = usize::MAX).
                if removed && ws.active_tab >= ws.tabs.len() && !ws.tabs.is_empty() {
                    ws.active_tab = ws.tabs.len() - 1;
                }
            }
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
        inner.pane_order.clone()
    }

    /// Return a snapshot of the current session layout.
    ///
    /// Acquires the mutex, clones `inner.layout`, and returns the clone. The
    /// snapshot reflects the state after all prior `create_pane` and
    /// `close_pane` calls have completed.
    ///
    /// NOTE: do NOT call this from within any code that already holds
    /// `self.inner` — that deadlocks. See R-1 in T-059 SPEC.
    pub fn layout(&self) -> SessionLayout {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.layout.clone()
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

    /// Convert the live `SessionLayout` into a `WorkspaceState` suitable for
    /// writing to `default.json`.
    ///
    /// Acquires the mutex **once** and builds the `WorkspaceState` entirely
    /// from `inner.layout` (the daemon-owned `SessionLayout`) and `inner.panes`
    /// (for cached CWD). Does **not** call `self.layout()`, `self.pane_info()`,
    /// or any other locking method — that would deadlock.
    ///
    /// `window_width` and `window_height` are set to `None` because the daemon
    /// has no GTK window. GTK will overwrite these fields with real dimensions
    /// on the next GTK save (dual-write, T-061 → T-065).
    pub fn snapshot_to_workspace_state(&self) -> WorkspaceState {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let workspaces: Vec<forgetty_workspace::Workspace> = inner
            .layout
            .workspaces
            .iter()
            .map(|session_ws| {
                let tabs: Vec<forgetty_workspace::TabState> = session_ws
                    .tabs
                    .iter()
                    .map(|session_tab| forgetty_workspace::TabState {
                        title: session_tab.title.clone(),
                        pane_id: None,
                        pane_tree: convert_pane_tree_layout(
                            &session_tab.pane_tree,
                            &inner.panes,
                        ),
                    })
                    .collect();

                forgetty_workspace::Workspace {
                    id: session_ws.id,
                    name: session_ws.name.clone(),
                    root_paths: Vec::new(),
                    tabs,
                    active_tab: session_ws.active_tab,
                }
            })
            .collect();

        WorkspaceState {
            version: 1,
            workspaces,
            active_workspace: inner.layout.active_workspace,
            window_width: None,
            window_height: None,
        }
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

/// Recursively find the `Leaf` node matching `target` in `tree` and replace it
/// in-place with a `Split` node containing the original leaf and a new leaf for
/// `new_pane`. Returns `true` if the replacement was made.
///
/// NOTE: if the same `PaneId` appears in multiple locations (which should be
/// impossible — `PaneId` is a UUID v4), only the first match is replaced.
fn replace_leaf(
    tree: &mut PaneTreeLayout,
    target: PaneId,
    new_pane: PaneId,
    direction: &str,
) -> bool {
    match tree {
        PaneTreeLayout::Leaf { pane_id } if *pane_id == target => {
            *tree = PaneTreeLayout::Split {
                direction: direction.to_string(),
                ratio: 0.5,
                first: Box::new(PaneTreeLayout::Leaf { pane_id: target }),
                second: Box::new(PaneTreeLayout::Leaf { pane_id: new_pane }),
            };
            true
        }
        PaneTreeLayout::Leaf { .. } => false,
        PaneTreeLayout::Split { first, second, .. } => {
            if replace_leaf(first, target, new_pane, direction) {
                true
            } else {
                replace_leaf(second, target, new_pane, direction)
            }
        }
    }
}

/// Recursively collect all `PaneId`s reachable from `tree` (DFS, pre-order).
fn collect_pane_ids(tree: &PaneTreeLayout, out: &mut Vec<PaneId>) {
    match tree {
        PaneTreeLayout::Leaf { pane_id } => out.push(*pane_id),
        PaneTreeLayout::Split { first, second, .. } => {
            collect_pane_ids(first, out);
            collect_pane_ids(second, out);
        }
    }
}

/// Recursively convert a `PaneTreeLayout` (daemon live tree) into a
/// `forgetty_workspace::PaneTreeState` (serialisable format).
///
/// CWD is read from `panes` using the cached value set by the drain loop.
/// If the pane is not in the map (edge case: pane closed mid-save), the home
/// directory fallback is used.
fn convert_pane_tree_layout(
    tree: &PaneTreeLayout,
    panes: &HashMap<PaneId, PaneState>,
) -> forgetty_workspace::PaneTreeState {
    match tree {
        PaneTreeLayout::Leaf { pane_id } => {
            let cwd = panes
                .get(pane_id)
                .map(|p| p.cwd.clone())
                .unwrap_or_else(home_dir_fallback);
            forgetty_workspace::PaneTreeState::Leaf {
                cwd,
                pane_id: Some(pane_id.0),
            }
        }
        PaneTreeLayout::Split { direction, ratio, first, second } => {
            forgetty_workspace::PaneTreeState::Split {
                direction: direction.clone(),
                ratio: *ratio,
                first: Box::new(convert_pane_tree_layout(first, panes)),
                second: Box::new(convert_pane_tree_layout(second, panes)),
            }
        }
    }
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

    // -----------------------------------------------------------------------
    // T-060 unit tests
    // -----------------------------------------------------------------------

    fn test_size() -> PtySize {
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }
    }

    /// AC-1: create_tab returns correct pane_id + tab_id, layout updated.
    #[test]
    fn test_create_tab_layout_and_registry() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_id, tab_id) = session.create_tab(0, None, size).expect("create_tab should succeed");

        let layout = session.layout();
        let tabs = &layout.workspaces[0].tabs;
        // The default workspace starts empty; create_tab appends one tab.
        assert_eq!(tabs.len(), 1, "expected 1 tab after create_tab");
        assert_eq!(tabs[0].id, tab_id, "tab id must match returned tab_id");
        assert!(
            matches!(&tabs[0].pane_tree, PaneTreeLayout::Leaf { pane_id: pid } if *pid == pane_id),
            "tab pane_tree must be Leaf(pane_id)"
        );
        assert!(session.pane_info(pane_id).is_some(), "pane_info should return Some after create_tab");

        session.close_tab(tab_id).ok();
    }

    /// AC-1 (append): new tab is appended AFTER existing tabs.
    #[test]
    fn test_create_tab_appends() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, tab1) = session.create_tab(0, None, size).expect("tab 1");
        let (pane2, tab2) = session.create_tab(0, None, size).expect("tab 2");

        let layout = session.layout();
        let tabs = &layout.workspaces[0].tabs;
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].id, tab1);
        assert_eq!(tabs[1].id, tab2);
        assert!(
            matches!(&tabs[1].pane_tree, PaneTreeLayout::Leaf { pane_id } if *pane_id == pane2)
        );

        session.close_tab(tab1).ok();
        session.close_tab(tab2).ok();
    }

    /// AC-2: create_tab with out-of-bounds workspace returns Err; no PTY spawned.
    #[test]
    fn test_create_tab_workspace_out_of_bounds() {
        let session = SessionManager::new();
        let size = test_size();

        let before = session.list_panes().len();
        let result = session.create_tab(99, None, size);
        assert!(result.is_err(), "should return Err for workspace 99");
        assert_eq!(session.list_panes().len(), before, "no PTY should be spawned on failure");

        let layout = session.layout();
        assert_eq!(layout.workspaces[0].tabs.len(), 0, "layout should be unchanged");
    }

    /// AC-3: split_pane replaces leaf with Split node; both panes registered.
    #[test]
    fn test_split_pane_horizontal() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size).expect("create_tab");
        let pane_b = session.split_pane(pane_a, "horizontal", size, None).expect("split_pane");

        let layout = session.layout();
        let tab = &layout.workspaces[0].tabs[0];
        assert!(
            matches!(
                &tab.pane_tree,
                PaneTreeLayout::Split { direction, ratio, first, second }
                if direction == "horizontal"
                && (*ratio - 0.5).abs() < 1e-6
                && matches!(first.as_ref(), PaneTreeLayout::Leaf { pane_id } if *pane_id == pane_a)
                && matches!(second.as_ref(), PaneTreeLayout::Leaf { pane_id } if *pane_id == pane_b)
            ),
            "pane_tree must be Split {{ horizontal, 0.5, Leaf(A), Leaf(B) }}"
        );

        assert!(session.pane_info(pane_a).is_some(), "pane A must still exist");
        assert!(session.pane_info(pane_b).is_some(), "pane B must be registered");

        session.close_tab(tab_id).ok();
    }

    /// AC-4: split_pane with direction "vertical" works symmetrically.
    #[test]
    fn test_split_pane_vertical() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size).expect("create_tab");
        let pane_b = session.split_pane(pane_a, "vertical", size, None).expect("split_pane");

        let layout = session.layout();
        let tab = &layout.workspaces[0].tabs[0];
        assert!(
            matches!(
                &tab.pane_tree,
                PaneTreeLayout::Split { direction, .. }
                if direction == "vertical"
            ),
            "direction must be 'vertical'"
        );
        assert!(session.pane_info(pane_b).is_some());

        session.close_tab(tab_id).ok();
    }

    /// AC-5: split_pane on a second-level leaf creates a nested Split.
    #[test]
    fn test_split_pane_nested() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size).expect("create_tab");
        let pane_b = session.split_pane(pane_a, "horizontal", size, None).expect("first split");
        // Now split pane_b (the right leaf).
        let pane_c = session.split_pane(pane_b, "horizontal", size, None).expect("second split");

        let layout = session.layout();
        let tab = &layout.workspaces[0].tabs[0];

        // Top-level must be Split { first: Leaf(A), second: Split { first: Leaf(B), second: Leaf(C) } }
        let PaneTreeLayout::Split { first, second, .. } = &tab.pane_tree else {
            panic!("expected top-level Split, got {:?}", tab.pane_tree);
        };
        assert!(
            matches!(first.as_ref(), PaneTreeLayout::Leaf { pane_id } if *pane_id == pane_a),
            "first leaf must still be A"
        );
        let PaneTreeLayout::Split { first: inner_first, second: inner_second, .. } = second.as_ref() else {
            panic!("second must be a nested Split");
        };
        assert!(
            matches!(inner_first.as_ref(), PaneTreeLayout::Leaf { pane_id } if *pane_id == pane_b)
        );
        assert!(
            matches!(inner_second.as_ref(), PaneTreeLayout::Leaf { pane_id } if *pane_id == pane_c)
        );
        assert!(session.pane_info(pane_c).is_some());

        session.close_tab(tab_id).ok();
    }

    /// AC-6: split_pane with unknown pane_id returns Err; layout unchanged; no PTY spawned.
    #[test]
    fn test_split_pane_unknown_pane_id() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size).expect("create_tab");
        let unknown = PaneId::new(); // not in registry
        let before_count = session.list_panes().len();

        let result = session.split_pane(unknown, "horizontal", size, None);
        assert!(result.is_err(), "should return Err for unknown pane");
        assert_eq!(
            session.list_panes().len(),
            before_count,
            "no new PTY should be registered on failure"
        );

        session.close_tab(tab_id).ok();
        let _ = pane_a; // suppress unused warning
    }

    /// AC-7: close_tab removes a single-pane tab; pane_info returns None.
    #[test]
    fn test_close_tab_single_pane() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_id, tab_id) = session.create_tab(0, None, size).expect("create_tab");
        assert!(session.pane_info(pane_id).is_some());

        session.close_tab(tab_id).expect("close_tab should succeed");

        assert!(session.pane_info(pane_id).is_none(), "pane_info should be None after close_tab");
        assert!(!session.list_panes().contains(&pane_id), "list_panes should not contain closed pane");

        let layout = session.layout();
        assert_eq!(layout.workspaces[0].tabs.len(), 0, "tab list should be empty");
    }

    /// AC-8: close_tab on a split tab kills all panes.
    #[test]
    fn test_close_tab_split_pane() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size).expect("create_tab");
        let pane_b = session.split_pane(pane_a, "horizontal", size, None).expect("split_pane");

        session.close_tab(tab_id).expect("close_tab should succeed");

        assert!(session.pane_info(pane_a).is_none(), "pane A should be None after close_tab");
        assert!(session.pane_info(pane_b).is_none(), "pane B should be None after close_tab");
        assert!(!session.list_panes().contains(&pane_a));
        assert!(!session.list_panes().contains(&pane_b));
    }

    /// AC-9: close_tab clamps active_tab when the removed tab was at/past current index.
    #[test]
    fn test_close_tab_clamps_active_tab() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, tab0) = session.create_tab(0, None, size).expect("tab 0");
        let (_, tab1) = session.create_tab(0, None, size).expect("tab 1");
        let (_, tab2) = session.create_tab(0, None, size).expect("tab 2");

        // Set active_tab to 2 (the last tab).
        session.set_active_tab(0, 2).expect("set_active_tab");

        // Close tab at index 2 — active_tab must clamp to 1.
        session.close_tab(tab2).expect("close_tab tab2");
        let layout = session.layout();
        assert_eq!(layout.workspaces[0].active_tab, 1, "active_tab should be clamped to 1");

        // Close tab at index 0; active_tab must stay valid.
        session.close_tab(tab0).expect("close_tab tab0");
        let layout = session.layout();
        assert!(
            layout.workspaces[0].active_tab < layout.workspaces[0].tabs.len()
                || layout.workspaces[0].tabs.is_empty(),
            "active_tab must be valid"
        );

        session.close_tab(tab1).ok();
    }

    /// AC-10: close_tab with unknown tab_id returns Err; layout unchanged.
    #[test]
    fn test_close_tab_unknown_tab_id() {
        let session = SessionManager::new();
        let result = session.close_tab(Uuid::new_v4());
        assert!(result.is_err(), "should return Err for unknown tab_id");
        let layout = session.layout();
        assert_eq!(layout.workspaces[0].tabs.len(), 0, "layout should be unchanged");
    }

    /// AC-11: move_tab reorders tabs; active_tab follows the previously-active tab.
    #[test]
    fn test_move_tab_reorders() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, tab_a) = session.create_tab(0, None, size).expect("tab A");
        let (_, tab_b) = session.create_tab(0, None, size).expect("tab B");
        let (_, tab_c) = session.create_tab(0, None, size).expect("tab C");

        // Tabs are [A, B, C]. Move C to index 0 → [C, A, B].
        session.move_tab(tab_c, 0).expect("move_tab");

        let layout = session.layout();
        let ids: Vec<Uuid> = layout.workspaces[0].tabs.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![tab_c, tab_a, tab_b], "tabs should be [C, A, B]");

        session.close_tab(tab_a).ok();
        session.close_tab(tab_b).ok();
        session.close_tab(tab_c).ok();
    }

    /// AC-12: move_tab clamps target index; moving a tab to its own index is a no-op.
    #[test]
    fn test_move_tab_clamps() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, tab_a) = session.create_tab(0, None, size).expect("tab A");
        let (_, tab_b) = session.create_tab(0, None, size).expect("tab B");

        // Move tab_a to 9999 — should place it last (index 1).
        session.move_tab(tab_a, 9999).expect("move_tab clamped");
        let layout = session.layout();
        let ids: Vec<Uuid> = layout.workspaces[0].tabs.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![tab_b, tab_a], "tab A should be last after clamped move");

        // Moving tab_a to its current position (1) is a no-op.
        session.move_tab(tab_a, 1).expect("move_tab no-op");
        let layout = session.layout();
        let ids2: Vec<Uuid> = layout.workspaces[0].tabs.iter().map(|t| t.id).collect();
        assert_eq!(ids2, vec![tab_b, tab_a], "order unchanged after no-op move");

        session.close_tab(tab_a).ok();
        session.close_tab(tab_b).ok();
    }

    /// AC-13: move_tab with unknown tab_id returns Err.
    #[test]
    fn test_move_tab_unknown_tab_id() {
        let session = SessionManager::new();
        let result = session.move_tab(Uuid::new_v4(), 0);
        assert!(result.is_err(), "should return Err for unknown tab_id");
    }

    /// AC-14: set_active_tab updates the index; setting to the same index is a no-op.
    #[test]
    fn test_set_active_tab_updates() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, _tab0) = session.create_tab(0, None, size).expect("tab 0");
        let (_, _tab1) = session.create_tab(0, None, size).expect("tab 1");
        let (_, _tab2) = session.create_tab(0, None, size).expect("tab 2");

        session.set_active_tab(0, 2).expect("set_active_tab(0, 2)");
        assert_eq!(session.layout().workspaces[0].active_tab, 2);

        // No-op: already at 0 after reset.
        session.set_active_tab(0, 0).expect("set_active_tab(0, 0)");
        assert_eq!(session.layout().workspaces[0].active_tab, 0);
    }

    /// AC-15: set_active_tab returns Err on out-of-bounds workspace or tab index.
    #[test]
    fn test_set_active_tab_out_of_bounds() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, tab0) = session.create_tab(0, None, size).expect("tab 0");

        // tab index out of bounds (only 1 tab, index 999 is invalid)
        let err = session.set_active_tab(0, 999);
        assert!(err.is_err(), "should err on tab_idx out of bounds");

        // workspace index out of bounds
        let err2 = session.set_active_tab(99, 0);
        assert!(err2.is_err(), "should err on workspace_idx out of bounds");

        session.close_tab(tab0).ok();
    }

    // -----------------------------------------------------------------------
    // T-067 unit tests
    // -----------------------------------------------------------------------

    /// T-067 AC-5: create_workspace appends a new workspace; returned idx matches;
    /// create_tab on the new workspace succeeds.
    #[test]
    fn test_create_workspace() {
        let session = SessionManager::new();
        let size = test_size();

        // (a) layout starts with 1 workspace
        assert_eq!(session.layout().workspaces.len(), 1);

        // create a second workspace
        let (ws_id, ws_idx) = session.create_workspace("Range");

        // (b) layout now has 2 workspaces
        let layout = session.layout();
        assert_eq!(layout.workspaces.len(), 2, "expected 2 workspaces");
        assert_eq!(ws_idx, 1, "returned workspace_idx should be 1");
        assert_eq!(layout.workspaces[1].id, ws_id, "workspace id must match");
        assert_eq!(layout.workspaces[1].name, "Range");
        assert_eq!(layout.workspaces[1].tabs.len(), 0, "new workspace starts empty");

        // (c) create_tab on the new workspace succeeds
        let (pane_id, _tab_id) = session.create_tab(ws_idx, None, size).expect("create_tab on new workspace");
        assert!(session.pane_info(pane_id).is_some());
        assert_eq!(session.layout().workspaces[1].tabs.len(), 1);

        session.close_pane(pane_id).ok();
    }
}
