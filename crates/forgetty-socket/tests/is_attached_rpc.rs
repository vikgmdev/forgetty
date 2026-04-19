//! Integration test for the V2-007 fix cycle 2 `is_attached` RPC.
//!
//! Covers the launcher's restore-path correctness question: "is any GUI
//! currently attached to this daemon?" The handler must return `false` when
//! the probe is the only open connection, and `true` when at least one
//! other connection is held.

use std::sync::Arc;
use std::time::Duration;

use forgetty_pty::PtySize;
use forgetty_session::SessionManager;
use forgetty_socket::SocketServer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
    format!("{nanos:x}")
}

/// Send one `is_attached` request over an ephemeral connection, parse the
/// response, and return the reported `attached` bool.
async fn probe_is_attached(sock_path: &std::path::Path) -> bool {
    let stream = UnixStream::connect(sock_path).await.expect("connect probe");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"is_attached\",\"id\":1}\n")
        .await
        .expect("send is_attached");
    writer.flush().await.expect("flush");

    let line = lines.next_line().await.expect("read response").expect("response line present");
    let parsed: serde_json::Value = serde_json::from_str(&line).expect("parse response");
    parsed
        .get("result")
        .and_then(|r| r.get("attached"))
        .and_then(|b| b.as_bool())
        .expect("attached field present")
}

#[tokio::test]
async fn is_attached_false_when_probe_is_sole_client() {
    let dir = std::env::temp_dir().join(format!(
        "forgetty-isattached-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let sock_path = dir.join("test.sock");
    let _ = std::fs::remove_file(&sock_path);

    let sm = Arc::new(SessionManager::new());
    let server = SocketServer::new_with_path(sock_path.clone()).expect("server");
    let sm_for_server = Arc::clone(&sm);
    let _server_handle =
        tokio::spawn(async move { server.run_with_streaming(sm_for_server, None).await });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // No other clients — only the probe itself. Expect false.
    let attached = probe_is_attached(&sock_path).await;
    assert!(!attached, "expected attached=false with single probe client");

    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_dir(&dir);
}

#[tokio::test]
async fn is_attached_true_when_another_client_holds_socket() {
    let dir = std::env::temp_dir().join(format!(
        "forgetty-isattached-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let sock_path = dir.join("test.sock");
    let _ = std::fs::remove_file(&sock_path);

    let sm = Arc::new(SessionManager::new());
    let server = SocketServer::new_with_path(sock_path.clone()).expect("server");
    let sm_for_server = Arc::clone(&sm);
    let _server_handle =
        tokio::spawn(async move { server.run_with_streaming(sm_for_server, None).await });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // First, hold a long-lived connection open (simulates the attached GUI).
    let held = UnixStream::connect(&sock_path).await.expect("connect held");
    // Give the server accept loop a chance to register the held connection
    // with its ConnGuard before we probe from a second connection.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Probe from a separate, ephemeral connection.
    let attached = probe_is_attached(&sock_path).await;
    assert!(attached, "expected attached=true when another client holds the socket");

    // Drop the held connection — the server's ConnGuard decrements on task
    // exit. After a short yield, attached should flip back to false.
    drop(held);
    // Give the server a beat to notice the EOF and drop the guard.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let attached_after = probe_is_attached(&sock_path).await;
    assert!(!attached_after, "expected attached=false after held connection dropped");

    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_dir(&dir);
}

/// V2-007 fix cycle 3 regression guard.
///
/// An idle `subscribe_output` client whose socket closes must release the
/// `ConnGuard` promptly. Before the fix, the streaming loop awaited only the
/// broadcast receiver — an idle pane produced no bytes, the write side was
/// never exercised, and the closed client was never noticed. The orphaned
/// task held the counter up, making `is_attached` wrongly return `true` on
/// the next probe and breaking the bare-launch restore path for vim sessions
/// (V2-007 AC-18).
#[tokio::test]
async fn is_attached_flips_false_after_idle_subscribe_output_client_disconnects() {
    let dir = std::env::temp_dir().join(format!(
        "forgetty-isattached-eof-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let sock_path = dir.join("test.sock");
    let _ = std::fs::remove_file(&sock_path);

    let sm = Arc::new(SessionManager::new());
    // Spawn a real PTY so subscribe_output validation passes. The pane is
    // intentionally idle — no write_pty — so there is nothing on the
    // broadcast channel while the subscribe_output client is connected.
    let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
    let pane_id = sm.create_pane(size, None, None, None, true).expect("create pane");

    let server = SocketServer::new_with_path(sock_path.clone()).expect("server");
    let sm_for_server = Arc::clone(&sm);
    let server_handle =
        tokio::spawn(async move { server.run_with_streaming(sm_for_server, None).await });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect, send subscribe_output, consume the ack, then drop the
    // connection while the pane is still idle.
    {
        let stream = UnixStream::connect(&sock_path).await.expect("connect sub");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let req = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"subscribe_output\",\"params\":{{\"pane_id\":\"{pane_id}\"}},\"id\":1}}\n"
        );
        writer.write_all(req.as_bytes()).await.expect("write sub");
        writer.flush().await.expect("flush sub");
        // Read the ack so we know the server entered the streaming loop.
        let ack_line = lines.next_line().await.expect("ack io").expect("ack line present");
        assert!(ack_line.contains("\"ok\":true"), "ack = {ack_line}");
        // Drop the stream halves — closes the client end while the pane is idle.
        drop(lines);
        drop(writer);
    }

    // Give the server a moment to notice the EOF via the new tokio::select!
    // read arm, break the loop, and drop the ConnGuard.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Probe: the only live connection should be the probe itself.
    let attached = probe_is_attached(&sock_path).await;
    assert!(
        !attached,
        "expected attached=false after idle subscribe_output client dropped (ConnGuard must have released)"
    );

    sm.close_pane(pane_id).ok();
    server_handle.abort();
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_dir(&dir);
}
