//! P2b gate scenarios that only the real server can answer: exactly-once
//! across drops and a restart, the size policy, revoke, the stopping state and
//! byte log offsets. Every test skips itself unless `FORGETTY_REAL_SERVER` is
//! set (the test server has no PTYs, no registry and no restart state).

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{TestServer, TestServerConfig};
use forgetty_client::proto::{ClientKind, ServerEvent};
use forgetty_client::{
    Client, ClientHandle, ClientInfo, ConnectionState, EventItem, PaneItem, SecretKey,
};
use serde_json::json;
use uuid::Uuid;

fn real_only() -> bool {
    if common::is_real() {
        return true;
    }
    println!("skipped: needs the real server (set FORGETTY_REAL_SERVER)");
    false
}

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
    // yet are still in the client buffer: the new process must not write them
    // again.
    pause.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    wait_for("echo to catch up", Duration::from_secs(10), || {
        echoed.load(Ordering::SeqCst) == typed.load(Ordering::SeqCst) * 4
    })
    .await;
    let t_stop = Instant::now();
    let unacked = h.unacked_input_bytes();
    let saved = server.stop_for_restart().await;
    pause.store(false, Ordering::SeqCst); // typing goes on while the server is down
    let started = Instant::now();
    let fresh = TestServer::start_again(saved, port).await;
    let stop_time = started.duration_since(t_stop);

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

    // Attach from an offset older than the log: reset at the first byte.
    let h = connect(&server, "offsets").await;
    let mut s = h.attach_pane(pane).await.unwrap();
    let mut got_first = None;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match s.next().await.unwrap() {
                PaneItem::Reset { .. } => {}
                PaneItem::Data { offset, bytes } => {
                    got_first.get_or_insert(offset);
                    if offset + bytes.len() as u64 >= total {
                        return;
                    }
                }
                PaneItem::Ended => panic!("ended"),
            }
        }
    })
    .await
    .expect("replay");
    assert_eq!(got_first, Some(first), "a fresh attach starts at the oldest byte");
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

/// Frames the server sees while it shuts down are not written. The client
/// resends them to the new process, so nothing is lost or doubled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_typed_during_a_restart_arrives_once() {
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
    h.send_input(pane, b"after").await.unwrap();
    while got.len() < 19 {
        if let PaneItem::Data { bytes, .. } =
            tokio::time::timeout(Duration::from_secs(15), stream.next())
                .await
                .expect("output")
                .unwrap()
        {
            got.extend_from_slice(&bytes);
        }
    }
    assert_eq!(got, b"before during after");
    h.close().await;
    fresh.shutdown().await;
}
