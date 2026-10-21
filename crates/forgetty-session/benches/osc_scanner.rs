//! OSC scanner cost on the drain path (AD-019, target: under 1 % of drain time).
//!
//! 1. The gated numbers: the scanner's cost per byte (ns/byte) over a 100 MiB
//!    mixed stream fed in FIXED 16 KiB chunks, hot in cache as the PTY read
//!    leaves them, and its share of the real drain time. The share is that
//!    cost over the whole stream divided by the best (shortest) real drain
//!    time with the scanner off. A loaded machine makes the PTY reader
//!    deliver small chunks (a drain at load 16 saw 453 B on average) and
//!    stretches the drain, and the per-chunk cost of the scanner over those
//!    chunks is not what the scanner costs per byte. Fixing the chunk size
//!    and taking the best of several runs for both numbers removes that.
//! 2. Information only: the scanner's cost over the chunking the real drain
//!    task actually saw, as a share of the median real drain time. It varies
//!    with machine load. If it stays at 1 % or more on a quiet machine (load
//!    under 6), that is a finding, not noise.
//! 3. Micro numbers at 64 KiB chunks and a dense OSC stress stream, against
//!    the in-process drain step (no PTY, so a much smaller denominator).
//!
//! Run: `cargo bench -p forgetty-session --bench osc_scanner`
//! The process exits 1 if the gated share is 1 % or more.

use std::time::{Duration, Instant};

use forgetty_session::SessionEvent;

use forgetty_pty::PtySize;
use forgetty_session::osc::OscScanner;
use forgetty_session::SessionManager;

const TOTAL: usize = 100 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;
/// The chunk size of the gated number: what a pane reader delivers when the
/// machine is not starved.
const GATE_CHUNK: usize = 16 * 1024;
const RUNS: usize = 5;
const E2E_RUNS: usize = 7;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn b64(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s)
}

/// 8 MiB of terminal output: colored build log lines, cursor moves, progress
/// bars, hyperlinks, plus titles, cwd reports, prompts, notifications and
/// status reports at realistic rates (one OSC of interest per few KiB).
fn mixed_stream(dense: bool, cwd_reports: bool) -> Vec<u8> {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut out = Vec::with_capacity(9 << 20);
    let words =
        ["compiling", "warning", "error", "Finished", "test", "ok", "src/lib.rs", "foo::bar"];
    let mut n = 0u64;
    while out.len() < 8 << 20 {
        n += 1;
        let r = rng.next();
        // A colored log line.
        let color = 30 + (r % 8);
        out.extend_from_slice(format!("\x1b[1;{color}m{:>5}\x1b[0m ", n % 10_000).as_bytes());
        for _ in 0..(3 + r % 9) {
            let w = words[(rng.next() % words.len() as u64) as usize];
            out.extend_from_slice(w.as_bytes());
            out.push(b' ');
        }
        out.extend_from_slice(b"\r\n");
        // Typical output has few OSCs of interest: a title and a prompt mark per
        // command, a status report or notification now and then. `dense` is the
        // stress variant with one in every ~8 lines.
        let slot = if dense { r % 64 } else { (r >> 8) % 512 };
        match slot {
            0 => out.extend_from_slice(b"\x1b[2K\x1b[1A\x1b[2K[=====>    ] 52%\r"),
            1 => out.extend_from_slice(b"\x1b]8;;https://example.com/x\x07link\x1b]8;;\x07\r\n"),
            2 => out.extend_from_slice(format!("\x1b]2;job {n}\x07").as_bytes()),
            3 if cwd_reports => out.extend_from_slice(b"\x1b]7;file://host/home/user/project\x07"),
            4 => out.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07"),
            5 => out.extend_from_slice(
                format!(
                    "\x1b]7501;state=working:app=cargo:progress={}:msg={}\x1b\\",
                    n % 100,
                    b64("Building crate")
                )
                .as_bytes(),
            ),
            6 => out.extend_from_slice(b"\x1b]9;build finished\x07"),
            7 => out.extend_from_slice(b"\x1b]777;notify;CI;green\x07"),
            _ => {}
        }
    }
    out
}

fn time<F: FnMut()>(mut f: F) -> Duration {
    (0..RUNS)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap()
}

/// Time the scanner alone and the real drain step over `stream`, fed in
/// chunks of `chunk` bytes.
fn measure(
    label: &str,
    stream: &[u8],
    chunk: usize,
    sm: &SessionManager,
    pane: forgetty_core::PaneId,
) {
    println!("{label}: {} MiB, {} KiB chunks", stream.len() >> 20, chunk >> 10);

    let mut events = 0usize;
    let scan = time(|| {
        let mut sc = OscScanner::new();
        events = 0;
        for c in stream.chunks(chunk) {
            sc.feed(c, &mut |_| events += 1);
        }
    });
    println!(
        "  scanner alone:         {:>8.1} ms  ({:.2} GB/s, {} events found)",
        scan.as_secs_f64() * 1e3,
        stream.len() as f64 / scan.as_secs_f64() / 1e9,
        events
    );

    let drain = time(|| {
        for c in stream.chunks(chunk) {
            sm.process_pane_bytes(pane, c).expect("drain");
        }
    });
    println!(
        "  drain step:            {:>8.1} ms  (includes the scanner)",
        drain.as_secs_f64() * 1e3
    );
    let without = drain.saturating_sub(scan);
    println!("  drain without scanner: {:>8.1} ms", without.as_secs_f64() * 1e3);
    let share = scan.as_secs_f64() / without.as_secs_f64() * 100.0;
    let ns_per_byte = scan.as_secs_f64() * 1e9 / stream.len() as f64;
    println!(
        "  scanner cost {ns_per_byte:.3} ns/byte, {share:.3} % of the in-process drain step \
         (no PTY: informational)"
    );
}

/// The scanner's cost per byte over `stream` in chunks of `chunk` bytes, in
/// nanoseconds. Each chunk is copied into a hot buffer first (as the PTY read
/// leaves it in cache) and the copy is subtracted. Best of [`RUNS`].
fn scanner_ns_per_byte(stream: &[u8], chunk: usize) -> f64 {
    let mut buf = vec![0u8; chunk];
    let copy_only = time(|| {
        for c in stream.chunks(chunk) {
            buf[..c.len()].copy_from_slice(c);
            std::hint::black_box(&buf);
        }
    });
    let copy_and_scan = time(|| {
        let mut sc = OscScanner::new();
        let mut n = 0usize;
        for c in stream.chunks(chunk) {
            buf[..c.len()].copy_from_slice(c);
            sc.feed(&buf[..c.len()], &mut |_| n += 1);
        }
        std::hint::black_box(n);
    });
    copy_and_scan.saturating_sub(copy_only).as_secs_f64() * 1e9 / stream.len() as f64
}

/// Run `file` through a real PTY into the drain task. Returns the time from
/// pane creation until the end marker has been drained.
fn pty_run(
    rt: &tokio::runtime::Runtime,
    sm: &SessionManager,
    file: &std::path::Path,
    mut record: Option<&mut Vec<bytes::Bytes>>,
) -> Duration {
    const MARKER: &[u8] = b"BENCH_DONE_MARKER";
    let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
    let cmd = vec!["sh".into(), "-c".into(), format!("cat '{}'; exec sleep 600", file.display())];
    let mut rx = sm.subscribe_output();
    let t0 = Instant::now();
    let (pane, _) = sm.create_tab(0, None, size, Some(cmd)).expect("pane");
    rt.block_on(async {
        let mut carry: Vec<u8> = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(60), rx.recv()).await {
                Ok(Ok(SessionEvent::PtyOutput { pane_id, data })) if pane_id == pane => {
                    carry.extend_from_slice(&data);
                    if let Some(r) = record.as_deref_mut() {
                        r.push(data);
                    }
                    if carry.windows(MARKER.len()).any(|w| w == MARKER) {
                        break;
                    }
                    let keep = carry.len().saturating_sub(MARKER.len());
                    carry.drain(..keep);
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                    // The tail is in the ring.
                    if let Some((ring, _)) = sm.get_ring_snapshot(pane) {
                        if ring.windows(MARKER.len()).any(|w| w == MARKER) {
                            break;
                        }
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => panic!("PTY run did not finish"),
            }
        }
    });
    let elapsed = t0.elapsed();
    let _ = sm.close_pane(pane);
    elapsed
}

fn median(v: &mut [Duration]) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// What the real-drain run found.
struct RealDrain {
    /// The scanner over the chunking the drain saw, as a share of the median
    /// drain time, percent. Information only.
    share: f64,
    /// Average chunk the drain saw, bytes.
    avg_chunk: usize,
    /// Shortest drain time with the scanner off.
    best_off: Duration,
    /// Bytes drained per run.
    bytes: usize,
}

/// Real PTY, real drain task.
///
/// The drain time of one run varies by about +-5 % between runs, more than the
/// scanner costs, so a difference of two medians cannot show a sub-percent
/// number. The cost is measured directly instead: the PTY output is captured
/// with the exact chunking the drain task saw, the scanner is timed over those
/// chunks, and that time is divided by the measured drain time with the
/// scanner off. The off/on medians are printed as a cross-check.
fn end_to_end(rt: &tokio::runtime::Runtime, sm: &SessionManager, stream: &[u8]) -> RealDrain {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("stream.bin");
    let mut data = stream.to_vec();
    data.extend_from_slice(b"\x1b]2;BENCH_DONE_MARKER\x07");
    std::fs::write(&file, &data).unwrap();

    println!("end to end through a real PTY: {} MiB", stream.len() >> 20);
    sm.set_osc_scanning(false);
    let mut chunks = Vec::new();
    let _ = pty_run(rt, sm, &file, Some(&mut chunks)); // warms the page cache, records chunking
    let (mut off, mut on) = (Vec::new(), Vec::new());
    for _ in 0..E2E_RUNS {
        sm.set_osc_scanning(false);
        off.push(pty_run(rt, sm, &file, None));
        sm.set_osc_scanning(true);
        on.push(pty_run(rt, sm, &file, None));
    }
    sm.set_osc_scanning(true);
    let (m_off, m_on) = (median(&mut off), median(&mut on));
    println!(
        "  scanner off: median {:.1} ms ({:.0} MB/s)   scanner on: median {:.1} ms ({:.0} MB/s)",
        m_off.as_secs_f64() * 1e3,
        data.len() as f64 / m_off.as_secs_f64() / 1e6,
        m_on.as_secs_f64() * 1e3,
        data.len() as f64 / m_on.as_secs_f64() / 1e6,
    );
    let ms = |v: &[Duration]| {
        v.iter().map(|d| format!("{:.0}", d.as_secs_f64() * 1e3)).collect::<Vec<_>>().join(" ")
    };
    println!("  runs off [{}] on [{}] (ms)", ms(&off), ms(&on));
    println!(
        "  off/on difference {:+.2} % (run-to-run spread is larger than the scanner cost)",
        (m_on.as_secs_f64() - m_off.as_secs_f64()) / m_off.as_secs_f64() * 100.0
    );

    let sizes: Vec<usize> = chunks.iter().map(|c| c.len()).collect();
    let total: usize = sizes.iter().sum();
    let avg = total / sizes.len().max(1);
    // In the server a chunk is hot in cache when the scanner sees it: the PTY
    // read just wrote it. Replaying 100 MiB from RAM would charge the scanner
    // for DRAM reads it never pays. So each chunk is first copied into a hot
    // buffer (as the PTY read does), and the cost of that copy is subtracted.
    let mut buf = vec![0u8; 256 * 1024];
    let copy_only = time(|| {
        for c in &chunks {
            buf[..c.len()].copy_from_slice(c);
            std::hint::black_box(&buf);
        }
    });
    let copy_and_scan = time(|| {
        let mut sc = OscScanner::new();
        let mut n = 0usize;
        for c in &chunks {
            buf[..c.len()].copy_from_slice(c);
            sc.feed(&buf[..c.len()], &mut |_| n += 1);
        }
        std::hint::black_box(n);
    });
    let scan = copy_and_scan.saturating_sub(copy_only);
    let overhead = scan.as_secs_f64() / m_off.as_secs_f64() * 100.0;
    println!(
        "  drain saw {} chunks, avg {} B; scanner over the same chunks: {:.1} ms",
        sizes.len(),
        avg,
        scan.as_secs_f64() * 1e3
    );
    println!(
        "  scanner over the drain's own chunks adds {overhead:.2} % ({:.1} ms of {:.1} ms median)",
        scan.as_secs_f64() * 1e3,
        m_off.as_secs_f64() * 1e3
    );
    RealDrain {
        share: overhead,
        avg_chunk: avg,
        best_off: *off.iter().min().unwrap(),
        bytes: data.len(),
    }
}

fn replicate(template: &[u8]) -> Vec<u8> {
    let reps = TOTAL.div_ceil(template.len());
    let mut stream = Vec::with_capacity(reps * template.len());
    for _ in 0..reps {
        stream.extend_from_slice(template);
    }
    stream.truncate(TOTAL);
    stream
}

fn main() {
    // Keep byte logs out of the user's real state.
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_DATA_HOME", tmp.path());

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let _g = rt.enter();
    let sm = SessionManager::new();
    let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
    let (pane, _) =
        sm.create_tab(0, None, size, Some(vec!["sleep".into(), "600".into()])).expect("pane");
    // A live subscriber, drained as fast as it is fed, as a connected client is.
    let mut rx = sm.subscribe_output();
    rt.spawn(async move {
        while !matches!(rx.recv().await, Err(tokio::sync::broadcast::error::RecvError::Closed)) {}
    });

    println!("-- micro, 64 KiB chunks (informational)");
    let mixed = replicate(&mixed_stream(false, true));
    measure("mixed stream", &mixed, CHUNK, &sm, pane);
    println!();
    // Stress: an OSC of interest in about every 8th line.

    measure("dense OSC stress stream", &replicate(&mixed_stream(true, true)), CHUNK, &sm, pane);
    let _ = sm.close_pane(pane);

    // Without OSC 7 here, so the stream matches what the real-drain run uses.
    let stream = replicate(&mixed_stream(false, false));
    println!();
    println!("-- gated: scanner cost at fixed {} KiB chunks", GATE_CHUNK >> 10);
    let ns_per_byte = scanner_ns_per_byte(&stream, GATE_CHUNK);
    println!("  scanner cost {ns_per_byte:.3} ns/byte ({:.2} GB/s)", 1.0 / ns_per_byte);

    println!();
    println!(
        "-- real drain (its own share is information; the best time is the gate's denominator)"
    );
    let real = end_to_end(&rt, &sm, &stream);
    println!(
        "REAL DRAIN (information): scanner share {:.2} %, drain chunks averaged {} B, best \
         drain {:.0} ms (chunk size and drain time move with machine load, check `uptime`)",
        real.share,
        real.avg_chunk,
        real.best_off.as_secs_f64() * 1e3
    );
    let scan_ms = ns_per_byte * real.bytes as f64 / 1e6;
    let gated = scan_ms / (real.best_off.as_secs_f64() * 1e3) * 100.0;
    println!();
    println!("SCANNER COST: {ns_per_byte:.3} ns/byte at {} KiB chunks", GATE_CHUNK >> 10);
    println!(
        "SCANNER OVERHEAD (100 MiB mixed stream, {} KiB chunks, best real drain): {gated:.2} % \
         ({scan_ms:.1} ms of {:.0} ms) (target < 1 %)",
        GATE_CHUNK >> 10,
        real.best_off.as_secs_f64() * 1e3
    );
    if gated >= 1.0 {
        std::process::exit(1);
    }
}
