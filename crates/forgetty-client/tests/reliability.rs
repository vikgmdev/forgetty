//! Liveness, exactly-once input and network-change handling
//! (PROTOCOL_V2 sections 4, 5 and 9). Own test binary: these tests depend on
//! wall-clock timing and must not compete for CPU with the others. Runs
//! against the in-process test server, or the real one with
//! `FORGETTY_REAL_SERVER` set.

mod common;

use std::time::{Duration, Instant};

use common::{Saved, TestServer, TestServerConfig};
use forgetty_client::proto::ClientKind;
use forgetty_client::{
    AddrCache, AddrCacheSnapshot, CachedAddr, Client, ClientHandle, ClientInfo, ClientOptions,
    EndpointAddr, PaneItem, PaneStream, SecretKey,
};
use uuid::Uuid;

fn pattern(i: u64) -> u8 {
    (i.wrapping_mul(31).wrapping_add(7) % 251) as u8
}

async fn connect_to(addr: EndpointAddr) -> ClientHandle {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let key = SecretKey::generate(&mut rand::rng());
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "reliability", "0.0.0");
    tokio::time::timeout(Duration::from_secs(10), Client::connect(addr, key, info))
        .await
        .expect("connect timed out")
        .expect("connect failed")
}

async fn read_bytes(stream: &mut PaneStream, total: usize, timeout: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let mut next = 0u64;
    tokio::time::timeout(timeout, async {
        while out.len() < total {
            match stream.next().await.expect("stream ended") {
                PaneItem::Reset { .. } => {}
                PaneItem::Data { offset, bytes } => {
                    assert_eq!(offset, next, "gap or duplicate at offset {offset}");
                    next += bytes.len() as u64;
                    out.extend_from_slice(&bytes);
                }
                PaneItem::Snapshot { .. } => panic!("unexpected snapshot catch-up"),
                PaneItem::Ended => panic!("ended"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out: {} of {total} bytes", out.len()));
    out
}

/// Poll until the client left `Connected` (detect) and is `Connected` again
/// after a new handshake (recover). Returns both instants.
async fn observe_reconnect(
    h: &ClientHandle,
    server: &TestServer,
    before: u64,
) -> (Instant, Instant) {
    let mut detect = None;
    let t0 = Instant::now();
    loop {
        if detect.is_none() && !h.state().is_connected() {
            detect = Some(Instant::now());
        }
        if let Some(d) = detect {
            if server.handshakes() > before && h.state().is_connected() {
                return (d, Instant::now());
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(12), "no reconnect, state {:?}", h.state());
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn wait_until(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silent_blackhole_is_detected_and_resumed_on_a_fresh_server() {
    let a = TestServer::start(TestServerConfig { own_runtime: true, ..Default::default() }).await;
    let pane = a.add_pane();
    let h = connect_to(a.addr()).await;
    let mut stream = h.attach_pane(pane).await.unwrap();
    // Output keeps flowing, and the client keeps typing, across the blackhole.
    const CHUNKS: u64 = 2400;
    const CHUNK: u64 = 100;
    let a_ref = &a;
    let producer = async {
        for i in 0..CHUNKS {
            let from = i * CHUNK;
            let bytes: Vec<u8> = (from..from + CHUNK).map(pattern).collect();
            a_ref.append(pane, &bytes);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    let typist = async {
        for i in 0..800u32 {
            h.send_input(pane, &i.to_be_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(6)).await;
        }
    };
    // The server hangs for 3.4 s: it reads and writes nothing, closes nothing.
    let outage = std::sync::Mutex::new(None);
    let saboteur = async {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let before = a.handshakes();
        let t0 = Instant::now();
        a.freeze(Duration::from_millis(3400));
        let (detected, recovered) = observe_reconnect(&h, &a, before).await;
        *outage.lock().unwrap() = Some((detected - t0, recovered - t0));
    };
    let total_expected = (CHUNKS * CHUNK) as usize + 800 * 4;
    let consumer = read_bytes(&mut stream, total_expected, Duration::from_secs(30));
    let ((), (), (), got) = tokio::join!(producer, typist, saboteur, consumer);

    let (detect, recover) = outage.lock().unwrap().unwrap();
    println!("hung server: dead link detected after {detect:?}, Connected again after {recover:?}");
    assert!(
        detect >= Duration::from_millis(1500) && detect <= Duration::from_millis(3300),
        "detected after {detect:?}"
    );
    assert!(recover < Duration::from_secs(4), "recovered after {recover:?}");

    // Output: the client saw exactly the server log, no gap, no duplicate.
    assert_eq!(got.len(), total_expected);
    assert!(got == a.log(pane), "pane stream differs from the server log");

    // Input: each keystroke frame reached the PTY exactly once, in order.
    let want: Vec<u8> = (0..800u32).flat_map(|i| i.to_be_bytes()).collect();
    assert_eq!(a.received_input(pane), want);
    wait_until("input acks", Duration::from_secs(5), || h.unacked_input_bytes() == 0).await;

    h.close().await;
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thousand_input_frames_survive_connection_kills_exactly_once() {
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect_to(server.addr()).await;
    let mut stream = h.attach_pane(pane).await.unwrap();

    let s = &server;
    let h_ref = &h;
    let typist = async move {
        for i in 0..1000u32 {
            h_ref.send_input(pane, &i.to_be_bytes()).await.unwrap();
            if i % 10 == 0 {
                tokio::task::yield_now().await;
            }
        }
    };
    let killer = async move {
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(15)).await;
            s.drop_connections();
        }
    };
    let consumer = read_bytes(&mut stream, 4000, Duration::from_secs(20));
    let ((), (), echoed) = tokio::join!(typist, killer, consumer);

    let want: Vec<u8> = (0..1000u32).flat_map(|i| i.to_be_bytes()).collect();
    assert_eq!(server.received_input(pane), want, "PTY input differs: loss, dup or reorder");
    assert_eq!(echoed, want, "pane output differs");
    println!(
        "1000 frames, {} handshakes, {} dedup drops",
        server.handshakes(),
        server.dedup_drops()
    );
    assert!(server.handshakes() >= 3, "connections were not actually killed");
    h.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn resend_after_dedup_hit_is_dropped() {
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect_to(server.addr()).await;

    // The server writes input but never acks it, so the client keeps it all.
    server.hold_acks(true);
    for i in 0..100u64 {
        h.send_input(pane, &i.to_be_bytes()).await.unwrap();
    }
    wait_until("server input", Duration::from_secs(5), || server.received_input(pane).len() == 800)
        .await;
    assert_eq!(h.unacked_input_bytes(), 800);

    // Reconnect: the client resends all 100 frames, the server drops them.
    let before = server.handshakes();
    server.drop_connections();
    wait_until("redial", Duration::from_secs(5), || server.handshakes() > before).await;
    wait_until("dedup drops", Duration::from_secs(5), || server.dedup_drops() == 100).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.dedup_drops(), 100, "exactly the 100 resent frames");
    assert_eq!(server.received_input(pane).len(), 800, "resend must not write again");

    // Once acks flow, the buffer empties and new input still goes through once.
    server.hold_acks(false);
    wait_until("acks", Duration::from_secs(4), || h.unacked_input_bytes() == 0).await;
    h.send_input(pane, b"tail").await.unwrap();
    wait_until("tail", Duration::from_secs(5), || server.received_input(pane).len() == 804).await;
    assert_eq!(&server.received_input(pane)[800..], b"tail");
    h.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_changed_redials_after_one_second_without_pong() {
    let a = TestServer::start(TestServerConfig { own_runtime: true, ..Default::default() }).await;
    let h = connect_to(a.addr()).await;

    // Healthy network: the probe is answered, no redial.
    let before = a.handshakes();
    let t0 = Instant::now();
    h.network_changed().await;
    assert!(t0.elapsed() < Duration::from_millis(500), "probe took {:?}", t0.elapsed());
    assert!(h.state().is_connected());
    assert_eq!(a.handshakes(), before);

    // The server hangs for 2 s: shorter than the 3 s liveness limit, so only
    // network_changed can end this connection early. No Pong in 1 s: redial.
    let before = a.handshakes();
    let t0 = Instant::now();
    a.freeze(Duration::from_secs(2));
    h.network_changed().await;
    let (detected, recovered) = observe_reconnect(&h, &a, before).await;
    let (detect, recover) = (detected - t0, recovered - t0);
    println!(
        "network_changed on a hung link: gave up after {detect:?}, Connected after {recover:?}"
    );
    assert!(detect >= Duration::from_millis(900), "gave up too early: {detect:?}");
    assert!(detect < Duration::from_millis(1500), "gave up after {detect:?}");
    assert!(recover < Duration::from_millis(3500), "recovered after {recover:?}");
    h.close().await;
    a.shutdown().await;
}

#[tokio::test]
async fn restarted_client_with_same_client_id_gets_its_first_input_written() {
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let client_id = Uuid::new_v4();
    let key = SecretKey::generate(&mut rand::rng());

    // Process 1: seq 1..=3, acked.
    let info = ClientInfo::new(client_id, ClientKind::Cli, "app", "1");
    let h1 = Client::connect(server.addr(), key.clone(), info.clone()).await.unwrap();
    for piece in [&b"one "[..], b"two ", b"three "] {
        h1.send_input(pane, piece).await.unwrap();
    }
    wait_until("acks", Duration::from_secs(4), || h1.unacked_input_bytes() == 0).await;
    h1.close().await;
    assert_eq!(server.received_input(pane), b"one two three ");

    // Process 2: same client_id, new input_epoch, seq starts at 1 again.
    let h2 = Client::connect(server.addr(), key, info).await.unwrap();
    h2.send_input(pane, b"after restart").await.unwrap();
    wait_until("restarted input", Duration::from_secs(5), || {
        server.received_input(pane).len() == 14 + 13
    })
    .await;
    assert_eq!(server.received_input(pane), b"one two three after restart");
    assert_eq!(server.dedup_drops(), 0, "a new epoch must not be deduped");
    h2.close().await;
    server.shutdown().await;
}

#[test]
fn addr_cache_snapshot_round_trips_through_serde() {
    let key = SecretKey::generate(&mut rand::rng());
    let addr: std::net::SocketAddr = "203.0.113.7:47297".parse().unwrap();
    let cache = AddrCache::new();
    cache.insert(key.public(), addr);
    let json = serde_json::to_string(&cache.snapshot()).unwrap();
    let back: AddrCacheSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(AddrCache::from_snapshot(&back).get(key.public()), Some(addr));
    // A bad entry is skipped, not fatal.
    let junk = AddrCacheSnapshot { entries: vec![CachedAddr { node_id: "garbage".into(), addr }] };
    assert!(AddrCache::from_snapshot(&junk).snapshot().entries.is_empty());
}

/// A server restart is the most common reconnect: same identity, same fixed
/// port, new process, byte logs restored. The client only knows the node id
/// and the cached direct address (no relay, no other address).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_restart_on_the_cached_port_reconnects_every_time() {
    const RESTARTS: usize = 10;
    let first = TestServer::start(TestServerConfig {
        panes: vec![(Uuid::new_v4(), Vec::new())],
        ..Default::default()
    })
    .await;
    let pane = first.initial_pane(0);
    let node_id = first.node_id();
    let port = first.udp_addr().port();

    // What the app would have persisted from an earlier run.
    let cache = AddrCache::from_snapshot(&AddrCacheSnapshot {
        entries: vec![CachedAddr { node_id: node_id.to_string(), addr: first.udp_addr() }],
    });
    let opts = ClientOptions { addr_cache: cache, ..Default::default() };
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "restart", "0");
    let key = SecretKey::generate(&mut rand::rng());
    // The real server only takes paired devices; the mock takes everyone.
    first.authorize(key.public());
    let h = Client::connect_with(
        EndpointAddr::new(node_id), // node id only: nothing but the cache to dial
        key,
        info,
        opts,
    )
    .await
    .expect("first connect through the cached address");
    let mut stream = h.attach_pane(pane).await.unwrap();

    let current = std::sync::Arc::new(tokio::sync::Mutex::new(Some(first)));
    let produced = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let producer = {
        let (current, produced, stop) = (current.clone(), produced.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(s) = current.lock().await.as_ref() {
                    let from = produced.load(std::sync::atomic::Ordering::SeqCst);
                    let bytes: Vec<u8> = (from..from + 50).map(pattern).collect();
                    s.append(pane, &bytes);
                    produced.fetch_add(50, std::sync::atomic::Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    };
    let (total_tx, mut total_rx) = tokio::sync::watch::channel(None::<u64>);
    let consumer = tokio::spawn(async move {
        let mut next = 0u64;
        let mut resets = 0;
        let mut data = Vec::new();
        loop {
            if total_rx.borrow().is_some_and(|t| next >= t) {
                return (data, resets);
            }
            tokio::select! {
                item = stream.next() => match item {
                    Some(PaneItem::Reset { .. }) => resets += 1,
                    Some(PaneItem::Data { offset, bytes }) => {
                        assert_eq!(offset, next, "gap or duplicate at {offset}");
                        next += bytes.len() as u64;
                        data.extend_from_slice(&bytes);
                    }
                    Some(PaneItem::Snapshot { .. }) => panic!("unexpected snapshot catch-up"),
                    Some(PaneItem::Ended) | None => panic!("pane ended"),
                },
                _ = total_rx.changed() => {}
            }
        }
    });

    let mut reconnect_times = Vec::new();
    for round in 0..RESTARTS {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut guard = current.lock().await;
        let old = guard.take().unwrap();
        let saved: Saved = old.stop_for_restart().await; // process A dies
        tokio::time::sleep(Duration::from_millis(50 + (round as u64 * 37) % 250)).await;
        let started = Instant::now();
        let fresh = TestServer::start_again(saved, port).await; // process B, same port
        *guard = Some(fresh);
        drop(guard);
        loop {
            let up = current.lock().await.as_ref().map(|s| s.handshakes()).unwrap_or(0);
            if up > 0 && h.state().is_connected() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "round {round}: not reconnected {:?} after B came up",
                started.elapsed()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        reconnect_times.push(started.elapsed());
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    producer.await.unwrap();
    let total = produced.load(std::sync::atomic::Ordering::SeqCst);
    total_tx.send(Some(total)).unwrap();
    let (have, resets) = tokio::time::timeout(Duration::from_secs(10), consumer)
        .await
        .expect("client never caught up")
        .unwrap();
    println!("server restarts: reconnect after B up: {reconnect_times:?}");
    let slowest = reconnect_times.iter().max().unwrap();
    assert!(
        *slowest < Duration::from_millis(1500),
        "slowest reconnect after a restart was {slowest:?}"
    );
    assert_eq!(resets, 1, "restarts with restored logs must resume, not reset");
    let want: Vec<u8> = (0..total).map(pattern).collect();
    assert_eq!(have.len(), want.len(), "client missed bytes");
    assert!(have == want, "bytes differ");
    h.close().await;
    let last = current.lock().await.take();
    if let Some(s) = last {
        s.shutdown().await;
    }
}

/// The network comes back while a dial to a dead cached address hangs.
/// `network_changed` must abort that dial and dial again at once, with no
/// backoff, so the client is Connected well under a second later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_changed_aborts_a_hung_dial_and_connects_at_once() {
    let first = TestServer::start(TestServerConfig::default()).await;
    let node_id = first.node_id();
    let port = first.udp_addr().port();
    let addr = first.udp_addr();
    let key = SecretKey::generate(&mut rand::rng());
    first.authorize(key.public());
    // The server is down: the cached address answers nothing.
    let saved: Saved = first.stop_for_restart().await;

    let cache = AddrCache::from_snapshot(&AddrCacheSnapshot {
        entries: vec![CachedAddr { node_id: node_id.to_string(), addr }],
    });
    let opts = ClientOptions { addr_cache: cache, ..Default::default() };
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "net-change", "0");
    let h = Client::spawn(EndpointAddr::new(node_id), key, info, opts).await.unwrap();

    // Let the dial hang, and the first failures push the backoff up.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(!h.state().is_connected());

    // The network returns: a live server on the cached address.
    let server = TestServer::start_again(saved, port).await;
    let t0 = Instant::now();
    h.network_changed().await;
    loop {
        if server.handshakes() > 0 && h.state().is_connected() {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "not connected {:?} after network_changed, state {:?}",
            t0.elapsed(),
            h.state()
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let took = t0.elapsed();
    println!("network_changed during a hung dial: Connected after {took:?}");
    assert!(took < Duration::from_millis(300), "redial took {took:?}");
    h.close().await;
    server.shutdown().await;
}
