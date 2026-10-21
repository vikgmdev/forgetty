#!/usr/bin/env bash
# P1-QA behaviour gate: one step per reviewed defect of P0/P1.
#
# Exits 0 only if every step passes:
#   1. revoke cuts a live iroh connection: pair, hold a stream, revoke, the
#      held client ends within 1 s, `client ls` shows 0, a marker sent after
#      the revoke never reaches it
#   2. concurrent first start: 3x `server run` + 2x `server start` on a fresh
#      state dir, ROUNDS rounds (default 20): exactly one live server, one
#      node id, the layout survives stop/start. A corrupt server_id is refused.
#   3. SIGTERM to the server and all its descendants keeps all 4 panes
#   4. kill -9 after a command: its output replays after restart
#   5. session file: .bak kept, corrupt main falls back to .bak, no temp files
#   6. modes: state dir 0700, identity.key / authorized_devices.json /
#      server_id / session file 0600, also under umask 000, and wide modes of
#      existing files are fixed at startup
#   7. `stop --if-idle` that is refused leaves the server open for attaches
#      (the stopping state is cleared); the race itself is covered by the
#      unit test racing_attaches_never_succeed_after_an_idle_stop
#   (item 8, OSC 7 spoofing, is gated in scripts/p4a_gate.sh: spawn_cwd == /tmp)
#
# Runs as the current user with all state under a temp dir (--state-dir).
# Env: PROFILE=debug|release (default debug), ROUNDS=N (step 2), KEEP=1 keeps the
# temp dir, ONLY="1 3" runs just those steps (used to show a step fails without its fix).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$HOME/.local/zig:$PATH"
PROFILE="${PROFILE:-debug}"
ROUNDS="${ROUNDS:-20}"

if [ "$(id -u)" = "0" ]; then
  echo "refusing to run as root" >&2
  exit 2
fi

if [ "$PROFILE" = "release" ]; then
  cargo build --release -p forgetty-server --features qa-tools || exit 2
  BIN="$ROOT/target/release"
else
  cargo build -p forgetty-server --features qa-tools || exit 2
  BIN="$ROOT/target/debug"
fi

STATE="$(readlink -f "$(mktemp -d /tmp/p1qa.XXXXXX)")"
CLIENT_DATA="$STATE/client-data"
mkdir -p "$CLIENT_DATA"
BG_PIDS=()
CUR="$STATE/main"   # state dir used by fg; steps switch it

fg() { "$BIN/forgetty" --state-dir "$CUR" "$@"; }
qa() { local tool="$1"; shift; XDG_DATA_HOME="$CLIENT_DATA" "$BIN/$tool" "$@"; }

cleanup() {
  for p in "${BG_PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  for d in "$STATE"/*/; do
    "$BIN/forgetty" --state-dir "$d" server stop >/dev/null 2>&1
  done
  # anything left that still has our state dir in its environment
  python3 -I - "$STATE" <<'PY' 2>/dev/null
import os, signal, sys
root = sys.argv[1].encode()
for pid in filter(str.isdigit, os.listdir("/proc")):
    try:
        env = open(f"/proc/{pid}/environ", "rb").read()
    except OSError:
        continue
    if root in env and int(pid) != os.getpid():
        try:
            os.kill(int(pid), signal.SIGKILL)
        except OSError:
            pass
PY
  [ "${KEEP:-0}" = "1" ] || rm -rf "$STATE"
}
trap cleanup EXIT

step() { echo "--- $*"; }
pass() { echo "PASS: $*"; }
fail() {
  echo "FAIL: $*"
  echo "--- server log tail ($CUR)"
  tail -25 "$CUR/data/forgetty/server.log" 2>/dev/null
  exit 1
}
expect_rc() {
  local want="$1" desc="$2"; shift 3
  "$@" >"$STATE/last.out" 2>&1
  local rc=$?
  [ "$rc" = "$want" ] || { cat "$STATE/last.out"; fail "$desc: exit $rc, wanted $want"; }
}
jget() { python3 -I -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"; }
pane_count() { fg ls --json | python3 -I -c 'import json,sys; print(len(json.load(sys.stdin)["tabs"]))'; }
server_pid() { fg server status --json | jget pid; }
client_count() { fg server status --json | jget clients; }
poll() { # poll DESC TRIES CMD...: run CMD every 0.2 s until it succeeds
  local desc="$1" tries="$2"; shift 2
  local i
  for i in $(seq 1 "$tries"); do "$@" && return 0; sleep 0.2; done
  fail "timed out: $desc"
}
want() { [ -z "${ONLY:-}" ] || [[ " $ONLY " == *" $1 "* ]]; }
mode_of() { stat -c %a "$1"; }
clients_is() { [ "$(client_count)" = "$1" ]; }

# live_servers DIR: number of running `forgetty server run` processes that use
# state dir DIR (zombies excluded)
live_servers() {
  python3 -I - "$1" <<'PY'
import os, sys
# A server started by `server start` inherits XDG_DATA_HOME; one started as
# `forgetty --state-dir DIR server run` has DIR on its command line.
root = sys.argv[1].encode()
want = b"XDG_DATA_HOME=" + root + b"/data"
n = 0
for pid in filter(str.isdigit, os.listdir("/proc")):
    try:
        args = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")
        stat = open(f"/proc/{pid}/stat").read()
        env = open(f"/proc/{pid}/environ", "rb").read().split(b"\0")
    except OSError:
        continue
    state = stat.rsplit(")", 1)[1].split()[0]
    if state == "Z":
        continue
    args = [a for a in args if a]
    mine = want in env or (b"--state-dir" in args and root in args)
    if len(args) >= 2 and args[-2:] == [b"server", b"run"] and mine:
        n += 1
print(n)
PY
}

# descendants PID: all descendant pids, deepest first
descendants() {
  python3 -I - "$1" <<'PY'
import os, sys
kids = {}
for pid in filter(str.isdigit, os.listdir("/proc")):
    try:
        stat = open(f"/proc/{pid}/stat").read()
    except OSError:
        continue
    ppid = int(stat.rsplit(")", 1)[1].split()[1])
    kids.setdefault(ppid, []).append(int(pid))
out, todo = [], [int(sys.argv[1])]
while todo:
    p = todo.pop()
    for c in kids.get(p, []):
        out.append(c)
        todo.append(c)
print(" ".join(str(p) for p in reversed(out)))
PY
}

wait_gone() { # wait_gone PID SECS
  local i
  for i in $(seq 1 $(( $2 * 10 ))); do kill -0 "$1" 2>/dev/null || return 0; sleep 0.1; done
  return 1
}

if want 1; then
# ================================================================== 1. revoke
step "1. revoke cuts live connections"
CUR="$STATE/revoke"
expect_rc 0 "server start" -- fg server start
NODE_ID="$(fg server status --json | jget node_id)"
PANE="$(fg new -- bash)" || fail "forgetty new"
sleep 0.5

fg pair >"$STATE/pair.out" 2>&1 &
PAIR_PID=$!; BG_PIDS+=("$PAIR_PID")
for _ in $(seq 1 50); do grep -q '^payload: ' "$STATE/pair.out" 2>/dev/null && break; sleep 0.1; done
PAYLOAD="$(sed -n 's/^payload: //p' "$STATE/pair.out")"
[ -n "$PAYLOAD" ] || { cat "$STATE/pair.out"; fail "no pairing payload"; }
OUT="$(qa forgetty-pair-test --payload "$PAYLOAD" --name revoke-phone 2>&1)"; RC=$?
[ "$RC" = 0 ] && grep -q "paired ok" <<<"$OUT" || { echo "$OUT" | tail -3; fail "pairing failed"; }
wait "$PAIR_PID" 2>/dev/null

qa forgetty-stream-test --dial "$NODE_ID" --pane-id "$PANE" --hold-secs 60 --print-output \
  >"$STATE/hold.out" 2>&1 &
HOLD_PID=$!; BG_PIDS+=("$HOLD_PID")
poll "held client counted" 100 clients_is 1
fg client ls | grep -q revoke-phone || fail "client ls does not show revoke-phone"

# positive control: the held client sees output before the revoke
fg send --enter -p "$PANE" 'echo PRE_$((1+1))' || fail "send (pre)"
poll "PRE_2 reaches the held client" 50 grep -q 'PRE_2' "$STATE/hold.out"

DEV_ID="$(fg devices | awk '/revoke-phone/ {print $1}')"
[ -n "$DEV_ID" ] || fail "device id not found"
T0=$(date +%s.%N)
expect_rc 0 "revoke" -- fg revoke "$DEV_ID"
cat "$STATE/last.out"
# the connection must be gone within 1 s: client ls shows 0 and the client ended
GONE=0
for _ in $(seq 1 10); do
  if [ "$(client_count)" = 0 ] && ! kill -0 "$HOLD_PID" 2>/dev/null; then GONE=1; break; fi
  sleep 0.1
done
T1=$(date +%s.%N)
[ "$GONE" = 1 ] || { fg client ls; fail "revoked client still connected 1 s after revoke"; }
python3 -I -c 'import sys; sys.exit(0 if float(sys.argv[2])-float(sys.argv[1]) <= 1.3 else 1)' "$T0" "$T1" \
  || fail "connection cut took more than 1 s"
fg send --enter -p "$PANE" 'echo POST_$((3+4))' || fail "send (post)"
sleep 1.5
grep -q 'POST_7' "$STATE/hold.out" && fail "marker reached the revoked client"
[ "$(client_count)" = 0 ] || fail "revoked device reconnected"
fg devices | grep -q revoke-phone && fail "device still listed"
pass "revoked client cut in under 1 s, client ls = 0, marker POST_7 never arrived"
fg server stop >/dev/null 2>&1
fi

if want 2; then
# ================================================================== 2. concurrent first start
step "2. concurrent first start ($ROUNDS rounds)"
for i in $(seq 1 "$ROUNDS"); do
  CUR="$STATE/r$i"
  mkdir -p "$CUR"
  RUN_PIDS=()
  for k in 1 2 3; do
    "$BIN/forgetty" --state-dir "$CUR" server run >"$CUR/run$k.out" 2>&1 &
    RUN_PIDS+=("$!")
  done
  START_PIDS=()
  for k in 1 2; do
    "$BIN/forgetty" --state-dir "$CUR" server start >"$CUR/start$k.out" 2>&1 &
    START_PIDS+=("$!")
  done
  for p in "${START_PIDS[@]}"; do
    wait "$p"; RC=$?
    [ "$RC" = 0 ] || { cat "$CUR"/start*.out; fail "round $i: server start exited $RC"; }
  done
  # losers exit; exactly one server stays
  OK=0
  for _ in $(seq 1 50); do
    [ "$(live_servers "$CUR")" = 1 ] && { OK=1; break; }
    sleep 0.1
  done
  [ "$OK" = 1 ] || { cat "$CUR"/run*.out; fail "round $i: $(live_servers "$CUR") live servers"; }
  sleep 0.3
  [ "$(live_servers "$CUR")" = 1 ] || fail "round $i: server count changed"
  NODE1="$(fg server status --json | jget node_id)"
  ID1="$(cat "$CUR/data/forgetty/server_id")"
  KEY1="$(sha1sum < "$CUR/data/forgetty/identity.key")"
  P="$(fg new -- sleep 600)" || fail "round $i: new"
  expect_rc 0 "round $i: stop" -- fg server stop
  for p in "${RUN_PIDS[@]}"; do wait "$p" 2>/dev/null; done
  [ "$(live_servers "$CUR")" = 0 ] || fail "round $i: server survived stop"
  expect_rc 0 "round $i: restart" -- fg server start
  NODE2="$(fg server status --json | jget node_id)"
  [ "$NODE1" = "$NODE2" ] || fail "round $i: node id changed ($NODE1 -> $NODE2)"
  [ "$ID1" = "$(cat "$CUR/data/forgetty/server_id")" ] || fail "round $i: server_id changed"
  [ "$KEY1" = "$(sha1sum < "$CUR/data/forgetty/identity.key")" ] || fail "round $i: identity.key changed"
  fg ls | grep -q "$P" || { fg ls; fail "round $i: pane lost over stop/start"; }
  fg server stop >/dev/null 2>&1
  rm -rf "$CUR"
done
pass "$ROUNDS rounds: one live server, one node id, layout kept"

step "2b. corrupt server_id is refused, never replaced"
CUR="$STATE/corrupt"
expect_rc 0 "server start" -- fg server start
fg new -- sleep 600 >/dev/null || fail "new"
fg server stop >/dev/null 2>&1
echo 'not-a-uuid' >"$CUR/data/forgetty/server_id"
"$BIN/forgetty" --state-dir "$CUR" server run >"$STATE/corrupt.out" 2>&1; RC=$?
[ "$RC" != 0 ] || fail "server started on a corrupt server_id"
grep -qi 'corrupt' "$STATE/corrupt.out" || { cat "$STATE/corrupt.out"; fail "no clear error for a corrupt server_id"; }
[ "$(cat "$CUR/data/forgetty/server_id")" = "not-a-uuid" ] || fail "corrupt server_id was overwritten"
[ "$(live_servers "$CUR")" = 0 ] || fail "a server is running on a corrupt id"
pass "corrupt server_id: refused with a clear error, file untouched"
rm -rf "$CUR"
fi

if want 3; then
# ================================================================== 3. SIGTERM to the whole tree
step "3. SIGTERM to the server and all descendants keeps all panes"
CUR="$STATE/sigterm"
expect_rc 0 "server start" -- fg server start
for _ in 1 2 3; do fg new -- sleep 600 >/dev/null || fail "new sleep"; done
fg new -- bash >/dev/null || fail "new bash"
[ "$(pane_count)" = 4 ] || fail "expected 4 panes before the signal, got $(pane_count)"
sleep 1.5   # let the layout save
SPID="$(server_pid)"
DESC="$(descendants "$SPID")"
[ -n "$DESC" ] || fail "server has no descendants"
# the children first, then the server: the exits race the shutdown
kill -TERM $DESC 2>/dev/null
kill -TERM "$SPID"
wait_gone "$SPID" 10 || fail "server did not exit on SIGTERM"
expect_rc 0 "restart" -- fg server start
N="$(pane_count)"
[ "$N" = 4 ] || { fg ls; fail "expected 4 panes after restart, got $N"; }
# same again with the server first
SPID="$(server_pid)"
DESC="$(descendants "$SPID")"
kill -TERM "$SPID"
kill -TERM $DESC 2>/dev/null
wait_gone "$SPID" 10 || fail "server did not exit on SIGTERM (2)"
expect_rc 0 "restart (2)" -- fg server start
N="$(pane_count)"
[ "$N" = 4 ] || { fg ls; fail "expected 4 panes after second restart, got $N"; }
pass "4 panes after SIGTERM to the whole tree (both orders)"
fg server stop >/dev/null 2>&1
fi

if want 4; then
# ================================================================== 4. kill -9 keeps scrollback
step "4. kill -9 keeps scrollback"
CUR="$STATE/kill9"
expect_rc 0 "server start" -- fg server start
PANE="$(fg new -- bash)" || fail "new bash"
sleep 0.7
fg send --enter -p "$PANE" 'echo MARK_$((40+2))' || fail "send"
sleep 1.5
kill -9 "$(server_pid)"
sleep 0.5
expect_rc 0 "restart after kill -9" -- fg server start
fg ls | grep -q "$PANE" || { fg ls; fail "pane lost after kill -9"; }
OUT="$(fg capture -p "$PANE" --trim)"
grep -q '^MARK_42' <<<"$OUT" || { echo "$OUT"; fail "MARK_42 not replayed after kill -9"; }
pass "MARK_42 replayed after kill -9"
fg server stop >/dev/null 2>&1
fi

if want 5; then
# ================================================================== 5. session file
step "5. session file: .bak, corrupt fallback, no temp files"
CUR="$STATE/session"
expect_rc 0 "server start" -- fg server start
for _ in 1 2 3 4 5 6; do fg new -- sleep 600 >/dev/null & done
wait
sleep 1
[ "$(pane_count)" = 6 ] || fail "expected 6 panes, got $(pane_count)"
SID="$(cat "$CUR/data/forgetty/server_id")"
SF="$CUR/data/forgetty/sessions/$SID.json"
[ -f "$SF" ] || fail "no session file"
[ -f "$SF.bak" ] || fail "no .bak next to the session file"
LEFT="$(ls -A "$CUR/data/forgetty/sessions" | grep -c '\.tmp' || true)"
[ "$LEFT" = 0 ] || { ls -A "$CUR/data/forgetty/sessions"; fail "temp files left in sessions/"; }
fg server stop >/dev/null 2>&1
echo '{ "truncated' >"$SF"
expect_rc 0 "start with a corrupt session file" -- fg server start
N="$(pane_count)"
[ "$N" -ge 5 ] || { fg ls; fail "corrupt main did not fall back to .bak (got $N panes)"; }
grep -qi 'corrupt' "$CUR/data/forgetty/server.log" || fail "no error logged for the corrupt file"
[ -f "$SF.corrupt" ] || fail "corrupt file was not kept"
pass "6 panes saved with a .bak, no temp files, corrupt main restored from .bak ($N panes), error logged"
fg server stop >/dev/null 2>&1
fi

if want 6; then
# ================================================================== 6. modes
step "6. file modes"
CUR="$STATE/modes"
mkdir -p "$CUR"
( umask 000; "$BIN/forgetty" --state-dir "$CUR" server start >"$STATE/modes.out" 2>&1 ) || { cat "$STATE/modes.out"; fail "start under umask 000"; }
NODE_ID="$(fg server status --json | jget node_id)"
fg new -- sleep 600 >/dev/null || fail "new"
fg pair >"$STATE/pair2.out" 2>&1 &
PAIR_PID=$!; BG_PIDS+=("$PAIR_PID")
for _ in $(seq 1 50); do grep -q '^payload: ' "$STATE/pair2.out" 2>/dev/null && break; sleep 0.1; done
PAYLOAD="$(sed -n 's/^payload: //p' "$STATE/pair2.out")"
qa forgetty-pair-test --payload "$PAYLOAD" --identity "$CLIENT_DATA/modes.key" --name mode-phone >/dev/null 2>&1 || fail "pairing"
wait "$PAIR_PID" 2>/dev/null
sleep 1
D="$CUR/data/forgetty"
[ "$(mode_of "$D")" = 700 ] || fail "state dir is $(mode_of "$D"), wanted 700"
for f in identity.key authorized_devices.json server_id; do
  [ -f "$D/$f" ] || fail "$f missing"
  [ "$(mode_of "$D/$f")" = 600 ] || fail "$f is $(mode_of "$D/$f"), wanted 600"
done
SID="$(cat "$D/server_id")"
[ "$(mode_of "$D/sessions/$SID.json")" = 600 ] || fail "session file is $(mode_of "$D/sessions/$SID.json"), wanted 600"
fg server stop >/dev/null 2>&1
# files an older build left wide are fixed at startup
chmod 755 "$D"; chmod 644 "$D/identity.key" "$D/authorized_devices.json"
( umask 000; "$BIN/forgetty" --state-dir "$CUR" server start >"$STATE/modes.out" 2>&1 ) || { cat "$STATE/modes.out"; fail "restart under umask 000"; }
[ "$(mode_of "$D")" = 700 ] || fail "state dir not tightened ($(mode_of "$D"))"
for f in identity.key authorized_devices.json; do
  [ "$(mode_of "$D/$f")" = 600 ] || fail "$f not tightened ($(mode_of "$D/$f"))"
done
pass "dir 0700, key/registry/server_id/session 0600 under umask 000; wide modes fixed at startup"
fg server stop >/dev/null 2>&1
fi

if want 7; then
# ================================================================== 7. stop --if-idle
step "7. refused stop --if-idle leaves attaches open"
CUR="$STATE/idle"
expect_rc 0 "server start" -- fg server start
PANE="$(fg new -- bash)" || fail "new"
"$BIN/forgetty" --state-dir "$CUR" events >"$STATE/events1.out" 2>&1 &
EV1=$!; BG_PIDS+=("$EV1")
poll "events subscriber counted" 50 clients_is 1
expect_rc 1 "stop --if-idle with a subscriber" -- fg server stop --if-idle
kill "$EV1" 2>/dev/null; wait "$EV1" 2>/dev/null
poll "subscriber gone" 50 clients_is 0
"$BIN/forgetty" --state-dir "$CUR" events >"$STATE/events2.out" 2>&1 &
EV2=$!; BG_PIDS+=("$EV2")
poll "attach works after a refused stop" 50 clients_is 1
kill "$EV2" 2>/dev/null; wait "$EV2" 2>/dev/null
poll "subscriber 2 gone" 50 clients_is 0
expect_rc 0 "stop --if-idle when idle" -- fg server stop --if-idle
pass "refused --if-idle did not leave the server stopping; idle stop works"
fi

echo "P1-QA GATE: PASS"
exit 0
