//! How long a reconnect takes, step by step, with two panes and a paced
//! history of snapshot catch-ups. Needs the real server. Run it with
//! `QA_RUST_LOG=warn,timeline=debug RUST_LOG=timeline=debug --nocapture` to
//! see the server and client timelines (`target: "timeline"`).

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{real_only, TestServer, TestServerConfig};
use forgetty_client::proto::{ClientKind, Value};
use forgetty_client::{Client, ClientHandle, ClientInfo, PaneItem, PaneStream, SecretKey};
use serde_json::json;
use uuid::Uuid;

/// Bytes a pane delivered, and when the last one came.
#[derive(Default)]
struct Seen {
    data: Mutex<Vec<u8>>,
    snapshots: AtomicU64,
}

fn follow(mut stream: PaneStream) -> Arc<Seen> {
    let seen = Arc::new(Seen::default());
    let out = seen.clone();
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            match item {
                PaneItem::Data { bytes, .. } => {
                    let mut d = out.data.lock().unwrap();
                    d.extend_from_slice(&bytes);
                    // Keep the tail only: the markers are at the end.
                    if d.len() > 1 << 20 {
                        let cut = d.len() - (1 << 19);
                        d.drain(..cut);
                    }
                }
                PaneItem::Snapshot { bytes, .. } => {
                    out.snapshots.fetch_add(1, Ordering::SeqCst);
                    let mut d = out.data.lock().unwrap();
                    d.extend_from_slice(&bytes);
                }
                PaneItem::Reset { .. } => {}
                PaneItem::Ended => return,
            }
        }
    });
    seen
}

fn has(seen: &Arc<Seen>, needle: &str) -> bool {
    let d = seen.data.lock().unwrap();
    d.windows(needle.len()).any(|w| w == needle.as_bytes())
}

async fn until(what: &str, limit: Duration, mut f: impl FnMut() -> bool) -> Duration {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out after {limit:?} waiting for {what}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    t0.elapsed()
}

async fn client_for(server: &TestServer) -> ClientHandle {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let key = SecretKey::generate(&mut rand::rng());
    server.authorize(key.public());
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "timeline", "0.0.0");
    tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), key, info))
        .await
        .expect("connect timed out")
        .expect("connect failed")
}

fn ms(d: Duration) -> u128 {
    d.as_millis()
}

/// One reconnect: drop the connection, then time Connected, `list_tabs`, and
/// an echo from each pane.
async fn round(
    label: &str,
    h: &ClientHandle,
    server: &TestServer,
    panes: [(Uuid, &Arc<Seen>); 2],
    n: usize,
    already_dropped_at: Option<u64>,
) {
    let before = already_dropped_at.unwrap_or_else(|| server.handshakes());
    let t0 = Instant::now();
    if already_dropped_at.is_none() {
        server.drop_connections();
    }
    until("a new connection", Duration::from_secs(30), || {
        server.handshakes() > before && h.state().is_connected()
    })
    .await;
    let connected = t0.elapsed();
    let t1 = Instant::now();
    h.request("list_tabs", Value::Map(vec![])).await.expect("list_tabs");
    let list_tabs = t1.elapsed();
    let mut echo = Vec::new();
    for (i, (pane, seen)) in panes.iter().enumerate() {
        let mark = format!("{label}-{n}-p{i}");
        let t = Instant::now();
        h.send_input(*pane, format!("{mark}\n").as_bytes()).await.unwrap();
        until("an echo", Duration::from_secs(30), || has(seen, &mark)).await;
        echo.push(t.elapsed());
    }
    // Connected and the first request never wait on the pacing of snapshot
    // catch-ups, and a resume with a small gap does not either.
    assert!(connected < Duration::from_millis(500), "{label} {n}: Connected after {connected:?}");
    assert!(list_tabs < Duration::from_millis(500), "{label} {n}: list_tabs took {list_tabs:?}");
    if label == "small gap" {
        for (i, d) in echo.iter().enumerate() {
            assert!(*d < Duration::from_millis(500), "{label} {n}: pane {i} echo after {d:?}");
        }
    }
    // A node and pane have 6 resume tokens. This idle pane gives no live byte
    // to prove a read, and the test lasts seconds, so the refill (one token
    // per 5 minutes, or per strong claim) brings none back. The first attach
    // of the client spent one: 5 far-behind resumes are left, and they wait
    // for the snapshot build only, not for the pacing (1 s and more).
    if label == "far behind" && n < 5 {
        assert!(echo[0] < Duration::from_millis(1000), "{label} {n}: pane A echo after {:?}", echo[0]);
    }
    println!(
        "{label} round {n}: Connected {} ms, list_tabs {} ms after that, echo A {} ms, echo B {} ms, \
         total {} ms",
        ms(connected),
        ms(list_tabs),
        ms(echo[0]),
        ms(echo[1]),
        ms(t0.elapsed())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_timeline_with_two_panes_after_many_catchups() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let a = server.add_burst_pane(4 << 20);
    let b = server.add_script_pane("stty raw -echo; exec cat");
    let h = client_for(&server).await;
    let sa = follow(h.attach_pane(a).await.unwrap());
    let sb = follow(h.attach_pane(b).await.unwrap());

    // History: a burst on A, then many forced snapshot catch-ups on both
    // panes, as the Android run has before its airplane phase.
    server.start_burst(a, 4 << 20);
    until("the burst", Duration::from_secs(60), || server.log_bounds(a).1 >= 4 << 20).await;
    for _ in 0..12 {
        for p in [a, b] {
            server.socket_call("qa_force_catchup", json!({"pane_id": p.to_string()})).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    println!("snapshots delivered: A {}, B {}", sa.snapshots.load(Ordering::SeqCst), sb.snapshots.load(Ordering::SeqCst));

    for n in 0..5 {
        round("small gap", &h, &server, [(a, &sa), (b, &sb)], n, None).await;
    }

    // Far behind, from a node with no history: the client is cut off while
    // 5 MiB of output arrive, so it resumes more than half a ring behind.
    // The first 6 far-behind attaches of a node and pane (the first attach
    // counts) spend a token each and wait for nothing but the snapshot; the
    // rest wait out the backoff (a token comes back every 5 minutes; the
    // unit tests in rpc_v2.rs cover that, a test of seconds cannot).
    let h2 = client_for(&server).await;
    let sa2 = follow(h2.attach_pane(a).await.unwrap());
    let sb2 = follow(h2.attach_pane(b).await.unwrap());
    for n in 0..9 {
        let before = server.handshakes();
        server.pause_handshakes(true);
        server.drop_connections();
        until("the client to notice", Duration::from_secs(10), || !h2.state().is_connected()).await;
        server.append(a, &vec![b'z'; 5 << 20]);
        server.append(b, b"gap-fill\n");
        server.pause_handshakes(false);
        round("far behind", &h2, &server, [(a, &sa2), (b, &sb2)], n, Some(before)).await;
    }

    // The server's timeline, for the log of the run.
    if let Some(dir) = server.state_dir() {
        let log = std::fs::read_to_string(dir.parent().unwrap().join("server.log")).unwrap_or_default();
        for l in log.lines().filter(|l| l.contains("timeline")) {
            println!("{l}");
        }
    }
    h.close().await;
    h2.close().await;
    server.shutdown().await;
}

/// A request of the app, sent as soon as the client is Connected, is answered
/// at once while the resume of two panes runs. Each `attach_pane` is made to
/// take 400 ms on the server (it stands for a lock a busy pane holds). The
/// control loop used to do the attaches one after the other before it took
/// the next request, so `list_tabs` came after 800 ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_right_after_connected_does_not_wait_for_the_resume() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let a = server.add_script_pane("stty raw -echo; exec cat");
    let b = server.add_script_pane("stty raw -echo; exec cat");
    let h = client_for(&server).await;
    let sa = follow(h.attach_pane(a).await.unwrap());
    let sb = follow(h.attach_pane(b).await.unwrap());
    let _events = h.events().await.unwrap();

    // Idle: what one request costs on this host.
    let mut idle = Duration::ZERO;
    for _ in 0..10 {
        let t = Instant::now();
        h.request("list_tabs", Value::Map(vec![])).await.unwrap();
        idle = idle.max(t.elapsed());
    }

    server.socket_call("qa_attach_delay", json!({"ms": 400})).unwrap();
    let before = server.handshakes();
    server.drop_connections();
    until("a new connection", Duration::from_secs(30), || {
        server.handshakes() > before && h.state().is_connected()
    })
    .await;
    let connected_at = Instant::now();
    h.request("list_tabs", Value::Map(vec![])).await.expect("list_tabs");
    let list_tabs = connected_at.elapsed();
    // Both panes come back with the resume, together, not one after the other.
    h.send_input(a, b"resumed-a\n").await.unwrap();
    h.send_input(b, b"resumed-b\n").await.unwrap();
    until("both panes echo", Duration::from_secs(30), || {
        has(&sa, "resumed-a") && has(&sb, "resumed-b")
    })
    .await;
    let resumed = connected_at.elapsed();
    println!("list_tabs {list_tabs:?} (idle {idle:?}), both panes back after {resumed:?}");
    server.socket_call("qa_attach_delay", json!({"ms": 0})).unwrap();
    assert!(
        list_tabs < Duration::from_millis(100).max(idle * 4 + Duration::from_millis(20)),
        "list_tabs waited {list_tabs:?} for the resume (idle {idle:?})"
    );
    assert!(resumed < Duration::from_millis(750), "the attaches ran one by one: {resumed:?}");
    h.close().await;
    server.shutdown().await;
}

/// 100 panes, every one far behind, and one reconnect. The resume sends at
/// most 16 attaches at a time, and the server frees an attach's slot when it
/// answers, so the connection is not closed for too many requests (64 in
/// flight) and a request of the app answers while the resume runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_of_100_far_behind_panes_does_not_close_and_does_not_block_a_request() {
    if !real_only() {
        return;
    }
    const PANES: usize = 100;
    // A 1 MiB ring: more than 512 KiB behind is far behind.
    let server = TestServer::start_with(TestServerConfig::default(), Some(4)).await;
    let h = client_for(&server).await;
    let mut panes = Vec::new();
    for _ in 0..PANES {
        let pane = server.add_script_pane("stty raw -echo; exec cat");
        panes.push((pane, follow(h.attach_pane(pane).await.unwrap())));
    }
    let _events = h.events().await.unwrap();
    let mut idle = Duration::ZERO;
    for _ in 0..10 {
        let t = Instant::now();
        h.request("list_tabs", Value::Map(vec![])).await.unwrap();
        idle = idle.max(t.elapsed());
    }
    let closes = |s: &TestServer| {
        s.socket_call("qa_state", json!({})).unwrap()["too_many_closes"].as_u64().unwrap()
    };
    let closes_before = closes(&server);

    // The client is cut off while every pane gets more than half a ring.
    let before = server.handshakes();
    server.pause_handshakes(true);
    server.drop_connections();
    until("the client to notice", Duration::from_secs(10), || !h.state().is_connected()).await;
    for (pane, _) in &panes {
        server.append(*pane, &vec![b'z'; 700_000]);
    }
    let snapshots_before: u64 = panes.iter().map(|(_, s)| s.snapshots.load(Ordering::SeqCst)).sum();
    server.pause_handshakes(false);
    until("a new connection", Duration::from_secs(60), || {
        server.handshakes() > before && h.state().is_connected()
    })
    .await;
    let connected_at = Instant::now();
    let handshakes_at_connect = server.handshakes();
    h.request("list_tabs", Value::Map(vec![])).await.expect("list_tabs");
    let list_tabs = connected_at.elapsed();
    until("every pane to get its snapshot", Duration::from_secs(120), || {
        panes.iter().map(|(_, s)| s.snapshots.load(Ordering::SeqCst)).sum::<u64>()
            >= snapshots_before + PANES as u64
    })
    .await;
    let resumed = connected_at.elapsed();
    println!(
        "100 far-behind panes: list_tabs {list_tabs:?} (idle {idle:?}), all snapshots after \
         {resumed:?}, connections {}",
        server.handshakes() - before
    );
    assert_eq!(closes(&server), closes_before, "the server closed the connection for requests");
    // A resume that was closed for too many requests would dial again.
    assert_eq!(server.handshakes(), handshakes_at_connect, "the client connected again");
    assert!(h.state().is_connected(), "the client is not connected after the resume");
    assert!(
        list_tabs < Duration::from_millis(100).max(idle * 4 + Duration::from_millis(20)),
        "list_tabs waited {list_tabs:?} for the resume (idle {idle:?})"
    );
    h.close().await;
    server.shutdown().await;
}
