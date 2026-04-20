//! Terminal streaming protocol over iroh QUIC (`forgetty/stream/1` ALPN).
//!
//! This module is the terminal-specific consumer of `forgetty-sync`. It owns
//! the wire protocol (`ClientMsg` / `DaemonMsg`), the frame codec, the
//! byte-log replay loop, and the authorization gate. `forgetty-sync` knows
//! nothing about these concerns — it only accepts iroh connections and
//! dispatches by ALPN to handlers registered by the daemon binary (V2-011 /
//! AD-015).
//!
//! # Protocol overview
//!
//! 1. Android connects with ALPN `forgetty/stream/1`.
//! 2. Daemon verifies device is in the authorized registry.
//! 3. Android opens a bidirectional stream and sends `ClientMsg::Subscribe { pane_id }`.
//! 4. Daemon sends `DaemonMsg::FullSnapshot` as a reset sentinel (V2-008:
//!    `rows = 0, cols = 0, lines = []`; any `FullSnapshot` is a VT reset
//!    marker per the Android protocol), then streams the pane's byte-log ring
//!    (V2-007) as `DaemonMsg::PtyBytes` frames so the client VT can rebuild
//!    state from raw bytes.
//! 5. Daemon forwards every `SessionEvent::PtyOutput` for that pane as `DaemonMsg::PtyBytes`.
//! 6. If the broadcast channel reports `RecvError::Lagged`, daemon sends a fresh sentinel
//!    `FullSnapshot` plus ring replay (backpressure recovery — no disconnect).
//! 7. Android can request scrollback via `ClientMsg::RequestScrollback` (V2-008:
//!    returns an empty `ScrollbackPage` until a future scrollback-over-iroh
//!    task lands; live streaming covers the 99% use case).
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
use forgetty_sync::registry::DeviceRegistry;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// ALPN identifier for Forgetty's terminal streaming protocol.
///
/// Owned by the terminal-side consumer under AD-015: `forgetty-sync` no
/// longer declares terminal-specific ALPNs.
pub const FORGETTY_STREAM_ALPN: &[u8] = b"forgetty/stream/1";

/// Maximum allowed MessagePack frame size (4 MiB).
const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

/// Chunk size for byte-log replay `PtyBytes` frames. Well under `MAX_FRAME_SIZE`
/// so a single replay chunk always fits, with plenty of MessagePack envelope
/// headroom.
const REPLAY_CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB

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
// Snapshot / replay builders
// ---------------------------------------------------------------------------

/// Build a degenerate `DaemonMsg::FullSnapshot` reset-sentinel (V2-008).
///
/// The Android protocol treats any `FullSnapshot` as "clear the screen; live
/// bytes follow". Since AD-007 forbids VT state in the daemon, the daemon
/// emits a zero-filled sentinel instead of a text snapshot and follows it
/// with the byte-log ring contents as `PtyBytes` frames (handled by the
/// caller). This preserves the wire schema byte-for-byte.
fn build_snapshot(pane_id: PaneId) -> DaemonMsg {
    DaemonMsg::FullSnapshot {
        pane_id: pane_id.to_string(),
        rows: 0,
        cols: 0,
        lines: Vec::new(),
        cursor_row: 0,
        cursor_col: 0,
    }
}

/// Build a degenerate `DaemonMsg::ScrollbackPage` (V2-008).
///
/// The daemon no longer parses VT cells, so it has no per-line scrollback to
/// return. An empty page is valid per the Android protocol doc ("Max 500
/// lines per request" plus the standard short-page termination convention).
/// Live streaming via byte-log replay (`handle_subscribe_stream`) covers the
/// 99% use case; explicit scrollback-over-iroh is deferred to a future task.
fn build_scrollback_page(pane_id: PaneId, from_row: i32) -> DaemonMsg {
    DaemonMsg::ScrollbackPage { pane_id: pane_id.to_string(), from_row, lines: Vec::new() }
}

/// Send a reset-sentinel `FullSnapshot` followed by the byte-log ring as
/// `PtyBytes` frames. Used on initial subscribe and on broadcast lag recovery.
///
/// Returns `false` if any write fails (caller should exit the streaming loop).
async fn send_sentinel_and_replay(
    send: &mut SendStream,
    pane_id: PaneId,
    replay_bytes: &[u8],
) -> bool {
    if !write_msg(send, &build_snapshot(pane_id)).await {
        return false;
    }
    for chunk in replay_bytes.chunks(REPLAY_CHUNK_SIZE) {
        let msg = DaemonMsg::PtyBytes { pane_id: pane_id.to_string(), data: chunk.to_vec() };
        if !write_msg(send, &msg).await {
            return false;
        }
    }
    true
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
pub async fn handle_terminal_stream(
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
            let _ =
                write_msg(&mut send, &DaemonMsg::Error { message: "not_authorized".to_string() })
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
        tokio::spawn(handle_single_stream(send, recv, Arc::clone(&sm), device_id.clone()));
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

    // `subscribe_with_snapshot` returns the broadcast receiver and the
    // current byte-log ring contents under a single lock, so no bytes are
    // duplicated or missed across the "send snapshot" / "start forwarding
    // live events" boundary. The Android client rebuilds screen state by
    // feeding the replay bytes into its own VT parser (AD-007 / AD-008).
    let (mut session_rx, replay_bytes, _hwm) = sm.subscribe_with_snapshot(pane_id);
    if !send_sentinel_and_replay(&mut send, pane_id, &replay_bytes).await {
        return;
    }

    // --- Spawn reader task for incoming client messages ---
    let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<ClientMsg>(16);
    tokio::spawn(async move {
        while let Some(msg) = read_msg(&mut recv).await {
            if client_tx.send(msg).await.is_err() {
                break;
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
                    Some(ClientMsg::RequestScrollback { pane_id: pid_str, from_row, count: _ }) => {
                        let req_pid = Uuid::parse_str(&pid_str)
                            .map(PaneId)
                            .unwrap_or(pane_id);
                        let page = build_scrollback_page(req_pid, from_row);
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
                        warn!("stream: device {device_id} lagged by {n} events, resending sentinel + ring");
                        // Re-acquire ring snapshot atomically; discard the
                        // fresh receiver since `session_rx` is already past
                        // the lag point (`broadcast::Receiver::recv` advances
                        // past dropped slots).
                        let (_new_rx, replay_bytes, _hwm) =
                            sm.subscribe_with_snapshot(pane_id);
                        if !send_sentinel_and_replay(&mut send, pane_id, &replay_bytes).await {
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
