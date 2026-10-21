//! P2b gate scenarios that only the real server can answer: exactly-once
//! across drops and a restart, the size policy, revoke, the stopping state and
//! byte log offsets. Every test skips itself unless `FORGETTY_REAL_SERVER` is
//! set (the test server has no PTYs, no registry and no restart state).

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{real_only, TestServer, TestServerConfig};
use forgetty_client::proto::{ClientKind, ServerEvent};
use forgetty_client::{
    Client, ClientHandle, ClientInfo, ConnectionState, EventItem, PaneItem, SecretKey,
};
use serde_json::json;
use uuid::Uuid;

fn key() -> SecretKey {
    SecretKey::generate(&mut rand::rng())
}

fn info(name: &str) -> ClientInfo {
    ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, name, "0.0.0")
}

async fn connect(server: &TestServer, name: &str) -> ClientHandle {
    let k = key();
    server.authorize(k.public());
    tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), k, info(name)))
        .await
        .expect("connect timed out")
        .expect("connect failed")
}

async fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Exactly-once output and input across 5 forced connection drops and one
/// server restart (SIGTERM, then a new process on the same port and state).
///
/// The pane is a raw echo (`stty raw -echo; exec cat`), so the pane output is
/// the input, byte for byte. If one input frame were written twice or lost,
/// or one output byte skipped or repeated, the output would differ from the
/// typed sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exactly_once_across_five_drops_and_one_restart() {
    if !real_only() {
        return;
    }
    const FRAMES: u32 = 3000;
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let port = server.udp_addr().port();
    let h = connect(&server, "exactly-once").await;
    let mut stream = h.attach_pane(pane).await.unwrap();

    let typed = Arc::new(AtomicU64::new(0));
    let echoed = Arc::new(AtomicU64::new(0));
    let pause = Arc::new(AtomicBool::new(false));

    let typist = {
        let (typed, pause, h) = (typed.clone(), pause.clone(), h.clone());
        tokio::spawn(async move {
            for i in 0..FRAMES {
                while pause.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                h.send_input(pane, &i.to_be_bytes()).await.unwrap();
                typed.store(u64::from(i) + 1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    };
    let consumer = {
        let echoed = echoed.clone();
        tokio::spawn(async move {
            let (mut next, mut resets, mut data) = (0u64, 0usize, Vec::new());
            while (data.len() as u64) < u64::from(FRAMES) * 4 {
                match stream.next().await.expect("stream ended") {
                    PaneItem::Reset { .. } => resets += 1,
                    PaneItem::Data { offset, bytes } => {
                        assert_eq!(offset, next, "gap or duplicate at offset {offset}");
                        next += bytes.len() as u64;
                        data.extend_from_slice(&bytes);
                        echoed.store(data.len() as u64, Ordering::SeqCst);
                    }
                    PaneItem::Snapshot { .. } => panic!("unexpected snapshot catch-up"),
                    PaneItem::Ended => panic!("pane ended"),
                }
            }
            (data, resets)
        })
    };

    // Five forced drops while typing.
    tokio::time::sleep(Duration::from_millis(400)).await;
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        server.drop_connections();
    }

    // From here the server writes input but acks nothing, so every frame typed
    // until the restart is still in the client buffer when the process dies.
    server.hold_acks(true);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // One restart. Typing stops first and the echo catches up, so no byte is
    // in flight inside the dying PTY. Frames the server wrote but did not ack
    // yet are still in the client buffer: input never crosses a restart, so
    // the client drops them when it sees the new `boot_id`, and the new
    // process never gets them (their programs are gone).
    pause.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    wait_for("echo to catch up", Duration::from_secs(10), || {
        echoed.load(Ordering::SeqCst) == typed.load(Ordering::SeqCst) * 4
    })
    .await;
    let t_stop = Instant::now();
    let unacked = h.unacked_input_bytes();
    let saved = server.stop_for_restart().await;
    let started = Instant::now();
    let fresh = TestServer::start_again(saved, port).await;
    let stop_time = started.duration_since(t_stop);
    wait_for("the client to drop its unacked input", Duration::from_secs(15), || {
        h.state().is_connected() && h.lost_input_bytes() > 0
    })
    .await;
    assert_eq!(h.lost_input_bytes(), unacked as u64, "what the user is told was not sent");
    assert_eq!(h.unacked_input_bytes(), 0);
    pause.store(false, Ordering::SeqCst); // typing goes on with the new process

    typist.await.unwrap();
    let (data, resets) = tokio::time::timeout(Duration::from_secs(30), consumer)
        .await
        .expect("output never completed")
        .unwrap();

    let want: Vec<u8> = (0..FRAMES).flat_map(|i| i.to_be_bytes()).collect();
    assert_eq!(data.len(), want.len());
    assert!(data == want, "pane output differs from the typed input: loss, duplicate or reorder");
    assert_eq!(resets, 1, "only the first attach may reset");
    assert_eq!(fresh.log(pane), want, "the server log differs from the typed input");
    println!(
        "exactly-once: {FRAMES} frames, 5 drops, 1 restart ({unacked} bytes unacked at the restart, \
         new process up {stop_time:?} after the stop); output and input identical"
    );
    h.close().await;
    fresh.shutdown().await;
}

/// Input never crosses a server restart (AD-017, `Welcome.boot_id`): what the
/// client held unacked is dropped and counted, an event says so, the new
/// process gets none of it, and input typed afterwards works. A reconnect to
/// the same run drops nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unacked_input_is_dropped_and_reported_when_the_server_restarts() {
    let server = TestServer::start(TestServerConfig {
        panes: vec![(Uuid::new_v4(), Vec::new())],
        ..Default::default()
    })
    .await;
    let port = server.udp_addr().port();
    let pane = server.initial_pane(0); // an initial pane survives the restart
    let h = connect(&server, "typist").await;
    let first_boot = h.welcome().unwrap().boot_id;
    assert!(first_boot.is_some(), "the server sends a boot_id");
    let mut events = h.events().await.unwrap();

    // A reconnect to the same run keeps the buffer and drops nothing.
    server.hold_acks(true);
    h.send_input(pane, b"one\n").await.unwrap();
    wait_for("the server to get the input", Duration::from_secs(5), || {
        server.received_input(pane).ends_with(b"one\n")
    })
    .await;
    server.drop_connections();
    wait_for("the same run again", Duration::from_secs(10), || {
        server.handshakes() >= 2 && h.state().is_connected()
    })
    .await;
    assert_eq!(h.welcome().unwrap().boot_id, first_boot);
    assert_eq!(h.lost_input_bytes(), 0, "a reconnect to the same run lost input");
    assert_eq!(h.unacked_input_bytes(), 4);

    // A restart drops it, and says how much.
    let saved = server.stop_for_restart().await;
    h.send_input(pane, b"two\n").await.unwrap(); // typed while the server is down
    let fresh = TestServer::start_again(saved, port).await;
    wait_for("the client to see the restart", Duration::from_secs(15), || {
        h.state().is_connected() && h.lost_input_bytes() > 0
    })
    .await;
    assert_ne!(h.welcome().unwrap().boot_id, first_boot, "a new run has a new boot_id");
    assert_eq!(h.lost_input_bytes(), 8);
    assert_eq!(h.unacked_input_bytes(), 0);
    let lost = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(EventItem::InputLost { bytes }) = events.next().await {
                return bytes;
            }
        }
    })
    .await
    .expect("no InputLost event");
    assert_eq!(lost, 8);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let got = fresh.received_input(pane);
    assert!(
        !got.windows(3).any(|w| w == b"one" || w == b"two"),
        "input crossed the restart: {:?}",
        String::from_utf8_lossy(&got)
    );
    // Input typed after the restart goes through.
    h.send_input(pane, b"three\n").await.unwrap();
    wait_for("new input", Duration::from_secs(5), || {
        fresh.received_input(pane).ends_with(b"three\n")
    })
    .await;
    h.close().await;
    fresh.shutdown().await;
}

/// Two clients on one pane: the PTY size follows the client that was active
/// last (AD-020).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_size_follows_the_last_active_one() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let a = connect(&server, "client-a").await;
    let b = connect(&server, "client-b").await;
    let mut a_events = a.events().await.unwrap();
    let mut b_events = b.events().await.unwrap();
    let _sa = a.attach_pane(pane).await.unwrap();
    let _sb = b.attach_pane(pane).await.unwrap();

    async fn resized(events: &mut forgetty_client::EventStream) -> (u16, u16) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(EventItem::Server {
                    event: ServerEvent::PaneResized { rows, cols, .. },
                    ..
                }) = events.next().await
                {
                    return (rows, cols);
                }
            }
        })
        .await
        .expect("no PaneResized")
    }
    async fn size_is(server: &TestServer, pane: Uuid, want: (u16, u16)) {
        wait_for("pane size", Duration::from_secs(5), || server.pane_size(pane) == want).await;
    }

    // A reports first and nobody is active yet: A's size applies.
    a.resize(pane, 30, 100).await.unwrap();
    assert_eq!(resized(&mut a_events).await, (30, 100));
    assert_eq!(resized(&mut b_events).await, (30, 100));

    // B reports while A is the active client: remembered, not applied.
    b.resize(pane, 40, 120).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(server.pane_size(pane), (30, 100));

    // B types: B is the latest active client, the PTY takes B's size.
    b.send_input(pane, b"b").await.unwrap();
    assert_eq!(resized(&mut a_events).await, (40, 120));
    assert_eq!(resized(&mut b_events).await, (40, 120));
    size_is(&server, pane, (40, 120)).await;

    // A types: back to A's size.
    a.send_input(pane, b"a").await.unwrap();
    assert_eq!(resized(&mut b_events).await, (30, 100));
    size_is(&server, pane, (30, 100)).await;

    // The active client resizes: applied. The other one resizes: remembered.
    a.resize(pane, 31, 101).await.unwrap();
    size_is(&server, pane, (31, 101)).await;
    b.resize(pane, 41, 121).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(server.pane_size(pane), (31, 101));
    b.send_input(pane, b"b").await.unwrap();
    size_is(&server, pane, (41, 121)).await;
    println!("size policy: followed the last active client through 4 switches");

    a.close().await;
    b.close().await;
    server.shutdown().await;
}

/// Revoking a device cuts its v2 connection, and `client ls` counts v2 clients.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoke_closes_a_v2_connection_and_the_client_registry_counts_it() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig {
        authorized: Some(Default::default()),
        ..Default::default()
    })
    .await;
    let k = key();
    forgetty_client::pair_with_addr(
        server.addr(),
        &server.pair_secret(),
        k.clone(),
        "revoke-me",
        ClientKind::Cli,
        &Default::default(),
    )
    .await
    .unwrap();
    let h = Client::connect(server.addr(), k.clone(), info("revoke-me")).await.unwrap();
    assert!(h.state().is_connected());

    let count = |server: &TestServer| {
        server.socket_call("list_clients", json!({})).unwrap()["clients"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["client_kind"] == "cli")
            .count()
    };
    assert_eq!(count(&server), 1, "client ls must list the v2 client");

    let r =
        server.socket_call("revoke_device", json!({"device_id": k.public().to_string()})).unwrap();
    assert_eq!(r["ok"], true);
    assert!(r["closed_connections"].as_u64().unwrap() >= 1);
    let t0 = Instant::now();
    wait_for("the client to lose the connection", Duration::from_secs(1), || {
        !h.state().is_connected()
    })
    .await;
    println!("revoke: v2 connection ended after {:?}", t0.elapsed());
    // The redial is refused with not_authorized: the client stops for good.
    wait_for("the client to fail for good", Duration::from_secs(10), || {
        matches!(h.state(), ConnectionState::Failed(_))
    })
    .await;
    wait_for("client ls to drop to 0", Duration::from_secs(2), || count(&server) == 0).await;
    h.close().await;
    server.shutdown().await;
}

/// A revoke whose save fails still cuts the connection and still denies the
/// device (fail closed), and the caller gets the save error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoke_that_cannot_be_saved_still_cuts_and_denies_the_device() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig {
        authorized: Some(Default::default()),
        ..Default::default()
    })
    .await;
    let k = key();
    forgetty_client::pair_with_addr(
        server.addr(),
        &server.pair_secret(),
        k.clone(),
        "revoke-unsaved",
        ClientKind::Cli,
        &Default::default(),
    )
    .await
    .unwrap();
    let h = Client::connect(server.addr(), k.clone(), info("revoke-unsaved")).await.unwrap();
    assert!(h.state().is_connected());

    // Make the next save fail: a directory sits where the file goes, so the
    // rename of the temp file onto it is refused.
    let file = server.state_dir().unwrap().join("data/forgetty/authorized_devices.json");
    std::fs::remove_file(&file).expect("the registry file exists after pairing");
    std::fs::create_dir(&file).unwrap();
    std::fs::write(file.join("keep"), b"x").unwrap();

    // The record in `revoked.pending` is written in place first, so the revoke
    // is durable although the registry file is not: the call succeeds and
    // carries a warning.
    let out = server
        .socket_call("revoke_device", json!({"device_id": k.public().to_string()}))
        .expect("a durable revoke succeeds");
    let warning = out["warning"].as_str().unwrap_or_default().to_string();
    assert!(warning.contains("revoked.pending") && warning.contains("not be saved"), "{out}");
    let pending = server.state_dir().unwrap().join("data/forgetty/revoked.pending");
    assert!(std::fs::read_to_string(&pending).unwrap().contains(&k.public().to_string()));

    wait_for("the client to lose the connection", Duration::from_secs(2), || {
        !h.state().is_connected()
    })
    .await;
    // The redial is refused: the device is denied although the file was not written.
    wait_for("the client to fail for good", Duration::from_secs(10), || {
        matches!(h.state(), ConnectionState::Failed(_))
    })
    .await;
    let again = tokio::time::timeout(
        Duration::from_secs(5),
        Client::connect(server.addr(), k.clone(), info("revoke-unsaved-2")),
    )
    .await;
    assert!(!matches!(again, Ok(Ok(_))), "a revoked device connected again");

    // Neither the record nor the registry file can be written: the revoke
    // still holds and cuts the device, and the caller is told it is not
    // durable (the CLI exits 1 with this text).
    let k2 = key();
    let _ = std::fs::remove_dir_all(&file);
    forgetty_client::pair_with_addr(
        server.addr(),
        &server.pair_secret(),
        k2.clone(),
        "revoke-undurable",
        ClientKind::Cli,
        &Default::default(),
    )
    .await
    .unwrap();
    let h2 = Client::connect(server.addr(), k2.clone(), info("revoke-undurable")).await.unwrap();
    std::fs::remove_file(&file).expect("the registry file exists after pairing");
    std::fs::create_dir(&file).unwrap();
    std::fs::write(file.join("keep"), b"x").unwrap();
    std::fs::remove_file(&pending).unwrap();
    std::fs::create_dir(&pending).unwrap();
    let err = server
        .socket_call("revoke_device", json!({"device_id": k2.public().to_string()}))
        .expect_err("an undurable revoke must say so");
    assert!(err.contains("NOT durable"), "{err}");
    wait_for("the second client to fail for good", Duration::from_secs(10), || {
        matches!(h2.state(), ConnectionState::Failed(_))
    })
    .await;
    h2.close().await;
    h.close().await;
    server.shutdown().await;
}

/// A stopping server takes no new v2 client (AD-017).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_server_refuses_v2_attaches() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    server.socket_call("qa_stopping", json!({"on": true})).unwrap();
    let k = key();
    server.authorize(k.public());
    let before = server.handshakes();
    let attempt = tokio::time::timeout(
        Duration::from_millis(1500),
        Client::connect(server.addr(), k.clone(), info("late")),
    )
    .await;
    assert!(attempt.is_err(), "a stopping server must not complete a v2 handshake");
    assert_eq!(server.handshakes(), before);

    server.socket_call("qa_stopping", json!({"on": false})).unwrap();
    let h = tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect(server.addr(), k, info("late")),
    )
    .await
    .expect("connect after the stop was cancelled")
    .expect("connect");
    assert!(h.state().is_connected());
    h.close().await;
    server.shutdown().await;
}

/// A server restart under output load, with a new client connecting while the
/// old process is stopping. The stopping server answers `server_stopping`, the
/// client retries with its normal backoff and connects to the new process. The
/// attached client resumes without a reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_under_load_with_a_client_connecting_in_the_stop_window() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let port = server.udp_addr().port();
    let echo = server.add_pane();
    let load = server.add_script_pane(
        // About 50 MiB/s: a load the client can sustain, so any reset is the restart's.
        "stty raw -echo; while :; do yes 0123456789abcdef | head -c 262144; sleep 0.005; done",
    );
    let a = connect(&server, "a").await;
    let mut echo_s = a.attach_pane(echo).await.unwrap();
    let mut load_s = a.attach_pane(load).await.unwrap();

    // Client A drains the load pane through the restart.
    let got = Arc::new(AtomicU64::new(0));
    let resets = Arc::new(AtomicU64::new(0));
    let drain = {
        let (got, resets) = (got.clone(), resets.clone());
        tokio::spawn(async move {
            while let Some(item) = load_s.next().await {
                match item {
                    PaneItem::Data { bytes, .. } => {
                        got.fetch_add(bytes.len() as u64, Ordering::SeqCst);
                    }
                    PaneItem::Reset { .. } => {
                        resets
                            .fetch_add(u64::from(got.load(Ordering::SeqCst) > 0), Ordering::SeqCst);
                    }
                    PaneItem::Snapshot { .. } => {}
                    PaneItem::Ended => break,
                }
            }
        })
    };
    wait_for("load output", Duration::from_secs(10), || got.load(Ordering::SeqCst) > 1 << 20).await;

    // The server is stopping: B dials, is told so, and keeps trying.
    server.socket_call("qa_stopping", json!({"on": true})).unwrap();
    let kb = key();
    server.authorize(kb.public());
    let addr = server.addr();
    let b = tokio::spawn(async move {
        Client::connect(addr, kb, info("b")).await.map(|h| (h, Instant::now()))
    });
    wait_for("B to be refused twice", Duration::from_secs(10), || server.stopping_refusals() >= 2)
        .await;
    assert!(!b.is_finished(), "server_stopping must be retried, not fatal");

    let saved = server.stop_for_restart().await;
    let started = Instant::now();
    let fresh = TestServer::start_again(saved, port).await;
    let (hb, b_at) = tokio::time::timeout(Duration::from_secs(15), b)
        .await
        .expect("B never connected to the new server")
        .unwrap()
        .expect("B failed for good");
    println!(
        "stop window: B was refused with server_stopping, connected {:?} after the new server started",
        b_at.duration_since(started)
    );
    assert!(hb.state().is_connected());
    wait_for("A to reconnect", Duration::from_secs(10), || a.state().is_connected()).await;

    // A still works on the restored pane and its load stream was not reset.
    a.send_input(echo, b"after\n").await.unwrap();
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !String::from_utf8_lossy(&seen).contains("after") {
            if let Some(PaneItem::Data { bytes, .. }) = echo_s.next().await {
                seen.extend_from_slice(&bytes);
            }
        }
    })
    .await
    .expect("echo after the restart");
    assert_eq!(resets.load(Ordering::SeqCst), 0, "the load stream was reset by the restart");
    drain.abort();
    hb.close().await;
    a.close().await;
    fresh.shutdown().await;
}

/// A client that stops reading Control while layout events fire. The server
/// keeps at most one pending `LayoutChanged` for it and pulls no more events
/// while its queue is over budget, so its memory stays flat. If the queue
/// fills anyway the connection is closed after 10 s. When the peer reads
/// again it finds a handful of layouts, not 100k. A client that reconnects
/// gets the right layout and live events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_stops_reading_control_costs_no_memory_and_is_closed() {
    if !real_only() {
        return;
    }
    use forgetty_client::proto::{
        read_msg, write_msg, ControlMsg, HandshakeReply, Hello, Value, ALPN_RPC,
    };
    use iroh::endpoint::ConnectionError;

    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    // A layout of six panes, so each event is several KiB. The P2c gate also
    // runs 31 (`STALL_PANES=30`).
    let extra: usize = std::env::var("STALL_PANES").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    for _ in 0..extra {
        server.add_pane();
    }

    // A raw peer: handshake, subscribe, then never read again.
    let kb = key();
    server.authorize(kb.public());
    let endpoint = iroh::Endpoint::empty_builder()
        .secret_key(kb.clone())
        .relay_mode(iroh::RelayMode::Disabled)
        .transport_config(forgetty_client::transport_config())
        .bind()
        .await
        .unwrap();
    let conn = endpoint.connect(server.addr(), ALPN_RPC).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = Hello::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        ClientKind::Cli,
        "stalled",
        "0.0.0",
        vec!["resume".into(), "layout_events".into()],
    );
    write_msg(&mut send, &hello).await.unwrap();
    match read_msg::<_, HandshakeReply>(&mut recv).await.unwrap() {
        Some(HandshakeReply::Welcome(_)) => {}
        other => panic!("handshake: {other:?}"),
    }
    write_msg(
        &mut send,
        &ControlMsg::Request { id: 1, method: "subscribe_events".into(), params: Value::Nil },
    )
    .await
    .unwrap();
    loop {
        match read_msg::<_, ControlMsg>(&mut recv).await.unwrap() {
            Some(ControlMsg::Response { id: 1, .. }) => break,
            Some(_) => {}
            None => panic!("control closed"),
        }
    }
    let closed_at = Arc::new(std::sync::Mutex::new(None));
    {
        let (conn, closed_at) = (conn.clone(), closed_at.clone());
        tokio::spawn(async move {
            let why = conn.closed().await;
            *closed_at.lock().unwrap() = Some((Instant::now(), why));
        });
    }

    // 100k layout events over 15 s (1000 every 150 ms). The peer reads nothing.
    let rss0 = server.rss_kib();
    let t0 = Instant::now();
    let mut peak = rss0;
    for i in 0..100 {
        server.fire_layout_events(1000);
        if i % 3 == 0 {
            peak = peak.max(server.rss_kib());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let grew_mb = peak.saturating_sub(rss0) as f64 / 1024.0;
    let closed = closed_at.lock().unwrap().take();
    println!(
        "stalled control reader ({} panes): 100000 events in 15 s, server RSS {:.1} MiB -> peak {:.1} MiB (+{grew_mb:.1}), {}",
        extra + 1,
        rss0 as f64 / 1024.0,
        peak as f64 / 1024.0,
        match &closed {
            Some((at, why)) => format!("connection closed after {:?} ({why:?})", at.duration_since(t0)),
            None => "connection kept open with one pending layout".to_string(),
        }
    );
    assert!(grew_mb < 20.0, "server RSS grew by {grew_mb:.1} MiB");
    if let Some((_, why)) = &closed {
        assert!(
            matches!(why, ConnectionError::ApplicationClosed(c) if u64::from(c.error_code) == 0x12),
            "closed for another reason: {why:?}"
        );
    } else {
        // Still open: the peer reads now and finds the QUIC window's worth of
        // layouts plus one, never the 100k that were fired.
        let mut seen = 0usize;
        while let Ok(Ok(Some(msg))) =
            tokio::time::timeout(Duration::from_millis(500), read_msg::<_, ControlMsg>(&mut recv))
                .await
        {
            if matches!(msg, ControlMsg::Event { .. }) {
                seen += 1;
            }
        }
        println!("stalled control reader: the peer then read {seen} events");
        assert!(seen < 2000, "the server queued {seen} events for a peer that did not read");
    }
    endpoint.close().await;

    // The same device reconnects: right layout, live events.
    let h = tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect(server.addr(), kb, info("back")),
    )
    .await
    .expect("reconnect timed out")
    .expect("reconnect failed");
    let mut events = h.events().await.unwrap();
    let layout = h.request("get_layout", Value::Nil).await.expect("get_layout");
    assert!(
        format!("{layout:?}").contains(&pane.to_string()),
        "the layout after the reconnect misses the pane: {layout:?}"
    );
    server.fire_layout_events(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(EventItem::Server { event: ServerEvent::LayoutChanged { .. }, .. }) =
                events.next().await
            {
                return;
            }
        }
    })
    .await
    .expect("no live event after the reconnect");
    h.close().await;
    server.shutdown().await;
}

/// Absolute offsets: a rotated log keeps counting, the first offset is stored
/// next to the log, and a restart gives the same offsets back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_offsets_persist_across_rotation_and_restart() {
    if !real_only() {
        return;
    }
    // 1 MiB cap: the file rotates at 1 MiB and keeps the newest 512 KiB.
    let server = TestServer::start_with(TestServerConfig::default(), Some(1)).await;
    let port = server.udp_addr().port();
    let pane = server.add_pane();
    let chunk: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
    for _ in 0..48 {
        server.append(pane, &chunk); // 3 MiB
    }
    let total = 48 * chunk.len() as u64;
    wait_for("the disk log to catch up", Duration::from_secs(10), || {
        let (first, t) = server.log_bounds(pane);
        t == total && first > 0
    })
    .await;
    let (first, t) = server.log_bounds(pane);
    println!("after 3 MiB: log starts at {first}, total {t}");
    assert_eq!(t, total);
    assert!(first > 0 && first <= total - 512 * 1024, "first offset {first}");

    // A fresh attach starts at the oldest byte. The oldest byte is more than
    // half the 1 MiB ring behind, so (AD-023) the server sends a snapshot of
    // the screen at the head instead of replaying the log.
    let h = connect(&server, "offsets").await;
    let mut s = h.attach_pane(pane).await.unwrap();
    let mut snap_at = None;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match s.next().await.unwrap() {
                PaneItem::Reset { .. } | PaneItem::Data { .. } => {}
                PaneItem::Snapshot { offset, .. } => {
                    snap_at = Some(offset);
                    return;
                }
                PaneItem::Ended => panic!("ended"),
            }
        }
    })
    .await
    .expect("snapshot");
    assert_eq!(snap_at, Some(total), "the snapshot stands at the head of the log");
    drop(s);
    h.close().await;

    // Restart: the offsets come back from the sidecar and the file.
    let saved = server.stop_for_restart().await;
    let fresh = TestServer::start_again(saved, port).await;
    // The ring held 1 MiB, the file keeps less after a rotation, so the oldest
    // byte can move forward. It never moves back, and the total is exact.
    let (first2, total2) = fresh.log_bounds(pane);
    assert_eq!(total2, total, "total changed across the restart");
    assert!(first2 >= first && first2 > 0, "first offset {first2} (was {first})");
    // The bytes are the right ones at the right absolute offsets.
    let log = fresh.log(pane);
    assert_eq!(log.len() as u64, total2 - first2);
    assert!(
        log.iter().enumerate().all(|(i, b)| {
            let o = first2 + i as u64;
            *b == (((o % (64 * 1024)) as u32) % 251) as u8
        }),
        "log content does not match its offsets"
    );

    // New output continues at the old total.
    fresh.append(pane, b"tail");
    assert_eq!(fresh.log_bounds(pane).1, total + 4);
    fresh.shutdown().await;
}

/// Input typed while the server restarts never reaches the new process: its
/// programs are gone, so the client drops what it holds when it sees the new
/// `boot_id` (PROTOCOL_V2 section 9) and counts it. Input typed once it is
/// connected again arrives once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_typed_during_a_restart_is_dropped_and_input_after_it_arrives_once() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let port = server.udp_addr().port();
    let pane = server.add_pane();
    let h = connect(&server, "restart-input").await;
    let mut stream = h.attach_pane(pane).await.unwrap();
    h.send_input(pane, b"before ").await.unwrap();
    let mut got = Vec::new();
    while got.len() < 7 {
        if let PaneItem::Data { bytes, .. } = stream.next().await.unwrap() {
            got.extend_from_slice(&bytes);
        }
    }
    let saved = server.stop_for_restart().await;
    h.send_input(pane, b"during ").await.unwrap();
    let fresh = TestServer::start_again(saved, port).await;
    wait_for("the client to see the restart", Duration::from_secs(15), || {
        h.state().is_connected() && h.lost_input_bytes() >= 7
    })
    .await;
    h.send_input(pane, b"after").await.unwrap();
    while got.len() < 12 {
        if let PaneItem::Data { bytes, .. } =
            tokio::time::timeout(Duration::from_secs(15), stream.next())
                .await
                .expect("output")
                .unwrap()
        {
            got.extend_from_slice(&bytes);
        }
    }
    // The echo has no more to say: "during" was dropped, not delayed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    while let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await
    {
        if let PaneItem::Data { bytes, .. } = item {
            got.extend_from_slice(&bytes);
        }
    }
    assert_eq!(got, b"before after");
    h.close().await;
    fresh.shutdown().await;
}

/// Throughput of `cat` on a 200 MiB file through the server with one v2
/// client attached. Prints MB/s at the server (PTY to byte log) and at the
/// client, and the resets the client saw. Never a wrong byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cat_200mb_through_the_server() {
    if !real_only() {
        return;
    }
    const N: u64 = 200 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.bin");
    {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(&file).unwrap());
        let block: Vec<u8> = (0..17 * 4096u64).map(common::burst_byte).collect();
        let mut left = N as usize;
        while left > 0 {
            let n = left.min(block.len());
            f.write_all(&block[..n]).unwrap();
            left -= n;
        }
        f.flush().unwrap();
    }
    let server = Arc::new(TestServer::start(TestServerConfig::default()).await);
    let script = format!("stty raw -echo; read -r l; cat {}; exec cat", file.display());
    let pane = server.add_script_pane(&script);
    let h = connect(&server, "bench").await;
    let mut s = h.attach_pane(pane).await.unwrap();

    let t0 = Instant::now();
    server.start_burst(pane, 0); // one line of input starts the cat
    let ingest = {
        let server = server.clone();
        tokio::spawn(async move {
            while server.log_bounds(pane).1 < N {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            t0.elapsed()
        })
    };
    let (mut got, mut resets, mut snapshots, mut bad, mut end) = (0u64, 0u32, 0u32, false, 0u64);
    let deadline = Instant::now() + Duration::from_secs(180);
    while end < N && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(10), s.next()).await {
            Ok(Some(PaneItem::Data { offset, bytes })) => {
                if !bytes
                    .iter()
                    .enumerate()
                    .all(|(i, b)| *b == common::burst_byte(offset + i as u64))
                {
                    bad = true;
                }
                got += bytes.len() as u64;
                end = end.max(offset + bytes.len() as u64);
            }
            Ok(Some(PaneItem::Reset { .. })) => resets += u32::from(end > 0),
            Ok(Some(PaneItem::Snapshot { offset, .. })) => {
                snapshots += 1;
                end = end.max(offset);
            }
            _ => break,
        }
    }
    let client_time = t0.elapsed();
    let ingest_time = tokio::time::timeout(Duration::from_secs(60), ingest)
        .await
        .map(|r| r.unwrap())
        .unwrap_or(Duration::from_secs(60));
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    println!(
        "cat 200 MiB: server ingest {:.0} MiB/s ({ingest_time:?}); client at the end after \
         {client_time:?} ({:.0} MiB/s of the stream covered), {:.0} MiB of it as live bytes, \
         {resets} resets, {snapshots} snapshot catch-ups, bad bytes: {bad}",
        mb(N) / ingest_time.as_secs_f64(),
        mb(end) / client_time.as_secs_f64(),
        mb(got),
    );
    // The program runs at full speed (AD-023). A reader slower than the PTY
    // falls behind and catches up by snapshot: that is counted, not an error.
    // A wrong byte is.
    assert!(!bad, "bytes differ from the pattern");
    assert!(end >= N, "the stream stopped at {end} of {N}");
    // A client that keeps pace with the program is not made to wait out the
    // backoff of a slow one: it is at the end soon after the program is.
    assert!(
        client_time < ingest_time + Duration::from_millis(1500),
        "the client reached the end after {client_time:?}, the program after {ingest_time:?}"
    );
    h.close().await;
    match Arc::try_unwrap(server) {
        Ok(server) => server.shutdown().await,
        Err(_) => panic!("the ingest task still holds the server"),
    }
}
