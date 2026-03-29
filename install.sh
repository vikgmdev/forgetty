#!/usr/bin/env bash
set -euo pipefail

# ── Forgetty installer ──────────────────────────────────────────────
# Builds the release binary and installs it along with the shared
# library, desktop entry, and icon.
# ────────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="$SCRIPT_DIR/target/release/forgetty"

# ── Colours (no-op when piped) ──────────────────────────────────────
if [ -t 1 ]; then
    GREEN='\033[0;32m'
    RED='\033[0;31m'
    BOLD='\033[1m'
    RESET='\033[0m'
else
    GREEN='' RED='' BOLD='' RESET=''
fi

info()  { printf "${GREEN}[+]${RESET} %s\n" "$1"; }
error() { printf "${RED}[!]${RESET} %s\n" "$1" >&2; }

# ── Build release binary ───────────────────────────────────────────
info "Building release binary..."
(cd "$SCRIPT_DIR" && cargo build --release)

if [ ! -f "$BINARY" ]; then
    error "Build failed: release binary not found at $BINARY"
    exit 1
fi

# ── Locate libghostty-vt.so ────────────────────────────────────────
SO_NAME="libghostty-vt.so.0.1.0"
SO_DIR=""

# 1. Canonical location: target/release/lib/ (post-build copy from T-023)
if [ -f "$SCRIPT_DIR/target/release/lib/$SO_NAME" ]; then
    SO_DIR="$SCRIPT_DIR/target/release/lib"
fi

# 2. Fallback: deep build output directory (newest match)
if [ -z "$SO_DIR" ]; then
    for candidate in $(ls -dt "$SCRIPT_DIR"/target/release/build/forgetty-vt-*/out/ghostty-install/lib/ 2>/dev/null); do
        if [ -f "$candidate/$SO_NAME" ]; then
            SO_DIR="$candidate"
            break
        fi
    done
fi

# 3. Fallback: check crates/forgetty-vt/ghostty-out/lib/
if [ -z "$SO_DIR" ] && [ -f "$SCRIPT_DIR/crates/forgetty-vt/ghostty-out/lib/$SO_NAME" ]; then
    SO_DIR="$SCRIPT_DIR/crates/forgetty-vt/ghostty-out/lib"
fi

if [ -z "$SO_DIR" ]; then
    error "Could not find $SO_NAME in build output."
    error "Run 'cargo build --release' first."
    exit 1
fi

info "Found shared library in: $SO_DIR"

# ── Install binary (requires sudo) ─────────────────────────────────
info "Installing binary to /usr/local/bin/forgetty"
sudo install -Dm755 "$BINARY" /usr/local/bin/forgetty

# ── Install shared library (requires sudo) ──────────────────────────
info "Installing $SO_NAME to /usr/local/lib/"
sudo install -Dm755 "$SO_DIR/$SO_NAME" "/usr/local/lib/$SO_NAME"
sudo ln -sf "$SO_NAME"            /usr/local/lib/libghostty-vt.so.0
sudo ln -sf libghostty-vt.so.0    /usr/local/lib/libghostty-vt.so

info "Running ldconfig"
sudo ldconfig

# ── Install desktop entry (user-local, no sudo) ─────────────────────
DESKTOP_SRC="$SCRIPT_DIR/dist/linux/dev.forgetty.Forgetty.desktop"
DESKTOP_DST="$HOME/.local/share/applications/dev.forgetty.Forgetty.desktop"

info "Installing desktop entry to $DESKTOP_DST"
install -Dm644 "$DESKTOP_SRC" "$DESKTOP_DST"

# ── Install icon (user-local, no sudo) ──────────────────────────────
ICON_SRC="$SCRIPT_DIR/assets/icons/dev.forgetty.Forgetty.svg"
ICON_DST="$HOME/.local/share/icons/hicolor/scalable/apps/dev.forgetty.Forgetty.svg"

info "Installing icon to $ICON_DST"
install -Dm644 "$ICON_SRC" "$ICON_DST"

# ── Update desktop database and icon cache ──────────────────────────
update-desktop-database "$HOME/.local/share/applications/" 2>/dev/null || true
gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor/" 2>/dev/null || true

# ── Done ────────────────────────────────────────────────────────────
printf "\n${BOLD}${GREEN}Forgetty installed successfully!${RESET}\n"
echo ""
echo "  Binary:   /usr/local/bin/forgetty"
echo "  Library:  /usr/local/lib/$SO_NAME"
echo "  Desktop:  $DESKTOP_DST"
echo "  Icon:     $ICON_DST"
echo ""
echo "  Search 'Forgetty' in GNOME Activities to launch."
echo "  Run './uninstall.sh' to remove."
