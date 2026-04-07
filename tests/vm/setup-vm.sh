#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# Forgetty VM Test Setup  (T-026)
#
# Installs test prerequisites on a fresh Ubuntu 24.04 Desktop VM.
# Idempotent -- safe to run multiple times.
#
# Usage:
#   sudo ./setup-vm.sh
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

# ── Colors ────────────────────────────────────────────────────────────
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

# ── Root check ────────────────────────────────────────────────────────
if [ "$(id -u)" -ne 0 ]; then
    error "This script must be run as root (use sudo)."
    exit 1
fi

# ── OS check ──────────────────────────────────────────────────────────
if [ -f /etc/os-release ]; then
    . /etc/os-release
    info "Detected OS: $PRETTY_NAME"
    if [[ "${ID:-}" != "ubuntu" ]]; then
        echo "  Warning: This script is designed for Ubuntu 24.04. Proceeding anyway."
    fi
else
    echo "  Warning: Cannot detect OS. Proceeding anyway."
fi

# ── Update package lists ─────────────────────────────────────────────
info "Updating package lists"
apt-get update -qq

# ── Install test prerequisites ────────────────────────────────────────
# These are programs exercised by the test suite, NOT build dependencies.
PACKAGES=(
    zsh
    fish
    tmux
    screen
    openssh-server
    desktop-file-utils
    xdotool
    vim
    htop
    man-db
)

info "Installing test prerequisites: ${PACKAGES[*]}"
apt-get install -y --no-install-recommends "${PACKAGES[@]}"

# ── Enable SSH ────────────────────────────────────────────────────────
info "Enabling SSH server"
systemctl enable --now ssh 2>/dev/null || systemctl enable --now sshd 2>/dev/null || true

# ── Verify installations ─────────────────────────────────────────────
info "Verifying installations"
ALL_OK=true
for cmd in zsh fish tmux screen sshd vim htop desktop-file-validate xdotool man; do
    if command -v "$cmd" &>/dev/null; then
        printf "  %-25s %s\n" "$cmd" "OK"
    else
        printf "  %-25s %s\n" "$cmd" "MISSING"
        ALL_OK=false
    fi
done

echo ""
if $ALL_OK; then
    printf "${BOLD}${GREEN}All test prerequisites installed successfully.${RESET}\n"
else
    error "Some prerequisites are missing. Check output above."
    exit 1
fi

echo ""
echo "Next steps:"
echo "  1. Transfer the forgetty .deb file to this VM"
echo "  2. Run: sudo dpkg -i forgetty_*.deb && sudo apt-get -f install -y"
echo "  3. Run: ./run-vm-tests.sh"
