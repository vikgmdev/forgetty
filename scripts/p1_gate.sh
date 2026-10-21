#!/usr/bin/env bash
# P1 behaviour gate: durable server, pairing secret, single `forgetty` binary.
#
# Exits 0 only if every step passes:
#   1. server start; a pane running `sleep 30; echo p1-done`
#   2. pairing: wrong secret refused, right secret ok, reuse refused, expiry
#   3. a client attaches and leaves; the pane stays alive; p1-done shows up
#      later in a fresh subscribe
#   4. `server stop --if-idle` refuses while a client is attached, works after
#   5. restart: same pane ids, scrollback replayed, CLI send works
#   6. exit codes 2/3/4
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
  cargo build --release -p forgetty-server --features qa-tools || exit 2
  BIN="$ROOT/target/release"
else
  cargo build -p forgetty-server --features qa-tools || exit 2
  BIN="$ROOT/target/debug"
fi

# Short path: unix socket paths are limited to ~108 bytes.
STATE="$(mktemp -d /tmp/p1gate.XXXXXX)"
CLIENT_DATA="$STATE/client-data"   # QA client identities live here, not in ~/.local/share
mkdir -p "$CLIENT_DATA"
BG_PIDS=()

fg() { "$BIN/forgetty" --state-dir "$STATE" "$@"; }
# `grep -q` leaves at the first match. A writer that still has output to send
# then gets SIGPIPE, and under `pipefail` the pipeline fails (the CLI died this
# way when the box was busy: 17 % of `server status | grep -q` under load).
# `qgrep` reads all its input, so the writer always finishes.
qgrep() { grep "$@" >/dev/null; }
# QA bins: own identity dir, never the user's real one.
qa() { local tool="$1"; shift; XDG_DATA_HOME="$CLIENT_DATA" "$BIN/$tool" "$@"; }

cleanup() {
  for p in "${BG_PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  fg server stop >/dev/null 2>&1
  [ "${KEEP:-0}" = "1" ] || rm -rf "$STATE"
}
trap cleanup EXIT

step() { echo "--- $*"; }
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; echo "--- server log tail"; tail -25 "$STATE/data/forgetty/server.log" 2>/dev/null; exit 1; }
# expect_rc WANT DESC -- cmd...
expect_rc() {
  local want="$1" desc="$2"; shift 3
  "$@" >"$STATE/last.out" 2>&1
  local rc=$?
  [ "$rc" = "$want" ] || { cat "$STATE/last.out"; fail "$desc: exit $rc, wanted $want"; }
}

# ------------------------------------------------------------------ 0. usage
step "exit codes without a server"
expect_rc 4 "ls with no server" -- fg ls
expect_rc 4 "status with no server" -- fg server status
expect_rc 2 "bad command line" -- fg frobnicate
pass "exit codes 4 and 2"

# ------------------------------------------------------------------ 1. start
step "server start + pane"
expect_rc 0 "server start" -- fg server start
cat "$STATE/last.out"
NODE_ID="$(fg server status --json | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["node_id"])')"
[ -n "$NODE_ID" ] || fail "no node id in status"
# `sleep 30; echo p1-done`, then `exec sleep` so the pane outlives the command
# (a pane closes when its process exits).
PANE="$(fg new -- bash -c 'sleep 30; echo p1-done; exec sleep 3600')" || fail "forgetty new"
PANE_T0=$(date +%s)
[ -n "$PANE" ] || fail "no pane id"
echo "node_id=$NODE_ID pane=$PANE"
fg ls | qgrep "$PANE" || fail "pane not in ls"
pass "server started, pane $PANE running"

# ------------------------------------------------------------------ 2. pairing
step "pairing (AD-021)"
fg pair >"$STATE/pair.out" 2>&1 &
PAIR_PID=$!
BG_PIDS+=("$PAIR_PID")
for _ in $(seq 1 50); do qgrep '^payload: ' "$STATE/pair.out" 2>/dev/null && break; sleep 0.1; done
PAYLOAD="$(sed -n 's/^payload: //p' "$STATE/pair.out")"
[ -n "$PAYLOAD" ] || { cat "$STATE/pair.out"; fail "forgetty pair printed no payload"; }
qgrep '"v":2' <<<"$PAYLOAD" || fail "payload is not v2: $PAYLOAD"
qgrep '"secret"' <<<"$PAYLOAD" || fail "payload has no secret"

OUT="$(qa forgetty-pair-test --dial "$NODE_ID" --secret AAAAAAAAAAAAAAAAAAAAAAAAAA 2>&1)"; RC=$?
echo "$OUT" | tail -2
[ "$RC" != 0 ] && qgrep "bad_secret" <<<"$OUT" || fail "wrong secret was not refused"
pass "wrong secret refused"

OUT="$(qa forgetty-pair-test --payload "$PAYLOAD" --name gate-phone 2>&1)"; RC=$?
echo "$OUT" | tail -2
[ "$RC" = 0 ] && qgrep "paired ok" <<<"$OUT" || fail "right secret was refused"
wait "$PAIR_PID" 2>/dev/null
qgrep '^paired: gate-phone' "$STATE/pair.out" || { cat "$STATE/pair.out"; fail "forgetty pair did not report the device"; }
pass "right secret paired: $(grep '^paired:' "$STATE/pair.out")"

OUT="$(qa forgetty-pair-test --payload "$PAYLOAD" --identity "$CLIENT_DATA/second.key" --name intruder 2>&1)"; RC=$?
echo "$OUT" | tail -1
[ "$RC" != 0 ] && qgrep "pair refused" <<<"$OUT" || fail "reused secret was accepted"
pass "second attempt with the same secret refused"

fg devices | qgrep gate-phone || fail "devices does not list gate-phone"
fg devices | qgrep intruder && fail "intruder was paired"
OUT="$(fg pair --ttl 2 2>&1)"; RC=$?
qgrep '^expired' <<<"$OUT" && [ "$RC" = 1 ] || { echo "$OUT" | tail -3; fail "pair window did not expire (rc=$RC)"; }
pass "devices lists the phone; unused window expires"

# ------------------------------------------------------------------ 3. attach / leave
step "client attaches and leaves"
qa forgetty-stream-test --dial "$NODE_ID" --pane-id "$PANE" --hold-secs 6 >"$STATE/hold.out" 2>&1 &
HOLD_PID=$!
BG_PIDS+=("$HOLD_PID")
for _ in $(seq 1 100); do
  [ "$(fg server status --json | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["clients"])')" = 1 ] && break
  sleep 0.2
done
[ "$(fg server status --json | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["clients"])')" = 1 ] || fail "attached client not counted"
fg client ls | qgrep gate-phone || { fg client ls; fail "client ls does not show gate-phone"; }
wait "$HOLD_PID"
qgrep "PASS: held" "$STATE/hold.out" || { cat "$STATE/hold.out"; fail "attach client failed"; }
sleep 1
fg ls | qgrep "$PANE" || fail "pane died when the client left"
pass "client attached (counted), left, pane still alive"

step "p1-done appears later in a fresh subscribe"
ELAPSED=$(( $(date +%s) - PANE_T0 ))
[ "$ELAPSED" -lt 34 ] && sleep $(( 34 - ELAPSED ))
OUT="$(qa forgetty-stream-test --dial "$NODE_ID" --pane-id "$PANE" --expect p1-done --timeout-secs 20 2>&1)"; RC=$?
qgrep "PASS: expected output found" <<<"$OUT" && [ "$RC" = 0 ] || { echo "$OUT" | tail -5; fail "p1-done not seen in fresh subscribe"; }
pass "p1-done seen after the original client was gone"

# ------------------------------------------------------------------ 4. stop --if-idle
step "server stop --if-idle"
qa forgetty-stream-test --dial "$NODE_ID" --pane-id "$PANE" --hold-secs 8 >"$STATE/hold2.out" 2>&1 &
HOLD2_PID=$!
BG_PIDS+=("$HOLD2_PID")
for _ in $(seq 1 100); do
  [ "$(fg server status --json | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["clients"])')" = 1 ] && break
  sleep 0.2
done
expect_rc 1 "stop --if-idle with a client attached" -- fg server stop --if-idle
qgrep "client(s) attached" "$STATE/last.out" || { cat "$STATE/last.out"; fail "busy message missing"; }
expect_rc 0 "server still up after refused stop" -- fg server status
pass "stop --if-idle refused while a client is attached"
# The client leaves on its own (graceful close), as a real app would.
wait "$HOLD2_PID" 2>/dev/null
qgrep "PASS: held" "$STATE/hold2.out" || { cat "$STATE/hold2.out"; fail "second attach client failed"; }
for _ in $(seq 1 100); do
  [ "$(fg server status --json | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["clients"])')" = 0 ] && break
  sleep 0.2
done
expect_rc 0 "stop --if-idle after the client left" -- fg server stop --if-idle
expect_rc 4 "status after stop" -- fg server status
pass "stop --if-idle succeeded once the client left"

# ------------------------------------------------------------------ 5. restart
step "restart restores layout and scrollback"
expect_rc 0 "server start (2)" -- fg server start
fg ls | qgrep "$PANE" || { fg ls; fail "pane id $PANE not listed after restart"; }
OUT="$(qa forgetty-stream-test --dial "$NODE_ID" --pane-id "$PANE" --expect p1-done --timeout-secs 20 2>&1)"; RC=$?
qgrep "PASS: expected output found" <<<"$OUT" && [ "$RC" = 0 ] || { echo "$OUT" | tail -5; fail "scrollback not replayed after restart"; }
pass "same pane id listed after restart, scrollback replayed"

step "CLI send with id suffix"
SUFFIX="${PANE: -6}"
fg send -p "$SUFFIX" 'echo p1-o"k"' || fail "forgetty send"
fg send-key -p "$SUFFIX" enter || fail "forgetty send-key"
OUT="$(qa forgetty-stream-test --dial "$NODE_ID" --pane-id "$PANE" --expect 'p1-ok' --timeout-secs 20 2>&1)"; RC=$?
qgrep "PASS: expected output found" <<<"$OUT" && [ "$RC" = 0 ] || { echo "$OUT" | tail -5; fail "send + send-key output not seen"; }
pass "send and send-key reached the restored pane"

expect_rc 3 "unknown pane suffix" -- fg send -p ffffff x
expect_rc 2 "pane suffix too short" -- fg send -p ab x
expect_rc 2 "unknown key" -- fg send-key -p "$SUFFIX" f13
pass "exit codes 3 and 2"

step "socket permissions"
SOCK="$STATE/run/forgetty/forgetty.sock"
[ "$(stat -c %a "$STATE/run/forgetty")" = 700 ] || fail "socket dir is not mode 700"
[ "$(stat -c %a "$SOCK")" = 600 ] || fail "socket is not mode 600"
pass "socket dir 0700, socket 0600"

step "new --keep: exited pane stays, exit code recorded, close_pane removes it"
KPANE="$(fg new --keep -- sh -c 'echo keep-out; exit 5')" || fail "forgetty new --keep"
[ -n "$KPANE" ] || fail "no keep pane id"
KOK=0
for _ in $(seq 1 50); do
  if fg ls --json | python3 -I -c '
import json,sys
pid=sys.argv[1]
t=json.load(sys.stdin)["tabs"]
p=[x for x in t if x["pane_id"]==pid]
sys.exit(0 if p and p[0].get("exited") is True and p[0].get("exit_code")==5 and p[0].get("remain_on_exit") is True else 1)
' "$KPANE"; then KOK=1; break; fi
  sleep 0.2
done
[ "$KOK" = 1 ] || { fg ls --json; fail "kept pane not listed as exited with code 5"; }
fg ls | grep "$KPANE" | qgrep "exited 5" || { fg ls; fail "ls STATE column does not show exited 5"; }
python3 -I - "$SOCK" "$KPANE" <<'PY' || fail "close_pane on the kept pane"
import json, socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(10)
s.connect(sys.argv[1])
s.sendall((json.dumps({"jsonrpc": "2.0", "method": "close_pane", "params": {"pane_id": sys.argv[2]}, "id": 1}) + "\n").encode())
buf = b""
while not buf.endswith(b"\n"):
    chunk = s.recv(65536)
    if not chunk:
        break
    buf += chunk
sys.exit(0 if "error" not in json.loads(buf) else 1)
PY
fg ls | qgrep "$KPANE" && fail "kept pane still listed after close_pane"
# a default pane that exits is gone
DPANE="$(fg new -- sh -c 'exit 0')" || fail "forgetty new (default)"
sleep 2
fg ls | qgrep "$DPANE" && fail "default pane still listed after its process exited"
pass "--keep pane stays (exit 5), close_pane removes it, default pane closes on exit"

# old pair/1 clients are refused, not paired
step "server version info"
fg server status | qgrep "protocol: 2.0" || fail "status does not show protocol 2.0"
pass "status reports protocol 2.0"

echo "P1 GATE: PASS"
exit 0
