//! Daemon client for communicating with `forgetty-daemon` via Unix socket.
//!
//! `DaemonClient` wraps all JSON-RPC 2.0 socket communication in a
//! GTK-main-thread-friendly API. The GTK main thread is single-threaded with
//! a GLib event loop, so all socket I/O runs on a background tokio runtime.
//!
//! Synchronous RPC methods use `runtime.block_on()` — tiny request/response
//! pairs that complete in microseconds on loopback. `subscribe_output` is
//! fully async: a background tokio task delivers bytes to the terminal poll
//! timer via a `std::sync::mpsc::channel`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;
use tokio::runtime::Runtime;
use tracing::{debug, warn};

use forgetty_core::PaneId;

/// A snapshot of a single pane's current screen state (plain text, no color).
#[derive(Debug, Clone)]
pub struct ScreenSnapshot {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

/// Metadata about a pane returned by `list_tabs`.
#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: PaneId,
    pub rows: u16,
    pub cols: u16,
    pub cwd: String,
    pub title: String,
}

/// Recursive pane tree node as returned by `get_layout`.
#[derive(Debug, Clone)]
pub enum PaneTreeNode {
    Leaf { pane_id: PaneId },
    Split { direction: String, ratio: f32, first: Box<PaneTreeNode>, second: Box<PaneTreeNode> },
}

/// One tab in the layout (from `get_layout`).
#[derive(Debug, Clone)]
pub struct TabInfo {
    pub tab_id: uuid::Uuid,
    pub title: String,
    pub pane_tree: PaneTreeNode,
}

/// One workspace in the layout (from `get_layout`).
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: uuid::Uuid,
    pub name: String,
    pub active_tab: usize,
    pub tabs: Vec<TabInfo>,
}

/// Full layout snapshot returned by `get_layout`.
#[derive(Debug, Clone)]
pub struct LayoutInfo {
    pub active_workspace: usize,
    pub workspaces: Vec<WorkspaceInfo>,
}

/// Information about a single paired device (from `list_devices` RPC).
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub device_id: String,
    pub name: String,
    pub paired_at: String,
    pub last_seen: Option<String>,
}

/// QR pairing information returned by `get_pairing_info` RPC.
#[derive(Debug, Clone)]
pub struct PairingInfo {
    pub node_id: String,
    pub machine: String,
    /// Base64-encoded PNG bytes of the QR code image.
    pub qr_png_base64: String,
}

/// Error type for daemon client operations.
#[derive(Debug)]
pub struct DaemonError(pub String);

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DaemonClient error: {}", self.0)
    }
}

impl std::error::Error for DaemonError {}

/// A client that speaks JSON-RPC 2.0 over a Unix domain socket to
/// `forgetty-daemon`. All synchronous calls are tiny request-response pairs.
/// The `subscribe_output` call spawns a background tokio task that delivers
/// bytes to the terminal poll timer via an mpsc channel.
pub struct DaemonClient {
    socket_path: std::path::PathBuf,
    /// Background tokio runtime for async socket I/O in subscribe_output.
    runtime: Arc<Runtime>,
}

impl DaemonClient {
    /// Connect to the daemon socket synchronously.
    ///
    /// This only verifies the socket exists and can be connected to.
    /// Returns `Err` if the socket is not found or connection fails.
    pub fn connect(socket_path: &Path) -> Result<Self, DaemonError> {
        // Verify we can connect (quick probe; we don't keep a persistent connection
        // for RPC calls — each RPC opens a fresh connection).
        let _probe = UnixStream::connect(socket_path)
            .map_err(|e| DaemonError(format!("cannot connect to daemon socket: {e}")))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("forgetty-daemon-client")
            .enable_all()
            .build()
            .map_err(|e| DaemonError(format!("failed to build tokio runtime: {e}")))?;

        Ok(Self { socket_path: socket_path.to_path_buf(), runtime: Arc::new(runtime) })
    }

    // -----------------------------------------------------------------------
    // Synchronous RPC helpers
    // -----------------------------------------------------------------------

    /// Send a single JSON-RPC request and parse the response.
    ///
    /// Opens a fresh Unix socket connection per call. All calls are tiny
    /// request-response pairs that complete in microseconds on loopback.
    fn rpc(&self, method: &str, params: Value) -> Result<Value, DaemonError> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1
        });

        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| DaemonError(format!("rpc connect failed: {e}")))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|e| DaemonError(format!("set_read_timeout: {e}")))?;

        let mut line = serde_json::to_string(&request)
            .map_err(|e| DaemonError(format!("serialize request: {e}")))?;
        line.push('\n');
        stream
            .write_all(line.as_bytes())
            .map_err(|e| DaemonError(format!("write request: {e}")))?;
        stream.flush().map_err(|e| DaemonError(format!("flush: {e}")))?;

        let mut reader = BufReader::new(&stream);
        let mut response_line = String::new();
        reader
            .read_line(&mut response_line)
            .map_err(|e| DaemonError(format!("read response: {e}")))?;

        let response: Value = serde_json::from_str(response_line.trim())
            .map_err(|e| DaemonError(format!("parse response: {e}")))?;

        if let Some(err) = response.get("error") {
            return Err(DaemonError(format!("RPC error: {err}")));
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// List all live pane IDs from the daemon.
    pub fn list_tabs(&self) -> Result<Vec<PaneInfo>, DaemonError> {
        let result = self.rpc("list_tabs", serde_json::json!({}))?;
        let tabs = result
            .get("tabs")
            .and_then(|v| v.as_array())
            .ok_or_else(|| DaemonError("list_tabs: missing tabs array".into()))?;

        let mut infos = Vec::new();
        for tab in tabs {
            let pane_id_str = tab
                .get("pane_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DaemonError("list_tabs: missing pane_id".into()))?;
            let uuid = uuid::Uuid::parse_str(pane_id_str)
                .map_err(|e| DaemonError(format!("list_tabs: invalid UUID {pane_id_str}: {e}")))?;
            let pane_id = PaneId(uuid);

            let rows = tab.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
            let cols = tab.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
            let cwd = tab.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let title = tab.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
            infos.push(PaneInfo { pane_id, rows, cols, cwd, title });
        }
        Ok(infos)
    }

    /// Create a new tab in the daemon with an optional starting CWD.
    /// Returns `(pane_id, tab_id)`.
    pub fn new_tab_with_cwd(
        &self,
        cwd: Option<&std::path::Path>,
    ) -> Result<(PaneId, uuid::Uuid), DaemonError> {
        let params = match cwd {
            Some(p) => serde_json::json!({ "cwd": p.to_string_lossy().as_ref() }),
            None => serde_json::json!({}),
        };
        let result = self.rpc("new_tab", params)?;
        let pane_id_str = result
            .get("pane_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("new_tab: missing pane_id".into()))?;
        let pane_uuid = uuid::Uuid::parse_str(pane_id_str)
            .map_err(|e| DaemonError(format!("new_tab: invalid pane_id UUID: {e}")))?;

        let tab_id_str = result
            .get("tab_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("new_tab: missing tab_id".into()))?;
        let tab_uuid = uuid::Uuid::parse_str(tab_id_str)
            .map_err(|e| DaemonError(format!("new_tab: invalid tab_id UUID: {e}")))?;

        Ok((PaneId(pane_uuid), tab_uuid))
    }

    /// Create a new tab in the daemon. Returns `(pane_id, tab_id)`.
    pub fn new_tab(&self) -> Result<(PaneId, uuid::Uuid), DaemonError> {
        self.new_tab_with_cwd(None)
    }

    /// Create a new tab using a shell profile. `command` is the pre-split argv
    /// (e.g. `["ssh", "user@host"]`). `cwd` is the profile's starting directory,
    /// already expanded and validated by the caller. Returns `(pane_id, tab_id)`.
    pub fn new_tab_with_profile(
        &self,
        command: Option<Vec<String>>,
        cwd: Option<&std::path::Path>,
    ) -> Result<(PaneId, uuid::Uuid), DaemonError> {
        let mut params = serde_json::json!({});
        if let Some(ref cmd) = command {
            params["command"] = serde_json::json!(cmd);
        }
        if let Some(p) = cwd {
            params["cwd"] = serde_json::json!(p.to_string_lossy().as_ref());
        }
        let result = self.rpc("new_tab", params)?;
        let pane_id_str = result
            .get("pane_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("new_tab_with_profile: missing pane_id".into()))?;
        let pane_uuid = uuid::Uuid::parse_str(pane_id_str)
            .map_err(|e| DaemonError(format!("new_tab_with_profile: invalid pane_id UUID: {e}")))?;

        let tab_id_str = result
            .get("tab_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("new_tab_with_profile: missing tab_id".into()))?;
        let tab_uuid = uuid::Uuid::parse_str(tab_id_str)
            .map_err(|e| DaemonError(format!("new_tab_with_profile: invalid tab_id UUID: {e}")))?;

        Ok((PaneId(pane_uuid), tab_uuid))
    }

    /// Create a new named workspace on the daemon and return the initial pane info.
    ///
    /// The daemon creates the workspace and immediately spawns a default tab in it.
    /// Returns `(workspace_id, workspace_idx, pane_id, tab_id)` on success.
    pub fn create_workspace(
        &self,
        name: &str,
    ) -> Result<(uuid::Uuid, usize, PaneId, uuid::Uuid), DaemonError> {
        let result = self.rpc("create_workspace", serde_json::json!({ "name": name }))?;

        let workspace_id_str = result
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("create_workspace: missing workspace_id".into()))?;
        let workspace_id = uuid::Uuid::parse_str(workspace_id_str).map_err(|e| {
            DaemonError(format!("create_workspace: invalid workspace_id UUID: {e}"))
        })?;

        let workspace_idx = result
            .get("workspace_idx")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| DaemonError("create_workspace: missing workspace_idx".into()))?
            as usize;

        let pane_id_str = result
            .get("pane_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("create_workspace: missing pane_id".into()))?;
        let pane_uuid = uuid::Uuid::parse_str(pane_id_str)
            .map_err(|e| DaemonError(format!("create_workspace: invalid pane_id UUID: {e}")))?;

        let tab_id_str = result
            .get("tab_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("create_workspace: missing tab_id".into()))?;
        let tab_id = uuid::Uuid::parse_str(tab_id_str)
            .map_err(|e| DaemonError(format!("create_workspace: invalid tab_id UUID: {e}")))?;

        Ok((workspace_id, workspace_idx, PaneId(pane_uuid), tab_id))
    }

    /// Close a tab in the daemon by its `tab_id` (UUID).
    pub fn close_tab(&self, tab_id: uuid::Uuid) -> Result<(), DaemonError> {
        self.rpc("close_tab", serde_json::json!({ "tab_id": tab_id.to_string() }))?;
        Ok(())
    }

    /// Close a tab in the daemon using a legacy `pane_id`.
    ///
    /// The daemon will look up the tab that owns this pane and close it.
    /// Use `close_tab(tab_id)` when a real tab UUID is available.
    pub fn close_tab_by_pane_id(&self, pane_id: PaneId) -> Result<(), DaemonError> {
        self.rpc("close_tab", serde_json::json!({ "pane_id": pane_id.to_string() }))?;
        Ok(())
    }

    /// Split a pane in the daemon. Returns the new `PaneId`.
    pub fn split_pane(&self, pane_id: PaneId, direction: &str) -> Result<PaneId, DaemonError> {
        let result = self.rpc(
            "split_pane",
            serde_json::json!({
                "pane_id": pane_id.to_string(),
                "direction": direction,
            }),
        )?;
        let new_pane_id_str = result
            .get("pane_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("split_pane: missing pane_id in response".into()))?;
        let uuid = uuid::Uuid::parse_str(new_pane_id_str)
            .map_err(|e| DaemonError(format!("split_pane: invalid UUID: {e}")))?;
        Ok(PaneId(uuid))
    }

    /// Focus (set as active) a tab in the daemon by its `tab_id`.
    pub fn focus_tab(&self, tab_id: uuid::Uuid) -> Result<(), DaemonError> {
        self.rpc("focus_tab", serde_json::json!({ "tab_id": tab_id.to_string() }))?;
        Ok(())
    }

    /// Move a tab to a new position in its workspace.
    pub fn move_tab(&self, tab_id: uuid::Uuid, new_index: usize) -> Result<(), DaemonError> {
        self.rpc(
            "move_tab",
            serde_json::json!({
                "tab_id": tab_id.to_string(),
                "new_index": new_index,
            }),
        )?;
        Ok(())
    }

    /// Retrieve the full layout snapshot from the daemon.
    pub fn get_layout(&self) -> Result<LayoutInfo, DaemonError> {
        let result = self.rpc("get_layout", serde_json::json!({}))?;

        let active_workspace =
            result.get("active_workspace").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let ws_array = result
            .get("workspaces")
            .and_then(|v| v.as_array())
            .ok_or_else(|| DaemonError("get_layout: missing workspaces".into()))?;

        let mut workspaces = Vec::new();
        for ws in ws_array {
            let id_str = ws
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DaemonError("get_layout: workspace missing id".into()))?;
            let id = uuid::Uuid::parse_str(id_str)
                .map_err(|e| DaemonError(format!("get_layout: invalid workspace id UUID: {e}")))?;
            let name = ws.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let active_tab = ws.get("active_tab").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let tabs_array = ws
                .get("tabs")
                .and_then(|v| v.as_array())
                .ok_or_else(|| DaemonError("get_layout: workspace missing tabs".into()))?;

            let mut tabs = Vec::new();
            for tab in tabs_array {
                let tab_id_str = tab
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| DaemonError("get_layout: tab missing id".into()))?;
                let tab_id = uuid::Uuid::parse_str(tab_id_str)
                    .map_err(|e| DaemonError(format!("get_layout: invalid tab id UUID: {e}")))?;
                let title = tab.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let pane_tree_val = tab
                    .get("pane_tree")
                    .ok_or_else(|| DaemonError("get_layout: tab missing pane_tree".into()))?;
                let pane_tree = parse_pane_tree(pane_tree_val)?;
                tabs.push(TabInfo { tab_id, title, pane_tree });
            }

            workspaces.push(WorkspaceInfo { id, name, active_tab, tabs });
        }

        Ok(LayoutInfo { active_workspace, workspaces })
    }

    /// Resize a pane in the daemon.
    pub fn resize_pane(&self, pane_id: PaneId, rows: u16, cols: u16) -> Result<(), DaemonError> {
        self.rpc(
            "resize_pane",
            serde_json::json!({
                "pane_id": pane_id.to_string(),
                "rows": rows,
                "cols": cols,
            }),
        )?;
        Ok(())
    }

    /// Send input bytes to a pane's PTY. Bytes are base64-encoded before sending.
    pub fn send_input(&self, pane_id: PaneId, data: &[u8]) -> Result<(), DaemonError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        self.rpc(
            "send_input",
            serde_json::json!({
                "pane_id": pane_id.to_string(),
                "data": encoded,
            }),
        )?;
        Ok(())
    }

    /// Send SIGINT to the foreground process group of a pane (Ctrl+C).
    ///
    /// The daemon writes 0x03 to the PTY and calls kill(-pgid, SIGINT).
    pub fn send_sigint(&self, pane_id: PaneId) -> Result<(), DaemonError> {
        self.rpc("send_sigint", serde_json::json!({ "pane_id": pane_id.to_string() }))?;
        Ok(())
    }

    /// Get the current viewport snapshot (plain text, no color) for initial render.
    pub fn get_screen(&self, pane_id: PaneId) -> Result<ScreenSnapshot, DaemonError> {
        let result =
            self.rpc("get_screen", serde_json::json!({ "pane_id": pane_id.to_string() }))?;

        let lines_val = result
            .get("lines")
            .and_then(|v| v.as_array())
            .ok_or_else(|| DaemonError("get_screen: missing lines".into()))?;
        let lines: Vec<String> =
            lines_val.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect();

        let cursor = result.get("cursor");
        let cursor_row =
            cursor.and_then(|c| c.get("row")).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let cursor_col =
            cursor.and_then(|c| c.get("col")).and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        Ok(ScreenSnapshot { lines, cursor_row, cursor_col })
    }

    /// Pre-seed a new pane's VT buffer with the saved snapshot from an old pane.
    ///
    /// Returns `Ok(true)` if the snapshot was found and applied, `Ok(false)`
    /// if no snapshot existed (pane opens blank).
    pub fn preseed_snapshot(
        &self,
        new_pane_id: PaneId,
        old_uuid: uuid::Uuid,
    ) -> Result<bool, DaemonError> {
        let result = self.rpc(
            "preseed_snapshot",
            serde_json::json!({
                "pane_id": new_pane_id.to_string(),
                "snapshot_id": old_uuid.to_string(),
            }),
        )?;
        Ok(result.get("seeded").and_then(|v| v.as_bool()).unwrap_or(false))
    }

    // -----------------------------------------------------------------------
    // Sync / pairing API (T-052)
    // -----------------------------------------------------------------------

    /// List all paired devices from the daemon.
    pub fn list_devices(&self) -> Result<Vec<DeviceInfo>, DaemonError> {
        let result = self.rpc("list_devices", serde_json::json!({}))?;
        let devs = result
            .get("devices")
            .and_then(|v| v.as_array())
            .ok_or_else(|| DaemonError("list_devices: missing devices array".into()))?;

        let mut infos = Vec::new();
        for d in devs {
            let device_id = d.get("device_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let name = d.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let paired_at = d.get("paired_at").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let last_seen = d.get("last_seen").and_then(|v| v.as_str()).map(|s| s.to_string());
            infos.push(DeviceInfo { device_id, name, paired_at, last_seen });
        }
        Ok(infos)
    }

    /// Revoke a paired device by its `device_id`.
    pub fn revoke_device(&self, device_id: &str) -> Result<(), DaemonError> {
        self.rpc("revoke_device", serde_json::json!({ "device_id": device_id }))?;
        Ok(())
    }

    /// Temporarily open a pairing window in the daemon for `secs` seconds.
    ///
    /// The daemon will accept the next unknown device for this duration, then
    /// automatically close the window. Default: 120 seconds.
    pub fn enable_pairing(&self, secs: u64) -> Result<(), DaemonError> {
        self.rpc("enable_pairing", serde_json::json!({ "secs": secs }))?;
        Ok(())
    }

    /// Get the current pairing info (node ID + QR PNG as base64).
    pub fn get_pairing_info(&self) -> Result<PairingInfo, DaemonError> {
        let result = self.rpc("get_pairing_info", serde_json::json!({}))?;
        let node_id = result.get("node_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let machine = result.get("machine").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let qr_png_base64 = result
            .get("qr_png_base64")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("get_pairing_info: missing qr_png_base64".into()))?
            .to_string();
        Ok(PairingInfo { node_id, machine, qr_png_base64 })
    }

    /// Close a single pane (within a split) in the daemon by its `PaneId`.
    ///
    /// If the pane is part of a split, only this pane is removed; the sibling
    /// expands. If the pane is the sole leaf in its tab, the daemon closes the
    /// entire tab. Use `close_tab_by_pane_id` when you know you want to close
    /// the entire tab.
    pub fn close_pane(&self, pane_id: PaneId) -> Result<(), DaemonError> {
        self.rpc("close_pane", serde_json::json!({ "pane_id": pane_id.to_string() }))?;
        Ok(())
    }

    /// Push updated split ratios to the daemon's layout tree.
    ///
    /// Each entry is `(pane_id, ratio)`. Called by GTK's close handler so the
    /// daemon saves the actual widget-measured ratios, not stale creation-time
    /// values.
    pub fn update_split_ratios(&self, ratios: &[(PaneId, f32)]) -> Result<(), DaemonError> {
        let entries: Vec<serde_json::Value> = ratios
            .iter()
            .map(|(pid, r)| {
                serde_json::json!({
                    "pane_id": pid.to_string(),
                    "ratio": *r,
                })
            })
            .collect();
        self.rpc("update_split_ratios", serde_json::json!({ "ratios": entries }))?;
        Ok(())
    }

    /// Set the pinned state of this session.
    pub fn set_pinned(&self, pinned: bool) -> Result<(), DaemonError> {
        self.rpc("set_pinned", serde_json::json!({ "pinned": pinned }))?;
        Ok(())
    }

    /// Get the pinned state of this session.
    pub fn get_pinned(&self) -> Result<bool, DaemonError> {
        let result = self.rpc("get_pinned", serde_json::json!({}))?;
        Ok(result.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false))
    }

    /// Request the daemon to save, move session to trash, then exit.
    ///
    /// Used for browser-model close: the session is recoverable from trash
    /// but will not auto-restore on next launch.
    pub fn shutdown_clean(&self) {
        let _ = self.rpc("shutdown_clean", serde_json::json!({}));
    }

    /// Request the daemon to save its session file and exit.
    ///
    /// Used on every normal GTK window close (X button, Ctrl+Shift+Q, SIGTERM).
    /// The daemon saves the session layout so restore-by-default can bring it
    /// back on the next bare `forgetty` launch, then exits — no orphan process.
    /// Best-effort: errors are ignored if the daemon is already gone.
    pub fn shutdown_save(&self) {
        let _ = self.rpc("shutdown_save", serde_json::json!({}));
    }

    /// Request the daemon to shut down immediately without saving session state.
    ///
    /// Used by "Close Window Permanently" (T-070). Called after the session file
    /// has already been deleted, so the daemon must not re-save it on exit.
    /// Best-effort: errors are ignored because the daemon may have already exited.
    pub fn shutdown(&self) {
        let _ = self.rpc("shutdown", serde_json::json!({}));
    }

    /// Open a `subscribe_output` stream for a pane.
    ///
    /// Spawns a background tokio task that reads output notifications from the
    /// daemon and delivers decoded bytes to the terminal poll timer via the
    /// provided mpsc sender. When the receiver is dropped (pane closed), the
    /// task exits cleanly.
    pub fn subscribe_output(
        &self,
        pane_id: PaneId,
        tx: std::sync::mpsc::Sender<Vec<u8>>,
    ) -> Result<(), DaemonError> {
        let socket_path = self.socket_path.clone();

        self.runtime.spawn(async move {
            if let Err(e) = subscribe_output_task(socket_path, pane_id, tx).await {
                warn!("subscribe_output task error for pane {pane_id}: {e}");
            }
        });

        Ok(())
    }

    /// Open a `subscribe_layout` stream.
    ///
    /// Spawns a background tokio task that reads layout notifications from the
    /// daemon and delivers `LayoutEvent` values to the caller via the provided
    /// mpsc sender. The GLib layer polls the corresponding receiver via a
    /// `glib::timeout_add_local` timer and applies idempotent widget updates.
    pub fn subscribe_layout(
        &self,
        tx: std::sync::mpsc::Sender<LayoutEvent>,
    ) -> Result<(), DaemonError> {
        let socket_path = self.socket_path.clone();

        self.runtime.spawn(async move {
            if let Err(e) = subscribe_layout_task(socket_path, tx).await {
                warn!("subscribe_layout task error: {e}");
            }
        });

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Layout event types (T-065)
// ---------------------------------------------------------------------------

/// Layout change events delivered by the `subscribe_layout` background task.
///
/// The GLib layer polls these via a `timeout_add_local` timer.  Events for
/// panes/tabs that already exist in the widget tree should be silently ignored
/// (idempotency guarantee — see spec Section 4).
#[derive(Debug, Clone)]
pub enum LayoutEvent {
    /// A new tab was created in the given workspace.
    TabCreated { workspace_idx: usize, tab_id: uuid::Uuid, pane_id: PaneId },
    /// A tab was closed (all its panes have been killed).
    TabClosed { workspace_idx: usize, tab_id: uuid::Uuid },
    /// An existing pane was split, producing a new sibling.
    PaneSplit { tab_id: uuid::Uuid, parent_pane_id: PaneId, new_pane_id: PaneId, direction: String },
    /// A tab was moved to a new position within its workspace.
    TabMoved { workspace_idx: usize, tab_id: uuid::Uuid, new_index: usize },
    /// The active tab changed for a workspace.
    ActiveTabChanged { workspace_idx: usize, tab_idx: usize },
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Parse a `PaneTreeNode` from a JSON `Value` produced by the daemon's
/// `get_layout` response.
///
/// The JSON shape mirrors `PaneTreeLayout`'s externally-tagged serde repr:
/// - `{"Leaf": {"pane_id": "..."}}` → `PaneTreeNode::Leaf`
/// - `{"Split": {"direction":"...","ratio":...,"first":{...},"second":{...}}}` → `PaneTreeNode::Split`
fn parse_pane_tree(v: &Value) -> Result<PaneTreeNode, DaemonError> {
    if let Some(leaf) = v.get("Leaf") {
        let pane_id_str = leaf
            .get("pane_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("parse_pane_tree: Leaf missing pane_id".into()))?;
        let uuid = uuid::Uuid::parse_str(pane_id_str)
            .map_err(|e| DaemonError(format!("parse_pane_tree: invalid pane_id UUID: {e}")))?;
        return Ok(PaneTreeNode::Leaf { pane_id: PaneId(uuid) });
    }
    if let Some(split) = v.get("Split") {
        let direction =
            split.get("direction").and_then(|v| v.as_str()).unwrap_or("horizontal").to_string();
        let ratio = split.get("ratio").and_then(|v| v.as_f64()).unwrap_or(0.5) as f32;
        let first_val = split
            .get("first")
            .ok_or_else(|| DaemonError("parse_pane_tree: Split missing first".into()))?;
        let second_val = split
            .get("second")
            .ok_or_else(|| DaemonError("parse_pane_tree: Split missing second".into()))?;
        let first = Box::new(parse_pane_tree(first_val)?);
        let second = Box::new(parse_pane_tree(second_val)?);
        return Ok(PaneTreeNode::Split { direction, ratio, first, second });
    }
    Err(DaemonError(format!("parse_pane_tree: unrecognized shape: {v}")))
}

/// Background tokio task that drives the `subscribe_output` streaming connection.
///
/// Opens a persistent Unix socket, sends the `subscribe_output` RPC, reads the
/// initial `{"ok":true}` acknowledgment, then reads notification lines
/// indefinitely, decoding base64 PTY bytes and forwarding them to the terminal
/// poll timer via the mpsc sender.
async fn subscribe_output_task(
    socket_path: std::path::PathBuf,
    pane_id: PaneId,
    tx: std::sync::mpsc::Sender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(&socket_path).await?;
    let (reader, mut writer) = stream.into_split();

    // Send subscribe_output request.
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "subscribe_output",
        "params": { "pane_id": pane_id.to_string() },
        "id": 1
    });
    let mut req_line = serde_json::to_string(&request)?;
    req_line.push('\n');
    writer.write_all(req_line.as_bytes()).await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();

    // Read the initial acknowledgment line {"jsonrpc":"2.0","result":{"ok":true},"id":1}.
    let Some(ack_line) = lines.next_line().await? else {
        debug!("subscribe_output: server closed connection before ack for pane {pane_id}");
        return Ok(());
    };
    let ack: Value = serde_json::from_str(ack_line.trim())?;
    if ack.get("error").is_some() {
        return Err(format!("subscribe_output rejected: {ack}").into());
    }

    debug!("subscribe_output: streaming started for pane {pane_id}");

    // Read notification lines indefinitely.
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let notification: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                warn!("subscribe_output: failed to parse notification: {e}");
                continue;
            }
        };

        // Notification format: {"jsonrpc":"2.0","method":"output","params":{"pane_id":"...","data":"<b64>"}}
        let Some(params) = notification.get("params") else { continue };
        let Some(data_b64) = params.get("data").and_then(|v| v.as_str()) else { continue };

        let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
            Ok(b) => b,
            Err(e) => {
                warn!("subscribe_output: base64 decode error: {e}");
                continue;
            }
        };

        if bytes.is_empty() {
            continue;
        }

        // Deliver to the terminal poll timer via mpsc channel.
        // If the receiver is gone (pane closed), stop the task.
        if tx.send(bytes).is_err() {
            debug!("subscribe_output: mpsc receiver dropped, stopping task for {pane_id}");
            break;
        }
    }

    debug!("subscribe_output: stream ended for pane {pane_id}");
    Ok(())
}

/// Background tokio task that drives the `subscribe_layout` streaming connection.
///
/// Opens a persistent Unix socket, sends `subscribe_layout`, reads the `{"ok":true}`
/// ack, then reads layout notification lines indefinitely, parsing each into a
/// `LayoutEvent` and forwarding to the GLib poll timer via the mpsc sender.
async fn subscribe_layout_task(
    socket_path: std::path::PathBuf,
    tx: std::sync::mpsc::Sender<LayoutEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(&socket_path).await?;
    let (reader, mut writer) = stream.into_split();

    // Send subscribe_layout request.
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "subscribe_layout",
        "params": {},
        "id": 1
    });
    let mut req_line = serde_json::to_string(&request)?;
    req_line.push('\n');
    writer.write_all(req_line.as_bytes()).await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();

    // Read the initial acknowledgment.
    let Some(ack_line) = lines.next_line().await? else {
        debug!("subscribe_layout: server closed connection before ack");
        return Ok(());
    };
    let ack: Value = serde_json::from_str(ack_line.trim())?;
    if ack.get("error").is_some() {
        return Err(format!("subscribe_layout rejected: {ack}").into());
    }

    debug!("subscribe_layout: streaming started");

    // Read layout notification lines indefinitely.
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let notification: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                warn!("subscribe_layout: failed to parse notification: {e}");
                continue;
            }
        };

        let method = notification.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let params = notification.get("params").cloned().unwrap_or(Value::Null);

        let event = match method {
            "tab_created" => {
                let workspace_idx =
                    params.get("workspace_idx").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let tab_id_str = params.get("tab_id").and_then(|v| v.as_str()).unwrap_or("");
                let pane_id_str = params.get("pane_id").and_then(|v| v.as_str()).unwrap_or("");
                let tab_id = match uuid::Uuid::parse_str(tab_id_str) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                let pane_uuid = match uuid::Uuid::parse_str(pane_id_str) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                LayoutEvent::TabCreated { workspace_idx, tab_id, pane_id: PaneId(pane_uuid) }
            }
            "tab_closed" => {
                let workspace_idx =
                    params.get("workspace_idx").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let tab_id_str = params.get("tab_id").and_then(|v| v.as_str()).unwrap_or("");
                let tab_id = match uuid::Uuid::parse_str(tab_id_str) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                LayoutEvent::TabClosed { workspace_idx, tab_id }
            }
            "pane_split" => {
                let tab_id_str = params.get("tab_id").and_then(|v| v.as_str()).unwrap_or("");
                let parent_str =
                    params.get("parent_pane_id").and_then(|v| v.as_str()).unwrap_or("");
                let new_str = params.get("new_pane_id").and_then(|v| v.as_str()).unwrap_or("");
                let direction = params
                    .get("direction")
                    .and_then(|v| v.as_str())
                    .unwrap_or("horizontal")
                    .to_string();
                let tab_id = match uuid::Uuid::parse_str(tab_id_str) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                let parent_pane_id = match uuid::Uuid::parse_str(parent_str) {
                    Ok(u) => PaneId(u),
                    Err(_) => continue,
                };
                let new_pane_id = match uuid::Uuid::parse_str(new_str) {
                    Ok(u) => PaneId(u),
                    Err(_) => continue,
                };
                LayoutEvent::PaneSplit { tab_id, parent_pane_id, new_pane_id, direction }
            }
            "tab_moved" => {
                let workspace_idx =
                    params.get("workspace_idx").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let tab_id_str = params.get("tab_id").and_then(|v| v.as_str()).unwrap_or("");
                let new_index =
                    params.get("new_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let tab_id = match uuid::Uuid::parse_str(tab_id_str) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                LayoutEvent::TabMoved { workspace_idx, tab_id, new_index }
            }
            "active_tab_changed" => {
                let workspace_idx =
                    params.get("workspace_idx").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let tab_idx = params.get("tab_idx").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                LayoutEvent::ActiveTabChanged { workspace_idx, tab_idx }
            }
            _ => {
                // Unknown layout notification — ignore silently.
                continue;
            }
        };

        if tx.send(event).is_err() {
            debug!("subscribe_layout: mpsc receiver dropped, stopping task");
            break;
        }
    }

    debug!("subscribe_layout: stream ended");
    Ok(())
}
