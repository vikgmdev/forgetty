#!/usr/bin/env bash
# P6 gate: packages, installer, tarball binary and a self-hosted relay.
#
#  1. Build the deb, rpm and tarball (cargo-zigbuild, glibc 2.28).
#  2. Content checks of the deb (dpkg-deb, plus lintian when installed), the rpm
#     (scripts/rpm_inspect.py) and the tarball.
#  3. scripts/install.sh --tarball --dry-run prints the expected steps, then a
#     real install into a temporary HOME works.
#  4. The tarball binary runs from a temp dir with --state-dir; `server status`
#     answers.
#  5. A local `iroh-relay --dev` (pinned 0.97) runs on localhost, the server
#     starts with --relay-url pointing at it, and a forgetty-client that binds no
#     UDP socket pairs and connects through that relay. With the relay stopped
#     the same client fails (negative control).
#
# Exit 0 only if every check passes.
# Env: IROH_RELAY_BIN (path of iroh-relay 0.97), P6_KEEP=1 (keep the work dir).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$HOME/.local/zig:$PATH"
TARGET="x86_64-unknown-linux-gnu"
RELAY_VERSION="0.97.0"
RELAY_PORT=3340     # fixed by `iroh-relay --dev`
WORK="$(mktemp -d /tmp/p6gate.XXXXXX)"
PIDS=()
FAILS=0
PASSES=0

cleanup() {
  local p
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  sleep 0.3
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null; done
  [ -z "${P6_KEEP:-}" ] && rm -rf "$WORK"
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
has() { grep -qE -- "$2" <<<"$1"; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "missing tool: $1 ($2)" >&2; exit 2; }; }

need cargo-zigbuild "cargo install cargo-zigbuild"
need cargo-deb "cargo install cargo-deb"
need cargo-generate-rpm "cargo install cargo-generate-rpm"
need zig "Zig 0.15 in PATH"
need dpkg-deb "dpkg"
need python3 "python3"
need readelf "binutils"

# ---------------------------------------------------------------- 1. build
say "1. build deb, rpm, tarball ($TARGET, glibc 2.28)"
OUT="$WORK/out"
if "$ROOT/scripts/build-server-dist.sh" --target "$TARGET" --out "$OUT" > "$WORK/build.log" 2>&1; then
  ok "scripts/build-server-dist.sh"
else
  bad "scripts/build-server-dist.sh (see $WORK/build.log)"; tail -20 "$WORK/build.log"
  echo "P6 GATE FAIL (build)"; P6_KEEP=1; exit 1
fi
DEB="$(ls "$OUT"/forgetty-server_*_amd64.deb 2>/dev/null | head -1)"
RPM="$(ls "$OUT"/forgetty-server-*.x86_64.rpm 2>/dev/null | head -1)"
TGZ="$(ls "$OUT"/forgetty-server-*-"$TARGET".tar.gz 2>/dev/null | head -1)"
[ -f "$DEB" ] && [ -f "$RPM" ] && [ -f "$TGZ" ] && ok "deb, rpm and tarball exist" || { bad "missing output file"; exit 1; }
check "SHA256SUMS verifies" bash -c "cd '$OUT' && sha256sum -c SHA256SUMS"

# ---------------------------------------------------------------- 2. deb
say "2. deb content (dpkg-deb)"
FIELDS="$(dpkg-deb -f "$DEB")"
has "$FIELDS" '^Package: forgetty-server$' && ok "Package: forgetty-server" || bad "Package field"
has "$FIELDS" '^Architecture: amd64$' && ok "Architecture: amd64" || bad "Architecture field"
has "$FIELDS" '^Version: [0-9]+\.[0-9]+\.[0-9]+~beta-1$|^Version: [0-9]+\.[0-9]+\.[0-9]+-1$' && ok "Version has a revision" || bad "Version field"
has "$FIELDS" '^Depends: libc6 \(>= 2\.28\)$' && ok "Depends: libc6 (>= 2.28)" || bad "Depends field"
has "$FIELDS" '^Conflicts: forgetty$' && ok "Conflicts: forgetty (GTK package owns /usr/bin/forgetty)" || bad "Conflicts field"
has "$FIELDS" '^Maintainer: .+<.+@.+>' && ok "Maintainer set" || bad "Maintainer field"

LIST="$(dpkg-deb -c "$DEB")"
for f in usr/bin/forgetty usr/lib/forgetty/libghostty-vt.so.0 usr/lib/systemd/user/forgetty-server.service \
         usr/share/doc/forgetty-server/server.md usr/share/doc/forgetty-server/relay.md \
         usr/share/doc/forgetty-server/copyright \
         usr/share/doc/forgetty-server/examples/iroh-relay.toml.example \
         usr/share/doc/forgetty-server/examples/iroh-relay.service; do
  has "$LIST" " \./$f\$" && ok "contains /$f" || bad "missing /$f"
done
has "$LIST" '^-rwxr-xr-x 0/0 .* \./usr/bin/forgetty$' && ok "/usr/bin/forgetty is 0755 root" || bad "/usr/bin/forgetty mode"
# No system-wide service, nothing in /etc, nothing outside /usr.
! has "$LIST" ' \./etc/' && ok "nothing in /etc" || bad "files in /etc"
! has "$LIST" 'systemd/system/' && ok "no system-wide systemd unit" || bad "system-wide unit shipped"
! has "$LIST" ' \./(lib|sbin|bin|var|opt|home|root|tmp)/' && ok "only /usr paths" || bad "files outside /usr"
! has "$LIST" '^.{3}[sS]|^.{6}[sS]' && ok "no setuid or setgid file" || bad "setuid/setgid file"
! has "$LIST" '^.{8}w' && ok "nothing world-writable" || bad "world-writable file"
! grep -vE ' 0/0 ' <<<"$LIST" | grep -q . && ok "all files owned by root (0/0)" || bad "non-root owner"

mkdir -p "$WORK/deb/root" "$WORK/deb/ctl"
dpkg-deb -x "$DEB" "$WORK/deb/root"; dpkg-deb -e "$DEB" "$WORK/deb/ctl"
POSTINST="$WORK/deb/ctl/postinst"
[ -x "$POSTINST" ] && ok "postinst is executable" || bad "postinst missing or not executable"
check "postinst is valid sh" sh -n "$POSTINST"
POST="$(cat "$POSTINST" 2>/dev/null)"
has "$POST" 'loginctl enable-linger "\$USER"' && ok "postinst prints: loginctl enable-linger" || bad "postinst lacks enable-linger"
has "$POST" 'systemctl --user enable --now forgetty-server' && ok "postinst prints: systemctl --user enable --now" || bad "postinst lacks systemctl --user enable --now"
CODE="$(awk '/^cat <<.?MSG.?$/{skip=1; next} skip && /^MSG$/{skip=0; next} !skip' "$POSTINST" | grep -vE '^\s*(#|$)')"
! has "$CODE" '(systemctl|loginctl|useradd|adduser|chown|chmod|deb-systemd|invoke-rc|service )' \
  && ok "postinst code starts nothing and changes no accounts (it only prints)" || bad "postinst does more than print: $CODE"
! has "$POST" '#DEBHELPER#' && ok "no leftover #DEBHELPER#" || bad "#DEBHELPER# left in postinst"
UNIT="$WORK/deb/root/usr/lib/systemd/user/forgetty-server.service"
has "$(cat "$UNIT")" '^ExecStart=/usr/bin/forgetty server run$' && ok "unit: ExecStart=/usr/bin/forgetty server run" || bad "unit ExecStart"
! has "$(cat "$UNIT")" '^(User|Group)=' && ok "unit sets no User= (runs as the user)" || bad "unit has User="
has "$(cat "$UNIT")" '^WantedBy=default.target$' && ok "unit: WantedBy=default.target" || bad "unit WantedBy"
has "$(cat "$UNIT")" '^KillMode=mixed$' && ok "unit: KillMode=mixed" || bad "unit KillMode"

BIN="$WORK/deb/root/usr/bin/forgetty"
RUNPATH="$(readelf -d "$BIN" | grep -E 'RUNPATH|RPATH')"
has "$RUNPATH" '\$ORIGIN/\.\./lib/forgetty' && ok "RUNPATH reaches /usr/lib/forgetty" || bad "RUNPATH misses \$ORIGIN/../lib/forgetty"
MAXGLIBC="$(objdump -T "$BIN" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"
[ "$(printf '%s\n2.28\n' "$MAXGLIBC" | sort -V | tail -1)" = "2.28" ] && ok "needs glibc $MAXGLIBC (<= 2.28)" || bad "needs glibc $MAXGLIBC, more than 2.28"
VER="$(env -u LD_LIBRARY_PATH "$BIN" --version 2>&1)"
has "$VER" '^forgetty [0-9]' && ok "binary from the deb runs and finds libghostty-vt: $VER" || bad "binary from the deb does not run: $VER"
if command -v lintian >/dev/null 2>&1; then
  LT="$(lintian --no-tag-display-limit "$DEB" 2>&1 | grep -E '^E:' || true)"
  [ -z "$LT" ] && ok "lintian: no errors" || { bad "lintian errors"; echo "$LT"; }
else
  echo "  note  lintian is not installed: the checks above are the lintian-like content check"
fi

# ---------------------------------------------------------------- 2b. rpm
say "2b. rpm content (scripts/rpm_inspect.py)"
python3 "$ROOT/scripts/rpm_inspect.py" "$RPM" > "$WORK/rpm.json" 2>"$WORK/rpm.err" && ok "rpm header readable" || { bad "rpm header unreadable"; cat "$WORK/rpm.err"; }
RPMJ="$(cat "$WORK/rpm.json")"
has "$RPMJ" '"name": "forgetty-server"' && ok "rpm name" || bad "rpm name"
has "$RPMJ" '"arch": "x86_64"' && ok "rpm arch x86_64" || bad "rpm arch"
for f in /usr/bin/forgetty /usr/lib/forgetty/libghostty-vt.so.0 /usr/lib/systemd/user/forgetty-server.service \
         /usr/share/doc/forgetty-server/server.md /usr/share/doc/forgetty-server/relay.md; do
  has "$RPMJ" "\"path\": \"$f\"" && ok "rpm contains $f" || bad "rpm misses $f"
done
has "$(tr -d '\n ' <<<"$RPMJ")" '"path":"/usr/bin/forgetty","mode":"0o100755"' && ok "rpm /usr/bin/forgetty is 0755" || bad "rpm binary mode"
! has "$RPMJ" '"path": "/etc/|systemd/system/' && ok "rpm: no /etc, no system-wide unit" || bad "rpm ships /etc or a system unit"
has "$(tr -d '\n ' <<<"$RPMJ")" '"glibc","2\.28"' && ok "rpm requires glibc 2.28" || bad "rpm glibc requirement"
has "$RPMJ" '"forgetty"' && ok "rpm conflicts with forgetty" || bad "rpm conflicts"
has "$RPMJ" 'systemctl --user enable --now forgetty-server' && ok "rpm post-install prints the enable commands" || bad "rpm post-install script"

# ---------------------------------------------------------------- 3. tarball + installer
say "3. tarball and scripts/install.sh"
TLIST="$(tar -tzf "$TGZ")"
for f in bin/forgetty lib/libghostty-vt.so.0 share/systemd/forgetty-server.service; do
  has "$TLIST" "/$f\$" && ok "tarball contains $f" || bad "tarball misses $f"
done
FAKEHOME="$WORK/home"; mkdir -p "$FAKEHOME"
DRY="$(HOME="$FAKEHOME" bash "$ROOT/scripts/install.sh" --tarball "$TGZ" --dry-run 2>&1 | sed 's/\x1b\[[0-9;]*m//g')"
echo "$DRY" | sed 's/^/    | /'
has "$DRY" 'install format: tarball' && ok "dry-run: tarball route" || bad "dry-run route"
has "$DRY" "\[dry-run\] extract $TGZ" && ok "dry-run: extract step" || bad "dry-run extract step"
has "$DRY" "install -Dm755 bin/forgetty $FAKEHOME/.local/bin/forgetty" && ok "dry-run: binary into ~/.local/bin" || bad "dry-run binary step"
has "$DRY" "libghostty-vt.so.0 $FAKEHOME/.local/lib/forgetty/libghostty-vt.so.0" && ok "dry-run: library into ~/.local/lib/forgetty" || bad "dry-run library step"
has "$DRY" "forgetty-server.service $FAKEHOME/.config/systemd/user/forgetty-server.service" && ok "dry-run: user unit step" || bad "dry-run unit step"
has "$DRY" 'loginctl enable-linger' && has "$DRY" 'systemctl --user enable --now forgetty-server' && ok "dry-run: prints the two enable commands" || bad "dry-run next steps"
has "$DRY" 'sha256' && ok "dry-run: sha256 step" || bad "dry-run sha256 step"
has "$DRY" 'nothing was changed' && [ -z "$(ls -A "$FAKEHOME")" ] && ok "dry-run changed nothing" || bad "dry-run changed files"
has "$(bash "$ROOT/scripts/install.sh" --dry-run --channel beta 2>&1 | sed 's/\x1b\[[0-9;]*m//g')" 'resolve the newest beta' && ok "--channel beta accepted (dry-run)" || bad "--channel beta"
! bash "$ROOT/scripts/install.sh" --channel nightly --dry-run >/dev/null 2>&1 && ok "unknown channel refused" || bad "unknown channel accepted"

REAL="$(HOME="$FAKEHOME" bash "$ROOT/scripts/install.sh" --tarball "$TGZ" 2>&1 | sed 's/\x1b\[[0-9;]*m//g')"
has "$REAL" '^\[forgetty\] forgetty [0-9]|^forgetty [0-9]' && ok "real install into a temporary HOME runs --version" || { bad "real install"; echo "$REAL" | tail -5; }
[ -x "$FAKEHOME/.local/bin/forgetty" ] && [ -f "$FAKEHOME/.local/lib/forgetty/libghostty-vt.so.0" ] && [ -f "$FAKEHOME/.config/systemd/user/forgetty-server.service" ] \
  && ok "installed: bin, lib, user unit (no root used)" || bad "installed files missing"

# ---------------------------------------------------------------- 4. tarball binary, --state-dir
say "4. tarball binary from a temp dir, --state-dir, server status"
mkdir -p "$WORK/tb" "$WORK/elsewhere"
tar -xzf "$TGZ" -C "$WORK/tb"
TB_BIN="$(ls -d "$WORK"/tb/*/bin/forgetty)"
STATE1="$WORK/s1"
( cd "$WORK/elsewhere" && exec "$TB_BIN" --state-dir "$STATE1" server run --port 0 > "$WORK/server1.log" 2>&1 ) &
PIDS+=($!)
STATUS=""
for _ in $(seq 1 40); do
  STATUS="$(cd "$WORK/elsewhere" && "$TB_BIN" --state-dir "$STATE1" server status --json 2>/dev/null)" && [ -n "$STATUS" ] && break
  sleep 0.5
done
has "$STATUS" '"pid"' && ok "forgetty server status --json answers: $(tr -d '\n ' <<<"$STATUS" | cut -c1-110)" || { bad "server status did not answer"; tail -5 "$WORK/server1.log"; }
has "$(cd "$WORK/elsewhere" && "$TB_BIN" --state-dir "$STATE1" server status 2>&1)" 'running|pid' && ok "human server status works" || bad "human server status"
(cd "$WORK/elsewhere" && "$TB_BIN" --state-dir "$STATE1" server stop >/dev/null 2>&1) && ok "server stop" || bad "server stop"
sleep 1

# ---------------------------------------------------------------- 5. own relay
say "5. self-hosted relay on localhost (iroh-relay $RELAY_VERSION, --dev)"
RELAY="${IROH_RELAY_BIN:-}"
[ -z "$RELAY" ] && RELAY="$(command -v iroh-relay || true)"
[ -z "$RELAY" ] && [ -x "$HOME/.cache/forgetty/iroh-relay-$RELAY_VERSION/bin/iroh-relay" ] && RELAY="$HOME/.cache/forgetty/iroh-relay-$RELAY_VERSION/bin/iroh-relay"
if [ -z "$RELAY" ]; then
  echo "  installing iroh-relay $RELAY_VERSION (cargo install, a few minutes)"
  cargo install iroh-relay --version "=$RELAY_VERSION" --features server --locked \
    --root "$HOME/.cache/forgetty/iroh-relay-$RELAY_VERSION" > "$WORK/relay-install.log" 2>&1 \
    && RELAY="$HOME/.cache/forgetty/iroh-relay-$RELAY_VERSION/bin/iroh-relay" || { bad "cargo install iroh-relay"; tail -5 "$WORK/relay-install.log"; }
fi
RV="$("$RELAY" --version 2>&1)"
has "$RV" "^iroh-relay ${RELAY_VERSION%.*}\." && ok "$RV (matches iroh 0.97)" || bad "iroh-relay version is '$RV', need 0.97.x"

if (exec 3<>/dev/tcp/127.0.0.1/$RELAY_PORT) 2>/dev/null; then
  bad "port $RELAY_PORT is in use (iroh-relay --dev needs it)"; echo "P6 GATE FAIL"; exit 1
fi
"$RELAY" --dev > "$WORK/relay.log" 2>&1 &
RELAY_PID=$!; PIDS+=($RELAY_PID)
for _ in $(seq 1 40); do (exec 3<>/dev/tcp/127.0.0.1/$RELAY_PORT) 2>/dev/null && break; sleep 0.25; done
(exec 3<>/dev/tcp/127.0.0.1/$RELAY_PORT) 2>/dev/null && ok "iroh-relay --dev listens on 127.0.0.1:$RELAY_PORT" || { bad "relay did not start"; tail -5 "$WORK/relay.log"; }
RELAY_URL="http://127.0.0.1:$RELAY_PORT"

STATE2="$WORK/s2"
( cd "$WORK/elsewhere" && exec "$TB_BIN" --state-dir "$STATE2" server run --port 0 --relay-url "$RELAY_URL" > "$WORK/server2.log" 2>&1 ) &
PIDS+=($!)
for _ in $(seq 1 60); do grep -q "home is now relay" "$WORK/server2.log" 2>/dev/null && break; sleep 0.5; done
grep -q "relays: $RELAY_URL/" "$WORK/server2.log" && ok "server log: relays: $RELAY_URL/" || bad "server did not log the relay"
grep -q "home is now relay $RELAY_URL/" "$WORK/server2.log" && ok "server connected to the relay (home relay)" || bad "server has no home relay"
grep -q "n0 discovery: off" "$WORK/server2.log" && ok "server log: n0 discovery: off (relay configured)" || bad "n0 discovery not off with a relay configured"
# n0 discovery really is off: no TCP connection from the server to anything but localhost.
SPID="$(pgrep -f "[s]2 server run --port 0 --relay-url" | head -1)"
sleep 4
EXTERN="$(ss -tn state established -p 2>/dev/null | grep "pid=$SPID," | awk '{print $4}' | grep -vE '^(127\.0\.0\.1|\[::1\]):' || true)"
[ -n "$SPID" ] && [ -z "$EXTERN" ] && ok "server (n0 off) holds no connection outside localhost" || bad "server talks to the outside with n0 off: $EXTERN"
# control: --n0-discovery brings it back (soft check: needs internet access).
( cd "$WORK/elsewhere" && exec "$TB_BIN" --state-dir "$WORK/s5" server run --port 0 --relay-url "$RELAY_URL" --n0-discovery > "$WORK/server5.log" 2>&1 ) &
P5=$!; PIDS+=($P5)
for _ in $(seq 1 40); do grep -q "n0 discovery:" "$WORK/server5.log" 2>/dev/null && break; sleep 0.25; done
grep -q "n0 discovery: on" "$WORK/server5.log" && ok "--n0-discovery turns it back on" || bad "--n0-discovery had no effect"
sleep 6
EXTERN5="$(ss -tn state established -p 2>/dev/null | grep "pid=$P5," | awk '{print $4}' | grep -vE '^(127\.0\.0\.1|\[::1\]):' || true)"
[ -n "$EXTERN5" ] && ok "control: with --n0-discovery the server connects out ($EXTERN5)" || echo "  note  control: no outside connection seen (offline machine?)"
kill "$P5" 2>/dev/null

( cd "$WORK/elsewhere" && exec "$TB_BIN" --state-dir "$STATE2" pair > "$WORK/pair.log" 2>&1 ) &
PIDS+=($!)
PAYLOAD=""
for _ in $(seq 1 40); do
  PAYLOAD="$(grep -a '^payload: ' "$WORK/pair.log" 2>/dev/null | head -1 | sed 's/^payload: //')"; [ -n "$PAYLOAD" ] && break; sleep 0.25
done
has "$PAYLOAD" "\"relay\":\"$RELAY_URL/\"" && ok "pairing payload carries the relay: $RELAY_URL/" || bad "pairing payload has no relay: $PAYLOAD"

echo "  building the forgetty-client probe"
( cd "$ROOT" && cargo build -q -p forgetty-client --example relay_probe ) > "$WORK/probe-build.log" 2>&1 && ok "relay_probe example built" || { bad "relay_probe build"; tail -5 "$WORK/probe-build.log"; }
PROBE="$ROOT/target/debug/examples/relay_probe"
OUTP="$("$PROBE" --payload "$PAYLOAD" --dir "$WORK/probe" --timeout 30 2>&1)"
echo "    | $OUTP"
has "$OUTP" 'RELAY_PROBE_OK .*direct_paths=disabled' && ok "client with only the pairing payload paired and connected through the relay; n0 off on both sides, direct paths disabled" || bad "relay probe failed"
for _ in $(seq 1 20); do grep -aq '^paired:' "$WORK/pair.log" && break; sleep 0.25; done
grep -aq '^paired: relay-probe' "$WORK/pair.log" && ok "server confirms the paired device" || bad "server did not report the pairing"

# negative control: no relay, no connection
kill "$RELAY_PID" 2>/dev/null; wait "$RELAY_PID" 2>/dev/null
OUTN="$("$PROBE" --payload "$PAYLOAD" --dir "$WORK/probe" --no-pair --timeout 8 2>&1)"
has "$OUTN" 'RELAY_PROBE_FAIL' && ok "negative control: with the relay stopped the same client cannot connect" || bad "client connected without a relay: $OUTN"

# relay_urls from the config file, and a bad URL
CFG="$WORK/cfg.toml"; printf 'relay_urls = ["https://relay.example.org"]\n' > "$CFG"
( cd "$WORK/elsewhere" && exec "$TB_BIN" --state-dir "$WORK/s3" server run --port 0 --config-file "$CFG" > "$WORK/server3.log" 2>&1 ) &
P3=$!; PIDS+=($P3)
for _ in $(seq 1 40); do grep -q "relays:" "$WORK/server3.log" 2>/dev/null && break; sleep 0.25; done
grep -q "relays: https://relay.example.org/" "$WORK/server3.log" && ok "relay_urls in config.toml is used" || bad "config relay_urls not used"
kill "$P3" 2>/dev/null
BADOUT="$("$TB_BIN" --state-dir "$WORK/s4" server run --port 0 --relay-url 'not a url' 2>&1; echo "rc=$?")"
has "$BADOUT" 'invalid relay URL' && has "$BADOUT" 'rc=[1-9]' && ok "an invalid --relay-url stops the server with an error" || bad "bad relay url not refused: $BADOUT"

say "result"
echo "  $PASSES passed, $FAILS failed"
if [ "$FAILS" -eq 0 ]; then echo "P6 GATE PASS"; exit 0; fi
echo "P6 GATE FAIL (work dir kept: $WORK)"; P6_KEEP=1; exit 1
