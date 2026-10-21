#!/usr/bin/env bash
#
# Forgetty server installer. One line, no root needed for the tarball route.
#
#   curl -fsSL https://raw.githubusercontent.com/vikgmdev/forgetty/main/scripts/install.sh | bash
#   bash install.sh --dry-run                    # print the steps, change nothing
#   bash install.sh --channel beta               # stable (default) | beta
#   bash install.sh --tarball FILE_OR_URL        # binary into ~/.local/bin, no root
#
# What it does:
#   - Without --tarball it looks at the machine: dpkg -> the .deb, rpm -> the
#     .rpm (both through sudo), anything else -> the tarball.
#   - With --tarball it never uses root: bin/forgetty -> ~/.local/bin,
#     libghostty-vt -> ~/.local/lib/forgetty, the user unit -> ~/.config/systemd/user.
#   - It never enables or starts the server. It prints the two commands.
#
# Overrides: FORGETTY_BIN_DIR, FORGETTY_LIB_DIR, FORGETTY_UNIT_DIR, FORGETTY_REPO.
# Uninstall (tarball): rm ~/.local/bin/forgetty ~/.local/lib/forgetty/libghostty-vt.so.0 \
#   ~/.config/systemd/user/forgetty-server.service   (stop the service first).
set -euo pipefail

REPO="${FORGETTY_REPO:-vikgmdev/forgetty}"
BIN_DIR="${FORGETTY_BIN_DIR:-$HOME/.local/bin}"
LIB_DIR="${FORGETTY_LIB_DIR:-$HOME/.local/lib/forgetty}"
UNIT_DIR="${FORGETTY_UNIT_DIR:-$HOME/.config/systemd/user}"
TAG_PREFIX="server-v"

DRY=0
CHANNEL="stable"
TARBALL=""
VERSION=""

say()  { printf '\033[1;34m[forgetty]\033[0m %s\n' "$*"; }
step() { if [ "$DRY" = 1 ]; then printf '\033[1;33m[dry-run]\033[0m %s\n' "$*"; else say "$*"; fi; }
fail() { printf '\033[1;31m[forgetty]\033[0m %s\n' "$*" >&2; exit 1; }
usage() { sed -n '3,19p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --channel) [ $# -ge 2 ] || fail "--channel needs stable or beta"; CHANNEL="$2"; shift 2 ;;
    --tarball) [ $# -ge 2 ] || fail "--tarball needs a file or URL"; TARBALL="$2"; shift 2 ;;
    --version) [ $# -ge 2 ] || fail "--version needs a version"; VERSION="${2#"$TAG_PREFIX"}"; shift 2 ;;
    -h|--help) usage 0 ;;
    *) printf 'unknown option: %s\n' "$1" >&2; usage 2 ;;
  esac
done
case "$CHANNEL" in stable|beta) ;; *) fail "--channel must be stable or beta" ;; esac

# --- machine ---------------------------------------------------------------
[ "$(uname -s)" = Linux ] || fail "Linux only (this is $(uname -s))."
case "$(uname -m)" in
  x86_64|amd64)  ARCH=x86_64;  DEB_ARCH=amd64 ;;
  aarch64|arm64) ARCH=aarch64; DEB_ARCH=arm64 ;;
  *) fail "unsupported CPU: $(uname -m) (x86_64 and aarch64 only)." ;;
esac
TARGET="${ARCH}-unknown-linux-gnu"

if [ -n "$TARBALL" ]; then
  FORMAT=tarball
elif command -v dpkg >/dev/null 2>&1 && command -v apt-get >/dev/null 2>&1; then
  FORMAT=deb
elif command -v rpm >/dev/null 2>&1 && { command -v dnf >/dev/null 2>&1 || command -v yum >/dev/null 2>&1 || command -v zypper >/dev/null 2>&1; }; then
  FORMAT=rpm
else
  FORMAT=tarball
fi
say "machine: linux $ARCH, install format: $FORMAT, channel: $CHANNEL"

# --- which release ---------------------------------------------------------
if [ "$FORMAT" = tarball ] && [ -n "$TARBALL" ]; then
  SOURCE="$TARBALL"
else
  if [ -z "$VERSION" ]; then
    step "resolve the newest $CHANNEL server release from https://api.github.com/repos/$REPO/releases (tag $TAG_PREFIX*)"
    if [ "$DRY" = 1 ]; then
      VERSION="<version>"
    else
      command -v curl >/dev/null 2>&1 || fail "curl is required."
      command -v python3 >/dev/null 2>&1 || fail "python3 is required to read the release list. Pass --version X or --tarball FILE."
      VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=50" | python3 -c '
import json, sys
channel, prefix = sys.argv[1], sys.argv[2]
for r in json.load(sys.stdin):
    tag = r.get("tag_name", "")
    if not tag.startswith(prefix) or r.get("draft"):
        continue
    if channel == "stable" and r.get("prerelease"):
        continue
    print(tag[len(prefix):])
    break
' "$CHANNEL" "$TAG_PREFIX")"
      [ -n "$VERSION" ] || fail "no $CHANNEL server release found in $REPO. Try --channel beta, --version X or --tarball FILE."
    fi
  fi
  BASE="https://github.com/$REPO/releases/download/${TAG_PREFIX}${VERSION}"
  case "$FORMAT" in
    deb)     SOURCE="$BASE/forgetty-server_${VERSION}_${DEB_ARCH}.deb" ;;
    rpm)     SOURCE="$BASE/forgetty-server-${VERSION}.${ARCH}.rpm" ;;
    tarball) SOURCE="$BASE/forgetty-server-${VERSION}-${TARGET}.tar.gz" ;;
  esac
fi

NEXT_STEPS='  loginctl enable-linger "$USER"
  systemctl --user enable --now forgetty-server
  forgetty pair'

# --- fetch and verify ------------------------------------------------------
WORK=""
cleanup() { [ -n "$WORK" ] && rm -rf "$WORK"; return 0; }
trap cleanup EXIT

is_url() { case "$1" in http://*|https://*) return 0 ;; *) return 1 ;; esac; }

if is_url "$SOURCE"; then
  step "download $SOURCE"
  step "download $(dirname "$SOURCE")/SHA256SUMS and check sha256"
  FILE="$(basename "$SOURCE")"
  if [ "$DRY" != 1 ]; then
    command -v curl >/dev/null 2>&1 || fail "curl is required."
    WORK="$(mktemp -d)"
    curl -fsSL -o "$WORK/$FILE" "$SOURCE" || fail "download failed: $SOURCE"
    curl -fsSL -o "$WORK/SHA256SUMS" "$(dirname "$SOURCE")/SHA256SUMS" || fail "SHA256SUMS missing next to $SOURCE"
    # Same name with a URL-encoded "~" in the sums file is not expected: the
    # build writes the file names as they are.
    (cd "$WORK" && grep -F " $FILE" SHA256SUMS | sha256sum -c - >/dev/null) \
      || fail "sha256 check failed for $FILE"
    say "sha256 ok"
    LOCAL="$WORK/$FILE"
  else
    LOCAL="$FILE"
  fi
else
  LOCAL="$SOURCE"
  if [ "$DRY" != 1 ]; then
    [ -f "$LOCAL" ] || fail "no such file: $LOCAL"
  fi
  step "use local file $LOCAL"
  SUMS="$(dirname "$LOCAL")/SHA256SUMS"
  if [ -f "$SUMS" ] && grep -qF " $(basename "$LOCAL")" "$SUMS"; then
    step "check sha256 against $SUMS"
    if [ "$DRY" != 1 ]; then
      (cd "$(dirname "$LOCAL")" && grep -F " $(basename "$LOCAL")" SHA256SUMS | sha256sum -c - >/dev/null) \
        || fail "sha256 check failed for $LOCAL"
      say "sha256 ok"
    fi
  else
    step "no SHA256SUMS next to the file: skip the sha256 check"
  fi
fi

# --- install ---------------------------------------------------------------
as_root() {
  if [ "$(id -u)" = 0 ]; then "$@"; elif command -v sudo >/dev/null 2>&1; then sudo "$@"; else
    fail "root is needed for the $FORMAT package and sudo was not found. Use --tarball FILE (no root)."
  fi
}

case "$FORMAT" in
  deb)
    step "sudo apt-get install -y $LOCAL   (installs /usr/bin/forgetty and the systemd user unit; nothing is started)"
    [ "$DRY" = 1 ] || as_root apt-get install -y "$LOCAL"
    ;;
  rpm)
    if command -v dnf >/dev/null 2>&1; then CMD=(dnf install -y)
    elif command -v zypper >/dev/null 2>&1; then CMD=(zypper --non-interactive install)
    else CMD=(yum install -y); fi
    step "sudo ${CMD[*]} $LOCAL   (installs /usr/bin/forgetty and the systemd user unit; nothing is started)"
    [ "$DRY" = 1 ] || as_root "${CMD[@]}" "$LOCAL"
    ;;
  tarball)
    step "extract $LOCAL to a temporary directory"
    step "install -Dm755 bin/forgetty $BIN_DIR/forgetty"
    step "install -Dm644 lib/libghostty-vt.so.0 $LIB_DIR/libghostty-vt.so.0"
    step "install -Dm644 share/systemd/forgetty-server.service $UNIT_DIR/forgetty-server.service"
    step "run $BIN_DIR/forgetty --version to check the install"
    if [ "$DRY" != 1 ]; then
      WORK="${WORK:-$(mktemp -d)}"
      tar -xzf "$LOCAL" -C "$WORK"
      ROOT_DIR="$(find "$WORK" -mindepth 2 -maxdepth 2 -type d -name bin -print -quit | xargs -r dirname)"
      [ -n "$ROOT_DIR" ] && [ -x "$ROOT_DIR/bin/forgetty" ] || fail "the tarball has no bin/forgetty"
      install -Dm755 "$ROOT_DIR/bin/forgetty" "$BIN_DIR/forgetty"
      install -Dm644 "$ROOT_DIR/lib/libghostty-vt.so.0" "$LIB_DIR/libghostty-vt.so.0"
      install -Dm644 "$ROOT_DIR/share/systemd/forgetty-server.service" "$UNIT_DIR/forgetty-server.service"
      "$BIN_DIR/forgetty" --version || fail "the installed binary does not start (is glibc >= 2.28?)"
    fi
    case ":$PATH:" in
      *":$BIN_DIR:"*) ;;
      *) step "note: add $BIN_DIR to your PATH (export PATH=\"$BIN_DIR:\$PATH\")" ;;
    esac
    ;;
esac

step "next steps (the installer does not start anything):"
printf '%s\n' "$NEXT_STEPS"
[ "$DRY" = 1 ] && say "dry run: nothing was changed." || say "installed."
