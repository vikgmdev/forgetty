//! The control socket on Windows: a named pipe with the same JSON-RPC lines.
#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use forgetty_core::fsutil::acl;
use forgetty_session::SessionManager;
use forgetty_socket::SocketServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

const LIST_TABS: &[u8] = b"{\"jsonrpc\":\"2.0\",\"method\":\"list_tabs\",\"id\":1}\n";

fn pipe_name() -> std::path::PathBuf {
    std::path::PathBuf::from(format!(r"\\.\pipe\forgetty-test-{}", uuid::Uuid::new_v4()))
}

/// Connect, waiting for the server to create the pipe and for a free instance.
async fn connect(name: &std::path::Path) -> NamedPipeClient {
    for _ in 0..200 {
        match ClientOptions::new().open(name) {
            Ok(c) => return c,
            // 2: not created yet. 231: all instances busy.
            Err(e) if matches!(e.raw_os_error(), Some(2 | 231)) => {
                tokio::time::sleep(Duration::from_millis(25)).await
            }
            Err(e) => panic!("cannot open {}: {e}", name.display()),
        }
    }
    panic!("the pipe {} did not come up", name.display());
}

fn start(
    name: &std::path::Path,
    foreign: Option<&str>,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    let mut server = SocketServer::new_with_path(name.to_path_buf()).unwrap();
    if let Some(sid) = foreign {
        server = server.restrict_to_sid(sid);
    }
    let sm = Arc::new(SessionManager::new());
    tokio::spawn(async move { server.run_with_streaming(sm, None).await })
}

#[tokio::test]
async fn the_owner_gets_an_answer_over_the_pipe() {
    let name = pipe_name();
    let task = start(&name, None);
    let mut c = connect(&name).await;
    c.write_all(LIST_TABS).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = c.read(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf[..n]);
    assert!(text.contains("\"result\""), "{text}");
    // A second request on the same connection.
    c.write_all(LIST_TABS).await.unwrap();
    let n = c.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains("\"result\""));
    task.abort();
}

#[tokio::test]
async fn several_clients_at_once_are_served() {
    let name = pipe_name();
    let task = start(&name, None);
    let mut clients = Vec::new();
    for _ in 0..4 {
        clients.push(connect(&name).await);
    }
    for c in &mut clients {
        c.write_all(LIST_TABS).await.unwrap();
    }
    for c in &mut clients {
        let mut buf = vec![0u8; 4096];
        let n = c.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("\"result\""));
    }
    task.abort();
}

#[tokio::test]
async fn a_client_of_another_user_gets_no_service() {
    // The same-user client plays a foreign user: the server is told to serve only the
    // SYSTEM account.
    assert_ne!(acl::current_user_sid().unwrap(), "S-1-5-18");
    let name = pipe_name();
    let task = start(&name, Some("S-1-5-18"));
    let mut c = connect(&name).await;
    // The write may fail too if the server has dropped the connection already.
    let _ = c.write_all(LIST_TABS).await;
    let mut buf = vec![0u8; 4096];
    let got = tokio::time::timeout(Duration::from_secs(10), c.read(&mut buf))
        .await
        .expect("the server closes the connection of a foreign user");
    match got {
        Ok(0) | Err(_) => {}
        Ok(n) => panic!("a foreign user got an answer: {}", String::from_utf8_lossy(&buf[..n])),
    }
    task.abort();
}

#[tokio::test]
async fn a_second_server_cannot_take_the_name() {
    let name = pipe_name();
    let first = start(&name, None);
    // Wait until the first one is up.
    let _c = connect(&name).await;
    let second = start(&name, None);
    let result = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .expect("the second server gives up")
        .unwrap();
    let err = result.expect_err("the name is taken");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
    first.abort();
}
