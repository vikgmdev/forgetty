#!/usr/bin/env bash
#
# V2-013 — End-to-end typometer for the daemon -> stream-test path.
#
# Measures keystroke-to-byte-receipt latency over the iroh QUIC stream by
# wrapping `forgetty-stream-test --pane-id <uuid>` with `hyperfine`. Each
# hyperfine iteration restarts `forgetty-stream-test` (the dialing client);
# the daemon stays running across all iterations.
#
# WHAT THIS MEASURES
#
#   - From: stream-test process spawn (TCP/iroh QUIC connect + open_bi)
#   - To:   first `PASS: first PtyBytes received` line printed by stream-test
#
#   This is end-to-end across:
#     iroh dial -> stream auth -> Subscribe RPC -> daemon snapshot ->
#     PTY echo (cat) -> ByteLog tee -> broadcast -> framing -> iroh send.
#
#   It is intentionally coarser than the in-process Criterion bench
#   (`benches/daemon_hotpath.rs`); the two together bracket the latency
#   budget defined in `docs/architecture/ARCHITECTURE.md`.
#
# WHAT THIS DOES NOT MEASURE
#
#   - GTK rendering. Display-server perf is out of scope (AC-3, SPEC §5).
#
# FIRST-RUN PAIRING (R-4)
#
#   `forgetty-stream-test` connects via iroh; this requires the
#   `pair-test.key` identity to be authorized by the daemon. On first run
#   you MUST start the daemon manually with `--allow-pairing` once. The
#   typometer launches its own daemon (without `--allow-pairing`); for the
#   very first pairing, do this in a separate terminal:
#
#       ./target/release/forgetty-daemon --foreground --allow-pairing
#       ./target/release/forgetty-stream-test --dial $(./target/release/forgetty-daemon --show-pairing-qr | awk '/node_id:/ {print $2}') --pair-first --list-panes
#
#   Then re-run `./scripts/perf/typometer.sh`. The pair-test.key is reused.

set -euo pipefail

# ---------------------------------------------------------------------------
# Resolve workspace root regardless of where the script is invoked from.
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$WORKSPACE_ROOT"

DAEMON_BIN="$WORKSPACE_ROOT/target/release/forgetty-daemon"
STREAM_TEST_BIN="$WORKSPACE_ROOT/target/release/forgetty-stream-test"

# ---------------------------------------------------------------------------
# Per-run isolated paths so concurrent runs don't clash.
# ---------------------------------------------------------------------------

RUN_UUID="$(cat /proc/sys/kernel/random/uuid)"
SOCKET_PATH="/tmp/typometer-${RUN_UUID}.sock"
DAEMON_LOG="/tmp/typometer-${RUN_UUID}.log"
DAEMON_PID=""

# Number of hyperfine iterations. Per SPEC §5: 100, not 1000.
HYPERFINE_RUNS=100

# Hyperfine reports times in seconds. We convert to milliseconds for output
# clarity (terminal latency is in the ms regime).

# ---------------------------------------------------------------------------
# Cleanup trap (AC-20). Runs on normal exit, SIGINT, SIGTERM.
# ---------------------------------------------------------------------------

cleanup() {
    local exit_code=$?
    set +e
    echo "[typometer] Cleaning up..."
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill "$DAEMON_PID" 2>/dev/null || true
        # Give daemon a moment to flush logs and unlink the socket.
        for _ in 1 2 3 4 5; do
            kill -0 "$DAEMON_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -9 "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -f "$SOCKET_PATH"
    rm -f "$DAEMON_LOG"
    exit "$exit_code"
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Pre-flight checks (AC-15, AC-16).
# ---------------------------------------------------------------------------

if ! command -v hyperfine >/dev/null 2>&1; then
    cat >&2 <<'EOF'
[typometer] ERROR: `hyperfine` is not installed.

Install with one of:

    cargo install hyperfine
    sudo apt install hyperfine            # Debian/Ubuntu
    brew install hyperfine                # macOS
EOF
    exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
    cat >&2 <<'EOF'
[typometer] ERROR: `jq` is not installed.

Install with:

    sudo apt install jq                   # Debian/Ubuntu
    brew install jq                       # macOS
EOF
    exit 1
fi

if [[ ! -x "$DAEMON_BIN" ]]; then
    cat >&2 <<EOF
[typometer] ERROR: \`$DAEMON_BIN\` not found or not executable.

Build it first:

    cd $WORKSPACE_ROOT
    cargo build --release --features qa-tools
EOF
    exit 1
fi

if [[ ! -x "$STREAM_TEST_BIN" ]]; then
    cat >&2 <<EOF
[typometer] ERROR: \`$STREAM_TEST_BIN\` not found or not executable.

\`forgetty-stream-test\` requires the qa-tools feature flag. Build with:

    cd $WORKSPACE_ROOT
    cargo build --release --features qa-tools
EOF
    exit 1
fi

echo "[typometer] Checking dependencies... ok"

# ---------------------------------------------------------------------------
# Launch daemon (AC-17).
# ---------------------------------------------------------------------------

echo "[typometer] Starting daemon (socket=$SOCKET_PATH)..."
RUST_LOG="${RUST_LOG:-warn}" "$DAEMON_BIN" \
    --foreground \
    --session-id "$RUN_UUID" \
    --socket-path "$SOCKET_PATH" \
    >"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!

# Wait for the socket file to appear, polling every 0.1 s, max 10 s.
for _ in $(seq 1 100); do
    if [[ -S "$SOCKET_PATH" ]]; then
        break
    fi
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "[typometer] ERROR: daemon exited before socket appeared. Log:" >&2
        tail -50 "$DAEMON_LOG" >&2 || true
        exit 1
    fi
    sleep 0.1
done

if [[ ! -S "$SOCKET_PATH" ]]; then
    echo "[typometer] ERROR: socket $SOCKET_PATH did not appear within 10 s" >&2
    exit 1
fi

echo "[typometer] Daemon socket ready"

# ---------------------------------------------------------------------------
# Get the daemon's iroh node_id. We use --show-pairing-qr because that's the
# documented public way to print it (forgetty-stream-test source has the same
# instruction). Doing it through a second daemon invocation is fine because
# `--show-pairing-qr` exits immediately without touching the running daemon.
# ---------------------------------------------------------------------------

echo "[typometer] Resolving daemon node_id..."
NODE_ID="$("$DAEMON_BIN" --show-pairing-qr 2>/dev/null | awk '/^node_id:/ {print $2; exit}')"
if [[ -z "$NODE_ID" ]]; then
    echo "[typometer] ERROR: could not extract node_id from forgetty-daemon --show-pairing-qr" >&2
    exit 1
fi
# Defense-in-depth: validate the node_id is hex before passing it through to
# hyperfine's shell-evaluated command string (audit F-S2). iroh node IDs are
# 32-byte ed25519 pubkeys; expressed as hex they are 64 chars (or shorter for
# bech32-like encodings ≥52 chars in practice). If the daemon's output is
# ever malformed, we fail loudly instead of executing arbitrary text.
if [[ ! "$NODE_ID" =~ ^[a-f0-9]{52,128}$ ]]; then
    echo "[typometer] ERROR: node_id '$NODE_ID' is not a hex string of the expected length" >&2
    exit 1
fi
echo "[typometer] node_id=$NODE_ID"

# ---------------------------------------------------------------------------
# Create a tab via socket JSON-RPC `new_tab`. Extract the pane_id (UUID).
# ---------------------------------------------------------------------------

echo "[typometer] Creating tab via socket new_tab RPC..."
NEW_TAB_RESPONSE="$(
    printf '{"jsonrpc":"2.0","method":"new_tab","params":{"workspace_idx":0,"rows":24,"cols":80},"id":1}\n' \
        | socat - "UNIX-CONNECT:$SOCKET_PATH"
)"

PANE_ID="$(printf '%s' "$NEW_TAB_RESPONSE" | jq -er '.result.pane_id // empty')"
if [[ -z "$PANE_ID" ]]; then
    echo "[typometer] ERROR: new_tab did not return a pane_id. Response was:" >&2
    printf '%s\n' "$NEW_TAB_RESPONSE" >&2
    exit 1
fi
# Defense-in-depth: pane IDs are uuid::Uuid::new_v4() server-side. Validate
# the format before passing to hyperfine's shell-evaluated command string
# (audit F-S2).
if [[ ! "$PANE_ID" =~ ^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$ ]]; then
    echo "[typometer] ERROR: pane_id '$PANE_ID' is not a UUID" >&2
    exit 1
fi
echo "[typometer] pane_id=$PANE_ID"

# Give the new pane a moment to settle (PTY spawn + initial banner).
sleep 0.3

# ---------------------------------------------------------------------------
# Run hyperfine wrapping forgetty-stream-test (AC-18, AC-19).
# ---------------------------------------------------------------------------

# Send a single byte to the daemon's PTY before each hyperfine iteration so
# the freshly-spawned stream-test reliably observes a `PtyBytes` frame.
# Without this, stream-test's "PASS: first PtyBytes received" never fires
# on a quiescent `cat` pane.
PREPARE_CMD=$(cat <<EOF
printf '{"jsonrpc":"2.0","method":"send_input","params":{"pane_id":"$PANE_ID","data":"x"},"id":1}\n' \
    | socat - "UNIX-CONNECT:$SOCKET_PATH" >/dev/null
EOF
)

# Hyperfine writes JSON output to a temp file we parse for percentiles.
HYPERFINE_JSON="/tmp/typometer-${RUN_UUID}-hyperfine.json"

echo "[typometer] Running hyperfine ($HYPERFINE_RUNS iterations)..."

# Note: --runs $HYPERFINE_RUNS, --warmup 5 (small warmup so process-spawn
# caches stabilise), --prepare runs PREPARE_CMD before each iteration so
# the stream-test client observes one fresh PtyBytes batch.
#
# --max-msgs 1: PREPARE_CMD writes a single byte to the daemon's PTY each
# iteration, which produces exactly one PtyBytes broadcast (PTY local-echo).
# stream-test's default exit threshold is 5 messages — without --max-msgs 1
# the iteration would wait forever for messages 2..5 and hyperfine would
# hang during warmup.
if ! hyperfine \
    --runs "$HYPERFINE_RUNS" \
    --warmup 5 \
    --prepare "$PREPARE_CMD" \
    --export-json "$HYPERFINE_JSON" \
    "$STREAM_TEST_BIN --dial $NODE_ID --pane-id $PANE_ID --max-msgs 1" 2>&1; then
    cat >&2 <<EOF

[typometer] ERROR: hyperfine returned non-zero. Likely causes:

  * Pairing not done. The pair-test.key identity must be authorized.
    On first use only, run:

      $DAEMON_BIN --foreground --allow-pairing &
      $STREAM_TEST_BIN --dial $NODE_ID --pair-first --list-panes
      kill %1

    Then re-run this script.

  * The cat-backed pane has no output. send_input above writes 'x' once
    per iteration; if hyperfine's --prepare phase failed, the iteration
    times out waiting for echo.

  * Daemon log (last 50 lines):
EOF
    tail -50 "$DAEMON_LOG" >&2 || true
    exit 1
fi

# ---------------------------------------------------------------------------
# Parse hyperfine JSON for p50/p95/p99 (AC-19 format).
# ---------------------------------------------------------------------------

# hyperfine JSON: results[0].times = list of seconds for each run.
# Compute percentiles ourselves (hyperfine doesn't print p95/p99 by default).
#
# Defense-in-depth (audit F-S1): pass the path through env (HYPERFINE_JSON)
# instead of textually interpolating $HYPERFINE_JSON into the python source.
# The heredoc is single-quoted ('EOF') so bash performs no expansion inside.
PCT_OUTPUT="$(HYPERFINE_JSON="$HYPERFINE_JSON" python3 - <<'EOF'
import json, os
with open(os.environ["HYPERFINE_JSON"]) as f:
    data = json.load(f)
times_s = sorted(data["results"][0]["times"])
n = len(times_s)
def pct(p):
    k = (n - 1) * p
    f = int(k)
    c = min(f + 1, n - 1)
    return times_s[f] + (times_s[c] - times_s[f]) * (k - f)
# Convert to milliseconds.
print(f"typometer p50: {pct(0.50) * 1000:.2f} ms")
print(f"typometer p95: {pct(0.95) * 1000:.2f} ms")
print(f"typometer p99: {pct(0.99) * 1000:.2f} ms")
EOF
)"

echo
printf '%s\n' "$PCT_OUTPUT"

# Clean up the hyperfine JSON file (the trap handles socket + log).
rm -f "$HYPERFINE_JSON"
