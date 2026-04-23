#!/usr/bin/env bash
# Launch the forgetty dev build (this repo's target/release binaries)
# in an XDG-isolated sandbox so it cannot see or touch the installed
# production daemon's state (sessions, byte logs, identity key, socket).
#
# - Sessions live under /tmp/forgetty-dev/data/forgetty/sessions/
# - Socket lives under /tmp/forgetty-dev/runtime/forgetty-{uuid}.sock
# - Identity key lives under /tmp/forgetty-dev/data/forgetty/identity.key
#
# The installed production build at ~/.local/share/forgetty/ is untouched.
#
# Usage:
#   ./launch-dev.sh             # normal daemon-backed launch
#   ./launch-dev.sh --stop      # send shutdown_save to every dev daemon
#                               # socket in the sandbox. Preserves session
#                               # JSONs for the next cold-restart test.
#                               # (shutdown_clean would trash unpinned
#                               # sessions — that's "Close Permanently".)
#   ./launch-dev.sh --fresh     # --stop, then wipe sandbox, then launch
#   ./launch-dev.sh --clean     # --stop, then wipe sandbox, exit (no launch)
#
# Binary discovery: `ensure_daemon` in the GTK client resolves
# `forgetty-daemon` relative to its own binary path before falling back to
# $PATH, so running ./target/release/forgetty correctly picks up
# ./target/release/forgetty-daemon — never the installed one.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${REPO_DIR}/target/release/forgetty"
DAEMON_BIN="${REPO_DIR}/target/release/forgetty-daemon"
SANDBOX="/tmp/forgetty-dev"

mode="launch"
extra_args=()
for arg in "$@"; do
  case "$arg" in
    --fresh) mode="fresh-launch" ;;
    --clean) mode="clean" ;;
    --stop)  mode="stop" ;;
    *)       extra_args+=("$arg") ;;
  esac
done

stop_dev_daemons() {
  local socket_dir="$SANDBOX/runtime"
  [[ -d "$socket_dir" ]] || return 0
  shopt -s nullglob
  local socks=("$socket_dir"/forgetty-*.sock)
  shopt -u nullglob
  [[ ${#socks[@]} -gt 0 ]] || return 0
  if ! command -v socat >/dev/null 2>&1; then
    echo "warn: socat not found; cannot send shutdown_save RPC" >&2
    echo "      dev daemon sockets will become stale:" >&2
    printf '      %s\n' "${socks[@]}" >&2
    return 0
  fi
  # Use shutdown_save (save state + exit) rather than shutdown_clean,
  # which would move unpinned session JSONs to sessions/trash/ — the
  # "Close Permanently" semantics, not what a test cold-restart needs.
  for sock in "${socks[@]}"; do
    echo "stopping dev daemon at $sock"
    printf '{"jsonrpc":"2.0","method":"shutdown_save","params":{},"id":1}\n' \
      | socat -t 2 - "UNIX-CONNECT:$sock" >/dev/null 2>&1 || true
  done
  sleep 0.5
}

if [[ "$mode" == "stop" || "$mode" == "clean" || "$mode" == "fresh-launch" ]]; then
  stop_dev_daemons
fi

if [[ "$mode" == "clean" || "$mode" == "fresh-launch" ]]; then
  echo "Wiping sandbox at ${SANDBOX}"
  rm -rf "$SANDBOX"
fi

if [[ "$mode" == "clean" || "$mode" == "stop" ]]; then
  exit 0
fi

if [[ ! -x "$BIN" ]]; then
  echo "error: dev binary not found at $BIN" >&2
  echo "build first:  cargo build --release" >&2
  exit 1
fi
if [[ ! -x "$DAEMON_BIN" ]]; then
  echo "error: dev daemon not found at $DAEMON_BIN" >&2
  echo "build first:  cargo build --release" >&2
  exit 1
fi

mkdir -p "$SANDBOX"/{config,data,state,runtime}
chmod 700 "$SANDBOX/runtime"

echo "sandbox:     $SANDBOX"
echo "dev binary:  $BIN"
echo "dev daemon:  $DAEMON_BIN"
echo "sessions:    $SANDBOX/data/forgetty/sessions/"
echo "socket dir:  $SANDBOX/runtime/"
echo "identity:    $SANDBOX/data/forgetty/identity.key"
echo

export XDG_CONFIG_HOME="$SANDBOX/config"
export XDG_DATA_HOME="$SANDBOX/data"
export XDG_STATE_HOME="$SANDBOX/state"
export XDG_RUNTIME_DIR="$SANDBOX/runtime"

exec "$BIN" "${extra_args[@]}"
