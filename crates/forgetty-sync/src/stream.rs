//! Terminal streaming protocol over iroh QUIC (`forgetty/stream/1` ALPN).
//!
//! # Protocol overview
//!
//! 1. Android connects with ALPN `forgetty/stream/1`.
//! 2. Daemon verifies device is in the authorized registry.
//! 3. Android opens a bidirectional stream and sends `ClientMsg::Subscribe { pane_id }`.
//! 4. Daemon sends `DaemonMsg::FullSnapshot` (current viewport text + cursor).
//! 5. Daemon forwards every `SessionEvent::PtyOutput` for that pane as `DaemonMsg::PtyBytes`.
//! 6. If the broadcast channel reports `RecvError::Lagged`, daemon sends a fresh snapshot
//!    (backpressure recovery — no disconnect).
//! 7. Android can request scrollback via `ClientMsg::RequestScrollback`.
//! 8. `PaneGone` is sent when the pane closes.
//!
//! # Frame format
//!
//! `[ u32 big-endian length (4 bytes) ][ MessagePack payload (N bytes) ]`
//!
//! Maximum frame: 4 MiB.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use forgetty_core::PaneId;
use forgetty_session::{PaneTreeLayout, SessionEvent, SessionManager};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{info, warn};
use uuid::Uuid;

use crate::registry::DeviceRegistry;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum allowed MessagePack frame size (4 MiB).
const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

/// Maximum scrollback lines returned per `RequestScrollback`.
const MAX_SCROLLBACK_PAGE: usize = 500;

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

/// Serializable pane metadata sent in `DaemonMsg::PaneList`.
///
/// Intentionally different from `forgetty_session::PaneInfo` — this is the
/// wire format (all strings, no `PathBuf`, serde-friendly).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WirePaneInfo {
    /// Pane UUID as a string.
    pub id: String,
    /// Human-readable title (from OSC 0/2 or CWD basename).
    pub title: String,
    /// Current working directory (lossy UTF-8).
    pub cwd: String,
    /// Current git branch, empty string if not in a git repo or not yet known.
    pub git_branch: String,
    /// Running command (basename), empty string if not yet determined.
    pub running_cmd: String,
    /// True if this pane is in the active tab of the active workspace.
    pub is_active: bool,
}

/// Messages sent from Android → daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Request to start streaming a pane. Must be the first message on a Subscribe stream.
    Subscribe { pane_id: String },
    /// List all live panes. Can be the first (and only) message on its own stream.
    ListPanes,
    /// Graceful stop — daemon closes the connection cleanly.
    Unsubscribe,
    /// Fetch a page of scrollback lines.
    RequestScrollback { pane_id: String, from_row: i32, count: u32 },
    /// Send keyboard input to a pane.
    SendInput {
        pane_id: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
}

/// Messages sent from daemon → Android.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMsg {
    /// Full viewport snapshot (text only). Sent immediately after `Subscribe`
    /// and again on backpressure recovery.
    FullSnapshot {
        pane_id: String,
        rows: u16,
        cols: u16,
        /// One string per row, exactly `cols` characters wide (space-padded).
        lines: Vec<String>,
        cursor_row: usize,
        cursor_col: usize,
    },
    /// Raw PTY output bytes. Android feeds these directly into its VT parser.
    PtyBytes {
        pane_id: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// A page of scrollback lines in response to `ClientMsg::RequestScrollback`.
    ScrollbackPage { pane_id: String, from_row: i32, lines: Vec<String> },
    /// The subscribed pane has closed. Android should disconnect.
    PaneGone { pane_id: String },
    /// Response to `ClientMsg::ListPanes`.
    PaneList { panes: Vec<WirePaneInfo> },
    /// Protocol or authorization error.
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Frame I/O helpers
// ---------------------------------------------------------------------------

/// Write a length-prefixed MessagePack frame to a `SendStream`.
///
/// Returns `true` on success, `false` if the stream is closed or an error
/// occurs (caller should exit the streaming loop).
async fn write_msg(send: &mut SendStream, msg: &DaemonMsg) -> bool {
    let payload = match rmp_serde::to_vec_named(msg) {
        Ok(p) => p,
        Err(e) => {
            warn!("stream: failed to serialize DaemonMsg: {e}");
            return false;
        }
    };
    let len = payload.len() as u32;
    let len_bytes = len.to_be_bytes();
    if send.write_all(&len_bytes).await.is_err() {
        return false;
    }
    if send.write_all(&payload).await.is_err() {
        return false;
    }
    true
}

/// Read a length-prefixed MessagePack frame from a `RecvStream`.
///
/// Returns `None` on stream close or any error (caller should exit the loop).
async fn read_msg(recv: &mut RecvStream) -> Option<ClientMsg> {
    // Read the 4-byte length prefix.
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len == 0 || len > MAX_FRAME_SIZE {
        warn!("stream: frame length {len} out of range, closing");
        return None;
    }

    // Read the payload.
    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload).await.ok()?;

    match rmp_serde::from_slice::<ClientMsg>(&payload) {
        Ok(msg) => Some(msg),
        Err(e) => {
            warn!("stream: failed to deserialize ClientMsg: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot builder
// ---------------------------------------------------------------------------

/// Build a `DaemonMsg::FullSnapshot` from the current VT viewport state.
///
/// Returns `DaemonMsg::Error` if the pane is not found (caller should send
/// the error and close).
fn build_snapshot(sm: &SessionManager, pane_id: PaneId) -> DaemonMsg {
    let result = sm.with_vt(pane_id, |terminal| {
        let screen = terminal.screen();
        let rows = screen.rows();
        let cols = screen.cols();

        let lines: Vec<String> = (0..rows)
            .map(|r| {
                let row = screen.row(r);
                let mut line = String::with_capacity(cols);
                for cell in row.iter().take(cols) {
                    line.push_str(&cell.grapheme);
                }
                // Trim trailing spaces then pad back to exactly `cols` chars.
                let trimmed_len = line.trim_end_matches(' ').len();
                line.truncate(trimmed_len);
                while line.chars().count() < cols {
                    line.push(' ');
                }
                line
            })
            .collect();

        let (cursor_row, cursor_col) = terminal.cursor();
        (rows as u16, cols as u16, lines, cursor_row, cursor_col)
    });

    match result {
        Ok((rows, cols, lines, cursor_row, cursor_col)) => DaemonMsg::FullSnapshot {
            pane_id: pane_id.to_string(),
            rows,
            cols,
            lines,
            cursor_row,
            cursor_col,
        },
        Err(e) => DaemonMsg::Error { message: format!("failed to read VT: {e}") },
    }
}

/// Build a `DaemonMsg::ScrollbackPage` for the given pane.
fn build_scrollback_page(
    sm: &SessionManager,
    pane_id: PaneId,
    from_row: i32,
    count: u32,
) -> DaemonMsg {
    let clamped_count = (count as usize).min(MAX_SCROLLBACK_PAGE);

    let result = sm.with_vt(pane_id, |terminal| {
        let sb = terminal.scrollback();
        let len = sb.len();
        if len == 0 {
            return Vec::new();
        }
        let start = if from_row < 0 {
            // Negative: offset from the newest scrollback line.
            let offset = (-from_row) as usize;
            if offset >= len {
                0
            } else {
                len - offset
            }
        } else {
            (from_row as usize).min(len.saturating_sub(1))
        };
        let end = (start + clamped_count).min(len);

        sb[start..end]
            .iter()
            .map(|row| {
                let mut line = String::new();
                for cell in row {
                    line.push_str(&cell.grapheme);
                }
                let trimmed = line.trim_end_matches(' ').len();
                line.truncate(trimmed);
                line
            })
            .collect()
    });

    match result {
        Ok(lines) => DaemonMsg::ScrollbackPage { pane_id: pane_id.to_string(), from_row, lines },
        Err(e) => DaemonMsg::Error { message: format!("failed to read scrollback: {e}") },
    }
}

/// Recursively collect all pane IDs reachable from a `PaneTreeLayout`.
fn collect_pane_ids(tree: &PaneTreeLayout, out: &mut HashSet<PaneId>) {
    match tree {
        PaneTreeLayout::Leaf { pane_id } => {
            out.insert(*pane_id);
        }
        PaneTreeLayout::Split { first, second, .. } => {
            collect_pane_ids(first, out);
            collect_pane_ids(second, out);
        }
    }
}

/// Build the `PaneList` payload from the current session state.
///
/// Populates `is_active` from the layout: panes in the active tab of the
/// active workspace are marked active. `git_branch` and `running_cmd` are
/// empty strings (future work).
fn build_pane_list(sm: &SessionManager) -> Vec<WirePaneInfo> {
    let pane_ids = sm.list_panes();
    if pane_ids.is_empty() {
        return Vec::new();
    }

    // Determine which panes are in the active tab of the active workspace.
    let layout = sm.layout();
    let mut active_pane_ids = HashSet::new();
    if !layout.workspaces.is_empty() {
        let ws = &layout.workspaces[layout.active_workspace.min(layout.workspaces.len() - 1)];
        if !ws.tabs.is_empty() {
            let tab = &ws.tabs[ws.active_tab.min(ws.tabs.len() - 1)];
            collect_pane_ids(&tab.pane_tree, &mut active_pane_ids);
        }
    }

    pane_ids
        .iter()
        .filter_map(|&pid| {
            sm.pane_info(pid).map(|info| WirePaneInfo {
                id: pid.to_string(),
                title: info.title,
                cwd: info.cwd.to_string_lossy().into_owned(),
                git_branch: String::new(),
                running_cmd: String::new(),
                is_active: active_pane_ids.contains(&pid),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Main connection handler
// ---------------------------------------------------------------------------

/// Handle a single accepted iroh connection on the `forgetty/stream/1` ALPN.
///
/// Verifies authorization then loops, accepting bi-directional streams and
/// spawning a task per stream. Each stream handles one operation:
/// - First msg `ListPanes` → send `PaneList` and close.
/// - First msg `Subscribe { pane_id }` → run the full PTY streaming loop.
///
/// Multiple streams can be open simultaneously on the same QUIC connection,
/// enabling Android to call `ListPanes` without interrupting an active
/// subscribe stream.
pub async fn handle_stream_connection(
    conn: Connection,
    sm: Arc<SessionManager>,
    registry: Arc<Mutex<DeviceRegistry>>,
) {
    let remote_id = conn.remote_id();
    let device_id = remote_id.to_string();

    // --- Authorization check ---
    let is_authorized = {
        let reg = registry.lock().unwrap();
        reg.is_authorized(&remote_id)
    };

    if !is_authorized {
        warn!("stream: rejected unauthorized device {device_id}");
        if let Ok(mut send) = conn.open_uni().await {
            let _ = write_msg(
                &mut send,
                &DaemonMsg::Error { message: "not_authorized".to_string() },
            )
            .await;
            let _ = send.finish();
        }
        conn.close(1u8.into(), b"not-authorized");
        return;
    }

    info!("stream: authorized device {device_id} connected");

    // --- Accept bi-directional streams (one per operation) ---
    loop {
        let (send, recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(e) => {
                info!("stream: device {device_id} disconnected: {e}");
                break;
            }
        };
        let sm2 = sm.clone();
        let dev = device_id.clone();
        tokio::spawn(handle_single_stream(send, recv, sm2, dev));
    }

    conn.close(0u8.into(), b"done");
}

/// Handle one bi-directional stream to completion.
///
/// Reads the first `ClientMsg` and branches:
/// - `ListPanes` → send `PaneList`, finish stream.
/// - `Subscribe { pane_id }` → run the PTY streaming loop until disconnect or pane close.
async fn handle_single_stream(
    mut send: SendStream,
    mut recv: RecvStream,
    sm: Arc<SessionManager>,
    device_id: String,
) {
    match read_msg(&mut recv).await {
        Some(ClientMsg::ListPanes) => {
            let panes = build_pane_list(&sm);
            let _ = write_msg(&mut send, &DaemonMsg::PaneList { panes }).await;
            let _ = send.finish();
        }

        Some(ClientMsg::Subscribe { pane_id: pane_id_str }) => {
            handle_subscribe_stream(send, recv, sm, device_id, pane_id_str).await;
        }

        Some(other) => {
            warn!("stream: unexpected first message {:?} from {device_id}", other);
            let _ = write_msg(
                &mut send,
                &DaemonMsg::Error {
                    message: "expected Subscribe or ListPanes as first message".to_string(),
                },
            )
            .await;
            let _ = send.finish();
        }

        None => {
            warn!("stream: stream closed before first message from {device_id}");
        }
    }
}

/// Run the PTY subscribe streaming loop for one pane.
async fn handle_subscribe_stream(
    mut send: SendStream,
    mut recv: RecvStream,
    sm: Arc<SessionManager>,
    device_id: String,
    pane_id_str: String,
) {
    // Parse and validate pane UUID.
    let pane_id: PaneId = match Uuid::parse_str(&pane_id_str) {
        Ok(u) => PaneId(u),
        Err(_) => {
            let _ = write_msg(
                &mut send,
                &DaemonMsg::Error { message: format!("invalid pane_id UUID: {pane_id_str}") },
            )
            .await;
            let _ = send.finish();
            return;
        }
    };

    if sm.pane_info(pane_id).is_none() {
        let _ = write_msg(
            &mut send,
            &DaemonMsg::Error { message: format!("pane not found: {pane_id_str}") },
        )
        .await;
        let _ = send.finish();
        return;
    }

    info!("stream: device {device_id} subscribed to pane {pane_id}");

    // --- Send initial FullSnapshot ---
    let snapshot = build_snapshot(&sm, pane_id);
    if !write_msg(&mut send, &snapshot).await {
        return;
    }

    // --- Subscribe to session events ---
    let mut session_rx: broadcast::Receiver<SessionEvent> = sm.subscribe_output();

    // --- Spawn reader task for incoming client messages ---
    let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<ClientMsg>(16);
    tokio::spawn(async move {
        loop {
            match read_msg(&mut recv).await {
                Some(msg) => {
                    if client_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    });

    // --- Streaming loop ---
    loop {
        tokio::select! {
            client_msg = client_rx.recv() => {
                match client_msg {
                    None => {
                        info!("stream: device {device_id} closed stream");
                        break;
                    }
                    Some(ClientMsg::Unsubscribe) => {
                        info!("stream: device {device_id} unsubscribed from pane {pane_id}");
                        break;
                    }
                    Some(ClientMsg::RequestScrollback { pane_id: pid_str, from_row, count }) => {
                        let req_pid = Uuid::parse_str(&pid_str)
                            .map(PaneId)
                            .unwrap_or(pane_id);
                        let page = build_scrollback_page(&sm, req_pid, from_row, count);
                        if !write_msg(&mut send, &page).await {
                            break;
                        }
                    }
                    Some(ClientMsg::SendInput { pane_id: pid_str, data }) => {
                        let target = Uuid::parse_str(&pid_str)
                            .map(PaneId)
                            .unwrap_or(pane_id);
                        if let Err(e) = sm.write_pty(target, &data) {
                            warn!("stream: write_pty failed for {pid_str}: {e}");
                        }
                    }
                    Some(ClientMsg::Subscribe { .. }) | Some(ClientMsg::ListPanes) => {
                        // Duplicate or unexpected — ignore.
                    }
                }
            }

            event = session_rx.recv() => {
                match event {
                    Ok(SessionEvent::PtyOutput { pane_id: eid, data }) if eid == pane_id => {
                        let msg = DaemonMsg::PtyBytes {
                            pane_id: eid.to_string(),
                            data: data.to_vec(),
                        };
                        if !write_msg(&mut send, &msg).await {
                            break;
                        }
                    }
                    Ok(SessionEvent::PaneClosed { pane_id: eid }) if eid == pane_id => {
                        info!("stream: pane {pane_id} closed, notifying device {device_id}");
                        let _ = write_msg(
                            &mut send,
                            &DaemonMsg::PaneGone { pane_id: eid.to_string() },
                        )
                        .await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("stream: device {device_id} lagged by {n} events, sending snapshot");
                        let snapshot = build_snapshot(&sm, pane_id);
                        if !write_msg(&mut send, &snapshot).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("stream: broadcast closed, ending stream for {device_id}");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = send.finish();
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_subscribe_roundtrip() {
        let msg = ClientMsg::Subscribe { pane_id: "test-pane".to_string() };
        let bytes = rmp_serde::to_vec_named(&msg).unwrap();
        let decoded: ClientMsg = rmp_serde::from_slice(&bytes).unwrap();
        match decoded {
            ClientMsg::Subscribe { pane_id } => assert_eq!(pane_id, "test-pane"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn serialize_pty_bytes_roundtrip() {
        let data = b"hello world\r\n".to_vec();
        let msg = DaemonMsg::PtyBytes { pane_id: "p1".to_string(), data: data.clone() };
        let bytes = rmp_serde::to_vec_named(&msg).unwrap();
        let decoded: DaemonMsg = rmp_serde::from_slice(&bytes).unwrap();
        match decoded {
            DaemonMsg::PtyBytes { pane_id, data: d } => {
                assert_eq!(pane_id, "p1");
                assert_eq!(d, data);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn serialize_full_snapshot_roundtrip() {
        let msg = DaemonMsg::FullSnapshot {
            pane_id: "p1".to_string(),
            rows: 24,
            cols: 80,
            lines: vec!["hello".to_string()],
            cursor_row: 0,
            cursor_col: 5,
        };
        let bytes = rmp_serde::to_vec_named(&msg).unwrap();
        let decoded: DaemonMsg = rmp_serde::from_slice(&bytes).unwrap();
        match decoded {
            DaemonMsg::FullSnapshot { rows, cols, cursor_col, .. } => {
                assert_eq!(rows, 24);
                assert_eq!(cols, 80);
                assert_eq!(cursor_col, 5);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn serialize_unsubscribe_roundtrip() {
        let msg = ClientMsg::Unsubscribe;
        let bytes = rmp_serde::to_vec_named(&msg).unwrap();
        let decoded: ClientMsg = rmp_serde::from_slice(&bytes).unwrap();
        assert!(matches!(decoded, ClientMsg::Unsubscribe));
    }

    #[test]
    fn serialize_list_panes_roundtrip() {
        let msg = ClientMsg::ListPanes;
        let bytes = rmp_serde::to_vec_named(&msg).unwrap();
        let decoded: ClientMsg = rmp_serde::from_slice(&bytes).unwrap();
        assert!(matches!(decoded, ClientMsg::ListPanes));
    }

    #[test]
    fn serialize_pane_list_roundtrip() {
        let msg = DaemonMsg::PaneList {
            panes: vec![WirePaneInfo {
                id: "abc-123".to_string(),
                title: "vim ~/src".to_string(),
                cwd: "/home/user/src".to_string(),
                git_branch: "main".to_string(),
                running_cmd: "vim".to_string(),
                is_active: true,
            }],
        };
        let bytes = rmp_serde::to_vec_named(&msg).unwrap();
        let decoded: DaemonMsg = rmp_serde::from_slice(&bytes).unwrap();
        match decoded {
            DaemonMsg::PaneList { panes } => {
                assert_eq!(panes.len(), 1);
                assert_eq!(panes[0].id, "abc-123");
                assert_eq!(panes[0].title, "vim ~/src");
                assert!(panes[0].is_active);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn serialize_send_input_roundtrip() {
        let data = b"ls -la\n".to_vec();
        let msg = ClientMsg::SendInput { pane_id: "p1".to_string(), data: data.clone() };
        let bytes = rmp_serde::to_vec_named(&msg).unwrap();
        let decoded: ClientMsg = rmp_serde::from_slice(&bytes).unwrap();
        match decoded {
            ClientMsg::SendInput { pane_id, data: d } => {
                assert_eq!(pane_id, "p1");
                assert_eq!(d, data);
            }
            _ => panic!("wrong variant"),
        }
    }
}
