//! Integration test for the V2-003 binary framing on the `subscribe_output`
//! streaming path.
//!
//! Spawns a real `SocketServer` running `run_with_streaming`, connects as a
//! client over `UnixStream`, subscribes to a live pane, drives the PTY with
//! a known echo command, and asserts that the bytes the server streamed back
//! arrive verbatim inside `[u32 BE length][payload]` frames — no base64, no
//! JSON envelope.

use std::sync::Arc;
use std::time::Duration;

use forgetty_pty::PtySize;
use forgetty_session::SessionManager;
use forgetty_socket::SocketServer;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

/// Exercise subscribe_output end-to-end: JSON-RPC ack, then binary frames
/// carrying raw PTY bytes.
#[tokio::test]
async fn subscribe_output_streams_binary_frames() {
    // --- Server setup ---
    let dir = std::env::temp_dir().join(format!(
        "forgetty-v2003-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let sock_path = dir.join("test.sock");
    let _ = std::fs::remove_file(&sock_path);

    let sm = Arc::new(SessionManager::new());
    // Spawn a real PTY so the server's pane_id validation passes and
    // real PtyOutput events land on the broadcast channel.
    let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
    let pane_id = sm.create_pane(size, None, None, None, true).expect("create pane");

    let server = SocketServer::new_with_path(sock_path.clone()).expect("server");
    let sm_for_server = Arc::clone(&sm);
    let server_handle =
        tokio::spawn(async move { server.run_with_streaming(sm_for_server, None).await });

    // Give the listener a beat to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // --- Client: send subscribe_output, parse the JSON ack line ---
    let stream = UnixStream::connect(&sock_path).await.expect("connect");
    let (reader, mut writer) = stream.into_split();

    let req = format!(
        "{{\"jsonrpc\":\"2.0\",\"method\":\"subscribe_output\",\"params\":{{\"pane_id\":\"{pane_id}\"}},\"id\":1}}\n"
    );
    writer.write_all(req.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    let mut buf_reader = BufReader::new(reader);
    let mut ack_line = String::new();
    buf_reader.read_line(&mut ack_line).await.expect("ack line");
    let ack: serde_json::Value = serde_json::from_str(ack_line.trim()).expect("ack JSON");
    assert_eq!(ack.get("result").and_then(|v| v.get("ok")).and_then(|v| v.as_bool()), Some(true));
    assert!(ack.get("error").is_none());

    // --- Drive the PTY and collect incoming frames until the echoed string arrives ---
    tokio::time::sleep(Duration::from_millis(200)).await;
    let marker = b"v2003_binary_marker";
    let mut send = Vec::new();
    send.extend_from_slice(b"printf '%s' ");
    send.extend_from_slice(marker);
    send.extend_from_slice(b"\n");
    sm.write_pty(pane_id, &send).expect("write_pty");

    let mut collected: Vec<u8> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        // Read one frame.
        let mut len_buf = [0u8; 4];
        match tokio::time::timeout(Duration::from_millis(300), buf_reader.read_exact(&mut len_buf))
            .await
        {
            Ok(Ok(_)) => {}
            _ => continue,
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        assert!(len <= MAX_FRAME_SIZE, "length {len} exceeds cap");
        if len == 0 {
            continue;
        }
        let mut payload = vec![0u8; len];
        buf_reader.read_exact(&mut payload).await.expect("frame payload");
        collected.extend_from_slice(&payload);
        if memchr_find(&collected, marker).is_some() {
            break;
        }
    }

    let found = memchr_find(&collected, marker);
    assert!(found.is_some(), "marker {:?} not found in {} streamed bytes", marker, collected.len());

    // Assert the bytes we got are NOT JSON-wrapped. A JSON notification line
    // would start with `{` and contain `"method":"output"` — our raw PTY
    // bytes must not be that shape.
    assert!(
        !collected.windows(17).any(|w| w == b"\"method\":\"output\""),
        "stream must not contain JSON `method: output` envelopes"
    );

    // --- Cleanup ---
    drop(writer);
    drop(buf_reader);
    sm.close_pane(pane_id).ok();
    server_handle.abort();
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_dir(&dir);
}

/// Simple byte-needle-in-haystack search (no external dep).
fn memchr_find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Cheap per-test suffix so parallel runs don't collide on the socket path.
fn rand_suffix() -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
        .hash(&mut h);
    h.finish() as u32
}
