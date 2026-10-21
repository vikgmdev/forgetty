//! QA round fixes. Every test needs the real server (`FORGETTY_REAL_SERVER`).

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{TestServer, TestServerConfig};
use forgetty_client::proto::{
    read_msg, to_value, write_msg, ClientKind, ControlMsg, HandshakeReply, Hello, Value, ALPN_RPC,
};
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

// ---------------------------------------------------------------------------
// Raw peers
// ---------------------------------------------------------------------------

/// Handshake a raw peer with `caps` and return its connection and control stream.
async fn raw_peer(
    server: &TestServer,
    caps: &[&str],
    client_id: Uuid,
) -> (iroh::Endpoint, iroh::endpoint::Connection, iroh::endpoint::SendStream) {
    let k = SecretKey::generate(&mut rand::rng());
    server.authorize(k.public());
    let endpoint = iroh::Endpoint::empty_builder()
        .secret_key(k)
        .relay_mode(iroh::RelayMode::Disabled)
        .transport_config(forgetty_client::transport_config())
        .bind()
        .await
        .unwrap();
    let conn = endpoint.connect(server.addr(), ALPN_RPC).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = Hello::new(
        client_id,
        Uuid::new_v4(),
        ClientKind::Cli,
        "raw",
        "0.0.0",
        caps.iter().map(|c| c.to_string()).collect(),
    );
    write_msg(&mut send, &hello).await.unwrap();
    match read_msg::<_, HandshakeReply>(&mut recv).await.unwrap() {
        Some(HandshakeReply::Welcome(_)) => {}
        other => panic!("handshake: {other:?}"),
    }
    // Keep reading the control stream, so only the pane stream goes unread.
    tokio::spawn(async move { while let Ok(Some(_)) = read_msg::<_, ControlMsg>(&mut recv).await {} });
    (endpoint, conn, send)
}

async fn attach_raw(send: &mut iroh::endpoint::SendStream, id: u64, pane: Uuid) {
    let params = to_value(&json!({"pane_id": pane.to_string(), "from_offset": null})).unwrap();
    let msg = ControlMsg::Request { id, method: "attach_pane".into(), params };
    write_msg(send, &msg).await.unwrap();
}

fn qa_state(server: &TestServer) -> serde_json::Value {
    server.socket_call("qa_state", json!({})).expect("qa_state")
}

/// A peer that attaches and never reads its pane stream: the QUIC send buffer
/// takes a window of bytes at once, which must not count as keeping pace.
/// The server rebuilds snapshots for it at the backoff rate, not every 100 ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_that_never_reads_does_not_get_a_snapshot_every_100_ms() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("exec yes 0123456789abcdef");
    let (endpoint, conn, mut send) =
        raw_peer(&server, &["resume", "snapshot_catchup"], Uuid::new_v4()).await;
    attach_raw(&mut send, 1, pane).await;
    tokio::time::sleep(Duration::from_secs(15)).await;
    let st = qa_state(&server);
    let catchups = st["catchups"].as_u64().unwrap();
    let builds = st["snapshot_builds"].as_u64().unwrap();
    println!("peer that never reads, 15 s: {catchups} catch-ups, {builds} snapshot builds");
    assert!(catchups <= 10, "{catchups} catch-ups for a peer that reads nothing");
    drop(conn);
    endpoint.close().await;
    server.shutdown().await;
}

/// Attaching again and again must not reset the catch-up backoff: the pacing
/// is kept per (client, pane), not per stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn re_attaching_does_not_reset_the_catch_up_backoff() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("exec yes 0123456789abcdef");
    let (endpoint, conn, mut send) =
        raw_peer(&server, &["resume", "snapshot_catchup"], Uuid::new_v4()).await;
    // Let the log grow past half a ring, so every attach starts with a snapshot.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let t0 = Instant::now();
    let mut id = 1;
    while t0.elapsed() < Duration::from_secs(8) {
        attach_raw(&mut send, id, pane).await;
        id += 1;
        // Long enough for each attach's snapshot to be built and sent.
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let st = qa_state(&server);
    let catchups = st["catchups"].as_u64().unwrap();
    println!("{} re-attaches in 8 s: {catchups} catch-ups", id - 1);
    assert!(catchups <= 3, "{catchups} snapshot catch-ups for {} re-attaches", id - 1);
    drop(conn);
    endpoint.close().await;
    server.shutdown().await;
}

/// A bad frame on the input stream closes the connection (0x02), so the
/// client redials instead of typing into a dead stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_input_frame_closes_the_connection() {
    use forgetty_client::proto::write_frame;
    use iroh::endpoint::ConnectionError;
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let (endpoint, conn, _send) = raw_peer(&server, &["resume"], Uuid::new_v4()).await;
    let mut input = conn.open_uni().await.unwrap();
    write_frame(&mut input, &[1, 2, 3]).await.unwrap(); // shorter than the 24 byte header
    let why = tokio::time::timeout(Duration::from_secs(5), conn.closed())
        .await
        .expect("the connection stayed open after a bad input frame");
    match why {
        ConnectionError::ApplicationClosed(c) => assert_eq!(u64::from(c.error_code), 0x02),
        other => panic!("closed for another reason: {other:?}"),
    }
    endpoint.close().await;
    server.shutdown().await;
}

/// A device that opens more than four connections loses its oldest one (0x15).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_holds_at_most_four_connections() {
    use iroh::endpoint::ConnectionError;
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let k = SecretKey::generate(&mut rand::rng());
    server.authorize(k.public());
    let endpoint = iroh::Endpoint::empty_builder()
        .secret_key(k)
        .relay_mode(iroh::RelayMode::Disabled)
        .transport_config(forgetty_client::transport_config())
        .bind()
        .await
        .unwrap();
    let mut conns = Vec::new();
    let mut keep = Vec::new();
    for i in 0..6 {
        let conn = endpoint.connect(server.addr(), ALPN_RPC).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let hello = Hello::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            ClientKind::Cli,
            "raw",
            "0.0.0",
            vec![],
        );
        write_msg(&mut send, &hello).await.unwrap();
        match read_msg::<_, HandshakeReply>(&mut recv).await.unwrap() {
            Some(HandshakeReply::Welcome(_)) => {}
            other => panic!("handshake {i}: {other:?}"),
        }
        keep.push((send, recv));
        conns.push(conn);
    }
    // The first two were over the cap and got closed; the last four stay.
    for (i, conn) in conns.iter().enumerate() {
        let r = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
        if i < 2 {
            match r.unwrap_or_else(|_| panic!("connection {i} was not closed")) {
                ConnectionError::ApplicationClosed(c) => {
                    assert_eq!(u64::from(c.error_code), 0x15, "connection {i}")
                }
                other => panic!("connection {i} closed for another reason: {other:?}"),
            }
        } else {
            assert!(r.is_err(), "connection {i} should stay open");
        }
    }
    endpoint.close().await;
    server.shutdown().await;
}

/// A client that reads one answer every few seconds keeps "a write completing"
/// but drains far less than the minimum rate: the watchdog still closes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_drips_reads_is_closed_as_stalled() {
    use iroh::endpoint::ConnectionError;
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    server.add_pane();
    let k = SecretKey::generate(&mut rand::rng());
    server.authorize(k.public());
    let endpoint = iroh::Endpoint::empty_builder()
        .secret_key(k)
        .relay_mode(iroh::RelayMode::Disabled)
        .transport_config(forgetty_client::transport_config())
        .bind()
        .await
        .unwrap();
    let conn = endpoint.connect(server.addr(), ALPN_RPC).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = Hello::new(Uuid::new_v4(), Uuid::new_v4(), ClientKind::Cli, "drip", "0.0.0", vec![]);
    write_msg(&mut send, &hello).await.unwrap();
    assert!(matches!(
        read_msg::<_, HandshakeReply>(&mut recv).await.unwrap(),
        Some(HandshakeReply::Welcome(_))
    ));
    let sender = tokio::spawn(async move {
        for id in 1..=200_000u64 {
            let msg = ControlMsg::Request { id, method: "list_clients".into(), params: Value::Nil };
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
        send
    });
    // One answer every 4 s: the writer completes a frame now and then.
    let reader = tokio::spawn(async move {
        loop {
            if read_msg::<_, ControlMsg>(&mut recv).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    });
    let why = tokio::time::timeout(Duration::from_secs(60), conn.closed())
        .await
        .expect("a client draining one answer every 4 s was never closed");
    match why {
        ConnectionError::ApplicationClosed(c) => assert_eq!(u64::from(c.error_code), 0x12),
        other => panic!("closed for another reason: {other:?}"),
    }
    sender.abort();
    reader.abort();
    endpoint.close().await;
    server.shutdown().await;
}

/// A client asks for a huge size: v2 and the local socket both clamp to 1000.
/// The socket used to cut 65537 down to 1 with an `as u16` cast.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resize_is_clamped_to_1000() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let dims = |server: &TestServer| {
        let v = server
            .socket_call("get_pane_info", json!({"pane_id": pane.to_string()}))
            .expect("get_pane_info");
        (v["rows"].as_u64().unwrap(), v["cols"].as_u64().unwrap())
    };
    server
        .socket_call("resize_pane", json!({"pane_id": pane.to_string(), "rows": 65537, "cols": 40000}))
        .expect("socket resize");
    assert_eq!(dims(&server), (1000, 1000));
    server
        .socket_call("resize_pane", json!({"pane_id": pane.to_string(), "rows": 24, "cols": 80}))
        .expect("socket resize back");
    assert_eq!(dims(&server), (24, 80));

    let c = connect(&server, "big").await;
    c.resize(pane, 60_000, 2_000).await.expect("v2 resize");
    // The resize runs on a blocking task; give it a moment.
    let t0 = Instant::now();
    while dims(&server) != (1000, 1000) {
        assert!(t0.elapsed() < Duration::from_secs(5), "v2 resize not clamped: {:?}", dims(&server));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    c.close().await;
    server.shutdown().await;
}
