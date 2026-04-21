# Forgetty Performance Harness

This directory ships with two complementary tools that together lock the v0.2
performance baseline for Forgetty's daemon hot path. Per V2-013 (SPEC §1) and
`docs/architecture/ARCHITECTURE.md` §"Performance contract", every release cycle
must run these and record real numbers in the review.

| Tool | Scope | Lives at | Output |
| --- | --- | --- | --- |
| Criterion micro-bench | In-process daemon hot path (PTY bytes -> broadcast subscriber) | `benches/daemon_hotpath.rs` | `docs/benchmarks/baseline.json` + `target/criterion/.../report/index.html` |
| Typometer | End-to-end keystroke-to-byte over iroh QUIC (real daemon, real `forgetty-stream-test` client) | `scripts/perf/typometer.sh` | `docs/benchmarks/typometer-baseline.txt` |

The micro-bench is fast (3-5 minutes, no display, no socket) and lets you
attribute regressions to the daemon-internal portion of the budget. The
typometer is slow (~90 s for 100 iterations) and measures the full
PTY->broadcast->framing->iroh->client receipt path. Together they bracket the
1.5 ms PTY-to-pixel target documented in `ARCHITECTURE.md`. GTK render time is
out of scope for both (SPEC §5, AC-3).

---

## Prerequisites

```bash
# Rust toolchain (stable)
cargo --version

# Both binaries built in release mode with the qa-tools feature
cd ${FORGETTY_REPO_ROOT}
cargo build --release --features qa-tools

# Tools used by the typometer shell script
sudo apt install hyperfine jq socat        # Debian/Ubuntu
# or
brew install hyperfine jq socat            # macOS
# or
cargo install hyperfine                    # any platform with cargo
```

### File-descriptor limit for `cargo bench`

Before running `cargo bench --bench daemon_hotpath`, raise the per-shell open-fd
limit to at least 4096:

```bash
ulimit -n 4096 && cargo bench --bench daemon_hotpath
```

Why: PTY master file descriptors from earlier bench groups linger briefly while
their drain tasks finish on the bench's process-global tokio runtime; by the
time the `pty_to_subscriber_load_10` group runs, the default limit of 1024 has
been exhausted and `openpty()` (or the subsequent `dup`) panics with
`dup of fd 1022 failed`. Bumping to 4096 absorbs the leak.

This is a workaround. The underlying issue — `SessionManager` clones held by
spawned drain tasks for slightly longer than the benchmark's per-iteration
guard — is tracked separately (see `` "drain-task Arc cycle"). Once
that lands, the `ulimit` bump is no longer required and this section will be
removed.

---

## 1. Criterion micro-bench

### What it measures

Four bench groups (SPEC AC-5 through AC-8):

| Group | What | SPEC ceiling |
| --- | --- | --- |
| `pty_to_subscriber_idle` | Single-pane `write_pty` -> broadcast subscriber wake | p50 <= 500 us (AC-12) |
| `pty_to_subscriber_load_10` | Same, with 9 sibling panes emitting noise | (no hard ceiling; reported in baseline) |
| `send_input_latency` | `SessionManager::write_pty` call only (no echo wait) | sub-ms |
| `daemon_cold_start` | `SessionManager::new()` + first `create_tab(cat)` | < 500 ms (AC-8) |

What it does NOT measure: real Unix socket, GTK, the iroh transport, real
shells. PTY processes are deterministic (`cat`) so output timing is stable.

### Running it

```bash
cd ${FORGETTY_REPO_ROOT}

# Default suite (idle, load_10, send_input, cold_start)
cargo bench --bench daemon_hotpath

# View the HTML report
xdg-open target/criterion/daemon_hotpath/report/index.html
```

### Heavier load (N=50 and N=100 panes)

These groups are env-gated to avoid PTY exhaustion when the default suite is
combined with subsequent runs (Linux devpts allocator pressure on a busy box).

```bash
BENCH_LOAD_HEAVY=1 cargo bench --bench daemon_hotpath -- \
    'pty_to_subscriber_load_(50|100)'
```

If you see `ENOSPC` from `openpty` mid-run, close other PTY-heavy processes
(other terminals, screen sessions) and rerun. The bench's per-iteration
teardown sleeps briefly to give devpts time to reclaim slots.

### Interpreting results

Criterion prints lines like:

```
daemon_hotpath/pty_to_subscriber_idle
                        time:   [14.84 us 15.20 us 15.57 us]
```

The middle number is the point estimate (mean of the slope estimator). The
HTML report shows p50/p95/p99 distributions. Per AC-9, the noise estimate
("slope" or "median absolute deviation") should be under 10%; if not, close
heavy background processes and rerun (R-1 in the SPEC).

### Reproducibility check (AC-10)

Run `cargo bench --bench daemon_hotpath -- pty_to_subscriber_idle` twice.
The two p50 values must agree within 15% on the same machine. If they don't,
something else is using the box. This is human-verified, not enforced.

---

## 2. Typometer (end-to-end)

### What it measures

The wall-clock time from `forgetty-stream-test` process spawn to its
`PASS: received 1 PtyBytes messages` line (one message because the script
launches stream-test with `--max-msgs 1` to match the single byte of PTY
local-echo produced by each `--prepare` hook). This covers:

- iroh dial + connection authentication (R-4)
- `Subscribe` RPC + daemon snapshot reply
- PTY echo from `cat` (because the script writes one byte before each
  iteration via the `send_input` JSON-RPC over the daemon's Unix socket)
- `ByteLog` tee + tokio broadcast send
- Binary framing on the iroh QUIC stream
- Receive + decode in `forgetty-stream-test`

### Running it

First-run pairing (one-time per machine):

```bash
# Terminal A
./target/release/forgetty-daemon --foreground --allow-pairing

# Terminal B
NODE_ID=$(./target/release/forgetty-daemon --show-pairing-qr | \
    awk '/^node_id:/ {print $2; exit}')
./target/release/forgetty-stream-test --dial "$NODE_ID" \
    --pair-first --list-panes
# Stop the daemon in Terminal A with Ctrl-C once "PASS" prints.
```

The `pair-test.key` identity is now authorized. From here on:

```bash
./scripts/perf/typometer.sh
```

The script launches its own daemon (separate socket, isolated `--session-id`),
creates one tab via JSON-RPC, runs `hyperfine --runs 100 --warmup 5`, prints
p50/p95/p99 in milliseconds, and cleans up.

Expected output tail:

```
typometer p50: 3.10 ms
typometer p95: 5.40 ms
typometer p99: 8.20 ms
[typometer] Cleaning up...
```

### What it does NOT measure

GTK rendering. Display-server perf is out of scope (SPEC AC-3, §5).
`subscribe_layout` stream latency is out of scope (SPEC §5).

### Iteration count

100 iterations, not 1000. The BACKLOG entry suggested 1000 but each iteration
respawns `forgetty-stream-test` which dials the daemon over iroh QUIC --
~200-400 ms of connection overhead per iter. 1000 would take 3-7 minutes;
100 gives stable percentiles in ~90 s. Locked in SPEC §5.

---

## 3. Updating the baselines

Baselines live in `docs/benchmarks/` and are committed. Regenerate **only when the change is intentional**:

```bash
# Re-run the bench, capture numbers manually, edit baseline.json
cargo bench --bench daemon_hotpath
$EDITOR docs/benchmarks/baseline.json   # update p50_us/p95_us/p99_us + git_sha + timestamp

# Re-run the typometer, append a new measurement block to the baseline
./scripts/perf/typometer.sh
$EDITOR docs/benchmarks/typometer-baseline.txt
```

Commit with a message that names the cause of the shift, e.g.:

```
perf(V2-XXX): rebaseline pty_to_subscriber_idle after broadcast change
```

This makes regressions visible in `git log` for the benchmark files and
forces every shift to be deliberate. R-2 in the SPEC.

### Schema for `baseline.json`

```json
{
  "schema_version": 1,
  "timestamp": "ISO-8601",
  "git_sha": "abbrev",
  "hostname": "hostname -s",
  "machine_class": "string set manually",
  "benchmarks": [
    { "name": "...", "p50_us": 0, "p95_us": 0, "p99_us": 0,
      "unit": "microseconds", "notes": "" }
  ]
}
```

`machine_class` is a free-form descriptor (e.g. `vick-dev-linux`,
`ci-runner-x86_64`). Used only for cross-machine eyeballing -- not normalized.

---

## 4. Regression detection (AC-24)

To validate that the bench actually catches regressions, on a scratch branch:

1. Add `std::thread::sleep(std::time::Duration::from_millis(5));` near the
   top of `SessionManager::process_pane_bytes` in
   `crates/forgetty-session/src/manager.rs`.
2. Run `cargo bench --bench daemon_hotpath -- pty_to_subscriber_idle`.
3. The reported time should jump from ~15 us to ~5000 us (~300x). Any sane
   regression filter would catch this.
4. Revert the change.

Do not commit the injected sleep.

---

## 5. CI integration

Out of scope for V2-013. The harness ships with manual gates only. Wiring
`cargo bench` and `typometer.sh` into CI is a separately-scoped follow-up.
Until that lands, the gate is human discipline at QA time.
