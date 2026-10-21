#!/usr/bin/env bash
# P0 behaviour gate: headless `forgetty server`, local socket path + iroh path.
#
# Exits 0 only if BOTH paths saw the expected output:
#   1. Unix socket JSON-RPC: new_tab, send_input, subscribe_output
#   2. iroh QUIC via the QA bins: pair, list panes, subscribe, send input
#
# Runs the server as the current user, with all state under a temp dir
# (--state-dir), never under the real ~/.local/share/forgetty.
#
# Env: PROFILE=debug|release (default debug), KEEP=1 keeps the temp dir.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$HOME/.local/zig:$PATH"
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
STATE="$(mktemp -d /tmp/p0gate.XXXXXX)"
SOCK="$STATE/run/forgetty.sock"
fg() { "$BIN/forgetty" --state-dir "$STATE" "$@"; }
PAIR_PID=""
cleanup() {
  [ -n "$PAIR_PID" ] && kill "$PAIR_PID" 2>/dev/null
  fg server stop >/dev/null 2>&1
  [ "${KEEP:-0}" = "1" ] || rm -rf "$STATE"
}
trap cleanup EXIT

# Isolated identity for the QA client too (pair-test.key lives under XDG_DATA_HOME).
CLIENT_DATA="$STATE/client-data"
mkdir -p "$CLIENT_DATA"

# Marker is built so the command echo ("p0-o\"k\"") differs from its output ("p0-ok").
SEND='echo p0-o"k"'
EXPECT='p0-ok'
# The iroh path uses a different marker: it replays the pane's byte log on
# subscribe, so reusing the socket marker would pass without any new output.
IROH_SEND='echo p0-ir"oh"'
IROH_EXPECT='p0-iroh'

SHELL=/bin/bash fg server start || { echo "FAIL: server did not start"; exit 1; }
LOG="$STATE/data/forgetty/server.log"
NODE_ID="$(fg server status --json | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["node_id"])')"
[ -n "$NODE_ID" ] || { echo "FAIL: could not read node_id" >&2; exit 1; }
echo "node_id: $NODE_ID"

# ---------------------------------------------------------------- path 1: socket
SOCKET_RESULT=0
PANE_FILE="$STATE/pane_id"
python3 -I - "$SOCK" "$SEND" "$EXPECT" "$PANE_FILE" <<'PY' || SOCKET_RESULT=1
import base64, json, socket, struct, sys, time

sock_path, send, expect, pane_file = sys.argv[1:5]

def rpc(s, buf, method, params, rid):
    s.sendall((json.dumps({"jsonrpc": "2.0", "method": method, "params": params, "id": rid}) + "\n").encode())
    while b"\n" not in buf:
        chunk = s.recv(65536)
        if not chunk:
            raise SystemExit("socket closed during " + method)
        buf += chunk
    line, _, rest = buf.partition(b"\n")
    resp = json.loads(line)
    if "error" in resp:
        raise SystemExit("rpc error in %s: %s" % (method, resp["error"]))
    return resp["result"], rest

def connect():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sock_path)
    s.settimeout(20)
    return s

c = connect()
res, _ = rpc(c, b"", "new_tab", {"workspace_idx": 0}, 1)
pane = res["pane_id"]
open(pane_file, "w").write(pane)
print("socket: new_tab pane_id=%s" % pane)

sub = connect()
ack, rest = rpc(sub, b"", "subscribe_output", {"pane_id": pane}, 1)
if ack != {"ok": True}:
    raise SystemExit("bad subscribe ack: %r" % (ack,))

res, _ = rpc(c, b"", "send_input", {"pane_id": pane, "data": base64.b64encode((send + "\n").encode()).decode()}, 2)
print("socket: send_input ok")

seen = b""
deadline = time.time() + 30
buf = rest
while time.time() < deadline:
    while len(buf) >= 4:
        (n,) = struct.unpack(">I", buf[:4])
        if len(buf) < 4 + n:
            break
        seen += buf[4:4 + n]
        buf = buf[4 + n:]
    if expect.encode() in seen:
        print("PASS socket: saw %r via subscribe_output" % expect)
        sys.exit(0)
    try:
        chunk = sub.recv(65536)
    except socket.timeout:
        continue
    if not chunk:
        break
    buf += chunk
print("FAIL socket: did not see %r; got %r" % (expect, seen[-200:]))
sys.exit(1)
PY

# ---------------------------------------------------------------- path 2: iroh
IROH_RESULT=0
# AD-021: pairing needs the one-time secret from `forgetty pair`.
pair_with_secret() {
  fg pair >"$STATE/pair.out" 2>&1 &
  PAIR_PID=$!
  for _ in $(seq 1 50); do grep -q '^payload: ' "$STATE/pair.out" 2>/dev/null && break; sleep 0.1; done
  local payload
  payload="$(sed -n 's/^payload: //p' "$STATE/pair.out")"
  XDG_DATA_HOME="$CLIENT_DATA" "$BIN/forgetty-pair-test" --payload "$payload" 2>&1 | tail -3
  [ "${PIPESTATUS[0]}" = 0 ]
}
qa() { local tool="$1"; shift; XDG_DATA_HOME="$CLIENT_DATA" "$BIN/$tool" "$@"; }
PANE_ID="$(cat "$PANE_FILE" 2>/dev/null || true)"
if [ -z "$PANE_ID" ]; then
  echo "FAIL iroh: no pane id from socket path"
  IROH_RESULT=1
else
  {
    echo "--- pair"
    pair_with_secret \
      && echo "--- list panes" \
      && qa forgetty-stream-test --dial "$NODE_ID" --list-panes 2>&1 | tee "$STATE/list.out" | tail -5 \
      && grep -q "$PANE_ID" "$STATE/list.out" \
      && echo "PASS iroh: pane $PANE_ID listed" \
      && echo "--- subscribe + send + expect" \
      && qa forgetty-stream-test --dial "$NODE_ID" --pane-id "$PANE_ID" \
           --send-line "$IROH_SEND" --expect "$IROH_EXPECT" --timeout-secs 40 2>&1 | tail -5
  } > "$STATE/iroh.out" 2>&1
  cat "$STATE/iroh.out"
  grep -q "PASS: expected output found" "$STATE/iroh.out" && grep -q "PASS iroh: pane" "$STATE/iroh.out" || IROH_RESULT=1
fi

if [ "$SOCKET_RESULT" = 0 ] && [ "$IROH_RESULT" = 0 ]; then
  echo "P0 GATE: PASS (socket=ok iroh=ok)"
  exit 0
fi
echo "P0 GATE: FAIL (socket=$SOCKET_RESULT iroh=$IROH_RESULT; 0=ok)"
echo "--- server log tail"
tail -30 "$LOG"
exit 1
