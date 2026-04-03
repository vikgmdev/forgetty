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

use std::sync::{Arc, Mutex};

use forgetty_core::PaneId;
use forgetty_session::{SessionEvent, SessionManager};
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

/// Messages sent from Android → daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Request to start streaming a pane. Must be the first message sent.
    Subscribe { pane_id: String },
    /// Graceful stop — daemon closes the connection cleanly.
    Unsubscribe,
    /// Fetch a page of scrollback lines.
    RequestScrollback { pane_id: String, from_row: i32, count: u32 },
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

// ---------------------------------------------------------------------------
// Main connection handler
// ---------------------------------------------------------------------------

/// Handle a single accepted iroh connection on the `forgetty/stream/1` ALPN.
///
/// Verifies authorization, accepts a bi-directional stream, reads the
/// `Subscribe` message, and then runs the streaming loop until the client
/// disconnects or the pane closes.
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
        // Best-effort: send an error on a uni stream before closing.
        if let Ok(mut send) = conn.open_uni().await {
            let _ = write_msg(
                // Adapt uni stream — we need SendStream; open_uni gives SendStream directly.
                // However write_msg takes &mut SendStream. We can call it via a shim below.
                {
                    // We can't easily reuse write_msg here since open_uni gives a different
                    // stream type in some iroh versions, but in iroh 0.97 open_uni() →
                    // SendStream (same type as the send half of open_bi). So this is fine.
                    &mut send
                },
                &DaemonMsg::Error { message: "not_authorized".to_string() },
            )
            .await;
            let _ = send.finish();
        }
        conn.close(1u8.into(), b"not-authorized");
        return;
    }

    info!("stream: authorized device {device_id} connected");

    // --- Accept the bi-directional stream that Android opens ---
    let (mut send, mut recv): (SendStream, RecvStream) = match conn.accept_bi().await {
        Ok(s) => s,
        Err(e) => {
            warn!("stream: failed to accept_bi from {device_id}: {e}");
            conn.close(1u8.into(), b"stream-error");
            return;
        }
    };

    // --- Read the Subscribe message ---
    let pane_id_str = match read_msg(&mut recv).await {
        Some(ClientMsg::Subscribe { pane_id }) => pane_id,
        Some(other) => {
            warn!("stream: expected Subscribe, got {:?} from {device_id}", other);
            let _ = write_msg(
                &mut send,
                &DaemonMsg::Error { message: "expected Subscribe as first message".to_string() },
            )
            .await;
            conn.close(1u8.into(), b"protocol-error");
            return;
        }
        None => {
            warn!("stream: stream closed before Subscribe from {device_id}");
            conn.close(1u8.into(), b"stream-closed");
            return;
        }
    };

    // Parse and validate pane UUID.
    let pane_id: PaneId = match Uuid::parse_str(&pane_id_str) {
        Ok(u) => PaneId(u),
        Err(_) => {
            let _ = write_msg(
                &mut send,
                &DaemonMsg::Error { message: format!("invalid pane_id UUID: {pane_id_str}") },
            )
            .await;
            conn.close(1u8.into(), b"bad-pane-id");
            return;
        }
    };

    if sm.pane_info(pane_id).is_none() {
        let _ = write_msg(
            &mut send,
            &DaemonMsg::Error { message: format!("pane not found: {pane_id_str}") },
        )
        .await;
        conn.close(1u8.into(), b"pane-not-found");
        return;
    }

    info!("stream: device {device_id} subscribed to pane {pane_id}");

    // --- Send initial FullSnapshot ---
    let snapshot = build_snapshot(&sm, pane_id);
    if !write_msg(&mut send, &snapshot).await {
        conn.close(1u8.into(), b"snapshot-send-failed");
        return;
    }

    // --- Subscribe to session events ---
    let mut session_rx: broadcast::Receiver<SessionEvent> = sm.subscribe_output();

    // --- Spawn reader task for incoming client messages ---
    // Using a task + mpsc channel keeps the recv side cancellation-safe in
    // the select! loop below.
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
            // Incoming client message (Unsubscribe or RequestScrollback).
            client_msg = client_rx.recv() => {
                match client_msg {
                    None => {
                        // Reader task exited — client closed stream.
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
                    Some(ClientMsg::Subscribe { .. }) => {
                        // Duplicate Subscribe — ignore.
                    }
                }
            }

            // Session event from broadcast channel.
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
                        let _ = write_msg(&mut send, &DaemonMsg::PaneGone {
                            pane_id: eid.to_string(),
                        })
                        .await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Android fell behind — drop the lag and send a fresh snapshot.
                        warn!("stream: device {device_id} lagged by {n} events, sending fresh snapshot");
                        let snapshot = build_snapshot(&sm, pane_id);
                        if !write_msg(&mut send, &snapshot).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Broadcast channel closed (daemon shutting down).
                        info!("stream: broadcast channel closed, ending stream for {device_id}");
                        break;
                    }
                    _ => {
                        // Other events (PaneCreated, Notification for other panes, etc.) — ignore.
                    }
                }
            }
        }
    }

    let _ = send.finish();
    conn.close(0u8.into(), b"done");
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
}
