//! `SessionManager` — the platform-agnostic owner of all PTY processes.
//!
//! Per AD-007 the daemon is a byte pipe: it spawns/owns PTYs, tees raw output
//! into per-pane `ByteLog` rings for replay, and broadcasts those bytes over
//! the session event channel. It does not parse VT sequences — clients own
//! all terminal semantics (AD-008).
//!
//! `SessionManager` is `Clone + Send + Sync`. Cloning it gives a second handle
//! to the same internal state (backed by `Arc<Mutex<>>`). All public methods
//! acquire the mutex, do their work, and release.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use libc;

use forgetty_core::{PaneId, Result};
use forgetty_pty::PtySize;
use forgetty_workspace::WorkspaceState;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use uuid::Uuid;

use crate::byte_log::ByteLog;
use crate::drain_result::DrainResult;
use crate::events::SessionEvent;
use crate::layout::{SessionLayout, SessionTab};
use crate::pane::{PaneInfo, PaneState};
use crate::pty_bridge::PtyBridge;
use crate::workspace::{build_workspace_state, PaneTreeLayout, WorkspaceLayout};

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
    /// Whether this session is pinned. Pinned sessions are not trashed on
    /// normal close — they stay in `sessions/` and restore on next launch.
    pinned: bool,
    /// Per-pane byte logs (V2-007 / AD-013). Populated in `spawn_drain_task`
    /// after PTY spawn; removed in `close_pane`/`close_tab` (drop closes the
    /// disk appender channel, ending its task).
    byte_logs: HashMap<PaneId, ByteLog>,
    /// Byte-log ring capacity in KiB. Set once by `daemon.rs` via
    /// `set_byte_log_config`. Default 1024 KiB = 1 MiB.
    byte_log_ring_kb: u32,
    /// Byte-log on-disk cap in MiB. Default 10 MiB.
    byte_log_max_mb: u32,
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// Platform-agnostic session manager.
///
/// Owns all PTY processes and per-pane byte logs (AD-007 / AD-013). Compiles
/// with zero GTK dependencies — clients own all VT state (AD-008). Clone to
/// share ownership across threads or callbacks.
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
                pinned: false,
                byte_logs: HashMap::new(),
                // Defaults match `forgetty_config::defaults::default_config`.
                byte_log_ring_kb: 1024,
                byte_log_max_mb: 10,
            })),
        }
    }

    /// Configure byte-log sizes for subsequently-created panes (V2-007).
    ///
    /// Called once by `daemon.rs` after constructing `SessionManager`. Existing
    /// panes' logs are not resized retroactively.
    pub fn set_byte_log_config(&self, ring_kb: u32, max_mb: u32) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.byte_log_ring_kb = ring_kb;
        inner.byte_log_max_mb = max_mb;
        debug!(ring_kb, max_mb, "set_byte_log_config");
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
        .map_err(forgetty_core::ForgettyError::Pty)?;

        let initial_cwd = cwd.unwrap_or_else(home_dir_fallback);

        let pane = PaneState {
            id,
            pty_bridge,
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

        self.spawn_drain_task(id);
        debug!(%id, rows = size.rows, cols = size.cols, "session pane created");
        Ok(id)
    }

    // -----------------------------------------------------------------------
    // Split ratio + CWD updates (B-002)
    // -----------------------------------------------------------------------

    /// Update split ratios in the daemon's layout tree.
    ///
    /// Each entry is `(pane_id, ratio)` where `pane_id` identifies the **first**
    /// child of a `Split` node. The walk finds the `Split` whose `first` subtree
    /// contains that pane as its leftmost leaf and updates the `ratio` field.
    ///
    /// This is called by GTK's close handler to push the actual widget-measured
    /// ratios before the session is saved, ensuring split proportions survive
    /// daemon restarts.
    pub fn update_split_ratios(&self, updates: &[(PaneId, f32)]) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for &(pane_id, ratio) in updates {
            let clamped = ratio.clamp(0.05, 0.95);
            for ws in inner.layout.workspaces.iter_mut() {
                for tab in ws.tabs.iter_mut() {
                    if update_ratio_for_pane(&mut tab.pane_tree, pane_id, clamped) {
                        debug!(%pane_id, ratio = clamped, "update_split_ratios: ratio updated");
                    }
                }
            }
        }
    }

    /// Override the cached CWD for a pane.
    ///
    /// Used by cold-start restore so the daemon's internal CWD matches the
    /// saved session file even before the drain loop has run.
    pub fn set_pane_cwd(&self, id: PaneId, cwd: PathBuf) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pane) = inner.panes.get_mut(&id) {
            pane.cwd = cwd;
        }
    }

    /// Mark this session as pinned. Pinned sessions survive normal close
    /// (session file stays in `sessions/` instead of moving to `trash/`).
    pub fn set_pinned(&self, pinned: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.pinned = pinned;
        debug!(pinned, "set_pinned");
    }

    /// Return whether this session is pinned.
    pub fn is_pinned(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.pinned
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

    /// Rename an existing workspace. Emits `WorkspaceRenamed` on actual change
    /// (FIX-001).
    ///
    /// Behaviour:
    /// - Returns `Err` if `workspace_idx` is out of bounds (same error style
    ///   as `create_tab`).
    /// - Returns `Ok(())` with no event if the new name equals the current
    ///   name (idempotence — AC-9).
    /// - Otherwise mutates `inner.layout.workspaces[workspace_idx].name` and
    ///   broadcasts a `SessionEvent::WorkspaceRenamed` event.
    ///
    /// The name is stored verbatim — no trimming, no validation, no length
    /// cap. The GTK dialog already trims and rejects empty strings (AC-7);
    /// the wire frame cap (4 MiB, AD-010) bounds pathological inputs.
    pub fn rename_workspace(&self, workspace_idx: usize, new_name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if workspace_idx >= inner.layout.workspaces.len() {
            return Err(forgetty_core::ForgettyError::Pty(format!(
                "workspace index {workspace_idx} out of bounds (len={})",
                inner.layout.workspaces.len()
            )));
        }

        // Idempotence: no-op (and no event) if the name is unchanged.
        if inner.layout.workspaces[workspace_idx].name == new_name {
            return Ok(());
        }

        inner.layout.workspaces[workspace_idx].name = new_name.to_string();
        let workspace_id = inner.layout.workspaces[workspace_idx].id;
        let _ = inner.event_tx.send(SessionEvent::WorkspaceRenamed {
            workspace_idx,
            workspace_id,
            name: new_name.to_string(),
        });
        debug!(
            new_name,
            workspace_idx, %workspace_id, "rename_workspace: workspace renamed"
        );
        Ok(())
    }

    /// Create a new tab in the given workspace, spawn a PTY for it, and return
    /// `(pane_id, tab_id)`.
    ///
    /// The tab is appended at the end of the workspace's tab list.
    /// `active_tab` is NOT advanced — that is UI state owned by GTK (AD-008).
    ///
    /// - `command` — explicit argv (e.g. from a profile); `None` → auto-detect shell.
    ///
    /// Returns `Err` if `workspace_idx` is out of bounds.
    pub fn create_tab(
        &self,
        workspace_idx: usize,
        cwd: Option<PathBuf>,
        size: PtySize,
        command: Option<Vec<String>>,
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
        // When a profile command is provided, pass it as the explicit argv.
        // When absent, fall back to existing auto-detect (None, None, login=true).
        let pty_bridge = PtyBridge::spawn(size, cwd.as_deref(), command.as_deref(), None, true)
            .map_err(forgetty_core::ForgettyError::Pty)?;

        let initial_cwd = cwd.unwrap_or_else(home_dir_fallback);

        let pane = PaneState {
            id: pane_id,
            pty_bridge,
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

        drop(inner);
        self.spawn_drain_task(pane_id);
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

        let initial_cwd = cwd.unwrap_or_else(home_dir_fallback);

        let new_pane = PaneState {
            id: new_pane_id,
            pty_bridge,
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

        drop(inner);
        self.spawn_drain_task(new_pane_id);
        debug!(%pane_id, %new_pane_id, direction, "split_pane: pane split");
        Ok(new_pane_id)
    }

    /// Like `split_pane` but preserves the saved split `ratio` instead of
    /// defaulting to 0.5. Used by cold-start restore so that pane proportions
    /// survive daemon restarts.
    pub fn split_pane_with_ratio(
        &self,
        pane_id: PaneId,
        direction: &str,
        ratio: f32,
        size: PtySize,
        cwd: Option<PathBuf>,
    ) -> Result<PaneId> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if !inner.panes.contains_key(&pane_id) {
            return Err(forgetty_core::ForgettyError::Pty(format!(
                "split_pane_with_ratio: pane {pane_id} not found"
            )));
        }

        let new_pane_id = PaneId::new();
        let pty_bridge = PtyBridge::spawn(size, cwd.as_deref(), None, None, true)
            .map_err(forgetty_core::ForgettyError::Pty)?;

        let initial_cwd = cwd.unwrap_or_else(home_dir_fallback);

        let new_pane = PaneState {
            id: new_pane_id,
            pty_bridge,
            cwd: initial_cwd,
            title: String::new(),
            rows: size.rows,
            cols: size.cols,
        };

        inner.panes.insert(new_pane_id, new_pane);
        inner.pane_order.push(new_pane_id);

        let mut replaced = false;
        let mut found_tab_id: Option<Uuid> = None;
        'outer: for ws in inner.layout.workspaces.iter_mut() {
            for tab in ws.tabs.iter_mut() {
                if replace_leaf_with_ratio(
                    &mut tab.pane_tree,
                    pane_id,
                    new_pane_id,
                    direction,
                    ratio,
                ) {
                    replaced = true;
                    found_tab_id = Some(tab.id);
                    break 'outer;
                }
            }
        }

        if !replaced {
            inner.panes.remove(&new_pane_id);
            inner.pane_order.retain(|&p| p != new_pane_id);
            return Err(forgetty_core::ForgettyError::Pty(format!(
                "split_pane_with_ratio: pane {pane_id} not found in any tab tree"
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

        drop(inner);
        self.spawn_drain_task(new_pane_id);
        debug!(%pane_id, %new_pane_id, direction, ratio, "split_pane_with_ratio: pane split");
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
            // V2-007: drop per-pane byte log (closes disk appender channel).
            inner.byte_logs.remove(&pid);
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
        let _ = inner.event_tx.send(SessionEvent::ActiveTabChanged { workspace_idx, tab_idx });
        debug!(workspace_idx, tab_idx, "set_active_tab: active tab updated");
        Ok(())
    }

    /// Set the globally-active workspace index.
    ///
    /// Returns `Err` if `workspace_idx` is out of bounds. Idempotent: setting
    /// the index to its current value is a no-op (no event emitted). On an
    /// actual change the method mutates `inner.layout.active_workspace` and
    /// broadcasts `SessionEvent::ActiveWorkspaceChanged` so the daemon's
    /// event watcher saves the updated layout. Used by the GTK client when
    /// the user switches workspaces (session-restore fix).
    pub fn set_active_workspace(&self, workspace_idx: usize) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if workspace_idx >= inner.layout.workspaces.len() {
            return Err(forgetty_core::ForgettyError::Pty(format!(
                "set_active_workspace: workspace index {workspace_idx} out of bounds (len={})",
                inner.layout.workspaces.len()
            )));
        }

        // Idempotence: no-op (and no event) if the index is unchanged.
        if inner.layout.active_workspace == workspace_idx {
            return Ok(());
        }

        inner.layout.active_workspace = workspace_idx;
        let _ = inner.event_tx.send(SessionEvent::ActiveWorkspaceChanged { workspace_idx });
        debug!(workspace_idx, "set_active_workspace: active workspace updated");
        Ok(())
    }

    /// Kill a pane's PTY and remove it from the registry.
    ///
    /// After this call, `pane_info(id)` returns `None`. The pane's `ByteLog`
    /// (V2-007) is also dropped here — its disk appender channel closes,
    /// ending the appender task.
    pub fn close_pane(&self, id: PaneId) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut pane) = inner.panes.remove(&id) {
            // V2-007: drop byte log (disk appender task exits on channel close).
            inner.byte_logs.remove(&id);
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

    /// Resize a pane's PTY to new dimensions.
    ///
    /// The daemon no longer owns a VT (AD-007) — clients resize their own
    /// parsers in response to the next `PtyOutput` (or ahead of it, via local
    /// UI state). The PTY resize ioctl propagates the new size to the child
    /// process, which will emit appropriate redraws on SIGWINCH.
    pub fn resize_pane(&self, id: PaneId, size: PtySize) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let pane = inner
            .panes
            .get_mut(&id)
            .ok_or_else(|| forgetty_core::ForgettyError::Pty(format!("pane {id} not found")))?;
        pane.pty_bridge.pty.resize(size)?;
        pane.rows = size.rows;
        pane.cols = size.cols;
        Ok(())
    }

    /// Take ownership of a pane's output receiver for the drain task.
    ///
    /// Returns `None` if the pane doesn't exist or the receiver was already taken.
    pub fn take_pane_output_rx(
        &self,
        id: PaneId,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.panes.get_mut(&id)?.pty_bridge.pty_rx.take()
    }

    /// Process one chunk of raw PTY bytes for a pane.
    ///
    /// - Updates the cached CWD from `/proc/{pid}/cwd`.
    /// - **Tees the raw bytes into the per-pane `ByteLog` ring BEFORE broadcast**
    ///   (V2-007 AC-13 ordering invariant — see `BUILDER_NOTES.md`).
    /// - Broadcasts a `PtyOutput` event on the session channel.
    /// - Returns `DrainResult` with `pty_exited` set if the PTY is no longer alive.
    ///
    /// Per AD-007 the daemon does not parse VT sequences — any VT responses
    /// (DSR, device-status, etc.) are produced by the client's parser and
    /// written back via `write_pty`.
    pub fn process_pane_bytes(&self, id: PaneId, bytes: &[u8]) -> Result<DrainResult> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let pane = inner
            .panes
            .get_mut(&id)
            .ok_or_else(|| forgetty_core::ForgettyError::Pty(format!("pane {id} not found")))?;

        if let Some(pid) = pane.pty_bridge.pty.pid() {
            if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                pane.cwd = cwd;
            }
        }
        let pty_exited = !pane.pty_bridge.pty.is_alive();

        // V2-007 AC-13: ring write BEFORE broadcast, both under the same
        // `inner` mutex guard. This is the ordering invariant that makes
        // `subscribe_with_snapshot` correct (V2-007 fix cycle 6): because
        // this critical section is atomic w.r.t. any other mutator of
        // `inner`, a subscriber that takes the event receiver and the ring
        // snapshot under the same lock cannot observe a partial state. Any
        // PtyOutput event the subscriber's receiver will deliver must come
        // from a `process_pane_bytes` call that happens strictly AFTER the
        // snapshot — its bytes are not in the snapshot, and no duplicate or
        // wrongly-skipped live bytes are possible.
        if let Some(log) = inner.byte_logs.get_mut(&id) {
            log.append(bytes);
        }

        let _ = inner.event_tx.send(SessionEvent::PtyOutput {
            pane_id: id,
            data: bytes::Bytes::copy_from_slice(bytes),
        });
        Ok(DrainResult { pty_exited })
    }

    // -----------------------------------------------------------------------
    // Byte-log lifecycle (V2-007 / AD-013)
    // -----------------------------------------------------------------------

    /// Create and register a `ByteLog` for a pane. Called from `spawn_drain_task`
    /// before the drain loop begins so the first byte through `process_pane_bytes`
    /// sees a populated `byte_logs` map.
    ///
    /// If the log file already exists (cold-start scenario), its tail is
    /// pre-loaded into the ring for AC-17 replay.
    ///
    /// Failures are logged — the pane still functions, but replay will be empty.
    pub fn create_byte_log_for(&self, pane_id: PaneId) {
        let (ring_kb, max_mb) = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            (inner.byte_log_ring_kb, inner.byte_log_max_mb)
        };
        let ring_bytes = (ring_kb as usize).saturating_mul(1024);
        let max_bytes = (max_mb as u64).saturating_mul(1024 * 1024);
        match ByteLog::new(pane_id.0, ring_bytes, max_bytes) {
            Ok(log) => {
                let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                inner.byte_logs.insert(pane_id, log);
                debug!(%pane_id, "create_byte_log_for: registered");
            }
            Err(e) => {
                warn!(%pane_id, "create_byte_log_for: ByteLog::new failed: {e}");
            }
        }
    }

    /// Return a snapshot of the pane's ring buffer plus the replay cursor
    /// high-water mark (total bytes written to ring since construction).
    ///
    /// Returns `None` if the pane has no byte log.
    pub fn get_ring_snapshot(&self, pane_id: PaneId) -> Option<(bytes::Bytes, u64)> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.byte_logs.get_mut(&pane_id).map(|log| log.ring_snapshot())
    }

    /// Flush every pane's on-disk log buffer.
    ///
    /// Called from disconnect / shutdown handlers. Waits for each pane's
    /// disk appender to drain its channel up to the flush marker.
    pub async fn flush_all_byte_logs(&self) {
        // Collect owning flush futures under the lock, then await OUTSIDE.
        // `ByteLog::make_flush_future` clones the mpsc Sender so the returned
        // future does not borrow from `inner`, making it safe to store after
        // the lock guard is dropped.
        let flush_futures: Vec<(PaneId, _)> = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.byte_logs.iter().map(|(id, log)| (*id, log.make_flush_future())).collect()
        };
        // Await sequentially — N is small (panes per session); simpler than
        // join_all and avoids pulling in the futures crate.
        for (id, fut) in flush_futures {
            if let Err(e) = fut.await {
                warn!(%id, "flush_all_byte_logs: flush failed: {e}");
            }
        }
    }

    /// Drop the pane's `ByteLog`, closing its disk appender channel and ending
    /// the appender task.
    pub fn close_pane_byte_log(&self, pane_id: PaneId) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.byte_logs.remove(&pane_id).is_some() {
            debug!(%pane_id, "close_pane_byte_log: log dropped");
        }
    }

    /// Spawn a per-pane tokio task that awaits on the output channel.
    ///
    /// The task calls process_pane_bytes() for each Vec<u8> produced by the
    /// PTY reader thread. On EOF (rx.recv() returns None) or when
    /// process_pane_bytes reports pty_exited, calls close_pane(pane_id).
    ///
    /// Also creates the pane's `ByteLog` (V2-007) before the drain loop begins
    /// so the first byte through `process_pane_bytes` hits a ready ring.
    pub fn spawn_drain_task(&self, pane_id: PaneId) {
        // V2-007: create the byte log first so process_pane_bytes can tee into it.
        self.create_byte_log_for(pane_id);

        if let Some(mut rx) = self.take_pane_output_rx(pane_id) {
            let sm = self.clone();
            tokio::spawn(async move {
                while let Some(bytes) = rx.recv().await {
                    match sm.process_pane_bytes(pane_id, &bytes) {
                        Ok(result) if result.pty_exited => {
                            let _ = sm.close_pane(pane_id);
                            return;
                        }
                        _ => {}
                    }
                }
                let _ = sm.close_pane(pane_id);
            });
        }
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

    /// Atomically subscribe to the session event stream AND take a snapshot of
    /// the pane's byte-log ring under a single lock acquisition.
    ///
    /// Returns `(receiver, replay_bytes, hwm)`. The receiver will *not* deliver
    /// any event that is already represented in `replay_bytes` — all future
    /// `PtyOutput` events for this pane arrive strictly after the snapshot's
    /// high-water mark. Callers can therefore stream `replay_bytes` first, then
    /// forward every received event verbatim, with no cursor/skip logic.
    ///
    /// If the pane has no byte log yet (e.g., freshly created,
    /// `create_byte_log_for` not yet called), `replay_bytes` is empty and
    /// `hwm` is 0.
    ///
    /// # Correctness (V2-007 fix cycle 6)
    ///
    /// Both `event_tx.subscribe()` and `byte_logs[pane].ring_snapshot()`
    /// execute under the same `inner` mutex guard. Because
    /// `process_pane_bytes` also acquires the same lock before appending to
    /// the ring and sending to `event_tx`, any `process_pane_bytes` call runs
    /// either entirely before this method (and its bytes are in
    /// `replay_bytes`) or entirely after (and its events are delivered to the
    /// new receiver). No overlap is possible; no cursor/skip is needed on the
    /// consumer side.
    pub fn subscribe_with_snapshot(
        &self,
        pane_id: PaneId,
    ) -> (broadcast::Receiver<SessionEvent>, bytes::Bytes, u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let rx = inner.event_tx.subscribe();
        let (replay_bytes, hwm) = inner
            .byte_logs
            .get_mut(&pane_id)
            .map(|log| log.ring_snapshot())
            .unwrap_or_else(|| (bytes::Bytes::new(), 0));
        (rx, replay_bytes, hwm)
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
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Refresh all pane CWDs from /proc before serialising so that idle
        // panes (which the drain loop hasn't polled recently) save their actual
        // working directory, not a stale home-dir value.
        refresh_pane_cwds(&mut inner.panes);

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
                        pane_tree: convert_pane_tree_layout(&session_tab.pane_tree, &inner.panes),
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
            pinned: inner.pinned,
        }
    }

    // -----------------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------------

    /// Send SIGINT to the foreground process group of a pane.
    ///
    /// This is the daemon-side implementation of the Ctrl+C signal path.
    /// It does two things:
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
    replace_leaf_with_ratio(tree, target, new_pane, direction, 0.5)
}

fn replace_leaf_with_ratio(
    tree: &mut PaneTreeLayout,
    target: PaneId,
    new_pane: PaneId,
    direction: &str,
    ratio: f32,
) -> bool {
    match tree {
        PaneTreeLayout::Leaf { pane_id } if *pane_id == target => {
            *tree = PaneTreeLayout::Split {
                direction: direction.to_string(),
                ratio,
                first: Box::new(PaneTreeLayout::Leaf { pane_id: target }),
                second: Box::new(PaneTreeLayout::Leaf { pane_id: new_pane }),
            };
            true
        }
        PaneTreeLayout::Leaf { .. } => false,
        PaneTreeLayout::Split { first, second, .. } => {
            if replace_leaf_with_ratio(first, target, new_pane, direction, ratio) {
                true
            } else {
                replace_leaf_with_ratio(second, target, new_pane, direction, ratio)
            }
        }
    }
}

/// Walk a `PaneTreeLayout` tree looking for a `Split` whose **first** subtree
/// contains `target` as its leftmost leaf. When found, update that Split's
/// `ratio` and return `true`.
fn update_ratio_for_pane(tree: &mut PaneTreeLayout, target: PaneId, new_ratio: f32) -> bool {
    match tree {
        PaneTreeLayout::Leaf { .. } => false,
        PaneTreeLayout::Split { ratio, first, second, .. } => {
            // Check if the leftmost leaf of `first` is the target.
            if leftmost_leaf_id(first) == Some(target) {
                *ratio = new_ratio;
                return true;
            }
            // Recurse into both subtrees.
            if update_ratio_for_pane(first, target, new_ratio) {
                return true;
            }
            update_ratio_for_pane(second, target, new_ratio)
        }
    }
}

/// Return the `PaneId` of the leftmost leaf in a `PaneTreeLayout`.
fn leftmost_leaf_id(tree: &PaneTreeLayout) -> Option<PaneId> {
    match tree {
        PaneTreeLayout::Leaf { pane_id } => Some(*pane_id),
        PaneTreeLayout::Split { first, .. } => leftmost_leaf_id(first),
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

/// Read `/proc/{pid}/cwd` for every live pane and update the cached CWD.
/// Called at the start of each snapshot so that idle panes (not polled by
/// the drain loop since last output) still save their current directory.
/// Failures (dead process, no /proc entry) are silently ignored and the
/// existing cached value is kept.
fn refresh_pane_cwds(panes: &mut HashMap<PaneId, PaneState>) {
    for pane in panes.values_mut() {
        if let Some(pid) = pane.pty_bridge.pty.pid() {
            if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                pane.cwd = cwd;
            }
        }
    }
}

/// Recursively convert a `PaneTreeLayout` (daemon live tree) into a
/// `forgetty_workspace::PaneTreeState` (serialisable format).
///
/// CWD is read from `panes` using the cached value (refreshed by
/// `refresh_pane_cwds` before this is called). If the pane is not in the
/// map (edge case: pane closed mid-save), the home directory fallback is used.
fn convert_pane_tree_layout(
    tree: &PaneTreeLayout,
    panes: &HashMap<PaneId, PaneState>,
) -> forgetty_workspace::PaneTreeState {
    match tree {
        PaneTreeLayout::Leaf { pane_id } => {
            let cwd = panes.get(pane_id).map(|p| p.cwd.clone()).unwrap_or_else(home_dir_fallback);
            forgetty_workspace::PaneTreeState::Leaf { cwd, pane_id: Some(pane_id.0) }
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

    /// AC-4: create_pane spawns a real PTY; drain task delivers output via broadcast channel.
    #[tokio::test]
    async fn test_create_pane_write_drain() {
        let session = SessionManager::new();
        let mut rx = session.subscribe_output();

        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let id =
            session.create_pane(size, None, None, None, true).expect("create_pane should succeed");

        // Give the shell a moment to start.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Write a command that produces a known output.
        session.write_pty(id, b"echo hello_session_test\n").expect("write_pty should succeed");

        // Wait for the drain task to broadcast PtyOutput containing our string.
        let mut got_hello = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(SessionEvent::PtyOutput { pane_id, data })) if pane_id == id => {
                    if data.windows(b"hello_session_test".len()).any(|w| w == b"hello_session_test")
                    {
                        got_hello = true;
                        break;
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => {}
            }
        }

        assert!(got_hello, "drain task should broadcast 'hello_session_test' via PtyOutput");

        // AC-5: close_pane removes the pane.
        session.close_pane(id).expect("close_pane should succeed");
        assert!(session.pane_info(id).is_none(), "pane_info should return None after close_pane");
    }

    /// AC-5: close_pane removes the pane from the registry.
    #[tokio::test]
    async fn test_close_pane_removes_from_registry() {
        let session = SessionManager::new();
        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let id = session.create_pane(size, None, None, None, true).expect("create pane");
        assert!(session.pane_info(id).is_some());
        session.close_pane(id).expect("close pane");
        assert!(session.pane_info(id).is_none());
    }

    /// AC-7: subscribe_output receives PtyOutput events within 2s.
    #[tokio::test]
    async fn test_subscribe_output_receives_events() {
        let session = SessionManager::new();
        let mut rx = session.subscribe_output();

        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let id = session.create_pane(size, None, None, None, true).expect("create pane");

        // The drain task was spawned by create_pane. Give the shell a moment to start.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Write something to trigger PTY output.
        session.write_pty(id, b"echo subscribe_test\n").expect("write_pty");

        // The drain task will call process_pane_bytes which broadcasts PtyOutput.
        let mut got_event = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(SessionEvent::PtyOutput { .. })) => {
                    got_event = true;
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => {}
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
    #[tokio::test]
    async fn test_create_tab_layout_and_registry() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_id, tab_id) =
            session.create_tab(0, None, size, None).expect("create_tab should succeed");

        let layout = session.layout();
        let tabs = &layout.workspaces[0].tabs;
        // The default workspace starts empty; create_tab appends one tab.
        assert_eq!(tabs.len(), 1, "expected 1 tab after create_tab");
        assert_eq!(tabs[0].id, tab_id, "tab id must match returned tab_id");
        assert!(
            matches!(&tabs[0].pane_tree, PaneTreeLayout::Leaf { pane_id: pid } if *pid == pane_id),
            "tab pane_tree must be Leaf(pane_id)"
        );
        assert!(
            session.pane_info(pane_id).is_some(),
            "pane_info should return Some after create_tab"
        );

        session.close_tab(tab_id).ok();
    }

    /// AC-1 (append): new tab is appended AFTER existing tabs.
    #[tokio::test]
    async fn test_create_tab_appends() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, tab1) = session.create_tab(0, None, size, None).expect("tab 1");
        let (pane2, tab2) = session.create_tab(0, None, size, None).expect("tab 2");

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
    #[tokio::test]
    async fn test_create_tab_workspace_out_of_bounds() {
        let session = SessionManager::new();
        let size = test_size();

        let before = session.list_panes().len();
        let result = session.create_tab(99, None, size, None);
        assert!(result.is_err(), "should return Err for workspace 99");
        assert_eq!(session.list_panes().len(), before, "no PTY should be spawned on failure");

        let layout = session.layout();
        assert_eq!(layout.workspaces[0].tabs.len(), 0, "layout should be unchanged");
    }

    /// AC-3: split_pane replaces leaf with Split node; both panes registered.
    #[tokio::test]
    async fn test_split_pane_horizontal() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size, None).expect("create_tab");
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
    #[tokio::test]
    async fn test_split_pane_vertical() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size, None).expect("create_tab");
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
    #[tokio::test]
    async fn test_split_pane_nested() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size, None).expect("create_tab");
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
        let PaneTreeLayout::Split { first: inner_first, second: inner_second, .. } =
            second.as_ref()
        else {
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
    #[tokio::test]
    async fn test_split_pane_unknown_pane_id() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size, None).expect("create_tab");
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

    /// split_pane_with_ratio preserves the saved ratio instead of defaulting to 0.5.
    #[tokio::test]
    async fn test_split_pane_with_ratio() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size, None).expect("create_tab");
        let pane_b = session
            .split_pane_with_ratio(pane_a, "horizontal", 0.3, size, None)
            .expect("split_pane_with_ratio");

        let layout = session.layout();
        let tab = &layout.workspaces[0].tabs[0];
        assert!(
            matches!(
                &tab.pane_tree,
                PaneTreeLayout::Split { direction, ratio, first, second }
                if direction == "horizontal"
                && (*ratio - 0.3).abs() < 1e-6
                && matches!(first.as_ref(), PaneTreeLayout::Leaf { pane_id } if *pane_id == pane_a)
                && matches!(second.as_ref(), PaneTreeLayout::Leaf { pane_id } if *pane_id == pane_b)
            ),
            "pane_tree must be Split {{ horizontal, 0.3, Leaf(A), Leaf(B) }}"
        );

        assert!(session.pane_info(pane_b).is_some());
        session.close_tab(tab_id).ok();
    }

    /// AC-7: close_tab removes a single-pane tab; pane_info returns None.
    #[tokio::test]
    async fn test_close_tab_single_pane() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_id, tab_id) = session.create_tab(0, None, size, None).expect("create_tab");
        assert!(session.pane_info(pane_id).is_some());

        session.close_tab(tab_id).expect("close_tab should succeed");

        assert!(session.pane_info(pane_id).is_none(), "pane_info should be None after close_tab");
        assert!(
            !session.list_panes().contains(&pane_id),
            "list_panes should not contain closed pane"
        );

        let layout = session.layout();
        assert_eq!(layout.workspaces[0].tabs.len(), 0, "tab list should be empty");
    }

    /// AC-8: close_tab on a split tab kills all panes.
    #[tokio::test]
    async fn test_close_tab_split_pane() {
        let session = SessionManager::new();
        let size = test_size();

        let (pane_a, tab_id) = session.create_tab(0, None, size, None).expect("create_tab");
        let pane_b = session.split_pane(pane_a, "horizontal", size, None).expect("split_pane");

        session.close_tab(tab_id).expect("close_tab should succeed");

        assert!(session.pane_info(pane_a).is_none(), "pane A should be None after close_tab");
        assert!(session.pane_info(pane_b).is_none(), "pane B should be None after close_tab");
        assert!(!session.list_panes().contains(&pane_a));
        assert!(!session.list_panes().contains(&pane_b));
    }

    /// AC-9: close_tab clamps active_tab when the removed tab was at/past current index.
    #[tokio::test]
    async fn test_close_tab_clamps_active_tab() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, tab0) = session.create_tab(0, None, size, None).expect("tab 0");
        let (_, tab1) = session.create_tab(0, None, size, None).expect("tab 1");
        let (_, tab2) = session.create_tab(0, None, size, None).expect("tab 2");

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
    #[tokio::test]
    async fn test_move_tab_reorders() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, tab_a) = session.create_tab(0, None, size, None).expect("tab A");
        let (_, tab_b) = session.create_tab(0, None, size, None).expect("tab B");
        let (_, tab_c) = session.create_tab(0, None, size, None).expect("tab C");

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
    #[tokio::test]
    async fn test_move_tab_clamps() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, tab_a) = session.create_tab(0, None, size, None).expect("tab A");
        let (_, tab_b) = session.create_tab(0, None, size, None).expect("tab B");

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
    #[tokio::test]
    async fn test_set_active_tab_updates() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, _tab0) = session.create_tab(0, None, size, None).expect("tab 0");
        let (_, _tab1) = session.create_tab(0, None, size, None).expect("tab 1");
        let (_, _tab2) = session.create_tab(0, None, size, None).expect("tab 2");

        session.set_active_tab(0, 2).expect("set_active_tab(0, 2)");
        assert_eq!(session.layout().workspaces[0].active_tab, 2);

        // No-op: already at 0 after reset.
        session.set_active_tab(0, 0).expect("set_active_tab(0, 0)");
        assert_eq!(session.layout().workspaces[0].active_tab, 0);
    }

    /// AC-15: set_active_tab returns Err on out-of-bounds workspace or tab index.
    #[tokio::test]
    async fn test_set_active_tab_out_of_bounds() {
        let session = SessionManager::new();
        let size = test_size();

        let (_, tab0) = session.create_tab(0, None, size, None).expect("tab 0");

        // tab index out of bounds (only 1 tab, index 999 is invalid)
        let err = session.set_active_tab(0, 999);
        assert!(err.is_err(), "should err on tab_idx out of bounds");

        // workspace index out of bounds
        let err2 = session.set_active_tab(99, 0);
        assert!(err2.is_err(), "should err on workspace_idx out of bounds");

        session.close_tab(tab0).ok();
    }

    // -----------------------------------------------------------------------
    // session-restore: set_active_workspace
    // -----------------------------------------------------------------------

    /// session-restore: `set_active_workspace` mutates the index and is
    /// idempotent; emits `ActiveWorkspaceChanged` exactly on real changes.
    #[tokio::test]
    async fn test_set_active_workspace_updates_and_is_idempotent() {
        let session = SessionManager::new();
        let mut rx = session.subscribe_output();
        let (_, _) = session.create_workspace("Second");
        let (_, _) = session.create_workspace("Third");

        // Drain broadcasts emitted so far (WorkspaceCreated x2).
        while rx.try_recv().is_ok() {}

        // Mutate: 0 -> 1 should emit.
        session.set_active_workspace(1).expect("set 1");
        assert_eq!(session.layout().active_workspace, 1);
        let evt = rx.recv().await.expect("first event");
        matches!(evt, SessionEvent::ActiveWorkspaceChanged { workspace_idx: 1 });

        // Idempotent: 1 -> 1 should NOT emit.
        session.set_active_workspace(1).expect("idempotent");
        assert!(rx.try_recv().is_err(), "no event on identical re-set");

        // Mutate again: 1 -> 2 should emit.
        session.set_active_workspace(2).expect("set 2");
        assert_eq!(session.layout().active_workspace, 2);
        let evt = rx.recv().await.expect("second event");
        matches!(evt, SessionEvent::ActiveWorkspaceChanged { workspace_idx: 2 });
    }

    /// session-restore: `set_active_workspace` returns Err on out-of-bounds
    /// index and does NOT mutate the layout.
    #[tokio::test]
    async fn test_set_active_workspace_out_of_bounds() {
        let session = SessionManager::new();
        assert_eq!(session.layout().workspaces.len(), 1);

        let err = session.set_active_workspace(1);
        assert!(err.is_err(), "should err on workspace_idx >= len");
        assert_eq!(session.layout().active_workspace, 0, "failed call must not mutate");

        let err2 = session.set_active_workspace(999);
        assert!(err2.is_err(), "should err on gross out-of-bounds");
    }

    // -----------------------------------------------------------------------
    // T-067 unit tests
    // -----------------------------------------------------------------------

    /// T-067 AC-5: create_workspace appends a new workspace; returned idx matches;
    /// create_tab on the new workspace succeeds.
    #[tokio::test]
    async fn test_create_workspace() {
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
        let (pane_id, _tab_id) =
            session.create_tab(ws_idx, None, size, None).expect("create_tab on new workspace");
        assert!(session.pane_info(pane_id).is_some());
        assert_eq!(session.layout().workspaces[1].tabs.len(), 1);

        session.close_pane(pane_id).ok();
    }

    // -----------------------------------------------------------------------
    // FIX-001 unit tests — rename_workspace
    // -----------------------------------------------------------------------

    /// FIX-001 / SPEC §5.4: rename_workspace updates the stored name.
    #[test]
    fn test_rename_workspace_updates_name() {
        let session = SessionManager::new();
        assert_eq!(session.layout().workspaces[0].name, "Default");

        session.rename_workspace(0, "Custom").expect("rename should succeed");
        assert_eq!(
            session.layout().workspaces[0].name,
            "Custom",
            "layout must reflect the new name"
        );
    }

    /// FIX-001 / SPEC §5.4: out-of-bounds workspace_idx returns Err without panic.
    #[test]
    fn test_rename_workspace_out_of_bounds_errors() {
        let session = SessionManager::new();
        // Default session has exactly one workspace (index 0), so index 5 is out of bounds.
        let result = session.rename_workspace(5, "Nope");
        assert!(result.is_err(), "rename with out-of-range idx must return Err");
        // And the existing workspace name must be untouched.
        assert_eq!(session.layout().workspaces[0].name, "Default");
    }

    /// FIX-001 / SPEC §5.4: rename emits a WorkspaceRenamed event with correct fields.
    #[tokio::test]
    async fn test_rename_workspace_emits_event() {
        let session = SessionManager::new();
        let mut rx = session.subscribe_output();
        let original_id = session.layout().workspaces[0].id;

        session.rename_workspace(0, "Renamed").expect("rename should succeed");

        // Drain events until we see the WorkspaceRenamed variant (a deadline
        // keeps the test bounded even if something goes wrong upstream).
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut got_event = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(SessionEvent::WorkspaceRenamed { workspace_idx, workspace_id, name })) => {
                    assert_eq!(workspace_idx, 0);
                    assert_eq!(workspace_id, original_id);
                    assert_eq!(name, "Renamed");
                    got_event = true;
                    break;
                }
                Ok(Ok(_)) => {} // ignore unrelated events
                Ok(Err(_)) | Err(_) => {}
            }
        }
        assert!(got_event, "WorkspaceRenamed event must be broadcast on rename");
    }

    /// FIX-001 / SPEC §5.4: renaming to the current name is a no-op (no event).
    #[tokio::test]
    async fn test_rename_workspace_idempotent() {
        let session = SessionManager::new();
        // Subscribe BEFORE the no-op call so we'd see an event if one were emitted.
        let mut rx = session.subscribe_output();

        // Call rename with the same name the workspace already has ("Default").
        session.rename_workspace(0, "Default").expect("rename idempotent call should succeed");

        // Drain briefly; no WorkspaceRenamed event must arrive.
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(SessionEvent::WorkspaceRenamed { .. })) => {
                    panic!("no-op rename must not emit WorkspaceRenamed");
                }
                Ok(Ok(_)) => {} // ignore unrelated events
                Ok(Err(_)) | Err(_) => break,
            }
        }

        // Name is still "Default" — no accidental mutation.
        assert_eq!(session.layout().workspaces[0].name, "Default");
    }

    // -----------------------------------------------------------------------
    // V2-007 fix cycle 6: subscribe_with_snapshot must not cause wrong-skip
    // of live bytes on idle-pane reattach.
    // -----------------------------------------------------------------------

    /// Regression guard for V2-007 fix cycle 6.
    ///
    /// Prior to cycle 6, `subscribe_output` took the receiver and the ring
    /// snapshot in two separate lock acquisitions, then used a byte cursor
    /// to skip the first `replay_len` bytes of the live broadcast — which
    /// wrongly discarded post-snapshot output whenever the actual overlap
    /// was less than `replay_len` (the common idle-pane reattach case).
    ///
    /// With `subscribe_with_snapshot`, both operations execute under the
    /// same lock as `process_pane_bytes`. Bytes written *before* the call
    /// land in `replay_bytes`; bytes written *after* the call must arrive
    /// on the receiver AND must not duplicate anything in the snapshot.
    ///
    /// This test writes a pre-snapshot payload, takes the snapshot, writes
    /// a post-snapshot payload, and asserts:
    ///   (a) the snapshot contains the pre-snapshot payload,
    ///   (b) the receiver delivers *exactly* the post-snapshot payload
    ///       (no pre-snapshot re-delivery, no wrongful skip).
    #[tokio::test]
    async fn test_subscribe_with_snapshot_no_wrong_skip() {
        let session = SessionManager::new();
        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let pane_id =
            session.create_pane(size, None, None, None, true).expect("create_pane should succeed");

        // Let the shell's own startup settle so its output is associated
        // with the pre-snapshot window (if any). We don't rely on the
        // shell's output — we only need the byte log to exist.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Pre-snapshot payload: inject bytes directly via the manager's
        // process_pane_bytes entry point. This bypasses the real PTY drain
        // task but still exercises the same ring-write-before-broadcast
        // path used in production.
        let pre = b"PRE_SNAPSHOT_PAYLOAD_XYZ";
        session.process_pane_bytes(pane_id, pre).expect("process_pane_bytes (pre)");

        // Give the broadcast a moment (even though it's synchronous, this
        // is defensive against scheduler quirks).
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Atomic subscribe + snapshot.
        let (mut rx, replay_bytes, hwm) = session.subscribe_with_snapshot(pane_id);

        // (a) Snapshot must cover the pre-snapshot payload.
        assert!(
            replay_bytes.windows(pre.len()).any(|w| w == pre),
            "snapshot must contain pre-snapshot payload; got {} bytes, hwm={hwm}",
            replay_bytes.len()
        );
        // hwm is monotonically ≥ replay_bytes.len() (equal when the ring
        // hasn't rotated yet).
        assert!(hwm >= replay_bytes.len() as u64);

        // Post-snapshot payload: these bytes are written strictly AFTER
        // the snapshot. They must NOT appear in `replay_bytes`; they MUST
        // arrive on `rx`.
        let post = b"POST_SNAPSHOT_UNIQUE_MARKER_123";
        assert!(
            !replay_bytes.windows(post.len()).any(|w| w == post),
            "post-snapshot payload must not be in snapshot (test setup sanity)"
        );
        session.process_pane_bytes(pane_id, post).expect("process_pane_bytes (post)");

        // Drain events until we see the post marker. We must NOT see the
        // pre marker on this receiver — `rx` was subscribed under the same
        // lock as the snapshot, so pre-snapshot events (from the earlier
        // process_pane_bytes call) are NOT in the broadcast queue for this
        // receiver.
        //
        // Cycle-1's cursor logic would have skipped `replay_len` bytes of
        // *any* live output here — including every byte of `post`, if
        // `replay_len > post.len()`. A test failure would manifest as the
        // deadline expiring without seeing `post`.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut saw_post = false;
        let mut saw_pre_on_rx = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(SessionEvent::PtyOutput { pane_id: evt_id, data })) if evt_id == pane_id => {
                    if data.windows(post.len()).any(|w| w == post) {
                        saw_post = true;
                        break;
                    }
                    if data.windows(pre.len()).any(|w| w == pre) {
                        saw_pre_on_rx = true;
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => {}
            }
        }

        assert!(
            !saw_pre_on_rx,
            "receiver must not re-deliver pre-snapshot payload (would imply overlap)"
        );
        assert!(
            saw_post,
            "receiver must deliver post-snapshot payload — the cycle-1 cursor would have skipped it"
        );

        session.close_pane(pane_id).ok();
    }
}
