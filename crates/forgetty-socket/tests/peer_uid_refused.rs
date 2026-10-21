//! A connection from another uid gets no service.
//!
//! The test cannot switch accounts, so the server is told to serve only
//! `getuid() + 1` (`restrict_to_uid`). The same-user client is then a foreign
//! peer from the server's point of view.

use std::sync::Arc;
use std::time::Duration;

use forgetty_session::SessionManager;
use forgetty_socket::SocketServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const LIST_TABS: &[u8] = b"{\"jsonrpc\":\"2.0\",\"method\":\"list_tabs\",\"id\":1}\n";

async fn start(
    restrict: Option<u32>,
) -> (tempfile::TempDir, std::path::PathBuf, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let mut server = SocketServer::new_with_path(path.clone()).unwrap();
    if let Some(uid) = restrict {
        server = server.restrict_to_uid(uid);
    }
    let sm = Arc::new(SessionManager::new());
    let task = tokio::spawn(async move {
        let _ = server.run_with_streaming(sm, None).await;
    });
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (dir, path, task)
}

#[tokio::test]
async fn foreign_uid_peer_is_dropped_without_a_reply() {
    let me = unsafe { libc::getuid() };
    let (_dir, path, task) = start(Some(me + 1)).await;

    let mut c = UnixStream::connect(&path).await.unwrap();
    let _ = c.write_all(LIST_TABS).await;
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("server must close the connection, not hang")
        .unwrap_or(0);
    assert_eq!(n, 0, "no reply for a foreign uid, got {:?}", String::from_utf8_lossy(&buf[..n]));
    task.abort();
}

#[tokio::test]
async fn same_uid_peer_is_served() {
    let (_dir, path, task) = start(None).await;

    let mut c = UnixStream::connect(&path).await.unwrap();
    c.write_all(LIST_TABS).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf)).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains("\"result\""));
    task.abort();
}
