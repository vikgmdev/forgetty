#!/usr/bin/env bash
set -euo pipefail

# ── Forgetty uninstaller ────────────────────────────────────────────
# Removes all files installed by install.sh.
# Safe to run multiple times (idempotent).
# ────────────────────────────────────────────────────────────────────

# ── Colours (no-op when piped) ──────────────────────────────────────
if [ -t 1 ]; then
    GREEN='\033[0;32m'
    BOLD='\033[1m'
    RESET='\033[0m'
else
    GREEN='' BOLD='' RESET=''
fi

info() { printf "${GREEN}[-]${RESET} %s\n" "$1"; }

# ── Remove binaries (requires sudo) ────────────────────────────────
info "Removing /usr/local/bin/forgetty"
sudo rm -f /usr/local/bin/forgetty

info "Removing /usr/local/bin/forgetty-daemon"
sudo rm -f /usr/local/bin/forgetty-daemon

# ── Remove shared library (requires sudo) ───────────────────────────
info "Removing libghostty-vt.so from /usr/local/lib/"
sudo rm -f /usr/local/lib/libghostty-vt.so \
           /usr/local/lib/libghostty-vt.so.0 \
           /usr/local/lib/libghostty-vt.so.0.1.0

info "Running ldconfig"
sudo ldconfig

# ── Remove desktop entry (user-local, no sudo) ──────────────────────
DESKTOP="$HOME/.local/share/applications/dev.forgetty.Forgetty.desktop"
info "Removing $DESKTOP"
rm -f "$DESKTOP"

# ── Remove icon (user-local, no sudo) ───────────────────────────────
ICON="$HOME/.local/share/icons/hicolor/scalable/apps/dev.forgetty.Forgetty.svg"
info "Removing $ICON"
rm -f "$ICON"

# ── Update desktop database and icon cache ──────────────────────────
update-desktop-database "$HOME/.local/share/applications/" 2>/dev/null || true
gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor/" 2>/dev/null || true

# ── Done ────────────────────────────────────────────────────────────
printf "\n${BOLD}${GREEN}Forgetty uninstalled successfully.${RESET}\n"
