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

/// Set by the diagnostic experiments (shell timing): a key whose echo does not come in time is
/// reported and skipped instead of failing the test. The tests that measure Forgetty's own stages
/// stay strict.
static LENIENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn lenient() -> bool {
    LENIENT.load(std::sync::atomic::Ordering::Relaxed)
}

/// The bytes of `chunk` outside escape sequences: a CSI final byte such as the `h` of `ESC[?25h`
/// is not an echo of the key `h`.
fn printable(chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chunk.len());
    let mut i = 0;
    while i < chunk.len() {
        if chunk[i] == 0x1b && i + 1 < chunk.len() {
            match chunk[i + 1] {
                b'[' => {
                    i += 2;
                    while i < chunk.len() && !(0x40..=0x7e).contains(&chunk[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                b']' => {
                    i += 2;
                    while i < chunk.len() && chunk[i] != 0x07 && chunk[i] != 0x1b {
                        i += 1;
                    }
                    i += if i < chunk.len() && chunk[i] == 0x1b { 2 } else { 1 };
                }
                _ => i += 2,
            }
        } else {
            out.push(chunk[i]);
            i += 1;
        }
    }
    out
}

/// Extra environment for the servers this test starts (one test at a time).
static EXTRA_ENV: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

fn forgetty(state: &Path, timer: &str, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_forgetty"));
    for (k, v) in EXTRA_ENV.lock().unwrap().iter() {
        command.env(k, v);
    }
    // "default" leaves the variable unset: the server's own setting (the 1 ms tick while panes
    // are in use).
    if timer == "default" {
        command.env_remove("FORGETTY_TIMER_RES");
    } else {
        command.env("FORGETTY_TIMER_RES", timer);
    }
    command
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .env("FORGETTY_TEST_ALLOW_ELEVATED", "1")
        .env("FORGETTY_PORT", "0")
        .env("FORGETTY_TRACE_LATENCY", "1")
        .env("RUST_LOG", "latency=info,forgetty_pty=info,forgetty_server=info")
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
        if self.sorted.is_empty() {
            return f64::NAN;
        }
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
            self.sorted.last().map_or(f64::NAN, |d| d.as_secs_f64() * 1000.0)
        );
    }
}

/// The same echo without a server: a pty of this process, a key written, the echo read.
fn in_process(label: &str) -> Stats {
    in_process_with(label, &[PROGRAM])
}

fn in_process_with(label: &str, argv: &[&str]) -> Stats {
    use std::io::{Read, Write};
    let cmd: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
    // This process never calls `forgetty_pty::timer_boost::init`, so it stands for a program that
    // asks for no finer timer. The servers started below do ask.
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
    // Let the prompt settle: quiet for 1.5 s (a PowerShell takes a few seconds to start).
    let give_up = Instant::now() + Duration::from_secs(25);
    while Instant::now() < give_up && rx.recv_timeout(Duration::from_millis(1500)).is_ok() {}
    let mut out = Vec::new();
    for i in 0..SAMPLES {
        let key = b'a' + (i % 26) as u8;
        let t0 = Instant::now();
        writer.write_all(&[key]).unwrap();
        writer.flush().unwrap();
        let end = t0 + Duration::from_secs(5);
        let mut echoed = true;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                assert!(lenient(), "no echo of {:?}", key as char);
                echoed = false;
                break;
            }
            match rx.recv_timeout(left) {
                Ok(chunk) if printable(&chunk).contains(&key) => break,
                Ok(_) => {}
                Err(_) => {
                    assert!(lenient(), "no echo of {:?}", key as char);
                    echoed = false;
                    break;
                }
            }
        }
        if !echoed {
            println!("PROBE {label}: no echo of {:?} in 5 s, sample skipped", key as char);
            continue;
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

/// The first ConPTY read chunks after an input, as the server saw them: how long after the
/// input, whether the key byte is in the chunk, and its first bytes. Says whether the character
/// comes in the first read or in a later one.
fn show_chunks(state: &Path, label: &str) {
    let log = state.join("data").join("forgetty").join("server.log");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| l.contains(" us after the input")).collect();
    let late = lines.iter().filter(|l| l.contains("key byte not in it")).count();
    println!("PROBE chunks {label}: {} chunk lines, {} without the key byte", lines.len(), late);
    for l in lines.iter().rev().take(10).rev() {
        println!("PROBE chunk {label}: {}", l.split("latency:").last().unwrap_or(l).trim());
    }
}

/// A real server started with `timer` as `FORGETTY_TIMER_RES`; a key in over protocol v2, the
/// echo out.
async fn through_server(timer: &str, client: &str) -> (Stats, Stats) {
    through_server_as(timer, client, Pane::Fresh, &[PROGRAM]).await
}

/// What the pane went through before the keys: `Fresh` is a new 80x24 pane, `Resized` is the
/// size the app's window gives it (200x50) by a `resize` after attach, `Scrolled` is that plus
/// 3000 lines of scrollback.
#[derive(Clone, Copy, Debug)]
enum Pane {
    Fresh,
    Resized,
    Scrolled,
}

async fn through_server_as(
    timer: &str,
    client: &str,
    kind: Pane,
    argv: &[&str],
) -> (Stats, Stats) {
    let client = &format!("{client}, {} {kind:?} pane", argv[0]);
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
    let mut new_args = vec!["new", "--"];
    new_args.extend_from_slice(argv);
    let new = forgetty(state.path(), timer, &new_args);
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
    if !matches!(kind, Pane::Fresh) {
        h.resize(pane, 50, 200).await.expect("resize");
    }
    if matches!(kind, Pane::Scrolled) {
        #[cfg(windows)]
        let fill: &[u8] = b"for /l %i in (1,1,3000) do @echo line %i\r";
        #[cfg(not(windows))]
        let fill: &[u8] = b"seq 1 3000\n";
        h.send_input(pane, fill).await.expect("fill");
        // Until the output has been quiet for a second.
        while let Ok(Some(_)) =
            tokio::time::timeout(Duration::from_millis(1000), stream.next()).await
        {}
    }

    // Idle round trip of a plain request, for scale.
    let mut rtt = Vec::new();
    for _ in 0..SAMPLES {
        let t0 = Instant::now();
        h.request("get_layout", forgetty_client::proto::Value::Nil).await.expect("get_layout");
        rtt.push(t0.elapsed());
    }

    // Let the prompt arrive and settle: quiet for 1.5 s. A cursor position request (what
    // `PSEUDOCONSOLE_INHERIT_CURSOR` makes the host send) is answered like the app's terminal does.
    let give_up = Instant::now() + Duration::from_secs(25);
    while Instant::now() < give_up {
        match tokio::time::timeout(Duration::from_millis(1500), stream.next()).await {
            Ok(Some(PaneItem::Data { bytes, .. })) => {
                if bytes.windows(4).any(|w| w == b"\x1b[6n") {
                    h.send_input_flagged(pane, b"\x1b[1;1R", true).await.expect("cpr reply");
                }
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }

    let mut echo = Vec::new();
    for i in 0..SAMPLES {
        let key = b'a' + (i % 26) as u8;
        let t0 = Instant::now();
        h.send_input(pane, &[key]).await.expect("send_input");
        let end = t0 + Duration::from_secs(5);
        let mut echoed = true;
        'wait: loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                assert!(lenient(), "no echo of {:?}", key as char);
                echoed = false;
                break 'wait;
            }
            match tokio::time::timeout(left, stream.next()).await {
                Ok(Some(PaneItem::Data { bytes, .. })) if printable(&bytes).contains(&key) => break 'wait,
                Ok(Some(_)) => {}
                other => {
                    assert!(lenient(), "no echo of {:?}: {:?}", key as char, other.is_ok());
                    echoed = false;
                    break 'wait;
                }
            }
        }
        if !echoed {
            println!("PROBE {client}: no echo of {:?} in 5 s, sample skipped", key as char);
            continue;
        }
        echo.push(t0.elapsed());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.close().await;
    show_stages(state.path(), &format!("{client}, server {timer}"));
    show_chunks(state.path(), &format!("{client}, server {timer}"));
    let log = std::fs::read_to_string(state.path().join("data/forgetty/server.log")).unwrap_or_default();
    for l in log.lines().filter(|l| l.contains("power throttling")).take(3) {
        println!("PROBE power {client}: {}", l.trim());
    }
    let stop = forgetty(state.path(), timer, &["server", "stop"]);
    assert!(stop.status.success(), "{}", text(&stop));
    (
        Stats::new(&format!("{client}, server {timer}: key to echo"), echo),
        Stats::new(&format!("{client}, server {timer}: get_layout"), rtt),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zz_key_to_echo_through_the_server_against_in_process() {
    // Strict, whatever ran before it in this process.
    LENIENT.store(false, std::sync::atomic::Ordering::Relaxed);
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
    // The app's situation with PowerShell: this process asks for 1 ms (as the app does), the
    // shell in process starts under that request, and the server is one that asks (default),
    // holds it (period) or does not (off).
    #[cfg(windows)]
    {
        LENIENT.store(true, std::sync::atomic::Ordering::Relaxed);
        let shell = ["powershell.exe", "-NoLogo"];
        let base = in_process_with("in process, powershell.exe, client timer 1 ms", &shell);
        base.show();
        for timer in ["off", "default", "period"] {
            let (echo, _) = through_server_as(timer, "client timer 1 ms", Pane::Fresh, &shell).await;
            echo.show();
            println!(
                "PROBE app-like powershell.exe, client 1 ms, server {timer}: delta to in process {:+.2} ms (p50)",
                echo.p50() - base.p50()
            );
        }
    }
}

/// The UI pane differs from a fresh one: it was resized to the window and holds scrollback, and
/// it runs the default shell (PowerShell, with PSReadLine, when the machine has one). One server
/// per shell and kind, each next to the same shell in process, so a slow echo shows which part of
/// the pane it follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "diagnostic experiment on shell timing: the timer-probe job runs it with --include-ignored"]
async fn key_to_echo_on_panes_like_the_ui_pane() {
    LENIENT.store(true, std::sync::atomic::Ordering::Relaxed);
    #[cfg(windows)]
    let shells: Vec<Vec<&str>> = {
        let mut v = vec![vec!["cmd.exe"], vec!["powershell.exe", "-NoLogo"]];
        if Command::new("pwsh.exe").arg("-Version").output().is_ok() {
            v.push(vec!["pwsh.exe", "-NoLogo"]);
        }
        v
    };
    #[cfg(not(windows))]
    let shells: Vec<Vec<&str>> = vec![vec![PROGRAM]];
    for shell in &shells {
        let base = in_process_with(&format!("in process, {}", shell[0]), shell);
        base.show();
        for kind in [Pane::Fresh, Pane::Resized, Pane::Scrolled] {
            let (echo, _) = through_server_as("off", "ui-like", kind, shell).await;
            echo.show();
            println!(
                "PROBE ui-like {} {kind:?}: delta to in process {:+.2} ms (p50)",
                shell[0],
                echo.p50() - base.p50()
            );
        }
    }
}

/// The gap PowerShell shows under the server, one change at a time: the server opted out of
/// power throttling, the shells too, then ConPTY flags like portable-pty's. Each step is a new
/// server and a new pane, next to the same shell in process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "diagnostic experiment on shell timing: the timer-probe job runs it with --include-ignored"]
async fn powershell_echo_by_power_throttling_and_conpty_flags() {
    LENIENT.store(true, std::sync::atomic::Ordering::Relaxed);
    #[cfg(windows)]
    {
        let mut shells = vec![vec!["powershell.exe", "-NoLogo"]];
        if Command::new("pwsh.exe").arg("-Version").output().is_ok() {
            shells.push(vec!["pwsh.exe", "-NoLogo"]);
        }
        // The test process asks for no finer timer (the matrix test, which does, runs after this
        // one), so the system timer is at its default unless another process on the machine asked.
        println!("PROBE system timer period at the start of the steps: {:.2} ms", system_tick_ms());
        let steps: [(&str, &str, &[(&str, &str)]); 8] = [
            ("baseline", "off", &[]),
            ("1 server opted out", "off", &[("FORGETTY_POWER_THROTTLING", "server")]),
            ("2 server and shells opted out", "off", &[("FORGETTY_POWER_THROTTLING", "shells")]),
            ("3a conpty win32 input mode", "off", &[("FORGETTY_CONPTY_FLAGS", "0x4")]),
            ("3b conpty inherit cursor and win32 input", "off", &[("FORGETTY_CONPTY_FLAGS", "0x5")]),
            ("4a server timeBeginPeriod(1)", "period", &[]),
            ("4b server timeBeginPeriod(1) and throttling off", "full", &[]),
            ("5 server default (1 ms while in use)", "default", &[]),
        ];
        for shell in &shells {
            let base = in_process_with(&format!("in process, {}", shell[0]), shell);
            base.show();
            for (name, timer, env) in steps {
                *EXTRA_ENV.lock().unwrap() =
                    env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                let (echo, _) = through_server_as(timer, name, Pane::Fresh, shell).await;
                echo.show();
                println!(
                    "PROBE step {} {}: delta to in process {:+.2} ms (p50)",
                    shell[0],
                    name,
                    echo.p50() - base.p50()
                );
            }
            EXTRA_ENV.lock().unwrap().clear();
        }
    }
}
