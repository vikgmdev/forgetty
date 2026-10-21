//! QA round fixes. Every test needs the real server (`FORGETTY_REAL_SERVER`).

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{TestServer, TestServerConfig};
use forgetty_client::proto::{ClientKind, Value};
use forgetty_client::{Client, ClientHandle, ClientInfo, PaneItem, SecretKey};
use serde_json::json;
use uuid::Uuid;

fn real_only() -> bool {
    if common::is_real() {
        return true;
    }
    println!("skipped: needs the real server (set FORGETTY_REAL_SERVER)");
    false
}

async fn connect(server: &TestServer, name: &str) -> ClientHandle {
    let k = SecretKey::generate(&mut rand::rng());
    server.authorize(k.public());
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, name, "0.0.0");
    tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), k, info))
        .await
        .expect("connect timed out")
        .expect("connect failed")
}

/// Pane A runs `a_script` and gets 1 MiB of input it never reads (or reads
/// into a full output). Pane B streams output. Meanwhile the server must
/// answer `list_tabs` (v2 and local socket) in under 100 ms, keep the
/// connection of the second client up, and keep B's stream moving.
async fn blocked_input_stalls_nothing(a_script: &str) {
    let server = Arc::new(TestServer::start(TestServerConfig::default()).await);
    let a = server.add_script_pane(a_script);
    let b = server.add_script_pane("exec yes 0123456789abcdef");
    let writer = connect(&server, "writer").await;
    let other = connect(&server, "other").await;

    let mut stream = other.attach_pane(b).await.unwrap();
    let progress = Arc::new(AtomicU64::new(0));
    let reader = {
        let progress = progress.clone();
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                match item {
                    PaneItem::Data { bytes, .. } | PaneItem::Snapshot { bytes, .. } => {
                        progress.fetch_add(bytes.len() as u64, Ordering::SeqCst);
                    }
                    PaneItem::Ended => break,
                    PaneItem::Reset { .. } => {}
                }
            }
        })
    };

    // 1 MiB into a pane that does not take it. The writer's own input stalls
    // (its send_input waits for acks); nobody else may.
    let flood = {
        let writer = writer.clone();
        tokio::spawn(async move {
            let chunk = vec![b'x'; 1024];
            for _ in 0..1024 {
                if writer.send_input(a, &chunk).await.is_err() {
                    break;
                }
            }
        })
    };
    // The same 1 MiB in one local socket call (the cat case deadlocked here).
    let socket_flood = {
        let server = server.clone();
        tokio::task::spawn_blocking(move || {
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD.encode(vec![b'y'; 1 << 20]);
            let _ = server.socket_call("send_input", json!({"pane_id": a.to_string(), "data": data}));
        })
    };
    tokio::time::sleep(Duration::from_millis(700)).await;
    let before = progress.load(Ordering::SeqCst);
    assert!(before > 0, "pane B never streamed");

    let mut worst = Duration::ZERO;
    let t_end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < t_end {
        let t0 = Instant::now();
        other.request("list_tabs", Value::Nil).await.expect("list_tabs over v2");
        worst = worst.max(t0.elapsed());
        let t0 = Instant::now();
        server.socket_call("list_tabs", json!({})).expect("list_tabs on the socket");
        worst = worst.max(t0.elapsed());
        assert!(other.state().is_connected(), "the other client lost its connection");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!(
        "blocked input ({a_script}): worst list_tabs {worst:?}, pane A took {} bytes, \
         {} unacked at the writer",
        server.received_input(a).len(),
        writer.unacked_input_bytes()
    );
    assert!(worst < Duration::from_millis(100), "list_tabs took {worst:?} while pane A blocked");
    assert!(progress.load(Ordering::SeqCst) > before, "pane B stopped streaming");
    assert!(!flood.is_finished(), "pane A took 1 MiB it cannot take, the test did not block it");
    flood.abort();
    socket_flood.abort();
    reader.abort();
    writer.close().await;
    other.close().await;
    match Arc::try_unwrap(server) {
        Ok(s) => s.shutdown().await,
        Err(_) => panic!("server still shared"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pane_that_never_reads_input_stalls_nothing_sleep() {
    if !real_only() {
        return;
    }
    blocked_input_stalls_nothing("stty raw -echo; exec sleep infinity").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pane_that_never_reads_input_stalls_nothing_cat() {
    if !real_only() {
        return;
    }
    // cat echoes what it gets. Before the writer queue, a PTY write held the
    // lock the output drain needs, so cat and the server blocked each other.
    blocked_input_stalls_nothing("stty raw -echo; exec cat").await;
}
