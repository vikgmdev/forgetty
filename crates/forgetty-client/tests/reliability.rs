//! Liveness, exactly-once input and network-change handling
//! (PROTOCOL_V2 sections 4, 5 and 9). Own test binary: these tests depend on
//! wall-clock timing and must not compete for CPU with the others.

use std::time::{Duration, Instant};

use forgetty_client::proto::ClientKind;
use forgetty_client::test_server::{TestServer, TestServerConfig, UdpProxy};
use forgetty_client::{
    Client, ClientHandle, ClientInfo, EndpointAddr, PaneItem, PaneStream, SecretKey,
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
                PaneItem::Ended => panic!("ended"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out: {} of {total} bytes", out.len()));
    out
}

/// Wait until `server` completed a handshake after `before` and the client is
/// Connected again. Returns when that happened. (State changes are not
/// watched: a fast redial would coalesce `Reconnecting` away.)
async fn wait_reconnected(h: &ClientHandle, server: &TestServer, before: u64) -> Instant {
    wait_until("reconnect", Duration::from_secs(10), || {
        server.handshakes() > before && h.state().is_connected()
    })
    .await;
    Instant::now()
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
    let a = TestServer::start(TestServerConfig::default()).await;
    let proxy = UdpProxy::start(a.udp_addr()).await;
    let pane = a.add_pane();
    let h = connect_to(proxy.addr_for(a.addr().id)).await;
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
    let blackhole_at = std::sync::Mutex::new(None);
    let handshakes_before = std::sync::atomic::AtomicU64::new(0);
    let reconnected_at = std::sync::Mutex::new(None);
    let saboteur = async {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let before = a.handshakes();
        handshakes_before.store(before, std::sync::atomic::Ordering::SeqCst);
        *blackhole_at.lock().unwrap() = Some(Instant::now());
        proxy.blackhole_until_redial(a.udp_addr());
        let back = wait_reconnected(&h, &a, before).await;
        *reconnected_at.lock().unwrap() = Some(back);
    };
    let total_expected = (CHUNKS * CHUNK) as usize + 800 * 4;
    let consumer = read_bytes(&mut stream, total_expected, Duration::from_secs(30));
    let ((), (), (), got) = tokio::join!(producer, typist, saboteur, consumer);

    let t0 = blackhole_at.lock().unwrap().unwrap();
    let detect = proxy.lifted_at().expect("client never redialed").duration_since(t0);
    let recover = reconnected_at.lock().unwrap().unwrap().duration_since(t0);
    println!(
        "blackhole: detected after {detect:?}, Connected again on fresh server after {recover:?}"
    );
    assert!(proxy.blackhole_lifted(), "client never redialed");
    assert!(
        detect >= Duration::from_millis(1500) && detect <= Duration::from_millis(3600),
        "{detect:?}"
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
    proxy.stop();
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
    let a = TestServer::start(TestServerConfig::default()).await;
    let proxy = UdpProxy::start(a.udp_addr()).await;
    let h = connect_to(proxy.addr_for(a.addr().id)).await;

    // Healthy network: the probe is answered, no redial.
    let before = a.handshakes();
    let t0 = Instant::now();
    h.network_changed().await;
    assert!(t0.elapsed() < Duration::from_millis(500), "probe took {:?}", t0.elapsed());
    assert!(h.state().is_connected());
    assert_eq!(a.handshakes(), before);

    // Dead network: no Pong in 1 s, redial (well before the 3 s liveness).
    let before = a.handshakes();
    proxy.blackhole_until_redial(a.udp_addr());
    let t0 = Instant::now();
    h.network_changed().await;
    let recover = wait_reconnected(&h, &a, before).await.duration_since(t0);
    println!("network_changed on a dead link: Connected again after {recover:?}");
    assert!(recover >= Duration::from_millis(900), "gave up too early: {recover:?}");
    assert!(recover < Duration::from_millis(2000), "recovered after {recover:?}");
    assert!(proxy.blackhole_lifted());
    h.close().await;
    proxy.stop();
    a.shutdown().await;
}
