//! P2c: a slow client never slows a program; it catches up by snapshot
//! (AD-023), and a client that floods or ignores Control cannot grow the
//! server. Every test needs the real server (`FORGETTY_REAL_SERVER`).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{TestServer, TestServerConfig};
use forgetty_client::proto::{ClientKind, ControlMsg, HandshakeReply, Hello, Value, ALPN_RPC};
use forgetty_client::{Client, ClientHandle, ClientInfo, PaneItem, SecretKey};
use forgetty_vt::{Terminal, ANSI_PALETTE_BLACK};
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

async fn connect(server: &TestServer, name: &str) -> ClientHandle {
    let k = key();
    server.authorize(k.public());
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, name, "0.0.0");
    tokio::time::timeout(Duration::from_secs(10), Client::connect(server.addr(), k, info))
        .await
        .expect("connect timed out")
        .expect("connect failed")
}

fn state(server: &TestServer) -> serde_json::Value {
    server.socket_call("qa_state", json!({})).expect("qa_state")
}

async fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// A terminal rebuilt from client items
// ---------------------------------------------------------------------------

const PAL: [forgetty_core::Rgba; 16] = [ANSI_PALETTE_BLACK; 16];

/// What a client does with the items of a pane stream.
struct ClientVt {
    term: Option<Terminal>,
    resets: usize,
    snapshots: usize,
    /// End of what the client has applied (data end, or the snapshot offset).
    end: u64,
}

impl ClientVt {
    fn new() -> Self {
        ClientVt { term: None, resets: 0, snapshots: 0, end: 0 }
    }

    fn apply(&mut self, item: PaneItem) {
        match item {
            PaneItem::Reset { rows, cols } => {
                self.term = Some(Terminal::new(rows as usize, cols as usize, PAL));
                self.resets += 1;
            }
            PaneItem::Snapshot { offset, bytes } => {
                self.term.as_mut().expect("snapshot before reset").feed(&bytes);
                self.snapshots += 1;
                self.end = self.end.max(offset);
            }
            PaneItem::Data { offset, bytes } => {
                self.term.as_mut().expect("data before reset").feed(&bytes);
                self.end = self.end.max(offset + bytes.len() as u64);
            }
            PaneItem::Ended => panic!("pane ended"),
        }
    }
}

fn screen_text(t: &Terminal) -> Vec<String> {
    let screen = t.screen();
    (0..screen.rows())
        .map(|r| {
            screen
                .row(r)
                .iter()
                .map(|c| c.grapheme.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn trimmed(lines: &[String]) -> String {
    let mut v = lines.to_vec();
    while v.last().is_some_and(String::is_empty) {
        v.pop();
    }
    v.join("\n")
}

// ---------------------------------------------------------------------------
// 1. A throttled client does not slow the program
// ---------------------------------------------------------------------------

/// Output of the paced program: `0123456789abcdef\r\n` lines (the PTY turns
/// `\n` into `\r\n`), 100 chunks of 30000 lines (540000 bytes out) with a
/// short sleep between them, about 40 MB/s, 51.5 MiB in all.
const CHUNKS: u64 = 100;
const CHUNK_IN: u64 = 30_000 * 17;
const TOTAL: u64 = CHUNKS * 30_000 * 18;

fn paced_script() -> String {
    format!(
        "stty -echo; read -r l; i=0; while [ $i -lt {CHUNKS} ]; do \
         yes 0123456789abcdef | head -c {CHUNK_IN}; i=$((i+1)); sleep 0.01; done; exec cat"
    )
}

fn pattern(i: u64) -> u8 {
    b"0123456789abcdef\r\n"[(i % 18) as usize]
}

/// Time from the program's first byte of input to its last byte in the log.
async fn run_program(server: &TestServer, pane: Uuid) -> Duration {
    // Let the script switch the echo off before it gets its first line.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let t0 = Instant::now();
    server.start_burst(pane, 0);
    wait_for("program output", Duration::from_secs(120), || server.log_bounds(pane).1 >= TOTAL)
        .await;
    t0.elapsed()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_throttled_client_does_not_slow_the_program() {
    if !real_only() {
        return;
    }
    let server = Arc::new(TestServer::start(TestServerConfig::default()).await);

    // Five pairs of runs, one with nobody attached and one with a throttled
    // client (100 KB/s) and a fast client. The program is a shell loop, so a
    // busy box moves a single run by 15 %: compare the best of each side.
    let mut base = Vec::new();
    let slow_h = connect(&server, "slow").await;
    let fast_h = connect(&server, "fast").await;
    let mut with = Vec::new();
    let (mut catchups, mut build_us, mut build_bytes) = (0u64, 0u64, 0u64);
    for round in 0..5 {
        let quiet = server.add_script_pane(&paced_script());
        base.push(run_program(&server, quiet).await);
        let pane = server.add_script_pane(&paced_script());
        let mut slow = slow_h.attach_pane(pane).await.unwrap();
        let mut fast = fast_h.attach_pane(pane).await.unwrap();
        let before = state(&server)["catchups"].as_u64().unwrap();

        // Fast client: contiguous, byte-exact, no reset after the first.
        let fast_task = tokio::spawn(async move {
            let (mut next, mut resets, mut snapshots, mut bad) = (0u64, 0usize, 0usize, 0u64);
            let deadline = Instant::now() + Duration::from_secs(60);
            while next < TOTAL && Instant::now() < deadline {
                match tokio::time::timeout(Duration::from_secs(10), fast.next()).await {
                    Ok(Some(PaneItem::Data { offset, bytes })) => {
                        if offset != next {
                            bad += 1;
                        }
                        if !bytes.iter().enumerate().all(|(i, b)| *b == pattern(offset + i as u64))
                        {
                            bad += 1;
                        }
                        next = offset + bytes.len() as u64;
                    }
                    Ok(Some(PaneItem::Reset { .. })) => resets += usize::from(next > 0),
                    Ok(Some(PaneItem::Snapshot { .. })) => snapshots += 1,
                    _ => break,
                }
            }
            (next, resets, snapshots, bad)
        });

        // Slow client: 100 KB/s.
        let slow_task = tokio::spawn(async move {
            let mut vt = ClientVt::new();
            let deadline = Instant::now() + Duration::from_secs(120);
            while vt.end < TOTAL && Instant::now() < deadline {
                let Ok(Some(item)) =
                    tokio::time::timeout(Duration::from_secs(20), slow.next()).await
                else {
                    break;
                };
                let n = match &item {
                    PaneItem::Data { bytes, .. } | PaneItem::Snapshot { bytes, .. } => bytes.len(),
                    _ => 0,
                };
                vt.apply(item);
                tokio::time::sleep(Duration::from_secs_f64(n as f64 / 100_000.0)).await;
            }
            vt
        });

        let t = run_program(&server, pane).await;
        with.push(t);

        let (next, resets, snapshots, bad) = fast_task.await.unwrap();
        assert_eq!(next, TOTAL, "round {round}: the fast client did not reach the end");
        assert_eq!(bad, 0, "round {round}: the fast client saw a gap or a wrong byte");
        assert_eq!(
            (resets, snapshots),
            (0, 0),
            "round {round}: the fast client was reset or sent a snapshot"
        );

        let vt = slow_task.await.unwrap();
        assert!(vt.end >= TOTAL, "round {round}: the throttled client stopped at {}", vt.end);
        assert!(vt.snapshots >= 1, "round {round}: the throttled client never needed a snapshot");
        let mine = trimmed(&screen_text(vt.term.as_ref().unwrap()));
        let capture = server
            .socket_call("capture", json!({"pane_id": pane.to_string(), "trim": true}))
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .to_string();
        if mine != capture {
            let t = vt.term.as_ref().unwrap();
            eprintln!(
                "DEBUG client cursor {:?} end {} resets {} snapshots {} rows {}",
                t.cursor(),
                vt.end,
                vt.resets,
                vt.snapshots,
                t.screen().rows()
            );
            eprintln!(
                "DEBUG client lines {} capture lines {}",
                mine.lines().count(),
                capture.lines().count()
            );
            eprintln!("DEBUG log bounds {:?}", server.log_bounds(pane));
            let full = screen_text(t);
            eprintln!(
                "DEBUG client rows 0,1,22,23: {:?} {:?} {:?} {:?}",
                full[0], full[1], full[22], full[23]
            );
            let cap = server.socket_call("capture", json!({"pane_id": pane.to_string()})).unwrap()
                ["text"]
                .as_str()
                .unwrap()
                .to_string();
            let rows: Vec<&str> = cap.lines().collect();
            eprintln!(
                "DEBUG capture rows {} 0,1,22,23: {:?} {:?} {:?} {:?}",
                rows.len(),
                rows[0],
                rows[1],
                rows[22],
                rows[23]
            );
            let tail = server.log(pane);
            eprintln!(
                "DEBUG log len {} tail {:?}",
                tail.len(),
                &tail[tail.len().saturating_sub(40)..]
            );
        }
        assert_eq!(
            mine, capture,
            "round {round}: the throttled client's screen differs from capture"
        );

        let st = state(&server);
        catchups += st["catchups"].as_u64().unwrap() - before;
        build_us = st["last_snapshot_us"].as_u64().unwrap();
        build_bytes = st["last_snapshot_bytes"].as_u64().unwrap();
    }
    println!("baseline runs {base:?}");
    println!("runs with clients {with:?}");
    base.sort();
    with.sort();
    let (base, with) = (base[0], with[0]);
    let diff = (with.as_secs_f64() - base.as_secs_f64()) / base.as_secs_f64() * 100.0;
    println!(
        "slow client: program run time {base:?} with no client, {with:?} with a 100 KB/s client \
         and a fast client ({diff:+.1}%); {catchups} snapshot catch-ups, last snapshot {build_bytes} \
         bytes built in {:.0} ms; fast client 0 resets, byte-identical",
        build_us as f64 / 1000.0
    );
    assert!(diff.abs() <= 10.0, "program run time changed by {diff:+.1}%");
    slow_h.close().await;
    fast_h.close().await;
}

// ---------------------------------------------------------------------------
// 1b. Many slow clients and a re-attach loop do not multiply the work
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_slow_clients_and_a_reattach_loop_stay_bounded() {
    if !real_only() {
        return;
    }
    let server = Arc::new(TestServer::start(TestServerConfig::default()).await);
    let mut slow_hs = Vec::new();
    for i in 0..20 {
        slow_hs.push(connect(&server, &format!("slow{i}")).await);
    }
    let loop_h = connect(&server, "reattach").await;

    let (mut base, mut with) = (Vec::new(), Vec::new());
    let (mut builds, mut joins, mut reuses, mut served) = (0u64, 0u64, 0u64, 0u64);
    let mut attaches = 0u64;
    for round in 0..5 {
        let quiet = server.add_script_pane(&paced_script());
        base.push(run_program(&server, quiet).await);

        let pane = server.add_script_pane(&paced_script());
        let before = state(&server);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tasks = Vec::new();
        // 20 clients at 100 KB/s on the same pane.
        for h in &slow_hs {
            let mut s = h.attach_pane(pane).await.unwrap();
            let stop = stop.clone();
            tasks.push(tokio::spawn(async move {
                let mut vt = ClientVt::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) && vt.end < TOTAL {
                    let Ok(Some(item)) =
                        tokio::time::timeout(Duration::from_millis(500), s.next()).await
                    else {
                        continue;
                    };
                    let n = match &item {
                        PaneItem::Data { bytes, .. } | PaneItem::Snapshot { bytes, .. } => {
                            bytes.len()
                        }
                        _ => 0,
                    };
                    vt.apply(item);
                    tokio::time::sleep(Duration::from_secs_f64(n as f64 / 100_000.0)).await;
                }
            }));
        }
        // One client that attaches, reads a little, and attaches again.
        let looped = {
            let (h, stop) = (loop_h.clone(), stop.clone());
            tokio::spawn(async move {
                let mut n = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let Ok(mut s) = h.attach_pane(pane).await else { break };
                    let _ = tokio::time::timeout(Duration::from_millis(40), async {
                        for _ in 0..3 {
                            s.next().await;
                        }
                    })
                    .await;
                    drop(s);
                    n += 1;
                    tokio::time::sleep(Duration::from_millis(60)).await;
                }
                n
            })
        };
        let t = run_program(&server, pane).await;
        with.push(t);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        attaches += looped.await.unwrap();
        for t in tasks {
            let _ = t.await;
        }
        let after = state(&server);
        let d = |k: &str| after[k].as_u64().unwrap() - before[k].as_u64().unwrap();
        builds += d("snapshot_builds");
        joins += d("snapshot_joins");
        reuses += d("snapshot_reuses");
        served += d("catchups");
        assert_eq!(d("too_many_closes"), 0, "round {round}: a connection was closed for requests");
        assert_eq!(d("stall_closes"), 0, "round {round}: a connection was closed as stalled");
    }
    let st = state(&server);
    let max_running = st["snapshot_max_running"].as_u64().unwrap();
    base.sort();
    with.sort();
    let diff = (with[0].as_secs_f64() - base[0].as_secs_f64()) / base[0].as_secs_f64() * 100.0;
    println!(
        "snapshot load (20 clients at 100 KB/s + a re-attach loop, 5 rounds): {builds} builds for \
         {served} snapshots served ({joins} joined, {reuses} reused, {attaches} re-attaches), \
         at most {max_running} builds at once; program run time {:?} vs {:?} with no client \
         ({diff:+.1}%)",
        with[0], base[0]
    );
    assert!(max_running <= 2, "{max_running} snapshot builds ran at once");
    assert!(builds <= 5 * 40, "{builds} snapshot builds in 5 rounds");
    assert!(served > builds, "snapshots were not shared ({served} served, {builds} built)");
    assert!(diff.abs() <= 10.0, "program run time changed by {diff:+.1}%");
}

// ---------------------------------------------------------------------------
// 2. A vim-like alt-screen session survives a forced catch-up
// ---------------------------------------------------------------------------

fn vim_bytes() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"$ vim notes.txt\r\n");
    // alt screen, bracketed paste, mouse (1000 + SGR 1006), application keypad,
    // hidden cursor, title
    v.extend_from_slice(b"\x1b[?1049h\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b=\x1b[?25l");
    v.extend_from_slice(b"\x1b]2;notes.txt - VIM\x07\x1b[2J\x1b[H");
    for i in 1..=20 {
        v.extend_from_slice(
            format!("\x1b[3{}m{i:>3}\x1b[0m  line {i} of the buffer\r\n", i % 7 + 1).as_bytes(),
        );
    }
    v.extend_from_slice(b"\x1b[24;1H\x1b[7m-- INSERT --\x1b[0m\x1b[10;12H\x1b[?25h");
    v
}

fn assert_same_terminal(what: &str, got: &Terminal, want: &Terminal) {
    let (g, w) = (got.screen(), want.screen());
    assert_eq!((g.rows(), g.cols()), (w.rows(), w.cols()), "{what}: size");
    for r in 0..g.rows() {
        let (gr, wr) = (g.row(r), w.row(r));
        let line =
            |row: &[forgetty_vt::Cell]| row.iter().map(|c| c.grapheme.clone()).collect::<String>();
        assert_eq!(line(gr).trim_end(), line(wr).trim_end(), "{what}: row {r} text");
        for (c, (a, b)) in gr.iter().zip(wr).enumerate() {
            if a.grapheme.trim().is_empty() && b.grapheme.trim().is_empty() {
                continue;
            }
            assert_eq!(a, b, "{what}: cell ({r},{c})");
        }
    }
    assert_eq!(got.cursor(), want.cursor(), "{what}: cursor");
    assert_eq!(got.cursor_visible(), want.cursor_visible(), "{what}: cursor visibility");
    assert_eq!(got.is_alternate_screen(), want.is_alternate_screen(), "{what}: alt screen");
    assert_eq!(got.is_mouse_tracking(), want.is_mouse_tracking(), "{what}: mouse tracking");
    for mode in [1000u16, 1006, 2004, 66, 25, 1049] {
        assert_eq!(got.dec_mode(mode), want.dec_mode(mode), "{what}: DEC mode {mode}");
    }
    assert_eq!(got.title(), want.title(), "{what}: title");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forced_catch_up_rebuilds_an_alt_screen_session() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    let pane = server.add_pane();
    let h = connect(&server, "vim").await;
    let mut stream = h.attach_pane(pane).await.unwrap();
    let mut vt = ClientVt::new();

    async fn pump(stream: &mut forgetty_client::PaneStream, vt: &mut ClientVt, until: u64) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while vt.end < until {
                let item = stream.next().await.expect("stream ended");
                vt.apply(item);
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out at offset {} of {until}", vt.end));
    }

    server.append(pane, &vim_bytes());
    let total = server.log_bounds(pane).1;
    pump(&mut stream, &mut vt, total).await;
    assert!(vt.term.as_ref().unwrap().is_alternate_screen());

    // Reference: the server's own bytes through a fresh VT at the pane size.
    let (rows, cols) = server.pane_size(pane);
    let reference = |log: &[u8]| {
        let mut t = Terminal::new(rows as usize, cols as usize, PAL);
        t.feed(log);
        t
    };
    assert_same_terminal("live", vt.term.as_ref().unwrap(), &reference(&server.log(pane)));

    // Force the catch-up: the stream is replaced by a snapshot.
    let (resets0, snaps0) = (vt.resets, vt.snapshots);
    server.socket_call("qa_force_catchup", json!({"pane_id": pane.to_string()})).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while vt.snapshots == snaps0 {
            let item = stream.next().await.expect("stream ended");
            vt.apply(item);
        }
    })
    .await
    .expect("no snapshot arrived");
    assert_eq!(vt.resets, resets0 + 1, "a snapshot follows a reset");
    assert_same_terminal(
        "after catch-up",
        vt.term.as_ref().unwrap(),
        &reference(&server.log(pane)),
    );

    // Live bytes continue on the new stream.
    server.append(pane, b"\x1b[5;1H\x1b[1;33mEDITED\x1b[0m\x1b[12;3H");
    let total = server.log_bounds(pane).1;
    pump(&mut stream, &mut vt, total).await;
    assert_same_terminal(
        "live after catch-up",
        vt.term.as_ref().unwrap(),
        &reference(&server.log(pane)),
    );

    let st = state(&server);
    println!(
        "vim catch-up: screen, cursor, alt screen, mouse 1000/1006, bracketed paste, keypad, \
         cursor visibility and title match the server VT; snapshot {} bytes built in {:.1} ms",
        st["last_snapshot_bytes"],
        st["last_snapshot_us"].as_u64().unwrap() as f64 / 1000.0
    );
    h.close().await;
}

// ---------------------------------------------------------------------------
// 3. A client that sends requests and never reads
// ---------------------------------------------------------------------------

/// Handshake a raw peer and return its connection and streams.
async fn raw_peer(
    server: &TestServer,
) -> (
    iroh::Endpoint,
    iroh::endpoint::Connection,
    iroh::endpoint::SendStream,
    iroh::endpoint::RecvStream,
) {
    use forgetty_client::proto::{read_msg, write_msg};
    let k = key();
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
        Uuid::new_v4(),
        Uuid::new_v4(),
        ClientKind::Cli,
        "flood",
        "0.0.0",
        vec!["resume".into()],
    );
    write_msg(&mut send, &hello).await.unwrap();
    match read_msg::<_, HandshakeReply>(&mut recv).await.unwrap() {
        Some(HandshakeReply::Welcome(_)) => {}
        other => panic!("handshake: {other:?}"),
    }
    (endpoint, conn, send, recv)
}

/// 100k requests, no reads. Returns (rss growth MiB, task growth, close code,
/// seconds to close).
async fn flood(server: &TestServer, method: &'static str) -> (f64, i64, u64, f64) {
    use forgetty_client::proto::write_msg;
    use iroh::endpoint::ConnectionError;
    let (endpoint, conn, mut send, _recv) = raw_peer(server).await;
    let rss0 = server.rss_kib();
    let tasks0 = state(server)["alive_tasks"].as_i64().unwrap();
    let t0 = Instant::now();
    let sender = tokio::spawn(async move {
        for id in 1..=100_000u64 {
            let msg = ControlMsg::Request { id, method: method.into(), params: Value::Nil };
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
        send
    });
    let closed = {
        let conn = conn.clone();
        tokio::spawn(async move { conn.closed().await })
    };
    let (mut peak_rss, mut peak_tasks) = (rss0, tasks0);
    let why = loop {
        if closed.is_finished() {
            break closed.await.unwrap();
        }
        assert!(t0.elapsed() < Duration::from_secs(40), "the flooding connection was never closed");
        peak_rss = peak_rss.max(server.rss_kib());
        peak_tasks = peak_tasks.max(state(server)["alive_tasks"].as_i64().unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let secs = t0.elapsed().as_secs_f64();
    let code = match why {
        ConnectionError::ApplicationClosed(c) => u64::from(c.error_code),
        other => panic!("closed for another reason: {other:?}"),
    };
    sender.abort();
    endpoint.close().await;
    // Let the server drop the connection's tasks.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let tasks_after = state(server)["alive_tasks"].as_i64().unwrap();
    assert!(
        tasks_after - tasks0 <= 8,
        "{method}: {} tasks still alive after the connection closed",
        tasks_after - tasks0
    );
    ((peak_rss.saturating_sub(rss0)) as f64 / 1024.0, peak_tasks - tasks0, code, secs)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_floods_requests_without_reading_is_closed() {
    if !real_only() {
        return;
    }
    let server = TestServer::start(TestServerConfig::default()).await;
    server.add_pane();

    // Cheap requests answered inline: the answers fill the byte budget, the
    // server stops reading, and the stall watchdog closes the connection.
    let (rss, tasks, code, secs) = flood(&server, "list_clients").await;
    println!(
        "request flood (list_clients x100000, no reads): server RSS +{rss:.1} MiB, task count +{tasks}, \
         closed with code {code:#x} after {secs:.1} s"
    );
    assert!(rss < 24.0, "server RSS grew by {rss:.1} MiB");
    assert!(tasks <= 16, "task count grew by {tasks}");
    assert_eq!(code, 0x12, "closed with the wrong code");

    // Requests that run on tasks: the 65th one in flight closes the connection.
    let (rss, tasks, code, secs) = flood(&server, "list_tabs").await;
    println!(
        "request flood (list_tabs x100000, no reads): server RSS +{rss:.1} MiB, task count +{tasks}, \
         closed with code {code:#x} after {secs:.1} s"
    );
    assert!(rss < 24.0, "server RSS grew by {rss:.1} MiB");
    assert!(tasks <= 80, "task count grew by {tasks}");
    assert!(code == 0x14 || code == 0x12, "closed with the wrong code {code:#x}");

    // A normal client still works.
    let h = connect(&server, "after").await;
    h.request("get_layout", Value::Nil).await.expect("get_layout after the floods");
    h.close().await;
}
