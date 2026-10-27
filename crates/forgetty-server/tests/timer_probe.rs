//! Server-only latency probe: one key in, its echo out, over protocol v2 to a real server
//! process (iroh on loopback), next to the same echo through a ConPTY or pty in this process.
//!
//! On Windows the server process can run with the system timer at 15.6 ms or at 1 ms
//! (`FORGETTY_TIMER_RES`, see `timerres.rs`). The probe starts one server per setting and prints
//! the median and the 95th percentile of each, so a CI log shows before and after. Run it with
//! `cargo test --release -p forgetty-server --test timer_probe -- --nocapture`.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use forgetty_client::proto::{ClientKind, QrPayload};
use forgetty_client::{pair, Client, ClientInfo, PaneItem, SecretKey};
use uuid::Uuid;

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
}

#[cfg(windows)]
#[link(name = "ntdll")]
extern "system" {
    fn NtQueryTimerResolution(min: *mut u32, max: *mut u32, current: *mut u32) -> i32;
}

/// The system timer period now, in ms. Any process that asked for a finer timer changes it for
/// the machine, so this says how clean a "default timer" measurement is.
fn system_tick_ms() -> f64 {
    #[cfg(windows)]
    {
        let (mut min, mut max, mut cur) = (0u32, 0u32, 0u32);
        // SAFETY: three valid out pointers.
        unsafe { NtQueryTimerResolution(&mut min, &mut max, &mut cur) };
        f64::from(cur) / 10_000.0
    }
    #[cfg(not(windows))]
    {
        0.0
    }
}

/// Ask for a 1 ms timer in this (the client) process, as the app does.
fn client_timer_1ms() {
    #[cfg(windows)]
    // SAFETY: plain winmm call.
    unsafe {
        timeBeginPeriod(1);
    }
}

#[cfg(windows)]
const PROGRAM: &str = "cmd.exe";
#[cfg(not(windows))]
const PROGRAM: &str = "cat";

const SAMPLES: usize = 60;

fn forgetty(state: &Path, timer: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forgetty"))
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .env("FORGETTY_TEST_ALLOW_ELEVATED", "1")
        .env("FORGETTY_PORT", "0")
        .env("FORGETTY_TIMER_RES", timer)
        .env("FORGETTY_TRACE_LATENCY", "1")
        .env("RUST_LOG", "latency=info")
        .output()
        .expect("run forgetty")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

struct Stats {
    label: String,
    sorted: Vec<Duration>,
}

impl Stats {
    fn new(label: &str, mut v: Vec<Duration>) -> Self {
        v.sort();
        Stats { label: label.into(), sorted: v }
    }
    fn at(&self, q: f64) -> f64 {
        let i = ((self.sorted.len() as f64 * q) as usize).min(self.sorted.len() - 1);
        self.sorted[i].as_secs_f64() * 1000.0
    }
    fn p50(&self) -> f64 {
        self.at(0.5)
    }
    fn show(&self) {
        println!(
            "PROBE {:<52} n={} p50={:.2} ms p95={:.2} ms max={:.2} ms",
            self.label,
            self.sorted.len(),
            self.at(0.5),
            self.at(0.95),
            self.sorted.last().unwrap().as_secs_f64() * 1000.0
        );
    }
}

/// The same echo without a server: a pty of this process, a key written, the echo read.
fn in_process(label: &str) -> Stats {
    use std::io::{Read, Write};
    let cmd = vec![PROGRAM.to_string()];
    let mut pty =
        forgetty_pty::PtyProcess::spawn(forgetty_pty::PtySize::default(), None, Some(&cmd), false)
            .expect("spawn pty");
    let mut reader = pty.take_reader().expect("reader");
    let mut writer = pty.take_writer().expect("writer");
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    // Let the prompt settle.
    std::thread::sleep(Duration::from_millis(1500));
    while rx.try_recv().is_ok() {}
    let mut out = Vec::new();
    for i in 0..SAMPLES {
        let key = b'a' + (i % 26) as u8;
        let t0 = Instant::now();
        writer.write_all(&[key]).unwrap();
        writer.flush().unwrap();
        let end = t0 + Duration::from_secs(5);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no echo of {:?}", key as char);
            match rx.recv_timeout(left) {
                Ok(chunk) if chunk.contains(&key) => break,
                Ok(_) => {}
                Err(_) => panic!("no echo of {:?}", key as char),
            }
        }
        out.push(t0.elapsed());
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(writer);
    drop(pty);
    Stats::new(label, out)
}

/// The stage gaps the server logged for each echo (`FORGETTY_TRACE_LATENCY`, `latency_trace.rs`),
/// as medians in microseconds.
fn show_stages(state: &Path, label: &str) {
    let log = state.join("data").join("forgetty").join("server.log");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let mut cols: Vec<Vec<i64>> = vec![Vec::new(); 5];
    for line in text.lines().filter(|l| l.contains("key echo:")) {
        let nums: Vec<i64> = line
            .split(|c: char| !(c.is_ascii_digit() || c == '-'))
            .filter_map(|t| t.parse().ok())
            .collect();
        if nums.len() == 5 {
            for (c, n) in cols.iter_mut().zip(nums) {
                c.push(n);
            }
        }
    }
    if cols[0].is_empty() {
        println!("PROBE stages {label}: no trace lines in {}", log.display());
        return;
    }
    let med = |v: &mut Vec<i64>| {
        v.sort();
        v[v.len() / 2]
    };
    let m: Vec<i64> = cols.iter_mut().map(med).collect();
    println!(
        "PROBE stages {label} (n={}, median us): input->pty write {} | ->first pty read {} | ->pane event {} | ->frame written {} | total {}",
        cols[0].len(), m[0], m[1], m[2], m[3], m[4]
    );
}

/// A real server started with `timer` as `FORGETTY_TIMER_RES`; a key in over protocol v2, the
/// echo out.
async fn through_server(timer: &str, client: &str) -> (Stats, Stats) {
    let state = tempfile::tempdir().unwrap();
    // Windows servers are loopback only with no relay by default. Elsewhere the config says so.
    #[cfg(not(windows))]
    {
        let dir = state.path().join("config/forgetty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "bind_addrs = [\"127.0.0.1\"]\nrelay = \"none\"\n")
            .unwrap();
    }
    let start = forgetty(state.path(), timer, &["server", "start"]);
    assert!(start.status.success(), "{}", text(&start));

    // The pane, and the pairing payload an app would scan.
    let new = forgetty(state.path(), timer, &["new", "--", PROGRAM]);
    assert!(new.status.success(), "{}", text(&new));
    let pane = Uuid::parse_str(String::from_utf8_lossy(&new.stdout).trim()).expect("pane id");
    let qr: QrPayload = {
        let mut child = Command::new(env!("CARGO_BIN_EXE_forgetty"))
            .arg("--state-dir")
            .arg(state.path())
            .arg("pair")
            .env("FORGETTY_TEST_ALLOW_ELEVATED", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("forgetty pair");
        use std::io::{BufRead, BufReader};
        let out = child.stdout.take().unwrap();
        let line = BufReader::new(out)
            .lines()
            .map_while(Result::ok)
            .find_map(|l| l.strip_prefix("payload: ").map(str::to_string))
            .expect("pair payload");
        let _ = child.kill();
        let _ = child.wait();
        serde_json::from_str(&line).expect("payload json")
    };

    let key = SecretKey::generate(&mut rand::rng());
    let paired = pair(&qr, key.clone(), "timer-probe", ClientKind::Cli).await.expect("pair");
    let info = ClientInfo::new(Uuid::new_v4(), ClientKind::Cli, "timer-probe", "0.0.0");
    let addr = forgetty_client::server_addr_direct(&qr.node_id, &paired.relays, &paired.pinned_addrs)
        .expect("server addr");
    let h = tokio::time::timeout(
        Duration::from_secs(20),
        Client::connect_with(addr, key, info, paired.options()),
    )
    .await
    .expect("connect timed out")
    .expect("connect");
    let mut stream = h.attach_pane(pane).await.expect("attach");

    // Idle round trip of a plain request, for scale.
    let mut rtt = Vec::new();
    for _ in 0..SAMPLES {
        let t0 = Instant::now();
        h.request("get_layout", forgetty_client::proto::Value::Nil).await.expect("get_layout");
        rtt.push(t0.elapsed());
    }

    // Let the prompt arrive and settle.
    let settle = Instant::now() + Duration::from_millis(2000);
    while Instant::now() < settle {
        let _ = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
    }

    let mut echo = Vec::new();
    for i in 0..SAMPLES {
        let key = b'a' + (i % 26) as u8;
        let t0 = Instant::now();
        h.send_input(pane, &[key]).await.expect("send_input");
        let end = t0 + Duration::from_secs(5);
        'wait: loop {
            let left = end.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no echo of {:?}", key as char);
            match tokio::time::timeout(left, stream.next()).await {
                Ok(Some(PaneItem::Data { bytes, .. })) if bytes.contains(&key) => break 'wait,
                Ok(Some(_)) => {}
                other => panic!("no echo of {:?}: {:?}", key as char, other.is_ok()),
            }
        }
        echo.push(t0.elapsed());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.close().await;
    show_stages(state.path(), &format!("{client}, server {timer}"));
    let stop = forgetty(state.path(), timer, &["server", "stop"]);
    assert!(stop.status.success(), "{}", text(&stop));
    (
        Stats::new(&format!("{client}, server {timer}: key to echo"), echo),
        Stats::new(&format!("{client}, server {timer}: get_layout"), rtt),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_to_echo_through_the_server_against_in_process() {
    // First with this process at the default timer (it stands for the app before it asks for 1 ms),
    // then at 1 ms (the app after `timeBeginPeriod(1)`), each against a server that does not ask
    // ("off") and one that does ("full", the shipped setting).
    println!("PROBE system timer period at the start: {:.2} ms", system_tick_ms());
    let mut all = Vec::new();
    let base_default = in_process("in process, client timer default");
    base_default.show();
    for timer in ["off", "full"] {
        let (echo, rtt) = through_server(timer, "client timer default").await;
        echo.show();
        rtt.show();
        println!("PROBE system timer period now: {:.2} ms", system_tick_ms());
        all.push((format!("client default, server {timer}"), echo.p50(), base_default.p50()));
    }
    client_timer_1ms();
    println!("PROBE client now asks for 1 ms; system timer period: {:.2} ms", system_tick_ms());
    let base_fine = in_process("in process, client timer 1 ms");
    base_fine.show();
    for timer in ["off", "period", "full"] {
        let (echo, rtt) = through_server(timer, "client timer 1 ms").await;
        echo.show();
        rtt.show();
        all.push((format!("client 1 ms, server {timer}"), echo.p50(), base_fine.p50()));
    }
    for (what, p50, base) in &all {
        println!("PROBE delta to in process, {what}: {:+.2} ms (p50)", p50 - base);
    }
}
