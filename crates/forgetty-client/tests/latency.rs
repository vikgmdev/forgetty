//! Input latency under a concurrent output burst. Own test binary, so it does
//! not compete for CPU with the other integration tests. Runs against the
//! in-process test server, or the real one with `FORGETTY_REAL_SERVER` set.

mod common;

use std::time::{Duration, Instant};

use common::{TestServer, TestServerConfig};
use forgetty_client::proto::ClientKind;
use forgetty_client::{Client, ClientHandle, ClientInfo, PaneItem, PaneStream, SecretKey};
use uuid::Uuid;

async fn connect(server: &TestServer) -> ClientHandle {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let key = SecretKey::generate(&mut rand::rng());
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "latency", "0.0.0");
    tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), key, info))
        .await
        .expect("connect timed out")
        .expect("connect failed")
}

// Timing budget is set for optimized builds. Debug builds on a loaded box
// exceed it without any real regression, so CI runs this with --release.
#[cfg_attr(debug_assertions, ignore = "latency budget needs --release")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keystroke_not_delayed_by_concurrent_50mb_output_burst() {
    const BURST: usize = 50 * 1024 * 1024;
    // Default byte log (8 MiB ring, 10 MiB file). A reader that is behind is
    // served from the log by offset and never has to reset.
    let server = TestServer::start(TestServerConfig::default()).await;
    let echo_pane = server.add_pane();
    let burst_pane = server.add_burst_pane(BURST);
    let h = connect(&server).await;
    let mut echo = h.attach_pane(echo_pane).await.unwrap();
    let mut burst = h.attach_pane(burst_pane).await.unwrap();

    // Drain the burst pane like a UI would. A client that falls more than half
    // the ring behind gets a snapshot instead of the bytes it missed (AD-023),
    // so the test asks for the end offset, not for every byte.
    let drain = tokio::spawn(async move {
        let (mut end, mut resets, mut snapshots, mut data) = (0u64, 0usize, 0usize, 0u64);
        let mut corrupt = false;
        let mut done_at = None;
        let deadline = Instant::now() + Duration::from_secs(60);
        while end < BURST as u64 && Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), burst.next()).await {
                Ok(Some(PaneItem::Data { offset, bytes })) => {
                    if !bytes
                        .iter()
                        .enumerate()
                        .all(|(i, b)| *b == common::burst_byte(offset + i as u64))
                    {
                        corrupt = true;
                    }
                    data += bytes.len() as u64;
                    end = end.max(offset + bytes.len() as u64);
                }
                // The first attach opens with a reset at the first offset.
                Ok(Some(PaneItem::Reset { .. })) => resets += usize::from(end > 0),
                Ok(Some(PaneItem::Snapshot { offset, .. })) => {
                    snapshots += 1;
                    end = end.max(offset);
                }
                Ok(Some(PaneItem::Ended)) | Ok(None) => break,
                Err(_) => break,
            }
        }
        if end < BURST as u64 {
            eprintln!("burst drain stopped at offset {end} of {BURST}, {resets} resets");
        }
        if end >= BURST as u64 {
            done_at = Some(Instant::now());
        }
        (end, data, resets, snapshots, corrupt, done_at)
    });

    // One 1 KiB keystroke frame, echoed on its own pane stream.
    async fn ping(h: &ClientHandle, echo: &mut PaneStream, pane: Uuid, n: u64) -> Duration {
        let mut frame = vec![b'.'; 1024];
        frame[..8].copy_from_slice(&n.to_be_bytes());
        let t0 = Instant::now();
        h.send_input(pane, &frame).await.unwrap();
        let mut got = 0usize;
        while got < 1024 {
            match echo.next().await.unwrap() {
                PaneItem::Data { bytes, .. } => got += bytes.len(),
                PaneItem::Reset { .. } | PaneItem::Snapshot { .. } => {}
                PaneItem::Ended => panic!("echo ended"),
            }
        }
        t0.elapsed()
    }

    // Baseline on an idle link.
    let mut idle = Vec::new();
    for n in 0..20 {
        idle.push(ping(&h, &mut echo, echo_pane, n).await);
    }

    // Start the burst, then ping continuously while it is in flight.
    let burst_start = Instant::now();
    server.start_burst(burst_pane, BURST);
    let mut during = Vec::new();
    let mut n = 1000;
    while !drain.is_finished() {
        during.push(ping(&h, &mut echo, echo_pane, n).await);
        n += 1;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let (end, data, resets, snapshots, corrupt, done_at) = drain.await.unwrap();
    assert_eq!(end, BURST as u64, "the client never reached the end of the burst");
    assert_eq!(resets, snapshots, "every reset after the first attach carries a snapshot");
    assert!(!corrupt, "burst bytes differ from the pattern");
    let burst_time = done_at.unwrap().duration_since(burst_start);

    during.sort();
    idle.sort();
    let p = |v: &Vec<Duration>, q: f64| v[((v.len() as f64 * q) as usize).min(v.len() - 1)];
    println!(
        "burst of 50 MiB took {burst_time:?} ({data} bytes live, {snapshots} snapshot catch-ups); \
         input RTT idle p50={:?} p95={:?}; \
         during burst n={} p50={:?} p95={:?} max={:?}",
        p(&idle, 0.5),
        p(&idle, 0.95),
        during.len(),
        p(&during, 0.5),
        p(&during, 0.95),
        during.last().unwrap()
    );
    assert!(during.len() >= 10, "burst too short to measure ({} samples)", during.len());
    assert!(
        p(&during, 0.95) < Duration::from_millis(20),
        "p95 input RTT during burst was {:?}",
        p(&during, 0.95)
    );
    h.close().await;
    server.shutdown().await;
}
