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
        stream.write_all(line.as_bytes()).map_err(|e| DaemonError(format!("write request: {e}")))?;
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

    /// Create a new pane in the daemon. Returns the assigned `PaneId`.
    pub fn new_tab(&self) -> Result<PaneId, DaemonError> {
        let result = self.rpc("new_tab", serde_json::json!({}))?;
        let tab_id_str = result
            .get("tab_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DaemonError("new_tab: missing tab_id".into()))?;
        let uuid = uuid::Uuid::parse_str(tab_id_str)
            .map_err(|e| DaemonError(format!("new_tab: invalid UUID: {e}")))?;
        Ok(PaneId(uuid))
    }

    /// Close a pane in the daemon.
    pub fn close_tab(&self, pane_id: PaneId) -> Result<(), DaemonError> {
        self.rpc("close_tab", serde_json::json!({ "pane_id": pane_id.to_string() }))?;
        Ok(())
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
        let cursor_row = cursor
            .and_then(|c| c.get("row"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let cursor_col = cursor
            .and_then(|c| c.get("col"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        Ok(ScreenSnapshot { lines, cursor_row, cursor_col })
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
            if let Err(e) =
                subscribe_output_task(socket_path, pane_id, tx).await
            {
                warn!("subscribe_output task error for pane {pane_id}: {e}");
            }
        });

        Ok(())
    }
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
