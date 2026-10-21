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
async fn blocked_input_stalls_nothing(a_script: &str, local_flood: bool) {
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

    // 1 MiB into a pane that does not take it. The pane's queue fills and the
    // rest is dropped (AD-024); nobody is held up.
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
    // A trusted local caller waits for room (it does not drop), so against a
    // pane that never reads it would hold the test's one socket connection:
    // only the pane that reads gets this flood.
    let socket_flood = {
        let server = server.clone();
        tokio::task::spawn_blocking(move || {
            if !local_flood {
                return;
            }
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD.encode(vec![b'y'; 1 << 20]);
            let _ =
                server.socket_call("send_input", json!({"pane_id": a.to_string(), "data": data}));
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
    let _ = &flood;
    assert!(worst < Duration::from_millis(100), "list_tabs took {worst:?} while pane A blocked");
    assert!(progress.load(Ordering::SeqCst) > before, "pane B stopped streaming");
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
    blocked_input_stalls_nothing("stty raw -echo; exec sleep infinity", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pane_that_never_reads_input_stalls_nothing_cat() {
    if !real_only() {
        return;
    }
    // cat echoes what it gets. Before the writer queue, a PTY write held the
    // lock the output drain needs, so cat and the server blocked each other.
    blocked_input_stalls_nothing("stty raw -echo; exec cat", true).await;
}

// ---------------------------------------------------------------------------
// Raw peers
// ---------------------------------------------------------------------------

/// A fresh endpoint with a new key the server accepts.
async fn raw_endpoint(server: &TestServer) -> iroh::Endpoint {
    let k = SecretKey::generate(&mut rand::rng());
    server.authorize(k.public());
    iroh::Endpoint::empty_builder()
        .secret_key(k)
        .relay_mode(iroh::RelayMode::Disabled)
        .transport_config(forgetty_client::transport_config())
        .bind()
        .await
        .unwrap()
}

/// Handshake a raw peer with `caps` on `endpoint` and return its connection
/// and control stream.
async fn raw_peer_on(
    endpoint: &iroh::Endpoint,
    server: &TestServer,
    caps: &[&str],
    client_id: Uuid,
) -> (iroh::endpoint::Connection, iroh::endpoint::SendStream) {
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
    tokio::spawn(
        async move { while let Ok(Some(_)) = read_msg::<_, ControlMsg>(&mut recv).await {} },
    );
    (conn, send)
}

/// Handshake a raw peer on a new key.
async fn raw_peer(
    server: &TestServer,
    caps: &[&str],
    client_id: Uuid,
) -> (iroh::Endpoint, iroh::endpoint::Connection, iroh::endpoint::SendStream) {
    let endpoint = raw_endpoint(server).await;
    let (conn, send) = raw_peer_on(&endpoint, server, caps, client_id).await;
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
    while t0.elapsed() < Duration::from_secs(30) {
        attach_raw(&mut send, id, pane).await;
        id += 1;
        // Long enough for each attach's snapshot to be built and sent.
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let st = qa_state(&server);
    let catchups = st["catchups"].as_u64().unwrap();
    println!("{} re-attaches in 30 s: {catchups} catch-ups", id - 1);
    // A peer may claim to keep pace 6 times in a burst; after that, one per 10 s.
    assert!(catchups <= 11, "{catchups} snapshot catch-ups for {} re-attaches", id - 1);
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
        let hello =
            Hello::new(Uuid::new_v4(), Uuid::new_v4(), ClientKind::Cli, "raw", "0.0.0", vec![]);
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
    let hello =
        Hello::new(Uuid::new_v4(), Uuid::new_v4(), ClientKind::Cli, "drip", "0.0.0", vec![]);
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
        .socket_call(
            "resize_pane",
            json!({"pane_id": pane.to_string(), "rows": 65537, "cols": 40000}),
        )
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
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "v2 resize not clamped: {:?}",
            dims(&server)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    c.close().await;
    server.shutdown().await;
}

/// `Hello.client_name` is peer text: control characters go, length is capped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_name_is_sanitised() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let name = format!("\u{1b}[31mevil\u{7}{}", "n".repeat(200));
    let c = connect(&server, &name).await;
    let list = c.request("list_clients", Value::Nil).await.expect("list_clients");
    let text = format!("{list:?}");
    assert!(!text.contains('\u{1b}') && !text.contains('\u{7}'), "control characters kept: {text}");
    let Value::Array(items) = list else { panic!("not an array: {text}") };
    let names: Vec<String> = items
        .iter()
        .filter_map(|e| match e {
            Value::Map(m) => m.iter().find_map(|(k, v)| match (k, v) {
                (Value::String(k), Value::String(v)) if k.as_str() == Some("client_name") => {
                    v.as_str().map(str::to_string)
                }
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 1, "{text}");
    assert!(names[0].chars().count() <= 64, "name of {} chars", names[0].chars().count());
    assert!(names[0].starts_with("evil"), "{:?}", names[0]);
    c.close().await;
    server.shutdown().await;
}

/// Pacing is per remote node, not per `client_id`: a client that reconnects
/// with a fresh `client_id` every time gets the same backoff (N3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_client_id_per_connection_does_not_reset_the_catch_up_backoff() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("exec yes 0123456789abcdef");
    let endpoint = raw_endpoint(&server).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let t0 = Instant::now();
    let mut n = 0;
    while t0.elapsed() < Duration::from_secs(30) {
        let (conn, mut send) =
            raw_peer_on(&endpoint, &server, &["resume", "snapshot_catchup"], Uuid::new_v4()).await;
        attach_raw(&mut send, 1, pane).await;
        n += 1;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        drop(conn);
    }
    let st = qa_state(&server);
    let catchups = st["catchups"].as_u64().unwrap();
    println!("{n} connections with fresh client ids in 30 s: {catchups} catch-ups");
    // Burst of 6 fast ones, then one per 10 s. Per client id it would be one
    // per connection (about 25).
    assert!(catchups <= 11, "{catchups} catch-ups for {n} reconnects of one node");
    endpoint.close().await;
    server.shutdown().await;
}

/// A pane that never reads costs only its own input (AD-024): typing into
/// another pane arrives at once, `input_acked` keeps moving, and Ctrl+C still
/// ends the program that hangs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hung_pane_costs_only_its_own_input_and_ctrl_c_still_works() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let a = server.add_script_pane("stty raw -echo; exec sleep infinity");
    let b = server.add_script_pane("stty raw -echo; exec cat");
    let c = connect(&server, "typist").await;
    let mut stream_b = c.attach_pane(b).await.unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let reader = {
        let seen = seen.clone();
        tokio::spawn(async move {
            while let Some(item) = stream_b.next().await {
                if let PaneItem::Data { bytes, .. } | PaneItem::Snapshot { bytes, .. } = item {
                    seen.lock().unwrap().extend_from_slice(&bytes);
                }
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 1 MiB into A. The queue (256 KiB) fills and the rest is dropped.
    let chunk = vec![b'x'; 4096];
    for _ in 0..256 {
        c.send_input(a, &chunk).await.unwrap();
    }
    // Typing into B arrives in under 50 ms (median), never seconds.
    let mut lat = Vec::new();
    for i in 0..20u8 {
        let marker = [b'a' + i % 26, b'0' + i % 10, b'Z'];
        let t0 = Instant::now();
        c.send_input(b, &marker).await.unwrap();
        loop {
            if seen.lock().unwrap().windows(3).any(|w| w == marker) {
                break;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(2),
                "typing into B never arrived (round {i})"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        lat.push(t0.elapsed());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    lat.sort();
    println!("typing into B while A is hung: median {:?}, worst {:?}", lat[10], lat[19]);
    assert!(lat[10] < Duration::from_millis(50), "median {:?}", lat[10]);
    assert!(lat[19] < Duration::from_millis(500), "worst {:?}", lat[19]);
    // The acks keep moving: nothing is stuck behind A's frames.
    let t0 = Instant::now();
    while c.unacked_input_bytes() != 0 {
        assert!(t0.elapsed() < Duration::from_secs(3), "input_acked stopped moving");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let st = qa_state(&server);
    println!("frames dropped for A: {}", st["input_drops"]);
    assert!(st["input_drops"].as_u64().unwrap() > 0, "the full queue dropped nothing");

    // Ctrl+C reaches the hung program although its queue is full.
    c.send_input(a, &[0x03]).await.unwrap();
    let t0 = Instant::now();
    loop {
        let info = server.socket_call("get_pane_info", json!({"pane_id": a.to_string()}));
        let ended = match &info {
            Err(_) => true, // the pane closed with its program
            Ok(v) => !v["exit_code"].is_null(),
        };
        if ended {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "Ctrl+C did not end the program in A");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    reader.abort();
    c.close().await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// AD-025: only the size owner's `auto` input reaches the PTY
// ---------------------------------------------------------------------------

const AD25_CAPS: [&str; 3] = ["size_policy_latest", "input_flags", "resume"];

async fn control(
    send: &mut iroh::endpoint::SendStream,
    id: u64,
    method: &str,
    params: serde_json::Value,
) {
    let msg = ControlMsg::Request { id, method: method.into(), params: to_value(&params).unwrap() };
    write_msg(send, &msg).await.unwrap();
}

fn pane_dims(server: &TestServer, pane: Uuid) -> (u64, u64) {
    let v = server
        .socket_call("get_pane_info", json!({"pane_id": pane.to_string()}))
        .expect("get_pane_info");
    (v["rows"].as_u64().unwrap(), v["cols"].as_u64().unwrap())
}

/// Everything the server queued for the pane's PTY so far.
fn pty_input(server: &TestServer, pane: Uuid) -> Vec<u8> {
    use base64::Engine;
    let v = server.socket_call("qa_input", json!({"pane_id": pane.to_string()})).expect("qa_input");
    base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().unwrap()).unwrap()
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ok() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_size_owners_auto_input_reaches_the_pty() {
    use forgetty_client::proto::{write_input_frame_flags, INPUT_FLAG_AUTO};
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("stty raw -echo; exec cat");
    let auto_drops = |s: &TestServer| qa_state(s)["auto_drops"].as_u64().unwrap();

    // A attaches first and asks for 30x100: it is the owner. B asks for 40x120.
    let (_ea, conn_a, mut ctl_a) = raw_peer(&server, &AD25_CAPS, Uuid::new_v4()).await;
    attach_raw(&mut ctl_a, 1, pane).await;
    control(
        &mut ctl_a,
        2,
        "resize_pane",
        json!({"pane_id": pane.to_string(), "rows": 30, "cols": 100}),
    )
    .await;
    wait_for("A's size", || pane_dims(&server, pane) == (30, 100)).await;
    let (_eb, conn_b, mut ctl_b) = raw_peer(&server, &AD25_CAPS, Uuid::new_v4()).await;
    attach_raw(&mut ctl_b, 1, pane).await;
    control(
        &mut ctl_b,
        2,
        "resize_pane",
        json!({"pane_id": pane.to_string(), "rows": 40, "cols": 120}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(pane_dims(&server, pane), (30, 100), "B's resize must not take the size");

    let mut in_a = conn_a.open_uni().await.unwrap();
    let mut in_b = conn_b.open_uni().await.unwrap();

    // B is not the owner: its auto DA reply is spent and acked, never written,
    // and it does not change the size.
    write_input_frame_flags(&mut in_b, pane, 1, INPUT_FLAG_AUTO, b"\x1b[?62;B\x1b").await.unwrap();
    wait_for("B's auto frame to be handled", || auto_drops(&server) == 1).await;
    assert!(
        !contains(&pty_input(&server, pane), b"?62;B"),
        "a non-owner's auto reply reached the PTY"
    );
    assert_eq!(pane_dims(&server, pane), (30, 100), "an auto frame changed the size");

    // The owner's auto reply goes through, and the size stays.
    write_input_frame_flags(&mut in_a, pane, 1, INPUT_FLAG_AUTO, b"\x1b[?62;A\x1b").await.unwrap();
    wait_for("A's auto reply", || contains(&pty_input(&server, pane), b"?62;A")).await;
    assert_eq!(auto_drops(&server), 1);
    assert_eq!(pane_dims(&server, pane), (30, 100));

    // A user key from B makes B the owner and the PTY takes B's size.
    write_input_frame_flags(&mut in_b, pane, 2, 0, b"k").await.unwrap();
    wait_for("B's key", || contains(&pty_input(&server, pane), b"k")).await;
    wait_for("B's size", || pane_dims(&server, pane) == (40, 120)).await;
    // Its next auto frame goes through; A's is now dropped.
    write_input_frame_flags(&mut in_b, pane, 3, INPUT_FLAG_AUTO, b"\x1b[?63;B\x1b").await.unwrap();
    wait_for("B's auto reply as owner", || contains(&pty_input(&server, pane), b"?63;B")).await;
    write_input_frame_flags(&mut in_a, pane, 2, INPUT_FLAG_AUTO, b"\x1b[?63;A\x1b").await.unwrap();
    wait_for("A's auto frame to be handled", || auto_drops(&server) == 2).await;
    assert!(!contains(&pty_input(&server, pane), b"?63;A"));
    assert_eq!(pane_dims(&server, pane), (40, 120));

    // When the owner (B) detaches, A, who is still attached, takes over with
    // the size it asked for.
    control(&mut ctl_b, 3, "detach_pane", json!({"pane_id": pane.to_string()})).await;
    wait_for("A's size after B left", || pane_dims(&server, pane) == (30, 100)).await;
    write_input_frame_flags(&mut in_a, pane, 3, INPUT_FLAG_AUTO, b"\x1b[?64;A\x1b").await.unwrap();
    wait_for("A's auto reply as the new owner", || contains(&pty_input(&server, pane), b"?64;A"))
        .await;

    drop((conn_a, conn_b));
    server.shutdown().await;
}

/// A client that did not negotiate `input_flags` sends the old frame, and all
/// of it is `user` input.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_without_the_flags_capability_is_user_input() {
    use forgetty_client::proto::write_input_frame;
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("stty raw -echo; exec cat");
    let (_ea, conn_a, mut ctl_a) = raw_peer(&server, &["size_policy_latest"], Uuid::new_v4()).await;
    attach_raw(&mut ctl_a, 1, pane).await;
    let mut input = conn_a.open_uni().await.unwrap();
    write_input_frame(&mut input, pane, 1, b"\x01plain").await.unwrap();
    wait_for("plain input", || contains(&pty_input(&server, pane), b"\x01plain")).await;
    assert_eq!(qa_state(&server)["auto_drops"].as_u64().unwrap(), 0);
    drop(conn_a);
    server.shutdown().await;
}

/// A hostile node that asks for catch-ups on many panes with a fresh
/// `client_id` per connection costs one build at a time, and an honest second
/// node still gets its own catch-up promptly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hostile_node_does_not_starve_an_honest_nodes_catch_up() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let busy: Vec<Uuid> =
        (0..8).map(|_| server.add_script_pane("exec yes 0123456789abcdef")).collect();
    let (hostile_panes, honest_panes) = busy.split_at(4);
    let endpoint = raw_endpoint(&server).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hostile = {
        let (stop, panes) = (stop.clone(), hostile_panes.to_vec());
        let endpoint = endpoint.clone();
        let addr = server.addr();
        tokio::spawn(async move {
            let mut conns = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                // Same node, fresh client id each time, never reads a pane.
                let conn = endpoint.connect(addr.clone(), forgetty_client::proto::ALPN_RPC).await;
                let Ok(conn) = conn else { break };
                let Ok((mut send, mut recv)) = conn.open_bi().await else { break };
                let hello = Hello::new(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    ClientKind::Cli,
                    "hostile",
                    "0.0.0",
                    vec!["resume".into(), "snapshot_catchup".into()],
                );
                write_msg(&mut send, &hello).await.unwrap();
                let _ = read_msg::<_, HandshakeReply>(&mut recv).await;
                tokio::spawn(async move {
                    while let Ok(Some(_)) = read_msg::<_, ControlMsg>(&mut recv).await {}
                });
                for (i, pane) in panes.iter().enumerate() {
                    attach_raw(&mut send, i as u64 + 1, *pane).await;
                }
                conns.push(conn);
                if conns.len() > 3 {
                    conns.remove(0);
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
    };
    tokio::time::sleep(Duration::from_secs(3)).await;

    let honest = connect(&server, "honest").await;
    let mut worst = Duration::ZERO;
    for pane in honest_panes.iter().take(3) {
        let t0 = Instant::now();
        let mut s = honest.attach_pane(*pane).await.unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), s.next()).await {
                Ok(Some(PaneItem::Snapshot { .. })) => break,
                Ok(Some(_)) => continue,
                other => panic!("no snapshot for the honest node: {other:?}"),
            }
        }
        let took = t0.elapsed();
        println!("honest node catch-up while a hostile node asks: {took:?}");
        worst = worst.max(took);
    }
    stop.store(true, Ordering::SeqCst);
    let _ = hostile.await;
    let st = qa_state(&server);
    println!(
        "worst {worst:?}, builds {}, max running {}, last build {} us",
        st["snapshot_builds"], st["snapshot_max_running"], st["last_snapshot_us"]
    );
    assert!(st["snapshot_max_running"].as_u64().unwrap() <= 2);
    assert!(worst < Duration::from_millis(300), "the honest node waited {worst:?}");
    honest.close().await;
    endpoint.close().await;
    server.shutdown().await;
}
