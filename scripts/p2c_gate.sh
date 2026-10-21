#!/usr/bin/env bash
# P2c behaviour gate: a slow client never slows a program (AD-023).
#
# Exits 0 only if every step passes:
#   1. no code path lets a reader (a client stream) affect the PTY reader:
#      the reader registry is gone from the byte log and the server
#   2. unit tests: snapshot builder (screen, modes, title, cut anywhere),
#      formatter FFI, control queue (byte cap, one replaceable layout), snapshot hub (shared
#      builds, reuse, 2 at once, abandoned builds), catch-up backoff,
#      protocol goldens
#   3. throttled client (100 KB/s) on a paced 51 MiB program: run time within
#      10 % of the run time with no client, its final screen equals a fresh
#      `capture`, a fast client attached at the same time sees 0 resets and
#      byte-identical output
#   4. vim-like alt-screen session after a forced catch-up: screen, cursor,
#      alt screen, mouse, bracketed paste, keypad, cursor visibility and title
#      match the server-side VT
#   5. 20 throttled clients on one pane plus a re-attach loop: at most 2
#      snapshot builds at once, builds shared across streams, program run time
#      within 10 %
#   6. snapshot build time from an 8 MiB ring (release)
#   7. 100k requests with no reads: task count and RSS stay flat, the
#      connection closes; stalled Control reader with 6 and 31 panes
#   8. `cat` of 200 MiB: server ingest MB/s with a client attached
#   9. the earlier gates still pass: p0, p1, p4a, p1qa, p2b
#
# Runs as the current user. All server state is under temp dirs.
# Env: SKIP_OLD=1 skips step 8, ONLY="3 4" runs just those steps.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$HOME/.local/zig:$PATH"
export CARGO_INCREMENTAL=0

if [ "$(id -u)" = "0" ]; then
  echo "refusing to run as root" >&2
  exit 2
fi

cargo build --release -p forgetty-server --features qa-tools || exit 2
cargo test --release -p forgetty-client --no-run || exit 2
REAL="$ROOT/target/release/forgetty"

TMP="$(readlink -f "$(mktemp -d /tmp/p2c.XXXXXX)")"
LOGS="$TMP/logs"
mkdir -p "$LOGS"
FAIL=0
SUMMARY=()
# A failed run keeps its logs (and those of the nested gates inside them).
cleanup() {
  if [ "${KEEP:-0}" = "1" ] || [ "$FAIL" != "0" ]; then
    echo "logs kept in $LOGS"
  else
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

want() { [ -z "${ONLY:-}" ] || [[ " $ONLY " == *" $1 "* ]]; }
step() { echo "--- $*"; }
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; FAIL=1; }

CT_REL="cargo test --release -p forgetty-client --test"
ok_result() { grep -qE "^test result: ok\. [0-9]+ passed; 0 failed" "$1" 2>/dev/null; }

# ---------------------------------------------------------------------------
if want 1; then
  step "1. no reader registration can touch the PTY reader"
  if grep -rnE "register_reader|ReaderGuard|READER_STALL|held_back|reader_progress" \
       crates/forgetty-session/src crates/forgetty-server/src >"$LOGS/hold.txt"; then
    fail "reader hold code is still present"; cat "$LOGS/hold.txt"
  else
    pass "no reader registry in forgetty-session or forgetty-server"
    SUMMARY+=("no reader registry: the PTY waits only for the byte-log disk queue")
  fi
fi

# ---------------------------------------------------------------------------
if want 2; then
  step "2. unit tests"
  ok=1
  cargo test -p forgetty-session snapshot >"$LOGS/u-session.log" 2>&1 || ok=0
  cargo test -p forgetty-session byte_log >>"$LOGS/u-session.log" 2>&1 || ok=0
  cargo test -p forgetty-vt format >"$LOGS/u-vt.log" 2>&1 || ok=0
  cargo test -p forgetty-server --lib control_queue >"$LOGS/u-server.log" 2>&1 || ok=0
  cargo test -p forgetty-server --lib snapshot_hub >>"$LOGS/u-server.log" 2>&1 || ok=0
  cargo test -p forgetty-server --lib limiter >>"$LOGS/u-server.log" 2>&1 || ok=0
  cargo test -p forgetty-proto >"$LOGS/u-proto.log" 2>&1 || ok=0
  [ $ok = 1 ] && { pass "unit tests"; SUMMARY+=("unit tests: pass"); } || { fail "unit tests"; tail -20 "$LOGS"/u-*.log; }
fi

# ---------------------------------------------------------------------------
if want 3 || want 4 || want 5 || want 7; then
  FORGETTY_REAL_SERVER="$REAL" $CT_REL p2c -- --nocapture --test-threads=1 >"$LOGS/p2c.log" 2>&1
  grep -E "^test result|^test " "$LOGS/p2c.log" | sed 's/^/    /'
fi

if want 3; then
  step "3. throttled client: program run time, final screen, fast client"
  line=$(grep -h "slow client:" "$LOGS/p2c.log" | head -1)
  if [ -n "$line" ] && ok_result "$LOGS/p2c.log"; then
    pass "$line"; SUMMARY+=("$line")
  else
    fail "throttled client test"; grep -A12 "a_throttled" "$LOGS/p2c.log" | head -30
  fi
fi

if want 4; then
  step "4. alt-screen session after a forced catch-up"
  line=$(grep -h "vim catch-up:" "$LOGS/p2c.log" | head -1)
  if [ -n "$line" ] && ok_result "$LOGS/p2c.log"; then
    pass "$line"; SUMMARY+=("$line")
  else
    fail "alt-screen catch-up test"
  fi
fi

# ---------------------------------------------------------------------------
if want 5; then
  step "5. 20 throttled clients plus a re-attach loop"
  line=$(grep -h "snapshot load (" "$LOGS/p2c.log" | head -1 | sed 's/^test [a-z_0-9]* \.\.\. //')
  if [ -n "$line" ] && ok_result "$LOGS/p2c.log"; then
    pass "$line"; SUMMARY+=("$line")
  else
    fail "snapshot load test"
  fi
fi

# ---------------------------------------------------------------------------
if want 6; then
  step "6. snapshot build time from an 8 MiB ring (release)"
  cargo test --release -p forgetty-session build_time -- --nocapture >"$LOGS/build.log" 2>&1
  line=$(grep -h "snapshot of" "$LOGS/build.log" | head -1)
  if [ -n "$line" ] && ok_result "$LOGS/build.log"; then
    pass "$line (limit 500 ms, asserted)"; SUMMARY+=("$line")
  else
    fail "snapshot build time"; tail -10 "$LOGS/build.log"
  fi
fi

# ---------------------------------------------------------------------------
if want 7; then
  step "7. request flood, stalled Control reader"
  for pat in "request flood (list_clients" "request flood (list_tabs"; do
    l=$(grep -h "$pat" "$LOGS/p2c.log" | head -1)
    if [ -n "$l" ]; then echo "    $l"; SUMMARY+=("$l"); else fail "missing: $pat"; fi
  done
  if ! ok_result "$LOGS/p2c.log" || ! grep -q "request flood (list_tabs" "$LOGS/p2c.log"; then
    fail "request flood test"
  else
    pass "100k unread requests: flat tasks and RSS, connection closed"
  fi
  for n in 5 30; do
    STALL_PANES=$n FORGETTY_REAL_SERVER="$REAL" $CT_REL p2b stops_reading -- --nocapture --test-threads=1 \
      >"$LOGS/stall-$n.log" 2>&1
    l=$(grep -h "stalled control reader (" "$LOGS/stall-$n.log" | head -1 | sed 's/^test [a-z_]* \.\.\. //')
    if [ -n "$l" ] && ok_result "$LOGS/stall-$n.log"; then
      pass "$l"; SUMMARY+=("$l")
    else
      fail "stalled reader with $((n + 1)) panes"; tail -10 "$LOGS/stall-$n.log"
    fi
  done
fi

# ---------------------------------------------------------------------------
if want 8; then
  step "8. cat 200 MiB through the server"
  FORGETTY_REAL_SERVER="$REAL" $CT_REL p2b cat_200mb -- --nocapture --test-threads=1 >"$LOGS/cat.log" 2>&1
  l=$(grep -h "cat 200 MiB:" "$LOGS/cat.log" | head -1 | sed 's/^test [a-z_0-9]* \.\.\. //')
  if [ -n "$l" ] && ok_result "$LOGS/cat.log"; then
    pass "$l"; SUMMARY+=("$l")
  else
    fail "cat throughput"; tail -10 "$LOGS/cat.log"
  fi
fi

# ---------------------------------------------------------------------------
if want 9 && [ "${SKIP_OLD:-0}" != "1" ]; then
  step "9. earlier gates"
  for g in p0 p1 p4a p1qa p2b; do
    if SKIP_OLD=1 bash "scripts/${g}_gate.sh" >"$LOGS/old-$g.log" 2>&1; then
      pass "${g}_gate.sh"; SUMMARY+=("${g}_gate: pass")
    else
      fail "${g}_gate.sh (see $LOGS/old-$g.log)"; tail -15 "$LOGS/old-$g.log"
    fi
  done
fi

echo
echo "=== summary ==="
for l in "${SUMMARY[@]}"; do echo "  $l"; done
if [ $FAIL = 0 ]; then echo "P2C GATE: PASS"; else echo "P2C GATE: FAIL"; fi
exit $FAIL
