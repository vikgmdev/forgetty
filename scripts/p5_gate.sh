#!/usr/bin/env bash
# P5 gate: self-describing API, labels and the agent skill.
#
#  1. cargo tests: the method table (examples match their schemas, live replies
#     match the result schemas, every protocol method has an entry, the v2
#     allow-list matches) and the doc staleness test.
#  2. `api list` covers every method in protocol.rs, `api describe` works for
#     each, `api call server_info` round-trips, errors exit 2 (bad_request) and
#     3 (method_not_found).
#  3. Labels: resolve in the order id, label, position, suffix, case-insensitive;
#     a duplicate label exits 3 and lists the matches; bad labels exit 2;
#     tab and workspace labels; labels survive a server restart; `events` shows
#     LabelChanged.
#  4. docs/socket-api.md equals `forgetty api list --markdown`, and a changed
#     copy makes the staleness test fail (negative control).
#  5. Protocol v2 against the real server: api.list, api.describe, set_label and
#     the LayoutChanged that carries the label (crates/forgetty-client/tests/p5.rs).
#  6. `skill setup|status|show|remove` write and remove the files (user and
#     project scope, claude and codex).
#  7. A headless `claude -p --model haiku` run that only has the installed skill
#     opens a pane, runs `echo skill-ok` and reads it back with capture. Skipped
#     with a message when `claude` cannot run non-interactively here
#     (P5_SKIP_AGENT=1 skips it too).
#  8. The earlier gates still pass: p0, p1, p4a, p1qa, p6 (SKIP_OLD=1 skips).
#
# Exit 0 only if every check passes. Runs as the current user, all server state
# under temp dirs (--state-dir).
# Env: P5_KEEP=1 keeps the temp dir, SKIP_OLD=1, P5_SKIP_AGENT=1.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$HOME/.local/zig:$PATH"
export CARGO_INCREMENTAL=0
export LC_ALL=C
# Another process may hold the fixed UDP port 47297: use any free port.
export FORGETTY_PORT="${FORGETTY_PORT:-0}"

if [ "$(id -u)" = "0" ]; then
  echo "refusing to run as root" >&2
  exit 2
fi

WORK="$(readlink -f "$(mktemp -d /tmp/p5gate.XXXXXX)")"
STATE="$WORK/state"
PIDS=()
PASSES=0
FAILS=0
DOC_BACKUP=""

cleanup() {
  local p
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  [ -n "$DOC_BACKUP" ] && [ -f "$DOC_BACKUP" ] && cp "$DOC_BACKUP" "$ROOT/docs/socket-api.md"
  [ -x "${BIN:-}/forgetty" ] && "$BIN/forgetty" --state-dir "$STATE" server stop >/dev/null 2>&1
  [ -z "${P5_KEEP:-}" ] && rm -rf "$WORK"
  return 0
}
trap cleanup EXIT

say()  { printf '\n== %s\n' "$*"; }
ok()   { PASSES=$((PASSES + 1)); printf '  ok    %s\n' "$*"; }
bad()  { FAILS=$((FAILS + 1)); printf '  FAIL  %s\n' "$*"; }
check() { # check "description" command...
  local what="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$what"; else bad "$what"; fi
}
# expect_rc N "description" cmd...: the command must exit with N
expect_rc() {
  local want="$1" what="$2"; shift 2
  "$@" >"$WORK/rc.out" 2>&1
  local rc=$?
  if [ "$rc" = "$want" ]; then ok "$what (exit $rc)"; else bad "$what: exit $rc, wanted $want"; sed 's/^/        /' "$WORK/rc.out" | head -5; fi
}
has() { grep -qE -- "$2" <<<"$1"; }

say "build"
cargo build -p forgetty-server --features qa-tools >"$WORK/build.log" 2>&1 || { tail -20 "$WORK/build.log"; exit 2; }
BIN="$ROOT/target/debug"
fg() { "$BIN/forgetty" --state-dir "$STATE" "$@"; }

# ------------------------------------------------------------------ 1. cargo tests
say "1. cargo tests"
if cargo test -p forgetty-socket >"$WORK/t-socket.log" 2>&1; then
  ok "forgetty-socket tests ($(grep -h '^test result' "$WORK/t-socket.log" | head -1 | sed 's/test result: ok. //; s/;.*//'))"
else bad "forgetty-socket tests"; grep -E 'FAILED|panicked' "$WORK/t-socket.log" | head; fi
if cargo test -p forgetty-session labels_ >"$WORK/t-session.log" 2>&1; then ok "session label tests"; else bad "session label tests"; fi
if cargo test -p forgetty-workspace label >"$WORK/t-ws.log" 2>&1; then ok "session file label tests"; else bad "session file label tests"; fi
if cargo test -p forgetty-server >"$WORK/t-server.log" 2>&1; then ok "forgetty-server tests (CLI resolution, skill)"; else bad "forgetty-server tests"; grep -E 'FAILED|panicked' "$WORK/t-server.log" | head; fi

# ------------------------------------------------------------------ server
say "server"
fg server start >/dev/null 2>&1 && ok "server started" || { bad "server start"; exit 1; }

# ------------------------------------------------------------------ 2. api
say "2. api list, describe, call"
NAMES="$(fg api list -q | sort)"
PROTO="$(awk '/pub mod methods/{f=1} f' crates/forgetty-socket/src/protocol.rs \
  | grep -oE 'pub const [A-Z_0-9]+: &str = "[^"]+"' | sed -E 's/.*"([^"]+)"/\1/' | grep -v '^qa_' | sort)"
NN="$(wc -l <<<"$NAMES")"
check "api list has methods ($NN)" [ "$NN" -gt 30 ]
MISSING="$(comm -13 <(echo "$NAMES") <(echo "$PROTO"))"
EXTRA="$(comm -23 <(echo "$NAMES") <(echo "$PROTO"))"
[ -z "$MISSING" ] && ok "api list covers every method in protocol.rs" || bad "api list lacks: $MISSING"
[ -z "$EXTRA" ] && ok "api list has no method outside protocol.rs" || bad "api list has extra: $EXTRA"
check "api list --json is valid JSON with kind and remote" \
  python3 -I -c 'import json,sys; d=json.load(sys.stdin)["methods"]; assert all({"name","summary","kind","remote"}<=set(m) for m in d)' < <(fg api list --json)

DESC_BAD=""
for m in $NAMES; do
  fg api describe "$m" --json >"$WORK/desc.json" 2>"$WORK/desc.err" || { DESC_BAD="$DESC_BAD $m(exit)"; continue; }
  python3 -I - "$m" "$WORK/desc.json" <<'PY' || DESC_BAD="$DESC_BAD $m"
import json, sys
m = sys.argv[1]
d = json.load(open(sys.argv[2]))
assert d["name"] == m
assert d["summary"] and d["description"]
assert d["kind"] in ("call", "stream", "lifecycle")
assert isinstance(d["remote"], bool)
assert isinstance(d["params_schema"], dict) and isinstance(d["result_schema"], dict)
assert d["params_schema"].get("$schema") and d["result_schema"].get("$schema")
assert d["request_example"] is not None and d["response_example"] is not None
PY
done
[ -z "$DESC_BAD" ] && ok "api describe works for each of $NN methods" || bad "api describe failed for:$DESC_BAD"
check "api describe prints text" bash -c "'$BIN/forgetty' --state-dir '$STATE' api describe set_label | grep -q 'params schema'"

INFO="$(fg api call server_info)"
SPID="$(fg server status --json | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["pid"])')"
check "api call server_info round-trips (pid, proto 2, version)" \
  python3 -I -c '
import json, sys
d = json.loads(sys.argv[1])
assert str(d["pid"]) == sys.argv[2], d
assert d["proto_major"] == 2
assert d["version"] == sys.argv[3]
' "$INFO" "$SPID" "$(fg --version | awk '{print $2}')"
expect_rc 3 "api call of an unknown method is method_not_found" fg api call no_such_method
has "$(cat "$WORK/rc.out")" 'method_not_found' && ok "error text names method_not_found" || bad "error text lacks method_not_found"
expect_rc 2 "api call with a missing param is bad_request" fg api call capture '{}'
has "$(cat "$WORK/rc.out")" 'bad_request' && ok "error text names bad_request" || bad "error text lacks bad_request"
expect_rc 2 "api call with invalid JSON is bad_request" fg api call capture '{nope'
expect_rc 3 "api describe of an unknown method exits 3" fg api describe no_such_method
echo '{"workspace_idx":0}' >"$WORK/p.json"
fg api call new_tab @"$WORK/p.json" >"$WORK/nt.json" 2>&1 && ok "api call with @file params" || bad "api call @file"
echo '{}' | fg api call list_tabs - >/dev/null 2>&1 && ok "api call with params from stdin" || bad "api call stdin"
fg api call list_tabs >/dev/null 2>&1 && ok "api call without params" || bad "api call without params"

# ------------------------------------------------------------------ 3. labels
say "3. labels"
fg ls --json | python3 -I -c 'import json,sys
for t in json.load(sys.stdin)["tabs"]: print(t["pane_id"])' | while read -r p; do fg close -p "$p" >/dev/null 2>&1; done
P1="$(fg new)"; P2="$(fg new)"; P3="$(fg new)"
[ -n "$P1" ] && [ -n "$P2" ] && [ -n "$P3" ] && ok "three panes: $P1 $P2 $P3" || { bad "could not create panes"; exit 1; }
pane_of() { fg api call -p "$1" get_pane_info 2>/dev/null | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["pane_id"])'; }

fg label -t pane "$P1" Build >/dev/null && ok "label set" || bad "label set"
[ "$(pane_of build)" = "$P1" ] && ok "label resolves, case-insensitive (-p build for Build)" || bad "label resolve"
[ "$(pane_of 2)" = "$P2" ] && ok "position resolves (-p 2 is the second pane)" || bad "position resolve"
SUF3="${P3: -8}"
[ "$(pane_of "$SUF3")" = "$P3" ] && ok "id suffix resolves" || bad "suffix resolve"
[ "$(pane_of "$P3")" = "$P3" ] && ok "full id resolves" || bad "full id resolve"

# order: id > label > position > suffix
fg label -t pane "$P2" "$SUF3" >/dev/null
[ "$(pane_of "$SUF3")" = "$P2" ] && ok "label beats suffix" || bad "label should beat suffix"
fg label -t pane "$P3" 1 >/dev/null
[ "$(pane_of 1)" = "$P3" ] && ok "label beats position (label 1 on pane 3, -p 1 is pane 3)" || bad "label should beat position"
fg label -t pane "$P1" "$P2" >/dev/null
[ "$(pane_of "$P2")" = "$P2" ] && ok "full id beats label (P1 labelled with P2's id)" || bad "full id should beat label"

# ambiguity
fg label -t pane "$P1" dup >/dev/null; fg label -t pane "$P2" DUP >/dev/null
expect_rc 3 "duplicate label (dup/DUP) exits 3" fg api call -p dup get_pane_info
has "$(cat "$WORK/rc.out")" "ambiguous" && has "$(cat "$WORK/rc.out")" "$P1" && has "$(cat "$WORK/rc.out")" "$P2" \
  && ok "ambiguity message lists both panes" || bad "ambiguity message: $(cat "$WORK/rc.out")"
expect_rc 3 "unknown label exits 3 (no such pane)" fg send -p zzzzzzzz hi
expect_rc 2 "label with a control character exits 2" fg label -t pane "$P1" $'a\nb'
expect_rc 2 "label over 128 characters exits 2" fg label -t pane "$P1" "$(printf 'x%.0s' $(seq 1 129))"

# clean labels for the persistence checks
fg label -t pane "$P1" build >/dev/null; fg label -t pane "$P2" logs >/dev/null; fg label -t pane "$P3" "" >/dev/null
[ "$(fg ls --json | python3 -I -c 'import json,sys; print({t["pane_id"]: t["label"] for t in json.load(sys.stdin)["tabs"]}[sys.argv[1]])' "$P3")" = "" ] \
  && ok "empty label clears" || bad "empty label should clear"
fg ls | grep -q "build" && ok "ls shows the label column" || bad "ls lacks the label"

# tab and workspace labels
fg label -t tab 1 maintab >/dev/null && ok "tab label by position" || bad "tab label"
fg label -t tab maintab main2 >/dev/null && ok "tab label by label" || bad "tab relabel by label"
fg label -t workspace 1 home >/dev/null && ok "workspace label by position" || bad "workspace label"
expect_rc 2 "workspace label cannot be empty" fg label -t workspace home ""
LAYOUT="$(fg api call get_layout)"
check "get_layout carries workspace name, tab label and pane labels" python3 -I -c '
import json, sys
d = json.loads(sys.argv[1])
ws = d["workspaces"][0]
assert ws["name"] == "home", ws["name"]
assert ws["tabs"][0]["label"] == "main2"
labels = {}
for t in ws["tabs"]:
    labels.update(t["pane_labels"])
assert labels.get(sys.argv[2]) == "build" and labels.get(sys.argv[3]) == "logs", labels
' "$LAYOUT" "$P1" "$P2"

# LabelChanged on the event stream
fg events --json >"$WORK/events.out" 2>/dev/null &
EV=$!; PIDS+=("$EV")
sleep 0.7
fg label -t pane "$P3" streamed >/dev/null
for _ in $(seq 1 30); do grep -q '"LabelChanged"' "$WORK/events.out" && break; sleep 0.2; done
kill "$EV" 2>/dev/null
grep '"LabelChanged"' "$WORK/events.out" | grep -q '"label":"streamed"' && ok "events shows LabelChanged" || bad "no LabelChanged event: $(head -c 300 "$WORK/events.out")"
fg label -t pane "$P3" "" >/dev/null

# persistence across a restart
fg server stop >/dev/null 2>&1; fg server start >/dev/null 2>&1 && ok "server restarted" || { bad "restart"; exit 1; }
RL="$(fg ls --json | python3 -I -c 'import json,sys; print(",".join(sorted(t["pane_id"]+"="+t["label"] for t in json.load(sys.stdin)["tabs"])))')"
WANT="$(printf '%s\n' "$P1=build" "$P2=logs" "$P3=" | sort | paste -sd,)"
[ "$RL" = "$WANT" ] && ok "pane labels survive a restart" || bad "pane labels after restart: $RL (wanted $WANT)"
check "tab label and workspace name survive a restart" python3 -I -c '
import json, sys
ws = json.loads(sys.argv[1])["workspaces"][0]
assert ws["name"] == "home", ws["name"]
assert ws["tabs"][0]["label"] == "main2", ws["tabs"][0]["label"]
' "$(fg api call get_layout)"
[ "$(pane_of build)" = "$P1" ] && ok "restored label resolves" || bad "restored label does not resolve"
SF="$(grep -rl 'main2' "$STATE/data" 2>/dev/null | head -1)"
[ -n "$SF" ] && ok "labels are in the session file ($(basename "$SF"))" || bad "labels not found in any file under the state dir"

# ------------------------------------------------------------------ 4. doc staleness
say "4. docs/socket-api.md"
if fg api list --markdown | diff -q - docs/socket-api.md >/dev/null; then ok "docs/socket-api.md equals api list --markdown"; else bad "docs/socket-api.md differs from api list --markdown"; fi
if cargo test -p forgetty-socket socket_api_doc >"$WORK/doc.log" 2>&1; then ok "doc staleness test passes"; else bad "doc staleness test"; fi
DOC_BACKUP="$WORK/socket-api.md.bak"
cp docs/socket-api.md "$DOC_BACKUP"
echo "stale" >>docs/socket-api.md
if cargo test -p forgetty-socket socket_api_doc >"$WORK/doc-neg.log" 2>&1; then bad "staleness test passed on a changed doc (negative control)"; else ok "staleness test fails on a changed doc (negative control)"; fi
cp "$DOC_BACKUP" docs/socket-api.md; DOC_BACKUP=""

# ------------------------------------------------------------------ 5. protocol v2
say "5. protocol v2 (real server)"
if FORGETTY_REAL_SERVER="$BIN/forgetty" cargo test -p forgetty-client --test p5 -- --nocapture >"$WORK/v2.log" 2>&1 && grep -q "1 passed" "$WORK/v2.log"; then
  ok "v2: api.list, api.describe, set_label, LayoutChanged with the label, refused methods"
else bad "v2 test"; tail -15 "$WORK/v2.log"; fi

# ------------------------------------------------------------------ 6. skill files
say "6. skill setup, status, show, remove"
SH="$WORK/home"; SP="$WORK/proj"; mkdir -p "$SH" "$SP"
for combo in "claude user $SH/.claude/skills/forgetty/SKILL.md" \
             "claude project $SP/.claude/skills/forgetty/SKILL.md" \
             "codex user $SH/.codex/skills/forgetty/SKILL.md" \
             "codex project $SP/.agents/skills/forgetty/SKILL.md"; do
  set -- $combo
  agent="$1"; scope="$2"; path="$3"
  env -u CODEX_HOME bash -c "cd '$SP' && HOME='$SH' '$BIN/forgetty' --state-dir '$STATE' skill setup --agent $agent --scope $scope" >/dev/null 2>&1
  if [ -f "$path" ] && head -2 "$path" | grep -q '^name: forgetty'; then ok "setup --agent $agent --scope $scope wrote $path"; else bad "setup $agent $scope: $path missing"; fi
  ST="$(env -u CODEX_HOME bash -c "cd '$SP' && HOME='$SH' '$BIN/forgetty' --state-dir '$STATE' skill status --agent $agent --scope $scope --json")"
  has "$ST" '"state": "current"' && ok "status says current ($agent $scope)" || bad "status ($agent $scope): $ST"
done
SHOWN="$("$BIN/forgetty" --state-dir "$STATE" skill show)"
[ "$SHOWN" = "$(cat "$SH/.claude/skills/forgetty/SKILL.md")" ] && ok "show prints the installed text" || bad "show differs from the file"
echo "old text" >"$SH/.claude/skills/forgetty/SKILL.md"
has "$(env -u CODEX_HOME bash -c "cd '$SP' && HOME='$SH' '$BIN/forgetty' --state-dir '$STATE' skill status --agent claude --scope user")" 'outdated' && ok "status flags an outdated file" || bad "status should say outdated"
env -u CODEX_HOME bash -c "cd '$SP' && HOME='$SH' '$BIN/forgetty' --state-dir '$STATE' skill setup" >/dev/null 2>&1
[ "$(cat "$SH/.claude/skills/forgetty/SKILL.md")" = "$SHOWN" ] && ok "setup rewrites an outdated file" || bad "setup did not rewrite"
for combo in "claude user $SH/.claude/skills/forgetty" "claude project $SP/.claude/skills/forgetty" \
             "codex user $SH/.codex/skills/forgetty" "codex project $SP/.agents/skills/forgetty"; do
  set -- $combo
  env -u CODEX_HOME bash -c "cd '$SP' && HOME='$SH' '$BIN/forgetty' --state-dir '$STATE' skill remove --agent $1 --scope $2" >/dev/null 2>&1
  [ ! -e "$3" ] && ok "remove --agent $1 --scope $2 removed the directory" || bad "remove $1 $2: $3 still there"
done
has "$(env -u CODEX_HOME bash -c "cd '$SP' && HOME='$SH' '$BIN/forgetty' --state-dir '$STATE' skill status --agent claude --scope user")" 'missing' && ok "status says missing after remove" || bad "status after remove"

# a planted symlink must not redirect a write or a remove
EVIL="$WORK/evil-proj"; OUT="$WORK/outside"; mkdir -p "$EVIL" "$OUT"
echo precious >"$OUT/precious"
ln -s "$OUT" "$EVIL/.claude"
env -u CODEX_HOME bash -c "cd '$EVIL' && HOME='$SH' '$BIN/forgetty' --state-dir '$STATE' skill setup --scope project" >"$WORK/sym.out" 2>&1
RC=$?
[ "$RC" = 1 ] && has "$(cat "$WORK/sym.out")" 'symlink' && ok "setup refuses a symlinked .claude (exit 1)" || bad "setup followed or misreported a symlinked .claude (exit $RC): $(cat "$WORK/sym.out")"
[ "$(ls "$OUT")" = "precious" ] && ok "nothing was written outside" || bad "files outside changed: $(ls "$OUT")"
env -u CODEX_HOME bash -c "cd '$EVIL' && HOME='$SH' '$BIN/forgetty' --state-dir '$STATE' skill remove --scope project" >/dev/null 2>&1 && bad "remove followed a symlink" || ok "remove refuses a symlinked .claude"
[ "$(cat "$OUT/precious")" = "precious" ] && ok "the outside file is intact" || bad "outside file changed"

# ------------------------------------------------------------------ 7. agent test
say "7. agent test (claude -p --model haiku)"
AGENT_OK=0
if [ -n "${P5_SKIP_AGENT:-}" ]; then
  echo "  SKIP  agent test (P5_SKIP_AGENT set)"
elif ! command -v claude >/dev/null 2>&1; then
  echo "  SKIP  agent test: claude is not installed"
elif ! [ "$(timeout 120 claude -p --model haiku "Reply with the single word ok." 2>/dev/null | tr -d '[:space:]' | tr 'A-Z' 'a-z')" = "ok" ]; then
  echo "  SKIP  agent test: claude cannot run non-interactively here (login or network)"
else
  AG="$WORK/agent"; mkdir -p "$AG/bin" "$AG/proj"
  cat >"$AG/bin/forgetty" <<EOF
#!/bin/sh
exec "$BIN/forgetty" --state-dir "$STATE" "\$@"
EOF
  chmod +x "$AG/bin/forgetty"
  (cd "$AG/proj" && PATH="$AG/bin:$PATH" forgetty skill setup --agent claude --scope project >/dev/null)
  # Start from a known pane count.
  BEFORE="$(fg ls --json | python3 -I -c 'import json,sys; print(len(json.load(sys.stdin)["tabs"]))')"
  (cd "$AG/proj" && PATH="$AG/bin:$PATH" timeout 400 claude -p --model haiku --allowedTools "Bash" --max-turns 20 \
    "Use the forgetty skill. Open a new terminal pane, run the command echo skill-ok in it, read the pane screen back with forgetty capture, and answer with the captured text. Use only forgetty commands from the skill." \
    >"$WORK/agent.out" 2>"$WORK/agent.err")
  AFTER="$(fg ls --json | python3 -I -c 'import json,sys; print(len(json.load(sys.stdin)["tabs"]))')"
  FOUND=0
  for p in $(fg ls --json | python3 -I -c 'import json,sys
for t in json.load(sys.stdin)["tabs"]: print(t["pane_id"])'); do
    fg capture -p "$p" --trim 2>/dev/null | grep -qx 'skill-ok' && FOUND=1
  done
  [ "$AFTER" -gt "$BEFORE" ] && ok "the agent opened a pane ($BEFORE -> $AFTER panes)" || bad "the agent opened no pane"
  [ "$FOUND" = 1 ] && ok "a pane shows the line skill-ok (echo ran)" || bad "no pane shows skill-ok"
  grep -q 'skill-ok' "$WORK/agent.out" && ok "the agent reported the captured text" || { bad "the agent's answer lacks skill-ok"; head -c 600 "$WORK/agent.out"; }
fi

# ------------------------------------------------------------------ 8. earlier gates
say "8. earlier gates"
if [ -n "${SKIP_OLD:-}" ]; then
  echo "  SKIP  earlier gates (SKIP_OLD set)"
else
  fg server stop >/dev/null 2>&1
  for g in p0 p1 p4a p1qa p6; do
    if "$ROOT/scripts/${g}_gate.sh" >"$WORK/$g.log" 2>&1; then ok "$g gate"; else bad "$g gate (see $WORK/$g.log)"; tail -8 "$WORK/$g.log" | sed 's/^/        /'; P5_KEEP=1; fi
  done
fi

printf '\n%s passed, %s failed\n' "$PASSES" "$FAILS"
if [ "$FAILS" = 0 ]; then echo "P5 GATE PASS"; exit 0; fi
echo "P5 GATE FAIL"; exit 1
