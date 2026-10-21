#!/usr/bin/env bash
# P4a behaviour gate: OSC scanner, per-pane state, capture, agent CLI.
#
# Exits 0 only if every step passes:
#   1. `forgetty run --wait -- bash -c 'exit 7'` exits 7
#   2. an OSC 7501 waiting report: `events --json` shows StatusChanged with
#      status waiting and the decoded message, `wait --status waiting` exits 0,
#      `ls --json` carries the status; `wait --status done` times out (5)
#   3. OSC 2 title and OSC 7 cwd (a real `cd /tmp` and a reported-only path)
#      show in `ls --json`; OSC 9 shows as last_notification
#   4. `capture --trim` returns an earlier line, also after a server restart
#   5. `close` removes a kept pane and `events` shows PaneClosed (P-010)
#   6. the scanner benchmark prints its ns/byte and its share at 16 KiB chunks,
#      and the share stays under 1 % (SKIP_BENCH=1 skips). The real-drain share
#      is printed for information: it depends on load.
#
# Runs as the current user with all state under a temp dir (--state-dir).
# Env: PROFILE=debug|release (default debug), KEEP=1 keeps the temp dir.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$HOME/.local/zig:$PATH"
# Another process may hold the fixed UDP port 47297: use any free port.
export FORGETTY_PORT="${FORGETTY_PORT:-0}"
PROFILE="${PROFILE:-debug}"

if [ "$(id -u)" = "0" ]; then
  echo "refusing to run as root" >&2
  exit 2
fi

if [ "$PROFILE" = "release" ]; then
  cargo build --release -p forgetty-server || exit 2
  BIN="$ROOT/target/release"
else
  cargo build -p forgetty-server || exit 2
  BIN="$ROOT/target/debug"
fi

STATE="$(mktemp -d /tmp/p4agate.XXXXXX)"
BG_PIDS=()
fg() { "$BIN/forgetty" --state-dir "$STATE" "$@"; }
# `grep -q` leaves at the first match. A writer that still has output to send
# then gets SIGPIPE, and under `pipefail` the pipeline fails (the CLI died this
# way when the box was busy: 17 % of `server status | grep -q` under load).
# `qgrep` reads all its input, so the writer always finishes.
qgrep() { grep "$@" >/dev/null; }

cleanup() {
  for p in "${BG_PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  fg server stop >/dev/null 2>&1
  [ "${KEEP:-0}" = "1" ] || rm -rf "$STATE"
}
trap cleanup EXIT

step() { echo "--- $*"; }
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; echo "--- server log tail"; tail -25 "$STATE/data/forgetty/server.log" 2>/dev/null; exit 1; }

# pane_field PANE KEY: a top-level field of the pane in `ls --json`
# pane_path PANE JSONPATH: a nested field, e.g. status.message
pane_json() {
  fg ls --json | python3 -I -c '
import json, sys
pane, path = sys.argv[1], sys.argv[2].split(".")
for t in json.load(sys.stdin)["tabs"]:
    if t["pane_id"] == pane:
        v = t
        for k in path:
            v = v.get(k) if isinstance(v, dict) else None
        print("" if v is None else v)
        break
' "$1" "$2"
}

# poll_until "desc" tries cmd...: run cmd every 0.2 s until it succeeds
poll_until() {
  local desc="$1" tries="$2"; shift 2
  local i
  for i in $(seq 1 "$tries"); do
    "$@" && return 0
    sleep 0.2
  done
  fail "timed out: $desc"
}

field_is() { [ "$(pane_json "$1" "$2")" = "$3" ]; }

step "server start"
fg server start || fail "server start"

# ------------------------------------------------------------------ 1. run --wait
step "run --wait exit code"
fg run --wait -- bash -c 'exit 7' >"$STATE/run.out" 2>&1
RC=$?
[ "$RC" = 7 ] || { cat "$STATE/run.out"; fail "run --wait exited $RC, wanted 7"; }
fg run --wait -- true >/dev/null 2>&1 || fail "run --wait -- true did not exit 0"
pass "run --wait returns the command's exit code (7)"

# ------------------------------------------------------------------ 2. OSC 7501
step "OSC 7501 waiting report"
fg events --json >"$STATE/events.out" 2>"$STATE/events.err" &
BG_PIDS+=("$!")
sleep 1   # the event subscription is up before the pane reports
MSG="Approve the gate?"
B64="$(printf '%s' "$MSG" | base64 -w0)"
WPANE="$(fg new --keep -- bash -c "sleep 1; printf '\\033]7501;state=blocked:kind=permission:app=gate:msg=%s\\033\\\\' '$B64'; sleep 600")" \
  || fail "new (waiting pane)"
fg wait -p "$WPANE" --status waiting --timeout 20 || fail "wait --status waiting did not return 0 (rc=$?)"
field_is "$WPANE" status.status waiting || fail "ls --json status is not waiting"
field_is "$WPANE" status.message "$MSG" || fail "ls --json status.message is wrong: $(pane_json "$WPANE" status.message)"
field_is "$WPANE" status.kind permission || fail "ls --json status.kind is wrong"
python3 -I - "$STATE/events.out" "$WPANE" "$MSG" <<'PY' || fail "events --json has no StatusChanged waiting with the message"
import json, sys
path, pane, msg = sys.argv[1:4]
for line in open(path):
    try:
        e = json.loads(line)
    except ValueError:
        continue
    if e.get("event") == "StatusChanged" and e.get("pane_id") == pane \
            and e.get("status") == "waiting" and e.get("message") == msg:
        sys.exit(0)
sys.exit(1)
PY
fg wait -p "$WPANE" --status done --timeout 1 >/dev/null 2>&1
[ "$?" = 5 ] || fail "wait --status done should time out with exit 5"
pass "StatusChanged waiting with decoded message, wait --status waiting = 0, timeout = 5"

# ------------------------------------------------------------------ 3. title, cwd, notification
step "OSC 0/2 title, OSC 7 cwd, OSC 9 notification"
TPANE="$(fg new -- bash --norc --noprofile)" || fail "new (bash pane)"
sleep 1
fg send -p "$TPANE" "printf '\\033]2;my-title\\007'" --enter
poll_until "title my-title" 50 field_is "$TPANE" title "my-title"
fg send -p "$TPANE" "cd /tmp; printf '\\033]7;file://%s%s\\007' \"\$HOSTNAME\" \"\$PWD\"" --enter
poll_until "cwd /tmp" 50 field_is "$TPANE" cwd "/tmp"
fg send -p "$TPANE" "printf '\\033]7;file://localhost/srv/osc7-only\\007'" --enter
poll_until "cwd from OSC 7 only" 50 field_is "$TPANE" cwd "/srv/osc7-only"
# OSC 7 is a display hint: the directory new panes spawn in stays the real one.
field_is "$TPANE" spawn_cwd "/tmp" || fail "OSC 7 changed spawn_cwd: $(pane_json "$TPANE" spawn_cwd)"
fg send -p "$TPANE" "printf '\\033]9;gate-note\\007'" --enter
poll_until "notification" 50 field_is "$TPANE" last_notification.body "gate-note"
pass "title my-title, cwd /tmp and /srv/osc7-only, notification gate-note"

# ------------------------------------------------------------------ 4. capture
step "capture, also after a restart"
CPANE="$(fg new -- bash --norc --noprofile)" || fail "new (capture pane)"
sleep 1
fg send -p "$CPANE" 'echo CAPTURE_$((20+22))' --enter
cap_has() { fg capture -p "$CPANE" --trim 2>/dev/null | qgrep "^CAPTURE_42$"; }
poll_until "CAPTURE_42 on screen" 50 cap_has
[ "$(fg capture -p "$CPANE" --trim --lines 1 | wc -l)" = 1 ] || fail "capture --lines 1 is not one line"
fg capture -p "$CPANE" --trim --json | python3 -I -c 'import json,sys; d=json.load(sys.stdin); assert "CAPTURE_42" in d["text"]' \
  || fail "capture --json has no text"
pass "capture --trim shows CAPTURE_42"

fg server stop || fail "server stop"
kill "${BG_PIDS[0]}" 2>/dev/null; wait "${BG_PIDS[0]}" 2>/dev/null; BG_PIDS=()
fg server start || fail "server start (2)"
fg ls | qgrep "$CPANE" || { fg ls; fail "capture pane not listed after restart"; }
poll_until "CAPTURE_42 after restart" 50 cap_has
pass "capture --trim shows CAPTURE_42 after a server restart"

# ------------------------------------------------------------------ 5. close, PaneClosed
step "close removes a kept pane, events show PaneClosed"
fg events --json >"$STATE/events2.out" 2>/dev/null &
BG_PIDS+=("$!")
sleep 1
fg run --keep --wait -- sh -c 'echo kept-out; exit 3' >"$STATE/keep.out" 2>&1
RC=$?
[ "$RC" = 3 ] || { cat "$STATE/keep.out"; fail "run --keep --wait exited $RC, wanted 3"; }
KPANE="$(head -1 "$STATE/keep.out")"
fg ls | grep "$KPANE" | qgrep "exited 3" || { fg ls; fail "kept pane not listed as exited 3"; }
fg capture -p "$KPANE" --trim | qgrep kept-out || fail "kept pane capture lacks its output"
fg close -p "$KPANE" || fail "close"
fg ls | qgrep "$KPANE" && fail "pane still listed after close"
poll_until "PaneClosed event" 25 qgrep "\"PaneClosed\".*$KPANE\|$KPANE.*\"PaneClosed\"" "$STATE/events2.out"
fg close -p ffffff >/dev/null 2>&1
[ "$?" = 3 ] || fail "close of an unknown pane should exit 3"
pass "close removed the kept pane; PaneClosed event seen; unknown pane exits 3"

# ------------------------------------------------------------------ 6. benchmark
step "scanner benchmark"
if [ "${SKIP_BENCH:-0}" = "1" ]; then
  echo "SKIPPED (SKIP_BENCH=1)"
else
  BENCH_OUT="$(cargo bench -p forgetty-session --bench osc_scanner 2>&1)"
  BRC=$?
  echo "$BENCH_OUT" | grep -E "scanner alone|scanner cost|scanner adds|drain saw|REAL DRAIN|SCANNER COST|SCANNER OVERHEAD"
  [ "$BRC" = 0 ] || fail "scanner benchmark failed or is at 1 % or more"
  echo "$BENCH_OUT" | qgrep "SCANNER OVERHEAD" || fail "benchmark printed no number"
  pass "benchmark number printed, under 1 %"
fi

echo "P4A GATE: PASS"
exit 0
