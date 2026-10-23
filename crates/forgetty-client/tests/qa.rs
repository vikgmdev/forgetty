//! QA round fixes. Every test needs the real server (`FORGETTY_REAL_SERVER`).

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{real_only, TestServer, TestServerConfig};
use forgetty_client::proto::{
    read_msg, to_value, write_msg, ClientKind, ControlMsg, HandshakeReply, Hello, Value, ALPN_RPC,
};
use forgetty_client::{Client, ClientHandle, ClientInfo, PaneItem, SecretKey};
use serde_json::json;
use uuid::Uuid;

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
/// answer `list_tabs` (v2 and local socket) in under 250 ms (or 10 times the
/// idle cost on a slow host, never over 2 s), keep the
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

    // The same two calls before the flood, with B streaming: what they cost
    // on this host right now. The bound below is relative to it, so a loaded
    // host does not fail the test for being slow.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut baseline = Duration::ZERO;
    for _ in 0..10 {
        let t0 = Instant::now();
        other.request("list_tabs", Value::Nil).await.expect("list_tabs over v2");
        baseline = baseline.max(t0.elapsed());
        let t0 = Instant::now();
        server.socket_call("list_tabs", json!({})).expect("list_tabs on the socket");
        baseline = baseline.max(t0.elapsed());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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
    // The failure this guards against is a stall of seconds, so the bound has
    // room for the load other tests put on the host while this one runs. It
    // has a ceiling, or a regression that slows every call would raise it.
    assert!(baseline < Duration::from_millis(250), "list_tabs is slow when idle: {baseline:?}");
    let bound = Duration::from_millis(250).max(baseline * 10).min(Duration::from_secs(2));
    println!("list_tabs before the flood (worst of 10): {baseline:?}, bound {bound:?}");
    assert!(worst < bound, "list_tabs took {worst:?} while pane A blocked (idle: {baseline:?})");
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
        // A flood: 4 attaches a second. At 1 a second or less they would be
        // served by the tokens (6 free, then one per second) all the time.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let st = qa_state(&server);
    let catchups = st["catchups"].as_u64().unwrap();
    println!("{} re-attaches in 30 s: {catchups} catch-ups", id - 1);
    // 6 free in a burst; with the tokens gone, the backoff (1 s doubling to
    // 10 s) sets the pace.
    assert!(catchups <= 16, "{catchups} snapshot catch-ups for {} re-attaches", id - 1);
    drop(conn);
    endpoint.close().await;
    server.shutdown().await;
}

/// A peer that never reads and attaches again every 2 s for a minute. The
/// resume tokens are 6 and time earns none, so it gets 6 snapshots, then only
/// what the doubling backoff lets through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_that_never_reads_and_re_attaches_every_two_seconds_gets_six_and_the_backoff() {
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
    while t0.elapsed() < Duration::from_secs(60) {
        attach_raw(&mut send, id, pane).await;
        id += 1;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let st = qa_state(&server);
    let catchups = st["catchups"].as_u64().unwrap();
    println!("{} re-attaches in 60 s, never reading: {catchups} catch-ups", id - 1);
    // 6 tokens, then the backoff (2 s, 4 s, then it outlasts the 2 s between
    // attaches) lets a few more through. The stream's own stall catch-ups
    // share the same backoff.
    assert!(catchups <= 6 + 4, "{catchups} snapshot catch-ups in 60 s for a peer that reads nothing");
    drop(conn);
    endpoint.close().await;
    server.shutdown().await;
}

/// A phone that reads, drops and is far behind again gets each snapshot
/// without waiting for pacing: its reads (a `Strong` claim each time the
/// server catches it up) earn the resume tokens back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_honest_far_behind_phone_gets_every_snapshot_in_under_300_ms() {
    use forgetty_client::proto::{read_output_frame, PaneStreamHeader};
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    // About 10 MB/s, in 1 MiB lumps.
    let pane = server.add_script_pane(
        "while :; do head -c 1048576 /dev/zero | tr '\\0' x; sleep 0.1; done",
    );
    let (endpoint, conn, mut ctl) =
        raw_peer(&server, &["resume", "snapshot_catchup"], Uuid::new_v4()).await;
    // Far behind: the log is past half a ring.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut worst = Duration::ZERO;
    for round in 0..10u64 {
        let t0 = Instant::now();
        attach_raw(&mut ctl, round + 1, pane).await;
        // Streams the server replaced (0x13) before this attach may still be
        // queued: the answer is the first one that gives a header and its
        // whole snapshot.
        let (mut s, h) = 'streams: loop {
            let mut s = conn.accept_uni().await.unwrap();
            let Ok(Some(h)) = read_msg::<_, PaneStreamHeader>(&mut s).await else { continue };
            let mut buf = Vec::new();
            let mut got = 0usize;
            while got < h.snapshot_len as usize {
                match read_output_frame(&mut s, &mut buf).await {
                    Ok(true) => got += buf.len(),
                    _ => continue 'streams,
                }
            }
            break (s, h);
        };
        let took = t0.elapsed();
        worst = worst.max(took);
        assert!(h.snapshot_len > 0, "round {round}: no snapshot, the phone was not far behind");
        // The wait is this round's build and the 100 ms floor, not the pacing
        // (1 s and more): a build and 900 ms is the bound, so a 1 s pacing
        // wait fails however long the build took.
        let build = Duration::from_micros(qa_state(&server)["last_snapshot_us"].as_u64().unwrap_or(0));
        assert!(
            took < build + Duration::from_millis(900),
            "round {round}: snapshot after {took:?}, this round's build is {build:?}"
        );
        // It reads at about 4 MiB/s, which is less than the program writes:
        // the server ends the stream once the phone is 4 MiB behind (the
        // claim is a strong one: live bytes at a good rate), and the phone
        // drops.
        let mut buf = Vec::new();
        let ended = tokio::time::timeout(Duration::from_secs(10), async {
            while let Ok(true) = read_output_frame(&mut s, &mut buf).await {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await;
        assert!(ended.is_ok(), "round {round}: the server never caught the phone up");
    }
    println!("honest phone, 10 far-behind attaches: slowest snapshot {worst:?}");
    drop(conn);
    endpoint.close().await;
    server.shutdown().await;
}

/// `attach_pane` and `detach_pane` run as tasks of their own. A detach that
/// the server took after an attach, but finished before it, still wins: the
/// attach finds a newer request for its pane and opens no stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_detach_that_finishes_first_still_wins_over_the_attach_before_it() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("exec yes 0123456789abcdef");
    let (endpoint, conn, mut ctl) =
        raw_peer(&server, &["resume", "snapshot_catchup"], Uuid::new_v4()).await;
    server.socket_call("qa_attach_delay", json!({"ms": 300})).unwrap();
    attach_raw(&mut ctl, 1, pane).await;
    let params = to_value(&json!({"pane_id": pane.to_string()})).unwrap();
    let msg = ControlMsg::Request { id: 2, method: "detach_pane".into(), params };
    write_msg(&mut ctl, &msg).await.unwrap();
    // The attach is still in its lookups when the detach is done.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let stream = tokio::time::timeout(Duration::from_millis(500), conn.accept_uni()).await;
    server.socket_call("qa_attach_delay", json!({"ms": 0})).unwrap();
    assert!(stream.is_err(), "a stream opened after the detach");
    // A later attach works.
    attach_raw(&mut ctl, 3, pane).await;
    let mut s = tokio::time::timeout(Duration::from_secs(5), conn.accept_uni())
        .await
        .expect("no stream for the later attach")
        .unwrap();
    let h: forgetty_client::proto::PaneStreamHeader = read_msg(&mut s).await.unwrap().unwrap();
    assert_eq!(h.pane_id, pane);
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
    // A client owns a pane's size only while it has a stream on it (AD-025).
    let _stream = c.attach_pane(pane).await.unwrap();
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
        tokio::time::sleep(Duration::from_millis(250)).await;
        drop(conn);
    }
    let st = qa_state(&server);
    let catchups = st["catchups"].as_u64().unwrap();
    println!("{n} connections with fresh client ids in 30 s: {catchups} catch-ups");
    // Burst of 6 fast ones, then the backoff. Per client id it would be one
    // per connection (about 120).
    assert!(catchups <= 16, "{catchups} catch-ups for {n} reconnects of one node");
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
    // A keeps ISIG on (no `raw`), so Ctrl+C can reach it as a signal.
    let a = server.add_script_pane("stty -echo; exec sleep infinity");
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
    // Lines, so the tty (canonical mode) holds them and then blocks the writer.
    let chunk = b"xxxxxxxxx\n".repeat(400);
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
/// The tests that burn CPU for a while (8 busy panes, full-ring captures) take
/// turns: the timing limit of the catch-up test does not hold next to them.
static HEAVY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Wait for the snapshot item of `s`. The limit is for the whole wait, not per
/// item: a busy pane sends output items all the time, so a per-item timeout
/// never fires while no snapshot comes. (This once hung a gate for 30 minutes.)
async fn wait_for_snapshot(s: &mut forgetty_client::PaneStream, limit: Duration, who: &str) {
    let t0 = Instant::now();
    loop {
        let left = limit.saturating_sub(t0.elapsed());
        match tokio::time::timeout(left, s.next()).await {
            Ok(Some(PaneItem::Snapshot { .. })) => return,
            Ok(Some(_)) => continue,
            other => panic!("no snapshot for {who} after {:?}: {other:?}", t0.elapsed()),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hostile_node_does_not_starve_an_honest_nodes_catch_up() {
    if !real_only() {
        return;
    }
    let _heavy = HEAVY.lock().await;
    let server = TestServer::start(TestServerConfig::default()).await;
    let busy: Vec<Uuid> =
        (0..8).map(|_| server.add_script_pane("exec yes 0123456789abcdef")).collect();
    let (hostile_panes, honest_panes) = busy.split_at(4);
    let endpoint = raw_endpoint(&server).await;
    // A fresh attach starts at the first offset of the log and gets a snapshot
    // when the log is more than half the ring ahead of it (the server's
    // `lag_limit`). The ring size comes from the server (`qa_state`). The log
    // file is capped at 10 MiB (`byte_log_max_mb`) and rotates down to 5 MiB,
    // so `total - first` stays between 5 and 10 MiB once it has grown that
    // far; the ring (8 MiB by default) leaves a limit of 4 MiB below that.
    // Wait for the fill on every busy pane, however slow `yes` runs on a loaded
    // host, instead of assuming that 2 s are enough.
    let ring = qa_state(&server)["panes"]
        .as_array()
        .and_then(|p| p.iter().find(|p| p["pane_id"] == busy[0].to_string()))
        .and_then(|p| p["ring_capacity"].as_u64())
        .expect("qa_state reports the ring size");
    assert!(ring > 0, "the pane has no byte log");
    let fill: u64 = ring / 2 + 512 * 1024;
    assert!(
        fill < 5 * 1024 * 1024,
        "a ring of {ring} bytes needs {fill} bytes of log, but the file keeps only 5 MiB after a rotation"
    );
    let t_fill = Instant::now();
    loop {
        let short = busy
            .iter()
            .filter(|p| {
                let (first, total) = server.log_bounds(**p);
                total.saturating_sub(first) < fill
            })
            .count();
        if short == 0 {
            break;
        }
        assert!(
            t_fill.elapsed() < Duration::from_secs(60),
            "{short} busy panes had under {fill} bytes of log after 60 s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // One snapshot on this host right now, from a node nobody competes with:
    // the bound for the contended ones is relative to it.
    let me = common::test_name();
    common::progress_for(&me, "test", "probe connect");
    let probe = connect(&server, "probe").await;
    common::progress_for(&me, "test", "probe attach_pane");
    let t0 = Instant::now();
    let mut s = probe.attach_pane(honest_panes[3]).await.unwrap();
    common::progress_for(&me, "test", "probe waits for its snapshot");
    wait_for_snapshot(&mut s, Duration::from_secs(20), "the probe").await;
    let one_build = t0.elapsed();
    drop(s);
    probe.close().await;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hostile = {
        let (stop, panes) = (stop.clone(), hostile_panes.to_vec());
        let endpoint = endpoint.clone();
        let addr = server.addr();
        let me = me.clone();
        tokio::spawn(async move {
            let mut conns = Vec::new();
            let mut round = 0u32;
            while !stop.load(Ordering::SeqCst) {
                round += 1;
                common::progress_for(&me, "hostile", &format!("round {round}: connect"));
                // Same node, fresh client id each time, never reads a pane.
                let conn = endpoint.connect(addr.clone(), forgetty_client::proto::ALPN_RPC).await;
                let Ok(conn) = conn else { break };
                common::progress_for(&me, "hostile", &format!("round {round}: open_bi"));
                let Ok((mut send, mut recv)) = conn.open_bi().await else { break };
                let hello = Hello::new(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    ClientKind::Cli,
                    "hostile",
                    "0.0.0",
                    vec!["resume".into(), "snapshot_catchup".into()],
                );
                common::progress_for(&me, "hostile", &format!("round {round}: hello"));
                write_msg(&mut send, &hello).await.unwrap();
                common::progress_for(&me, "hostile", &format!("round {round}: handshake reply"));
                let _ = read_msg::<_, HandshakeReply>(&mut recv).await;
                tokio::spawn(async move {
                    while let Ok(Some(_)) = read_msg::<_, ControlMsg>(&mut recv).await {}
                });
                common::progress_for(&me, "hostile", &format!("round {round}: attaches"));
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

    common::progress_for(&me, "test", "honest connect");
    let honest = connect(&server, "honest").await;
    let mut worst = Duration::ZERO;
    for pane in honest_panes.iter().take(3) {
        common::progress_for(&me, "test", &format!("honest attach {pane}"));
        let t0 = Instant::now();
        let mut s = honest.attach_pane(*pane).await.unwrap();
        wait_for_snapshot(&mut s, Duration::from_secs(20), "the honest node").await;
        let took = t0.elapsed();
        println!("honest node catch-up while a hostile node asks: {took:?}");
        worst = worst.max(took);
    }
    stop.store(true, Ordering::SeqCst);
    common::progress_for(&me, "test", "waiting for the hostile task to stop");
    let _ = hostile.await;
    common::progress_for(&me, "test", "reading qa_state");
    let st = qa_state(&server);
    println!(
        "worst {worst:?}, builds {}, max running {}, last build {} us",
        st["snapshot_builds"], st["snapshot_max_running"], st["last_snapshot_us"]
    );
    assert!(st["snapshot_max_running"].as_u64().unwrap() <= 2);
    // At most two builds (the hostile node's running one, then its own), and
    // some slack for the scheduler.
    let bound = one_build.max(Duration::from_micros(
        st["last_snapshot_us"].as_u64().unwrap_or(0),
    )) * 2
        + Duration::from_millis(100);
    println!("one build {one_build:?}, bound {bound:?}");
    assert!(worst < bound, "the honest node waited {worst:?}, one build is {one_build:?}");
    honest.close().await;
    endpoint.close().await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Writer thread and shell behaviour on stop and on close
// ---------------------------------------------------------------------------

/// A graceful stop must not run what is half typed at a prompt. portable-pty's
/// own writer sends `\n` and EOF to the terminal when it is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_graceful_stop_does_not_run_half_typed_input() {
    if !real_only() {
        return;
    }
    let ran = std::env::temp_dir().join(format!("forgetty-ran-{}", Uuid::new_v4()));
    let script =
        format!("stty -echo; while IFS= read -r l; do echo \"RAN:$l\" >> {}; done", ran.display());
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane(&script);
    let c = connect(&server, "typist").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // A full line runs, as the control: the pane does read lines.
    c.send_input(pane, b"echo CONTROL\n").await.unwrap();
    wait_for("the control line", || {
        std::fs::read_to_string(&ran).is_ok_and(|t| t.contains("RAN:echo CONTROL"))
    })
    .await;
    // Half a line, no Enter.
    c.send_input(pane, b"echo SHOULD_NOT_RUN").await.unwrap();
    wait_for("the typing to reach the PTY", || {
        contains(&pty_input(&server, pane), b"SHOULD_NOT_RUN")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.close().await;
    let port = server.udp_addr().port();
    let saved = server.stop_for_restart().await;
    let after_stop = std::fs::read_to_string(&ran).unwrap_or_default();
    assert!(!after_stop.contains("SHOULD_NOT_RUN"), "the stop ran half-typed input: {after_stop}");
    let again = TestServer::start_again(saved, port).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let after_restart = std::fs::read_to_string(&ran).unwrap_or_default();
    assert!(!after_restart.contains("SHOULD_NOT_RUN"), "ran after the restart: {after_restart}");
    again.shutdown().await;
    let _ = std::fs::remove_file(&ran);
}

/// Linux cuts a thread name to 15 characters.
fn proc_threads_named(pid: i32, name: &str) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .flatten()
        .filter(|t| std::fs::read_to_string(t.path().join("comm")).is_ok_and(|c| c.trim() == name))
        .count()
}

fn proc_fds(pid: i32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd")).unwrap().count()
}

/// Closing a pane frees its writer thread and PTY fd although the program
/// never read its input and another process still holds the slave open (N8).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_a_pane_frees_its_writer_thread_and_fd_with_the_slave_held() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pid = server.pid().expect("real server pid");
    let c = connect(&server, "typist").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let threads0 = proc_threads_named(pid, "session-pty-wri");
    let fds0 = proc_fds(pid);
    // A background `sleep` keeps the slave open after the pane's own program
    // is gone, and nothing reads it.
    let pane = server.add_script_pane("stty raw -echo; sleep 12 & exec sleep infinity");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let chunk = vec![b'x'; 4096];
    for _ in 0..256 {
        c.send_input(pane, &chunk).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(proc_threads_named(pid, "session-pty-wri"), threads0 + 1);
    let fds_open = proc_fds(pid);
    server.close_pane(pane);
    let t0 = Instant::now();
    loop {
        let (threads, fds) = (proc_threads_named(pid, "session-pty-wri"), proc_fds(pid));
        if threads == threads0 && fds <= fds0 {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "writer threads {threads} (was {threads0}), fds {fds} (was {fds0}, open {fds_open})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    c.close().await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Interrupts, holes and ISIG (AD-024)
// ---------------------------------------------------------------------------

/// A raw peer with `input_flags` that keeps the events it is sent.
async fn flag_peer(
    server: &TestServer,
) -> (
    iroh::Endpoint,
    iroh::endpoint::Connection,
    iroh::endpoint::SendStream,
    Arc<std::sync::Mutex<Vec<forgetty_client::proto::ServerEvent>>>,
) {
    let endpoint = raw_endpoint(server).await;
    let conn = endpoint.connect(server.addr(), ALPN_RPC).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = Hello::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        ClientKind::Cli,
        "flags",
        "0.0.0",
        vec!["input_flags".into(), "size_policy_latest".into()],
    );
    write_msg(&mut send, &hello).await.unwrap();
    match read_msg::<_, HandshakeReply>(&mut recv).await.unwrap() {
        Some(HandshakeReply::Welcome(_)) => {}
        other => panic!("handshake: {other:?}"),
    }
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let e2 = events.clone();
    tokio::spawn(async move {
        while let Ok(Some(m)) = read_msg::<_, ControlMsg>(&mut recv).await {
            if let ControlMsg::Event { event, .. } = m {
                e2.lock().unwrap().push(event);
            }
        }
    });
    (endpoint, conn, send, events)
}

/// Pane script: `sleep` does not read, then a shell loop appends every line
/// it reads to `file`. `trap :` lets Ctrl+C end the sleep and not the script.
fn hung_then_reading(file: &std::path::Path, hang: &str) -> String {
    format!(
        "trap : INT; stty -echo; {hang}; while IFS= read -r l; do echo \"RAN:$l\" >> {}; done",
        file.display()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupt_drops_the_stale_input_behind_it_and_reports_the_hole() {
    use forgetty_client::proto::{write_input_frame_flags, ServerEvent};
    if !real_only() {
        return;
    }
    let file = std::env::temp_dir().join(format!("forgetty-stale-{}", Uuid::new_v4()));
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane(&hung_then_reading(&file, "sleep 60"));
    let (_e, conn, mut ctl, events) = flag_peer(&server).await;
    attach_raw(&mut ctl, 1, pane).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut input = conn.open_uni().await.unwrap();
    let mut seq = 0;
    // 1 MiB of command lines for a program that does not read: they stall in
    // the tty, fill the pane queue, and the rest is dropped.
    let line = b"echo STALE\n".repeat(700);
    for _ in 0..140 {
        seq += 1;
        write_input_frame_flags(&mut input, pane, seq, 0, &line).await.unwrap();
    }
    wait_for("frames to be dropped", || qa_state(&server)["input_drops"].as_u64().unwrap() > 0)
        .await;
    // A frame behind the hole is dropped too, however small.
    seq += 1;
    write_input_frame_flags(&mut input, pane, seq, 0, b"echo HOLE\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!contains(&pty_input(&server, pane), b"HOLE"), "a frame behind the hole was queued");
    assert!(events.lock().unwrap().is_empty(), "reported while the pane still does not read");

    // Ctrl+C: ends the sleep, throws away what was queued behind it.
    seq += 1;
    write_input_frame_flags(&mut input, pane, seq, 0, &[0x03]).await.unwrap();
    wait_for("the loss to be reported", || {
        events.lock().unwrap().iter().any(
            |e| matches!(e, ServerEvent::InputDropped { pane_id, bytes } if *pane_id == pane && *bytes > 0),
        )
    })
    .await;
    // The shell loop now reads. It must see the fresh line and no stale one.
    seq += 1;
    write_input_frame_flags(&mut input, pane, seq, 0, b"echo FRESH\n").await.unwrap();
    wait_for("the fresh line", || {
        std::fs::read_to_string(&file).is_ok_and(|t| t.contains("RAN:echo FRESH"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let ran = std::fs::read_to_string(&file).unwrap_or_default();
    assert!(
        !ran.contains("STALE"),
        "a stale paste ran after the interrupt: {} lines",
        ran.lines().count()
    );
    assert!(!ran.contains("HOLE"), "input behind the hole ran");
    drop(conn);
    server.shutdown().await;
    let _ = std::fs::remove_file(&file);
}

/// Ctrl+C to a signal forwarder (ssh, mosh) that is busy: the server does not
/// kill a forwarder, it relies on the interrupt byte going into the tty. The
/// byte must go in even when the interrupt had to skip the queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_to_a_busy_forwarder_still_reaches_it_through_the_tty() {
    use forgetty_client::proto::write_input_frame_flags;
    use std::os::unix::fs::PermissionsExt;
    if !real_only() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("forgetty-fwd-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("got");
    // The script is named `ssh`: the foreground group's leader has that name
    // in /proc/<pid>/comm, so `send_sigint` leaves it alone. It never reads.
    let prog = dir.join("ssh");
    std::fs::write(
        &prog,
        format!(
            "#!/bin/sh\ntrap 'echo GOT_INT >> {}' INT\nstty -echo\nwhile :; do sleep 0.1; done\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane(&format!("exec {}", prog.display()));
    let (_e, conn, mut ctl, _events) = flag_peer(&server).await;
    attach_raw(&mut ctl, 1, pane).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut input = conn.open_uni().await.unwrap();
    let mut seq = 0;
    let line = b"echo STALE\n".repeat(700);
    for _ in 0..140 {
        seq += 1;
        write_input_frame_flags(&mut input, pane, seq, 0, &line).await.unwrap();
    }
    wait_for("frames to be dropped", || qa_state(&server)["input_drops"].as_u64().unwrap() > 0)
        .await;
    assert!(!log.exists(), "the program was interrupted before Ctrl+C");
    seq += 1;
    write_input_frame_flags(&mut input, pane, seq, 0, &[0x03]).await.unwrap();
    wait_for("the forwarder to get Ctrl+C", || {
        std::fs::read_to_string(&log).is_ok_and(|t| t.contains("GOT_INT"))
    })
    .await;
    drop(conn);
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// With ISIG off the program gets Ctrl+C as plain input: nothing signals it,
/// not even when its queue is full.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_is_data_when_isig_is_off() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("stty raw -echo; exec sleep infinity");
    let c = connect(&server, "typist").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let chunk = vec![b'x'; 4096];
    for _ in 0..256 {
        c.send_input(pane, &chunk).await.unwrap();
    }
    wait_for("the queue to fill", || qa_state(&server)["input_drops"].as_u64().unwrap() > 0).await;
    c.send_input(pane, &[0x03]).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let info = server.socket_call("get_pane_info", json!({"pane_id": pane.to_string()})).unwrap();
    assert!(info["exit_code"].is_null(), "Ctrl+C signalled a pane with ISIG off: {info}");
    // The local `send_sigint` does not signal either: the program is in raw
    // mode and gets the key.
    server.socket_call("send_sigint", json!({"pane_id": pane.to_string()})).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let info = server.socket_call("get_pane_info", json!({"pane_id": pane.to_string()})).unwrap();
    assert!(info["exit_code"].is_null(), "send_sigint killed a raw-mode program: {info}");
    c.close().await;
    server.shutdown().await;
}

/// A raw-mode program (`cfmakeraw`, as ssh, mosh, emacs and ink TUIs do) that
/// prints every byte it reads and counts its SIGINTs. `$1` sets VINTR: `d`
/// leaves the default, a number sets `c_cc[VINTR]`, `off` disables it.
const RAW_PROBE: &str = r#"
d=$(mktemp -d)
cat > "$d/r.c" <<'EOF'
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>
static volatile sig_atomic_t n;
static void h(int s) { (void)s; n++; }
int main(int argc, char **argv) {
    struct termios t;
    tcgetattr(0, &t);
    if (argc > 1 && !strcmp(argv[1], "cooked")) {
        /* ISIG stays on, VINTR is disabled: no byte can signal. */
        t.c_lflag &= ~(ICANON | ECHO);
        t.c_lflag |= ISIG;
        t.c_cc[VMIN] = 1;
        t.c_cc[VTIME] = 0;
        t.c_cc[VINTR] = 0;
    } else cfmakeraw(&t);
    if (argc > 1 && !strcmp(argv[1], "off")) t.c_cc[VINTR] = 0;
    else if (argc > 1 && strcmp(argv[1], "d") && strcmp(argv[1], "cooked")) t.c_cc[VINTR] = (cc_t)atoi(argv[1]);
    tcsetattr(0, TCSANOW, &t);
    struct sigaction a = {0};
    a.sa_handler = h;
    sigaction(SIGINT, &a, 0);
    setvbuf(stdout, 0, _IONBF, 0);
    puts("READY\r");
    int last = 0;
    for (;;) {
        unsigned char b;
        ssize_t r = read(0, &b, 1);
        if (n != last) { last = n; printf("SIGINT=%d\r\n", last); }
        if (r == 1) printf("BYTE=%d\r\n", b);
    }
}
EOF
cc -o "$d/r" "$d/r.c" && exec "$d/r" "#;

fn count(hay: &[u8], needle: &str) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle.as_bytes()).count()
}

/// With ISIG off the program reads Ctrl+C as a key and nothing signals it. The
/// key is 0x03 whatever VINTR is: a remapped VINTR is written only for a
/// forwarder. With ISIG on and VINTR disabled the key is 0x03 as well, plain
/// input, and nothing signals either. Every way to send Ctrl+C gets the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_in_raw_mode_is_the_interrupt_key_and_never_a_signal() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    for (arg, key) in [("d", 3u8), ("24", 3), ("off", 3), ("cooked", 3)] {
        let pane = server.add_script_pane(&format!("{RAW_PROBE}{arg}\n"));
        let logged = |needle: &str| contains(&server.log(pane), needle.as_bytes());
        wait_for("the program to start", || logged("READY")).await;
        let want = format!("BYTE={key}\r");
        let seen = || count(&server.log(pane), &want);
        let id = pane.to_string();

        server.socket_call("send_sigint", json!({"pane_id": id})).unwrap();
        wait_for("the key from send_sigint", || seen() == 1).await;
        server.cli(&["send-key", "-p", &id, "ctrl+c"]).unwrap();
        wait_for("the key from send-key", || seen() == 2).await;

        // A typed Ctrl+C (rpc v2) is plain input here as well.
        let c = connect(&server, "typist").await;
        c.send_input(pane, &[key]).await.unwrap();
        wait_for("the key from a typed Ctrl+C", || seen() == 3).await;
        c.close().await;

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!logged("SIGINT="), "arg {arg}: a raw-mode program was signalled");
        assert_eq!(seen(), 3, "arg {arg}: one key per Ctrl+C");
    }
    server.shutdown().await;
}

/// A signal forwarder (ssh, mosh-client) in raw mode gets 0x03 as input and
/// no signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raw_mode_forwarder_gets_the_interrupt_key() {
    use std::os::unix::fs::PermissionsExt;
    if !real_only() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("forgetty-rawfwd-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("got");
    let prog = dir.join("ssh");
    // Raw mode, one byte at a time, like ssh with a pty on the far side.
    std::fs::write(
        &prog,
        format!(
            "#!/bin/sh\ntrap 'echo GOT_INT >> {l}' INT\nstty raw -echo\n\
             while :; do b=$(dd bs=1 count=1 2>/dev/null | od -An -tu1); echo \"GOT:$b\" >> {l}; done\n",
            l = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane(&format!("exec {}", prog.display()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    server.socket_call("send_sigint", json!({"pane_id": pane.to_string()})).unwrap();
    wait_for("the forwarder to read 0x03", || {
        std::fs::read_to_string(&log).is_ok_and(|t| {
            t.lines().any(|l| l.strip_prefix("GOT:").is_some_and(|b| b.trim() == "3"))
        })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(!got.contains("GOT_INT"), "the forwarder was signalled: {got}");
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// The pane stream header says where replay ends (section 8).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stream_header_names_the_end_of_the_replay() {
    use forgetty_client::proto::PaneStreamHeader;
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("printf 'history'; exec cat");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (first, total) = server.log_bounds(pane);
    assert!(total >= 7, "the pane should have printed: {total}");
    let (_e, conn, mut ctl) = raw_peer(&server, &["resume"], Uuid::new_v4()).await;
    attach_raw(&mut ctl, 1, pane).await;
    let mut s = conn.accept_uni().await.unwrap();
    let h: PaneStreamHeader = read_msg(&mut s).await.unwrap().unwrap();
    assert_eq!(h.start_offset, first);
    assert_eq!(h.replay_end, Some(total), "history is replay, what comes after is live");
    drop(conn);
    server.shutdown().await;
}

/// Input never crosses a restart: a stop saves the dedup table and no list of
/// frames to take again, even with frames left in a hung pane's queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_carries_no_unwritten_frames_over() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("stty -echo; exec sleep infinity");
    let c = connect(&server, "typist").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let chunk = b"xxxxxxxxx\n".repeat(400);
    for _ in 0..40 {
        c.send_input(pane, &chunk).await.unwrap();
    }
    wait_for("input to reach the queue", || {
        qa_state(&server)["input_drops"].as_u64().unwrap() > 0
            || pty_input(&server, pane).len() > 100_000
    })
    .await;
    c.close().await;
    let dir = server.state_dir().unwrap();
    let saved = server.stop_for_restart().await;
    // The state dir layout is the server's: look for the saved table.
    let found =
        walk(&dir).into_iter().find(|p| p.file_name().is_some_and(|n| n == "input_seq.json"));
    let _ = saved;
    let text = std::fs::read_to_string(found.expect("input_seq.json was saved")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    let entries = doc["entries"].as_array().unwrap();
    assert!(!entries.is_empty(), "the dedup table was saved: {text}");
    assert!(
        entries.iter().all(|e| e.get("unwritten").is_none()),
        "frames to take again after the restart were saved: {text}"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// `capture` right after the ring is full answers with the screen, on the
/// same connection (B). A 20 MB burst once ended in "server closed the
/// connection" with nothing in the log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_right_after_a_full_ring_answers() {
    if !real_only() {
        return;
    }
    let _heavy = HEAVY.lock().await;
    let server = TestServer::start(TestServerConfig::default()).await;
    // More than the 8 MiB ring, ending with a marker on the screen.
    let pane = server.add_script_pane(
        "head -c 12000000 /dev/zero | tr '\\0' 'a' | fold -w 100; echo; echo DONE_MARKER; exec cat",
    );
    wait_for("the burst to finish", || {
        let (first, total) = server.log_bounds(pane);
        total > 9_000_000 && total.saturating_sub(first) >= 8 * 1024 * 1024
    })
    .await;
    // The marker comes last. Captures taken while the burst still runs must
    // answer too, so poll until it shows.
    wait_for("the marker on the screen", || {
        server
            .socket_call("capture", json!({"pane_id": pane.to_string(), "last_lines": 5}))
            .is_ok_and(|r| r["text"].as_str().is_some_and(|t| t.contains("DONE_MARKER")))
    })
    .await;
    // Several in a row on one connection, none may close it.
    for i in 0..3 {
        let r = server
            .socket_call("capture", json!({"pane_id": pane.to_string(), "trim": true}))
            .unwrap_or_else(|e| panic!("capture {i} failed: {e}"));
        let text = r["text"].as_str().expect("text");
        assert!(text.contains("DONE_MARKER"), "capture {i}: {text:?}");
        assert_eq!(r["incomplete"], json!(false), "capture {i} ran out of time");
    }
    server.shutdown().await;
}

/// Capture of a ring of many short lines, from several callers at once while
/// a snapshot catch-up builds. The server must survive and answer every call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_captures_of_a_short_line_ring_survive() {
    if !real_only() {
        return;
    }
    let _heavy = HEAVY.lock().await;
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("seq 1 1300000; echo DONE_MARKER; exec cat");
    wait_for("the burst to finish", || {
        let (first, total) = server.log_bounds(pane);
        total > 8_700_000 && total.saturating_sub(first) >= 8 * 1024 * 1024
    })
    .await;
    wait_for("the marker on the screen", || {
        server
            .socket_call("capture", json!({"pane_id": pane.to_string(), "last_lines": 5}))
            .is_ok_and(|r| r["text"].as_str().is_some_and(|t| t.contains("DONE_MARKER")))
    })
    .await;
    let server = std::sync::Arc::new(server);
    let mut joins = Vec::new();
    for i in 0..4 {
        let server = server.clone();
        joins.push(tokio::task::spawn_blocking(move || {
            for j in 0..2 {
                let r = server
                    .socket_call(
                        "capture",
                        json!({"pane_id": pane.to_string(), "trim": true, "last_lines": 5}),
                    )
                    .unwrap_or_else(|e| panic!("capture {i}.{j} failed: {e}"));
                let text = r["text"].as_str().expect("text");
                assert!(text.contains("DONE_MARKER"), "capture {i}.{j}: {text:?}");
            }
        }));
    }
    for j in joins {
        j.await.unwrap();
    }
    // The server still answers.
    server.socket_call("list_tabs", json!({})).expect("server alive");
    std::sync::Arc::into_inner(server).expect("sole owner").shutdown().await;
}

// ---------------------------------------------------------------------------
// AD-020: the size owner is visible to clients
// ---------------------------------------------------------------------------

/// A local socket client that stays attached: a `subscribe_layout` connection
/// with its own id and name. Dropping it closes the connection.
struct LocalClient {
    _stream: std::os::unix::net::UnixStream,
    id: Uuid,
    name: String,
    /// The `subscribe_output` connections, by pane.
    outputs: std::collections::HashMap<Uuid, std::os::unix::net::UnixStream>,
}

impl LocalClient {
    fn attach(server: &TestServer, name: &str) -> LocalClient {
        use std::io::{BufRead, BufReader, Write};
        let id = Uuid::new_v4();
        let mut stream = std::os::unix::net::UnixStream::connect(server.socket_path()).unwrap();
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "subscribe_layout",
            "params": {"client_id": id.to_string(), "client_name": name}});
        writeln!(stream, "{req}").unwrap();
        let mut ack = String::new();
        BufReader::new(stream.try_clone().unwrap()).read_line(&mut ack).unwrap();
        assert!(ack.contains("\"ok\""), "subscribe_layout: {ack}");
        LocalClient { _stream: stream, id, name: name.into(), outputs: Default::default() }
    }

    /// Start a `subscribe_output` on `pane`: the client has a stream there.
    fn watch(&mut self, server: &TestServer, pane: Uuid) {
        use std::io::{BufRead, BufReader, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(server.socket_path()).unwrap();
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "subscribe_output",
            "params": {"pane_id": pane.to_string(), "client_id": self.id.to_string(),
                       "client_name": self.name}});
        writeln!(stream, "{req}").unwrap();
        let mut ack = String::new();
        BufReader::new(stream.try_clone().unwrap()).read_line(&mut ack).unwrap();
        assert!(ack.contains("\"ok\""), "subscribe_output: {ack}");
        self.outputs.insert(pane, stream);
    }

    /// End the `subscribe_output` on `pane`.
    fn unwatch(&mut self, pane: Uuid) {
        self.outputs.remove(&pane);
    }

    fn resize(&self, server: &TestServer, pane: Uuid, rows: u16, cols: u16) {
        server
            .socket_call(
                "resize_pane",
                json!({"pane_id": pane.to_string(), "rows": rows, "cols": cols,
                       "client_id": self.id.to_string()}),
            )
            .expect("resize_pane with client_id");
    }
}

fn owner_of(server: &TestServer, pane: Uuid) -> serde_json::Value {
    server.socket_call("get_pane_info", json!({"pane_id": pane.to_string()})).unwrap()["size_owner"]
        .clone()
}

/// A remote client, `Android` named `phone`, with the events of the pane
/// collected as short labels, in arrival order.
async fn phone(
    server: &TestServer,
    pane: Uuid,
) -> (ClientHandle, Uuid, Arc<std::sync::Mutex<Vec<String>>>) {
    use forgetty_client::proto::ServerEvent;
    use forgetty_client::EventItem;
    let k = SecretKey::generate(&mut rand::rng());
    server.authorize(k.public());
    let id = Uuid::new_v4();
    let info = ClientInfo::new(id, ClientKind::Android, "phone", "0.0.0");
    let h = tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), k, info))
        .await
        .expect("connect timed out")
        .expect("connect failed");
    assert!(h.has_capability("size_owner"));
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut events = h.events().await.unwrap();
    let sink = log.clone();
    tokio::spawn(async move {
        while let Some(item) = events.next().await {
            let EventItem::Server { event, .. } = item else { continue };
            let label = match event {
                ServerEvent::SizeOwnerChanged { pane_id, owner } if pane_id == pane => format!(
                    "owner:{}",
                    owner.map_or("none".to_string(), |o| format!(
                        "{}:{}",
                        o.client_kind.as_str(),
                        o.client_name
                    ))
                ),
                ServerEvent::PaneResized { pane_id, rows, cols, size_owner } if pane_id == pane => {
                    format!(
                        "resized:{rows}x{cols}:{}",
                        size_owner.map_or("none", |o| o.client_kind.as_str())
                    )
                }
                _ => continue,
            };
            sink.lock().unwrap().push(label);
        }
    });
    (h, id, log)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_socket_client_owns_a_pane_and_is_listed() {
    use forgetty_client::proto::{from_value, ClientEntry};
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("exec cat");
    assert!(owner_of(&server, pane).is_null(), "a pane nobody attached has no owner");

    let mut gtk = LocalClient::attach(&server, "gtk-test");
    // No stream on the pane yet: the size applies, the client owns nothing.
    gtk.resize(&server, pane, 20, 80);
    assert_eq!(pane_dims(&server, pane), (20, 80));
    assert!(owner_of(&server, pane).is_null(), "a client with no stream owns the pane");
    gtk.watch(&server, pane);
    wait_for("the stream's owner", || !owner_of(&server, pane).is_null()).await;
    gtk.resize(&server, pane, 30, 100);
    assert_eq!(pane_dims(&server, pane), (30, 100));
    let owner = owner_of(&server, pane);
    assert_eq!(owner["client_id"], gtk.id.to_string());
    assert_eq!(owner["client_kind"], "local");
    assert_eq!(owner["client_name"], "gtk-test");

    // A client_id that is not an attached streaming client is refused.
    let err = server.socket_call(
        "resize_pane",
        json!({"pane_id": pane.to_string(), "rows": 10, "cols": 10,
               "client_id": Uuid::new_v4().to_string()}),
    );
    assert!(err.is_err(), "an unknown client_id was accepted");
    assert_eq!(pane_dims(&server, pane), (30, 100));

    // A one-shot resize (the CLI) applies the size and takes no ownership.
    server
        .socket_call("resize_pane", json!({"pane_id": pane.to_string(), "rows": 25, "cols": 90}))
        .unwrap();
    assert_eq!(pane_dims(&server, pane), (25, 90));
    assert_eq!(owner_of(&server, pane)["client_id"], gtk.id.to_string());

    // `list_clients` over the v2 protocol shows the local client once.
    let (h, _, _) = phone(&server, pane).await;
    let list: Vec<ClientEntry> =
        from_value(h.request("list_clients", Value::Nil).await.unwrap()).unwrap();
    let local: Vec<_> = list.iter().filter(|c| c.client_kind == ClientKind::Local).collect();
    assert_eq!(local.len(), 1, "list_clients: {list:?}");
    assert_eq!((local[0].client_id, local[0].client_name.as_str()), (gtk.id, "gtk-test"));
    assert!(list.iter().any(|c| c.client_kind == ClientKind::Android));

    // Its stream closes: the pane has no owner again, though the client is
    // still connected.
    gtk.unwatch(pane);
    wait_for("the owner to clear", || owner_of(&server, pane).is_null()).await;
    drop(gtk);
    h.close().await;
    server.shutdown().await;
}

/// Remote owner, local takeover, handover on detach, and the order of the
/// events: `SizeOwnerChanged` always comes before the `PaneResized` it causes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_size_owner_is_announced_before_its_size_and_handed_over() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("exec cat");
    let (h, id, log) = phone(&server, pane).await;
    let seen = |n: usize| log.lock().unwrap().len() >= n;

    // The phone attaches first: it owns the pane. Its resize follows.
    let _stream = h.attach_pane(pane).await.unwrap();
    wait_for("phone as owner", || seen(1)).await;
    h.resize(pane, 30, 100).await.unwrap();
    wait_for("phone's size", || seen(2)).await;
    let owner = owner_of(&server, pane);
    assert_eq!(owner["client_id"], id.to_string());
    assert_eq!(owner["client_kind"], "android");
    assert_eq!(owner["client_name"], "phone");

    // A local client resizes: it takes the pane.
    let mut gtk = LocalClient::attach(&server, "gtk-test");
    gtk.watch(&server, pane);
    gtk.resize(&server, pane, 40, 120);
    wait_for("local takeover", || seen(4)).await;
    assert_eq!(owner_of(&server, pane)["client_kind"], "local");

    // The local client's stream on the pane closes (it stays connected): the
    // phone, still attached, gets the pane and its own size back.
    gtk.unwatch(pane);
    wait_for("handover to the phone", || seen(6)).await;
    assert_eq!(pane_dims(&server, pane), (30, 100));
    assert_eq!(owner_of(&server, pane)["client_kind"], "android");
    drop(gtk);

    // The phone detaches: nobody owns the pane.
    h.detach_pane(pane).await.unwrap();
    wait_for("no owner", || seen(7)).await;
    assert!(owner_of(&server, pane).is_null());

    assert_eq!(
        *log.lock().unwrap(),
        [
            "owner:android:phone",
            "resized:30x100:android",
            "owner:local:gtk-test",
            "resized:40x120:local",
            "owner:android:phone",
            "resized:30x100:android",
            "owner:none",
        ]
    );
    h.close().await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// One Ctrl+C is one SIGINT
// ---------------------------------------------------------------------------

/// A cooked-mode program (ISIG on) that counts its SIGINTs and prints the
/// count. It never reads its input, so a paste leaves the pane's queue busy
/// and the Ctrl+C takes the interrupt path.
const SIGINT_COUNTER: &str = r#"
d=$(mktemp -d)
cat > "$d/c.c" <<'EOF'
#include <signal.h>
#include <stdio.h>
#include <unistd.h>
static volatile sig_atomic_t n;
static void h(int s) { (void)s; n++; }
int main(void) {
    struct sigaction a = {0};
    a.sa_handler = h;
    sigaction(SIGINT, &a, 0);
    int last = 0;
    setvbuf(stdout, 0, _IONBF, 0);
    puts("READY");
    for (;;) {
        if (n != last) { last = n; printf("SIGINT=%d\n", last); }
        usleep(1000);
    }
}
EOF
cc -o "$d/c" "$d/c.c" && exec "$d/c"
"#;

/// terraform and docker compose skip their graceful stop on a second SIGINT.
/// A typed Ctrl+C on a busy cooked-mode pane must signal once: the interrupt
/// byte goes through the tty, and the group is not signalled as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_ctrl_c_on_a_busy_cooked_pane_is_one_sigint() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane(SIGINT_COUNTER);
    let c = connect(&server, "typist").await;
    let logged = |needle: &str| contains(&server.log(pane), needle.as_bytes());
    wait_for("the program to start", || logged("READY")).await;

    let paste: Vec<u8> = b"0123456789abcdef\n".iter().copied().cycle().take(40_000).collect();
    for round in 1..=4 {
        c.send_input(pane, &paste).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        c.send_input(pane, &[0x03]).await.unwrap();
        // A double signal can show as `SIGINT={round+1}` at once.
        wait_for("the SIGINT", || {
            logged(&format!("SIGINT={round}\r\n")) || logged(&format!("SIGINT={}", round + 1))
        })
        .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !logged(&format!("SIGINT={}", round + 1)),
            "round {round}: one Ctrl+C signalled the program twice"
        );
    }
    c.close().await;
    server.shutdown().await;
}

/// A forwarder in raw mode whose VINTR was remapped gets the remapped key: the
/// far side's terminal uses the setting ssh forwarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raw_mode_forwarder_with_a_remapped_vintr_gets_that_key() {
    use std::os::unix::fs::PermissionsExt;
    if !real_only() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("forgetty-rawfwd2-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("got");
    let prog = dir.join("ssh");
    std::fs::write(
        &prog,
        format!(
            "#!/bin/sh\nstty intr '^X' raw -echo\n\
             while :; do b=$(dd bs=1 count=1 2>/dev/null | od -An -tu1); echo \"GOT:$b\" >> {l}; done\n",
            l = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane(&format!("exec {}", prog.display()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    server.socket_call("send_sigint", json!({"pane_id": pane.to_string()})).unwrap();
    wait_for("the forwarder to read ^X", || {
        std::fs::read_to_string(&log).is_ok_and(|t| {
            t.lines().any(|l| l.strip_prefix("GOT:").is_some_and(|b| b.trim() == "24"))
        })
    })
    .await;
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Input typed just before `close()` reaches the program: the client waits
/// (up to 300 ms) for the server to ack it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_sent_just_before_close_is_not_lost() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("stty raw -echo; exec cat");
    tokio::time::sleep(Duration::from_millis(500)).await;
    for i in 0..15 {
        let c = connect(&server, "closer").await;
        let marker = format!("<m{i:02}>");
        c.send_input(pane, marker.as_bytes()).await.unwrap();
        c.close().await;
        wait_for(&format!("marker {i}"), || contains(&pty_input(&server, pane), marker.as_bytes()))
            .await;
    }
    server.shutdown().await;
}

/// The local socket `send_sigint` and `forgetty send-key ctrl+c` signal a
/// cooked-mode program once each, as a typed Ctrl+C does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_local_interrupts_are_one_sigint_each_in_cooked_mode() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane(SIGINT_COUNTER);
    let logged = |needle: &str| contains(&server.log(pane), needle.as_bytes());
    wait_for("the program to start", || logged("READY")).await;
    let id = pane.to_string();

    server.socket_call("send_sigint", json!({"pane_id": id})).unwrap();
    wait_for("the first SIGINT", || logged("SIGINT=1")).await;
    server.cli(&["send-key", "-p", &id, "ctrl+c"]).unwrap();
    wait_for("the second SIGINT", || logged("SIGINT=2")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!logged("SIGINT=3"), "one interrupt signalled the program twice");
    server.shutdown().await;
}

/// `local` names the server's own socket clients: a remote `Hello` with it is
/// refused. `unknown` is a kind the decoder does not know: it is accepted and
/// listed as it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_hello_cannot_claim_to_be_local_but_an_unknown_kind_connects() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let endpoint = raw_endpoint(&server).await;
    let conn = endpoint.connect(server.addr(), ALPN_RPC).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello =
        Hello::new(Uuid::new_v4(), Uuid::new_v4(), ClientKind::Local, "liar", "0.0.0", Vec::new());
    write_msg(&mut send, &hello).await.unwrap();
    match read_msg::<_, HandshakeReply>(&mut recv).await.unwrap() {
        Some(HandshakeReply::Error(e)) => {
            assert_eq!(e.code, "bad_request");
            assert!(e.message.contains("bad_hello"), "{}", e.message);
        }
        other => panic!("local: expected a refusal, got {other:?}"),
    }
    endpoint.close().await;

    let endpoint = raw_endpoint(&server).await;
    let conn = endpoint.connect(server.addr(), ALPN_RPC).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = Hello::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        ClientKind::Unknown,
        "newcomer",
        "0.0.0",
        Vec::new(),
    );
    write_msg(&mut send, &hello).await.unwrap();
    match read_msg::<_, HandshakeReply>(&mut recv).await.unwrap() {
        Some(HandshakeReply::Welcome(_)) => {}
        other => panic!("unknown: expected a Welcome, got {other:?}"),
    }
    let listed = server.socket_call("list_clients", json!({})).unwrap();
    let kinds: Vec<_> = listed["clients"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["client_name"] == "newcomer")
        .map(|c| c["client_kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ["unknown"], "list_clients: {listed}");
    endpoint.close().await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Aborted dials leave nothing behind
// ---------------------------------------------------------------------------

/// A dial that is cut off (here by its timeout, while the server does not run)
/// must not leave half-open connections on the server. Its connect attempts
/// are retired, not dropped: they finish their handshake once the server is
/// back and are closed. Dropped half way, the server sees a handshake that the
/// peer abandons, and keeps the connection draining for seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aborted_dial_leaves_no_connection_on_the_server() {
    use forgetty_client::ClientOptions;
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pid = server.pid().expect("real server");
    let k = SecretKey::generate(&mut rand::rng());
    server.authorize(k.public());
    let count = |name: &str| qa_state(&server)[name].as_u64().unwrap();
    let (base_conns, base_quic) = (count("connections"), count("quic_open"));

    // The server does not run: attempts at 0, 400 and 800 ms get no answer,
    // and the 1 s dial timeout cuts the dial off.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let opts = ClientOptions { dial_timeout: Duration::from_secs(1), ..Default::default() };
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "aborted", "0.0.0");
    let h = Client::spawn(server.addr(), k, info, opts).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert!(!h.state().is_connected());
    unsafe { libc::kill(pid, libc::SIGCONT) };
    // The client finds the server again; then it goes away on purpose.
    let t0 = Instant::now();
    while !h.state().is_connected() {
        assert!(t0.elapsed() < Duration::from_secs(5), "never reconnected: {:?}", h.state());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.close().await;

    let t0 = Instant::now();
    loop {
        let (conns, quic) = (count("connections"), count("quic_open"));
        if conns <= base_conns && quic <= base_quic {
            println!("connections back to baseline after {:?}", t0.elapsed());
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(4),
            "after 4 s the server still holds {conns} connections ({base_conns} before) \
             and {quic} QUIC connections ({base_quic} before)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // No handshake was abandoned half way.
    assert_eq!(count("handshake_failures"), 0, "a connect attempt was dropped mid-handshake");
    server.shutdown().await;
}

/// An attach is a request in flight until it is answered, and no longer. A
/// peer that sends 90 attaches of far-behind panes at once (more than the 64
/// in flight allowed) is not closed: the server answers each attach as its
/// stream is set up, and the snapshot waits happen on the streams' own tasks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attaches_of_far_behind_panes_are_answered_before_their_snapshots() {
    if !real_only() {
        return;
    }
    const PANES: u64 = 90;
    // A 1 MiB ring: more than 512 KiB behind is far behind.
    let server = TestServer::start_with(TestServerConfig::default(), Some(4)).await;
    let mut panes = Vec::new();
    for _ in 0..PANES {
        let pane = server.add_script_pane("stty raw -echo; exec cat");
        server.append(pane, &vec![b'z'; 700_000]);
        panes.push(pane);
    }
    let endpoint = raw_endpoint(&server).await;
    let conn = endpoint.connect(server.addr(), ALPN_RPC).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = Hello::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        ClientKind::Cli,
        "raw",
        "0.0.0",
        vec!["resume".to_string(), "snapshot_catchup".to_string()],
    );
    write_msg(&mut send, &hello).await.unwrap();
    match read_msg::<_, HandshakeReply>(&mut recv).await.unwrap() {
        Some(HandshakeReply::Welcome(_)) => {}
        other => panic!("handshake: {other:?}"),
    }
    let answered = Arc::new(AtomicU64::new(0));
    let list_tabs_at = Arc::new(std::sync::Mutex::new(None::<Instant>));
    {
        let (answered, list_tabs_at) = (answered.clone(), list_tabs_at.clone());
        tokio::spawn(async move {
            while let Ok(Some(msg)) = read_msg::<_, ControlMsg>(&mut recv).await {
                match msg {
                    ControlMsg::Response { id, .. } if id == PANES + 1 => {
                        *list_tabs_at.lock().unwrap() = Some(Instant::now());
                    }
                    ControlMsg::Response { .. } => {
                        answered.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        });
    }
    let closes = |s: &TestServer| qa_state(s)["too_many_closes"].as_u64().unwrap();
    let closes_before = closes(&server);
    for (i, pane) in panes.iter().enumerate() {
        attach_raw(&mut send, i as u64 + 1, *pane).await;
    }
    let sent = Instant::now();
    let msg = ControlMsg::Request { id: PANES + 1, method: "list_tabs".into(), params: Value::Map(vec![]) };
    write_msg(&mut send, &msg).await.unwrap();
    // Every answer, however loaded the host: this test is about whether they
    // come, not how fast (the 90 snapshot builds share the host's cores).
    let t0 = Instant::now();
    while !(answered.load(Ordering::SeqCst) == PANES && list_tabs_at.lock().unwrap().is_some()) {
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "{} of {PANES} attaches answered, list_tabs {}",
            answered.load(Ordering::SeqCst),
            list_tabs_at.lock().unwrap().is_some()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let list_tabs = list_tabs_at.lock().unwrap().unwrap().duration_since(sent);
    println!("{PANES} far-behind attaches at once: all answered, list_tabs after {list_tabs:?}");
    assert!(conn.close_reason().is_none(), "the server closed the connection");
    assert_eq!(closes(&server), closes_before, "a connection was closed for requests");
    drop(conn);
    endpoint.close().await;
    server.shutdown().await;
}

/// A connection that drops right after its attach leaves no client on the
/// pane. `note_attached` runs under the lock the close takes, so the attach
/// is either noted before the close forgets the client, or it does not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_that_drops_with_its_attach_leaves_no_client_on_the_pane() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("stty raw -echo; exec cat");
    let endpoint = raw_endpoint(&server).await;
    for round in 0..150u64 {
        let (conn, mut send) = raw_peer_on(&endpoint, &server, &["resume"], Uuid::new_v4()).await;
        attach_raw(&mut send, 1, pane).await;
        // Let the request reach the server, then cut the connection at a
        // different moment each round.
        tokio::time::sleep(Duration::from_micros(round * 40)).await;
        conn.close(0u32.into(), b"gone");
    }
    wait_for("the server to forget every client", || {
        qa_state(&server)["attached_clients"].as_u64().unwrap() == 0
    })
    .await;
    endpoint.close().await;
    server.shutdown().await;
}

/// A `detach_pane` always gets its answer, within a second, however busy the
/// size policy is. Attach and detach churn on one pane while the size policy
/// lock is held for seconds (as a busy pane holds it): the attaches wait for
/// that lock, the detaches must not. Then the link drops in the middle of the
/// churn, a new connection churns again, and no client stays attached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_detach_is_answered_within_a_second_while_attaches_churn_and_the_link_drops() {
    if !real_only() {
        return;
    }
    use std::collections::HashMap;
    use std::sync::Mutex;
    type Times = Arc<Mutex<HashMap<u64, Instant>>>;

    /// A connection that records when each request was sent and answered.
    struct Churner {
        send: iroh::endpoint::SendStream,
        conn: iroh::endpoint::Connection,
        sent: Times,
        answered: Times,
        next_id: u64,
    }
    impl Churner {
        async fn start(endpoint: &iroh::Endpoint, server: &TestServer) -> Churner {
            let conn = endpoint.connect(server.addr(), ALPN_RPC).await.unwrap();
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            let hello = Hello::new(
                Uuid::new_v4(),
                Uuid::new_v4(),
                ClientKind::Cli,
                "churn",
                "0.0.0",
                vec!["resume".to_string(), "snapshot_catchup".to_string()],
            );
            write_msg(&mut send, &hello).await.unwrap();
            match read_msg::<_, HandshakeReply>(&mut recv).await.unwrap() {
                Some(HandshakeReply::Welcome(_)) => {}
                other => panic!("handshake: {other:?}"),
            }
            let answered: Times = Arc::default();
            let seen = answered.clone();
            tokio::spawn(async move {
                while let Ok(Some(msg)) = read_msg::<_, ControlMsg>(&mut recv).await {
                    if let ControlMsg::Response { id, .. } | ControlMsg::Failure { id, .. } = msg {
                        seen.lock().unwrap().entry(id).or_insert_with(Instant::now);
                    }
                }
            });
            Churner { send, conn, sent: Arc::default(), answered, next_id: 1 }
        }

        /// One attach and the detach right behind it. Returns the detach id.
        async fn round(&mut self, pane: Uuid) -> Option<u64> {
            let id = self.next_id;
            self.next_id += 2;
            let attach = ControlMsg::Request {
                id,
                method: "attach_pane".into(),
                params: to_value(&json!({"pane_id": pane.to_string(), "from_offset": null}))
                    .unwrap(),
            };
            let detach = ControlMsg::Request {
                id: id + 1,
                method: "detach_pane".into(),
                params: to_value(&json!({"pane_id": pane.to_string()})).unwrap(),
            };
            self.sent.lock().unwrap().insert(id + 1, Instant::now());
            write_msg(&mut self.send, &attach).await.ok()?;
            write_msg(&mut self.send, &detach).await.ok()?;
            Some(id + 1)
        }

        /// The slowest answer among `ids`, and the ids with none.
        fn latency(&self, ids: &[u64]) -> (Duration, Vec<u64>) {
            let (sent, answered) = (self.sent.lock().unwrap(), self.answered.lock().unwrap());
            let mut worst = Duration::ZERO;
            let mut missing = Vec::new();
            for id in ids {
                match answered.get(id) {
                    Some(a) => worst = worst.max(a.duration_since(sent[id])),
                    None => missing.push(*id),
                }
            }
            (worst, missing)
        }
    }

    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("stty raw -echo; exec cat");
    let endpoint = raw_endpoint(&server).await;
    let mut a = Churner::start(&endpoint, &server).await;
    let mut b = Churner::start(&endpoint, &server).await;

    // The size policy is busy for 2.5 s.
    server.socket_call("qa_hold_sizes", json!({"ms": 2500})).unwrap();
    let mut a_ids = Vec::new();
    for _ in 0..30 {
        a_ids.extend(a.round(pane).await);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let (worst, missing) = a.latency(&a_ids);
    println!("busy size policy: {} detaches, slowest {worst:?}, unanswered {}", a_ids.len(), missing.len());
    assert!(missing.is_empty(), "detaches without an answer after 1.2 s: {missing:?}");
    assert!(worst < Duration::from_secs(1), "a detach took {worst:?}");

    // The link drops in the middle of a churn on the second connection.
    let dropper = {
        let server_ref = &server;
        async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            server_ref.drop_connections();
        }
    };
    let churn_b = async {
        for _ in 0..40 {
            if b.round(pane).await.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::join!(dropper, churn_b);

    // A new connection churns again, after the lock was let go.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut c = Churner::start(&endpoint, &server).await;
    let mut c_ids = Vec::new();
    for _ in 0..30 {
        c_ids.extend(c.round(pane).await);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (worst, missing) = c.latency(&c_ids);
    println!("after the drop: {} detaches, slowest {worst:?}, unanswered {}", c_ids.len(), missing.len());
    assert!(missing.is_empty(), "detaches without an answer: {missing:?}");
    assert!(worst < Duration::from_secs(1), "a detach took {worst:?}");

    c.conn.close(0u32.into(), b"done");
    a.conn.close(0u32.into(), b"done");
    wait_for("the size policy to forget every client", || {
        qa_state(&server)["attached_clients"].as_u64().unwrap() == 0
    })
    .await;
    endpoint.close().await;
    server.shutdown().await;
}

/// A raw peer that records the ids of the answers it gets.
async fn raw_control(
    endpoint: &iroh::Endpoint,
    server: &TestServer,
) -> (
    iroh::endpoint::Connection,
    iroh::endpoint::SendStream,
    Arc<std::sync::Mutex<std::collections::HashSet<u64>>>,
) {
    let conn = endpoint.connect(server.addr(), ALPN_RPC).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = Hello::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        ClientKind::Cli,
        "raw",
        "0.0.0",
        vec!["resume".to_string()],
    );
    write_msg(&mut send, &hello).await.unwrap();
    match read_msg::<_, HandshakeReply>(&mut recv).await.unwrap() {
        Some(HandshakeReply::Welcome(_)) => {}
        other => panic!("handshake: {other:?}"),
    }
    let answered: Arc<std::sync::Mutex<std::collections::HashSet<u64>>> = Arc::default();
    let seen = answered.clone();
    tokio::spawn(async move {
        while let Ok(Some(msg)) = read_msg::<_, ControlMsg>(&mut recv).await {
            if let ControlMsg::Response { id, .. } | ControlMsg::Failure { id, .. } = msg {
                seen.lock().unwrap().insert(id);
            }
        }
    });
    (conn, send, answered)
}

async fn detach_raw(send: &mut iroh::endpoint::SendStream, id: u64, pane: Uuid) {
    let msg = ControlMsg::Request {
        id,
        method: "detach_pane".into(),
        params: to_value(&json!({"pane_id": pane.to_string()})).unwrap(),
    };
    write_msg(send, &msg).await.unwrap();
}

/// Wait (up to `limit`) until `want` ids are answered.
async fn answered_count(
    answered: &Arc<std::sync::Mutex<std::collections::HashSet<u64>>>,
    want: usize,
    limit: Duration,
) {
    let t0 = Instant::now();
    while answered.lock().unwrap().len() < want {
        assert!(
            t0.elapsed() < limit,
            "{} of {want} requests answered",
            answered.lock().unwrap().len()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// With the size policy lock held for 3 s, 500 detaches of 2 panes start at
/// most one size-policy sync per pane. Each one would hold a blocking thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detaches_start_at_most_one_size_sync_per_pane() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let panes = [
        server.add_script_pane("stty raw -echo; exec cat"),
        server.add_script_pane("stty raw -echo; exec cat"),
    ];
    let endpoint = raw_endpoint(&server).await;
    let (conn, mut send, answered) = raw_control(&endpoint, &server).await;
    let jobs = |s: &TestServer| qa_state(s)["sync_jobs"].as_u64().unwrap();
    let before = jobs(&server);
    server.socket_call("qa_hold_sizes", json!({"ms": 3000})).unwrap();
    let mut sent = 0usize;
    for batch in 0..10u64 {
        for i in 0..50u64 {
            detach_raw(&mut send, batch * 50 + i + 1, panes[(i % 2) as usize]).await;
            sent += 1;
        }
        // 50 in flight at most; the first of each pane waits its 250 ms.
        answered_count(&answered, sent, Duration::from_secs(5)).await;
    }
    let started = jobs(&server) - before;
    println!("500 detaches under a held size policy lock: {started} syncs started");
    assert!(started <= 2, "{started} syncs started for 500 detaches of 2 panes");
    // The lock is let go: nothing is left attached or pending.
    wait_for("the size policy to be clean", || {
        qa_state(&server)["attached_clients"].as_u64().unwrap() == 0
    })
    .await;
    conn.close(0u32.into(), b"done");
    endpoint.close().await;
    server.shutdown().await;
}

/// An attach stuck on a lock, then a detach of its pane and 300 detaches of
/// panes that do not exist: the mark of the real pane stays, so the late
/// attach starts no stream the client had stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_attach_does_not_start_a_stream_after_its_detach_and_a_flood_of_marks() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("stty raw -echo; exec cat");
    let endpoint = raw_endpoint(&server).await;
    let (conn, mut send, answered) = raw_control(&endpoint, &server).await;
    server.socket_call("qa_attach_delay", json!({"ms": 2500})).unwrap();
    attach_raw(&mut send, 1, pane).await;
    // The attach is read and counted before the detaches follow.
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.socket_call("qa_attach_delay", json!({"ms": 0})).unwrap();
    detach_raw(&mut send, 2, pane).await;
    let mut sent = 1usize;
    for batch in 0..8u64 {
        for i in 0..40u64 {
            detach_raw(&mut send, 3 + batch * 40 + i, Uuid::new_v4()).await;
            sent += 1;
        }
        answered_count(&answered, sent, Duration::from_secs(5)).await;
    }
    // The stuck attach answers last.
    answered_count(&answered, sent + 1, Duration::from_secs(10)).await;
    let stream = tokio::time::timeout(Duration::from_millis(700), conn.accept_uni()).await;
    assert!(stream.is_err(), "the late attach started a stream the client had stopped");
    assert_eq!(qa_state(&server)["attached_clients"].as_u64().unwrap(), 0);
    conn.close(0u32.into(), b"done");
    endpoint.close().await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Input buffer after a reconnect
// ---------------------------------------------------------------------------

/// The user (or the app) filled the input buffer (64 KiB, `INPUT_BUFFER_CAP`)
/// while the link was down. After the reconnect the backlog goes out, and the
/// server acks it at once, but the client used to free the space only at its
/// next once-a-second `Ping`: the keys typed right after `Connected` were
/// refused or held for 600 to 700 ms. The client now asks for the ack as soon
/// as the backlog is on the wire. Local, so one round trip is a few ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_is_accepted_at_once_after_a_reconnect_with_a_full_buffer() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_script_pane("stty raw -echo; exec cat");
    let c = connect(&server, "typist").await;
    // Warm up: the pane works, and the session is past its first Pong.
    c.send_input(pane, b"a").await.unwrap();
    wait_for("the first key to be acked", || c.unacked_input_bytes() == 0).await;

    // A loaded host can stall one attempt for a few hundred ms. The bug costs a
    // full second (the next liveness Ping) in every attempt, so the best of
    // three attempts tells them apart.
    let mut best = Duration::MAX;
    for attempt in 1..=3 {
        // The server hangs for 5 s, longer than the 3 s without a Pong.
        let handshakes = server.handshakes();
        server.freeze(Duration::from_secs(5));
        let t_wait = Instant::now();
        while c.state().is_connected() {
            assert!(t_wait.elapsed() < Duration::from_secs(15), "the client never noticed the freeze");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Fill the buffer while the link is down.
        let kb = [b'k'; 1024];
        let mut accepted = 0usize;
        loop {
            match c.send_input(pane, &kb).await {
                Ok(()) => accepted += kb.len(),
                Err(forgetty_client::ClientError::InputBufferFull) => break,
                Err(e) => panic!("send_input while disconnected: {e}"),
            }
            assert!(accepted <= 128 * 1024, "the buffer has no cap");
        }
        assert!(c.unacked_input_bytes() >= 60 * 1024, "the buffer is not full: {}", c.unacked_input_bytes());

        // Reconnect. Do not call `send_input` yet: a blocked call asks for an ack
        // itself. An app that checks the buffer (`unacked_input_bytes`, or a
        // status call that refuses at once) must see the space come back on its
        // own, within one round trip of the backlog going out.
        let t_wait = Instant::now();
        while !(c.state().is_connected() && server.handshakes() > handshakes) {
            assert!(t_wait.elapsed() < Duration::from_secs(30), "the client never reconnected");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let t0 = Instant::now();
        while c.unacked_input_bytes() > 0 {
            assert!(
                t0.elapsed() < Duration::from_secs(3),
                "the backlog was never acked: {} bytes",
                c.unacked_input_bytes()
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let freed = t0.elapsed();
        println!("the buffer was free {freed:?} after Connected");
        // And a key typed now goes in at once.
        let t1 = Instant::now();
        c.send_input(pane, b"z").await.expect("send_input after the ack");
        assert!(t1.elapsed() < Duration::from_millis(50), "send_input took {:?}", t1.elapsed());
        best = best.min(freed);
        println!("attempt {attempt}: free after {freed:?}");
        if best < Duration::from_millis(100) {
            break;
        }
    }
    assert!(
        best < Duration::from_millis(100),
        "the buffer stayed full for {best:?} after Connected in every attempt (the next liveness Ping?)"
    );
    c.close().await;
    server.shutdown().await;
}
