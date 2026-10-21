//! V2-013 — Daemon hot-path micro-bench suite.
//!
//! Measures the in-process portion of the PTY → subscriber path. This is the
//! daemon-internal share of the latency budget defined in
//! `docs/architecture/ARCHITECTURE.md` §"Performance contract" and locks in
//! the v0.2 baseline after V2-001..V2-012.
//!
//! ## Scope
//!
//! - **In scope:** `SessionManager::process_pane_bytes` → broadcast subscriber
//!   wake; `SessionManager::write_pty` call latency; `SessionManager::new()`
//!   + first `create_tab` cold-start latency.
//! - **Out of scope:** real shell, real Unix socket, GTK rendering, iroh QUIC,
//!   pairing. Those are exercised by `scripts/perf/typometer.sh`.
//!
//! ## Hot-path invariants the bench defends
//!
//! - **AD-009:** no polling. Each measured iteration awaits the broadcast
//!   subscriber via `rx.recv().await` — that wake is event-driven, not timed.
//! - **AD-010:** raw bytes only. The bench writes `b"x"` and friends to the
//!   PTY; no encoding happens anywhere on the path it measures.
//! - **AD-013:** byte log writes complete synchronously under the same mutex
//!   guard as the broadcast send, before the subscriber wake — so a regression
//!   that adds latency before broadcast is caught here.
//!
//! ## Bench groups
//!
//! - `pty_to_subscriber_idle` — single pane, low-volume PTY traffic, p50 ≤ 500 µs
//!   target (AC-12).
//! - `pty_to_subscriber_load_10` — same metric on a target pane while 9 sibling
//!   panes emit background noise (AC-6). N=50 / N=100 variants run via
//!   Criterion's filter (`--bench daemon_hotpath -- pty_to_subscriber_load_50`).
//! - `send_input_latency` — `SessionManager::write_pty` call alone, no echo
//!   wait (AC-7).
//! - `daemon_cold_start` — `SessionManager::new()` + `create_tab` (AC-8).
//!
//! ## PTY backing process
//!
//! Each bench pane runs `cat`. `cat` is a deterministic pass-through: bytes
//! written to its stdin reappear on its stdout, no prompt, no startup banner,
//! no shell parsing latency. A real interactive shell would dominate the
//! measurement.
//!
//! ## Runtime reuse (per SPEC §4.2)
//!
//! Exactly one tokio multi-thread runtime is created at process startup and
//! reused across every bench. Creating a runtime per iteration is forbidden
//! (it spawns worker threads and adds milliseconds of unrelated work).
//!
//! ## Cleanup (R-5)
//!
//! All `SessionManager` instances created here live inside RAII guards
//! (`PaneSetup` / `LoadSetup` / `ColdStartGuard`) whose `Drop` impls call
//! `kill_all()`. If a bench panics, criterion still drops its locals; the
//! `cat` subprocesses get SIGKILLed.

use std::sync::OnceLock;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use forgetty_core::PaneId;
use forgetty_pty::PtySize;
use forgetty_session::{SessionEvent, SessionManager};
use tokio::sync::broadcast::Receiver;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default PTY dimensions for every bench pane. Real units don't matter — we
/// never render — but `cat` needs a sane size. 80x24 mirrors the daemon's
/// default-size constant in `forgetty-socket/src/handlers.rs`.
const BENCH_PTY_SIZE: PtySize = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };

/// Single-byte payload written each iteration. The PTY's line discipline
/// echoes input back to the master immediately (Linux ICANON+ECHO default), so
/// the subscriber wake we measure is the local-echo round-trip — `cat` itself
/// never sees this byte until a newline arrives. That is exactly what we want:
/// the smallest possible PTY round-trip with no shell-side buffering.
/// Keep this short — a multi-kB payload would conflate fan-out latency with
/// PTY-buffer drain time.
const BENCH_PAYLOAD: &[u8] = b"x";

/// Background-noise payload for the load groups. 64 bytes per write keeps the
/// `cat` reader busy without saturating the broadcast channel.
const NOISE_PAYLOAD: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ\n\n";

// ---------------------------------------------------------------------------
// Single-runtime singleton
// ---------------------------------------------------------------------------

/// Process-global tokio runtime. Created on first access via `runtime()` and
/// reused for every bench iteration. Per SPEC §4.2 we must never construct a
/// new runtime per iteration.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build bench tokio runtime")
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spawn a `SessionManager` with one `cat`-backed pane and return a fresh
/// subscriber. Held by a guard so the cat process is cleaned up on drop.
struct PaneSetup {
    sm: SessionManager,
    pane_id: PaneId,
    rx: Receiver<SessionEvent>,
}

impl PaneSetup {
    fn new() -> Self {
        // Enter the runtime context so `SessionManager::create_tab` can spawn
        // its drain task and the per-pane `ByteLog` disk appender (both call
        // `tokio::spawn` from synchronous code). Note: holding this guard while
        // also calling `runtime().block_on(...)` below is legal — `enter()`
        // only registers the runtime as the current one for sync-context
        // tokio APIs (`tokio::spawn`); it does not put us inside an async
        // task, so `block_on` from this same thread is allowed.
        let _guard = runtime().enter();

        let sm = SessionManager::new();
        // Subscribe BEFORE create_tab so the PaneCreated event lands in the rx
        // queue and we drain it below — guarantees the first event we observe
        // in the measured loop is a PtyOutput from our explicit write.
        let mut rx = sm.subscribe_output();
        let (pane_id, _tab_id) = sm
            .create_tab(0, None, BENCH_PTY_SIZE, Some(vec!["cat".to_string()]))
            .expect("create_tab(cat) should succeed");

        // Drain the lifecycle events (PaneCreated, TabCreated) so the measured
        // loop only sees the PtyOutput events it itself triggers.
        runtime().block_on(async {
            // Use a brief recv_with_timeout loop instead of fixed sleeps —
            // event delivery is deterministic for in-process broadcast.
            for _ in 0..8 {
                match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                    Ok(Ok(SessionEvent::PtyOutput { .. })) => break,
                    Ok(Ok(_)) => continue,
                    _ => break,
                }
            }
        });

        Self { sm, pane_id, rx }
    }
}

impl Drop for PaneSetup {
    fn drop(&mut self) {
        // R-5: kill the cat process so iterations don't leak file descriptors
        // or zombie children across the bench run. Brief sleep gives the
        // kernel time to reap the SIGKILL'd children and free the PTY slot
        // (`/dev/ptmx` is system-wide capped at 4096; bench groups run in
        // sequence and starvation here would falsely fail later groups).
        self.sm.kill_all();
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Multi-pane setup. Holds N `cat` processes; the first pane is the measured
/// target, the rest emit periodic noise.
struct LoadSetup {
    sm: SessionManager,
    target_pane_id: PaneId,
    noise_pane_ids: Vec<PaneId>,
    rx: Receiver<SessionEvent>,
}

impl LoadSetup {
    fn new(noise_panes: usize) -> Self {
        // Enter the runtime context so create_tab's spawn calls succeed.
        let _guard = runtime().enter();

        let sm = SessionManager::new();
        let mut rx = sm.subscribe_output();

        let (target_pane_id, _) = sm
            .create_tab(0, None, BENCH_PTY_SIZE, Some(vec!["cat".to_string()]))
            .expect("create_tab(target) should succeed");

        let mut noise_pane_ids = Vec::with_capacity(noise_panes);
        for _ in 0..noise_panes {
            let (pid, _) = sm
                .create_tab(0, None, BENCH_PTY_SIZE, Some(vec!["cat".to_string()]))
                .expect("create_tab(noise) should succeed");
            noise_pane_ids.push(pid);
            // Brief inter-spawn pause: openpty() rejects with ENOSPC when too
            // many PTYs are allocated in quick succession (likely a rate
            // throttle inside `devpts`). 500 us per pane is empirically enough
            // to keep N=100 stable on stock Ubuntu/Debian.
            std::thread::sleep(Duration::from_micros(500));
        }

        // Drain lifecycle events from the subscriber — same idea as PaneSetup.
        runtime().block_on(async {
            for _ in 0..(noise_panes * 2 + 4) {
                match tokio::time::timeout(Duration::from_millis(20), rx.recv()).await {
                    Ok(Ok(_)) => continue,
                    _ => break,
                }
            }
        });

        Self { sm, target_pane_id, noise_pane_ids, rx }
    }

    /// Push a small chunk of noise to every background pane. Each bench iter
    /// calls this once before measuring so the broadcast fan-out sees real
    /// per-pane traffic, surfacing any per-subscriber regression.
    fn pump_noise(&self) {
        for &pid in &self.noise_pane_ids {
            // Ignore errors: a noise pane may have closed mid-bench (PTY
            // buffer full, child reaped) — that pane simply stops contributing
            // load. The TARGET pane's measurement path is unaffected, so the
            // measured number for `pty_to_subscriber_load_N` remains
            // meaningful (it just observes slightly less fan-out than `N`).
            let _ = self.sm.write_pty(pid, NOISE_PAYLOAD);
        }
    }
}

impl Drop for LoadSetup {
    fn drop(&mut self) {
        // R-5: see PaneSetup::drop for rationale on the sleep. Larger setups
        // need slightly more time for the kernel to reap N children and
        // release N PTY slots before the next parameterized group starts.
        self.sm.kill_all();
        let panes = 1 + self.noise_pane_ids.len();
        // ~1 ms per pane is empirically enough on Linux; cap so this never
        // dominates the bench wall-clock budget.
        let sleep_ms = (panes as u64).min(500);
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }
}

/// Drain the subscriber until we see a `PtyOutput` for the given pane.
/// Returns when one is observed. Panics on RecvError other than Lagged
/// (Lagged means the broadcast queue overflowed — we resubscribe and retry,
/// but in the idle bench this should never happen).
async fn await_pty_output_for(rx: &mut Receiver<SessionEvent>, pane_id: PaneId) {
    loop {
        match rx.recv().await {
            Ok(SessionEvent::PtyOutput { pane_id: pid, .. }) if pid == pane_id => return,
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(e) => panic!("bench subscriber recv error: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Bench: pty_to_subscriber_idle
// ---------------------------------------------------------------------------

fn bench_pty_to_subscriber_idle(c: &mut Criterion) {
    let setup = PaneSetup::new();
    let pane_id = setup.pane_id;
    // Use `tokio::sync::Mutex` so the guard can be safely held across the
    // `recv().await` inside `await_pty_output_for`. (Criterion's `iter`
    // closure is FnMut, so we need interior mutability for the receiver;
    // `setup.rx` itself cannot be moved out because PaneSetup implements
    // Drop (R-5 cleanup).)
    let rx_cell = tokio::sync::Mutex::new(setup.rx.resubscribe());

    c.bench_function("pty_to_subscriber_idle", |b| {
        b.to_async(runtime()).iter(|| async {
            // write_pty triggers `cat` to echo BENCH_PAYLOAD on its stdout,
            // which the reader thread forwards via the unbounded mpsc, the
            // drain task picks up, and process_pane_bytes broadcasts.
            setup.sm.write_pty(pane_id, BENCH_PAYLOAD).expect("write_pty should succeed");
            let mut rx_guard = rx_cell.lock().await;
            await_pty_output_for(&mut rx_guard, pane_id).await;
        });
    });
}

// ---------------------------------------------------------------------------
// Bench: pty_to_subscriber_load_N
//
// Per SPEC AC-6: N=10 runs as part of the default `cargo bench` invocation;
// N=50 and N=100 are filter-only so they don't bloat the default run-time.
// They also allocate enough PTY/thread state that running all three back-
// to-back consistently exhausts the kernel's per-mount devpts allocator on
// stock Linux (we observed `ENOSPC` from `openpty` after N=10 + N=50 + N=100
// in sequence). Each variant is a separate Criterion group, so a developer
// can opt in via, e.g.:
//
//     cargo bench --bench daemon_hotpath -- pty_to_subscriber_load_50
//     cargo bench --bench daemon_hotpath -- pty_to_subscriber_load_100
// ---------------------------------------------------------------------------

fn run_load_bench(c: &mut Criterion, group_name: &str, noise_panes: usize, sample_size: usize) {
    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(1));
    group.sample_size(sample_size);
    group.bench_with_input(BenchmarkId::from_parameter(noise_panes), &noise_panes, |b, &n| {
        let setup = LoadSetup::new(n);
        let target_pane_id = setup.target_pane_id;
        let rx_cell = tokio::sync::Mutex::new(setup.rx.resubscribe());

        b.to_async(runtime()).iter(|| async {
            // Pump noise to background panes BEFORE the measured round-trip
            // so fan-out costs are realistic.
            setup.pump_noise();
            setup
                .sm
                .write_pty(target_pane_id, BENCH_PAYLOAD)
                .expect("target write_pty should succeed");
            let mut rx_guard = rx_cell.lock().await;
            await_pty_output_for(&mut rx_guard, target_pane_id).await;
        });
    });
    group.finish();
}

fn bench_pty_to_subscriber_load_10(c: &mut Criterion) {
    run_load_bench(c, "pty_to_subscriber_load_10", 10, 100);
}

/// Opt-in heavy load test (50 panes). Skipped by default to keep `cargo bench`
/// runtime under the SPEC AC-1 5-minute budget and to avoid PTY exhaustion
/// when chained with N=100 on stock Linux. Enable with:
///
///     BENCH_LOAD_HEAVY=1 cargo bench --bench daemon_hotpath -- pty_to_subscriber_load_50
fn bench_pty_to_subscriber_load_50(c: &mut Criterion) {
    if std::env::var_os("BENCH_LOAD_HEAVY").is_none() {
        eprintln!(
            "skipping pty_to_subscriber_load_50 (set BENCH_LOAD_HEAVY=1 to enable; AC-6 filter group)"
        );
        return;
    }
    run_load_bench(c, "pty_to_subscriber_load_50", 50, 30);
}

/// Opt-in heaviest load test (100 panes). Skipped by default — see
/// `bench_pty_to_subscriber_load_50` for rationale. Enable with:
///
///     BENCH_LOAD_HEAVY=1 cargo bench --bench daemon_hotpath -- pty_to_subscriber_load_100
fn bench_pty_to_subscriber_load_100(c: &mut Criterion) {
    if std::env::var_os("BENCH_LOAD_HEAVY").is_none() {
        eprintln!(
            "skipping pty_to_subscriber_load_100 (set BENCH_LOAD_HEAVY=1 to enable; AC-6 filter group)"
        );
        return;
    }
    run_load_bench(c, "pty_to_subscriber_load_100", 100, 30);
}

// ---------------------------------------------------------------------------
// Bench: send_input_latency
// ---------------------------------------------------------------------------

fn bench_send_input_latency(c: &mut Criterion) {
    let setup = PaneSetup::new();
    let pane_id = setup.pane_id;

    c.bench_function("send_input_latency", |b| {
        // Synchronous closure — write_pty returns immediately after acquiring
        // the SessionManagerInner mutex and writing to the PTY fd. We do NOT
        // wait for echo here; that would re-measure the broadcast path.
        b.iter(|| {
            setup.sm.write_pty(pane_id, BENCH_PAYLOAD).expect("write_pty should succeed");
        });
    });
}

// ---------------------------------------------------------------------------
// Bench: daemon_cold_start
// ---------------------------------------------------------------------------

/// RAII guard: kill the spawned `cat` when the per-iter measurement leaves
/// scope, even though Criterion's iter_with_large_drop arrangement holds
/// the value briefly. Prevents accumulating cat processes during this group.
struct ColdStartGuard {
    sm: SessionManager,
}

impl Drop for ColdStartGuard {
    fn drop(&mut self) {
        // R-5: per-iter cleanup. The cold-start group runs `sample_size(30)`
        // iterations; each spawns one cat. A short sleep prevents the
        // accumulated PTY-slot pressure from starving Criterion's later runs.
        self.sm.kill_all();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn bench_daemon_cold_start(c: &mut Criterion) {
    // Hold a runtime-context guard for the whole bench function so each
    // measured `create_tab` call inside the iter closure can spawn its drain
    // task and byte-log appender. In production, `SessionManager::new()` is
    // always called from within a tokio runtime — entering it here matches.
    let _guard = runtime().enter();

    let mut group = c.benchmark_group("daemon_cold_start");
    // Cold-start spawns a real PTY each iteration — keep the sample size
    // modest so the suite completes under five minutes. Criterion still
    // produces p50/p95/p99 from these samples.
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(15));

    group.bench_function("new_then_create_tab", |b| {
        b.iter_with_large_drop(|| {
            let sm = SessionManager::new();
            sm.set_byte_log_config(1024, 10);
            let (_pane_id, _tab_id) = sm
                .create_tab(0, None, BENCH_PTY_SIZE, Some(vec!["cat".to_string()]))
                .expect("create_tab should succeed");
            ColdStartGuard { sm }
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion harness
// ---------------------------------------------------------------------------

criterion_group!(
    name = benches;
    config = Criterion::default()
        // 5% noise threshold — perf gates should fire on >5% real movement,
        // not normal jitter.
        .noise_threshold(0.05)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(8));
    targets =
        bench_pty_to_subscriber_idle,
        bench_pty_to_subscriber_load_10,
        bench_pty_to_subscriber_load_50,
        bench_pty_to_subscriber_load_100,
        bench_send_input_latency,
        bench_daemon_cold_start,
);
criterion_main!(benches);
