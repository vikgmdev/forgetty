//! Pairing protocol handler for incoming iroh connections.
//!
//! # Decision tree
//!
//! 1. **Known device** (`registry.is_authorized`): accept, update `last_seen`,
//!    emit `DeviceConnected`, close stream.
//! 2. **Unknown + `allow_pairing == true`**: auto-accept, add to registry,
//!    emit `DevicePaired`, close stream.
//! 3. **Unknown + `allow_pairing == false`**: accept QUIC (iroh requires it to
//!    send a rejection), send `{"v":1,"error":"not_authorized"}` on a bi stream
//!    (bi, not uni — Android calls `accept_bi()`), log the rejection, close.
//!
//! T-052 scope: the QUIC stream is closed after the pairing handshake.
//! No terminal streaming in T-052 — that is T-053.

use std::sync::{Arc, Mutex};

use iroh::{endpoint::Connection, EndpointId};
use tracing::{info, warn};

use crate::{
    registry::{iso8601_now, DeviceEntry, DeviceRegistry},
    SyncEvent,
};

/// Handle a single accepted iroh connection through the pairing decision tree.
///
/// `allow_pairing` reflects the current state of the pairing window, sampled
/// at the moment the connection is accepted.
pub async fn handle_connection(
    conn: Connection,
    registry: Arc<Mutex<DeviceRegistry>>,
    allow_pairing: bool,
    event_tx: tokio::sync::broadcast::Sender<SyncEvent>,
) {
    let remote_id: EndpointId = conn.remote_id();
    let device_id = remote_id.to_string();

    // --- Decision tree ---
    let is_authorized = {
        let reg = registry.lock().unwrap();
        reg.is_authorized(&remote_id)
    };

    if is_authorized {
        // Known device: update last_seen and emit DeviceConnected.
        {
            let mut reg = registry.lock().unwrap();
            let _ = reg.update_last_seen(&device_id);
        }
        info!("totem-sync: known device connected, device_id={device_id}");
        let _ = event_tx.send(SyncEvent::DeviceConnected { device_id });
        // T-052: close immediately; streaming is T-053.
        conn.close(0u8.into(), b"connected-ok");
        return;
    }

    if allow_pairing {
        // Auto-accept: read optional name from client (5-second timeout).
        let name = read_client_name(&conn).await.unwrap_or_else(|| {
            let ts = {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
            };
            format!("device-{ts}")
        });

        // Send pairing acknowledgment to client before saving.
        send_pairing_ack(&conn, &name).await;

        let entry = DeviceEntry {
            device_id: device_id.clone(),
            name: name.clone(),
            paired_at: iso8601_now(),
            last_seen: None,
        };

        {
            let mut reg = registry.lock().unwrap();
            if let Err(e) = reg.add(entry.clone()) {
                warn!("totem-sync: failed to save device {device_id}: {e}");
            }
        }

        info!("totem-sync: new device paired, device_id={device_id}, name={name}");
        let _ = event_tx.send(SyncEvent::DevicePaired { entry });
        conn.close(0u8.into(), b"paired-ok");
    } else {
        // Unknown device, pairing disabled: reject at application layer.
        reject_connection(&conn).await;
        warn!("totem-sync: rejected unknown device {device_id} (pairing not enabled)");
        conn.close(1u8.into(), b"not-authorized");
    }
}

// ---------------------------------------------------------------------------
// Pairing protocol helpers
// ---------------------------------------------------------------------------

/// Send `{"v":1,"status":"ok","machine":"<hostname>"}` on a uni stream, then
/// read the optional `{"v":1,"name":"<label>"}` response within 5 seconds.
///
/// Returns the name provided by the client, or `None` if no response arrives.
async fn read_client_name(conn: &Connection) -> Option<String> {
    // Open a bi-directional stream: we write first, then read.
    let (mut send, mut recv): (iroh::endpoint::SendStream, iroh::endpoint::RecvStream) =
        match conn.open_bi().await {
            Ok(s) => s,
            Err(e) => {
                warn!("totem-sync: failed to open bi stream for pairing: {e}");
                return None;
            }
        };

    // Send daemon greeting.
    let hostname = hostname::get()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string());
    let greeting = serde_json::json!({ "v": 1, "status": "ok", "machine": hostname });
    let mut line = serde_json::to_string(&greeting).unwrap_or_default();
    line.push('\n');

    if let Err(e) = send.write_all(line.as_bytes()).await {
        warn!("totem-sync: write greeting error: {e}");
        return None;
    }

    // Read client response with 5-second timeout.
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(5), read_line_from_recv(&mut recv))
            .await;

    match result {
        Ok(Some(line)) => {
            let val: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            val.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())
        }
        _ => None,
    }
}

/// Send the pairing acknowledgment back to the client.
async fn send_pairing_ack(conn: &Connection, _name: &str) {
    // In --allow-pairing auto-accept mode, the ack is the greeting in read_client_name.
    // This function is a no-op placeholder; the real handshake is in read_client_name.
    let _ = conn;
}

/// Send `{"v":1,"error":"not_authorized"}` on a bi-directional stream.
///
/// Uses a bi-stream (not uni) so that clients calling `accept_bi()` can read
/// the rejection reason. Waits up to 300 ms for the client to close its side
/// before returning, ensuring the stream data is delivered before the caller
/// closes the connection.
async fn reject_connection(conn: &Connection) {
    if let Ok((mut send, mut recv)) = conn.open_bi().await {
        let msg = b"{\"v\":1,\"error\":\"not_authorized\"}\n";
        let _ = send.write_all(msg).await;
        let _ = send.finish();
        // Wait for client to close its send-side (or timeout) so the rejection
        // message is delivered before we close the connection.
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(300), recv.read(&mut [0u8; 16]))
                .await;
    }
}

/// Read a single newline-terminated line from a `RecvStream`.
async fn read_line_from_recv(recv: &mut iroh::endpoint::RecvStream) -> Option<String> {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match recv.read_exact(&mut byte).await {
            Ok(()) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > 4096 {
                    // Guard against oversized lines.
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if buf.is_empty() {
        None
    } else {
        String::from_utf8(buf).ok()
    }
}
