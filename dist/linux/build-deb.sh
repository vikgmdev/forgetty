#!/usr/bin/env bash
set -euo pipefail
umask 022

# ── Forgetty DEB package builder ───────────────────────────────────
# Builds a .deb package from the release artifacts.
# Prerequisites: cargo build --release
# Output: forgetty_<version>_amd64.deb in the project root
# ────────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

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

# ── Pre-flight checks ──────────────────────────────────────────────
BINARY="$PROJECT_ROOT/target/release/forgetty"
DAEMON_BINARY="$PROJECT_ROOT/target/release/forgetty-daemon"
SO_FILE="$PROJECT_ROOT/target/release/lib/libghostty-vt.so.0.1.0"

if [ ! -f "$BINARY" ]; then
    error "Release binary not found: $BINARY"
    error "Run 'cargo build --release' first."
    exit 1
fi

if [ ! -f "$DAEMON_BINARY" ]; then
    error "Daemon binary not found: $DAEMON_BINARY"
    error "Run 'cargo build --release --bin forgetty-daemon' first."
    exit 1
fi

if [ ! -f "$SO_FILE" ]; then
    error "Shared library not found: $SO_FILE"
    error "Run 'cargo build --release' first."
    exit 1
fi

# ── Extract version from Cargo.toml ────────────────────────────────
VERSION=$(grep -m1 '^version' "$PROJECT_ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')
# Handle workspace version reference
if [ "$VERSION" = "" ] || echo "$VERSION" | grep -q "workspace"; then
    VERSION=$(grep -A5 '^\[workspace\.package\]' "$PROJECT_ROOT/Cargo.toml" | grep '^version' | sed 's/.*"\(.*\)".*/\1/')
fi

if [ -z "$VERSION" ]; then
    error "Could not extract version from Cargo.toml"
    exit 1
fi

info "Building DEB package for forgetty $VERSION"

ARCH="amd64"
PKG_NAME="forgetty_${VERSION}_${ARCH}"
DEB_OUTPUT="$PROJECT_ROOT/${PKG_NAME}.deb"
STAGING="$PROJECT_ROOT/target/deb-staging/${PKG_NAME}"

# ── Clean up any previous staging directory ─────────────────────────
rm -rf "$STAGING"
rm -f "$DEB_OUTPUT"

# ── Create FHS directory layout ─────────────────────────────────────
mkdir -p "$STAGING/DEBIAN"
mkdir -p "$STAGING/usr/bin"
mkdir -p "$STAGING/usr/lib/forgetty"
mkdir -p "$STAGING/usr/share/applications"
mkdir -p "$STAGING/usr/share/icons/hicolor/scalable/apps"
mkdir -p "$STAGING/usr/share/man/man1"
mkdir -p "$STAGING/usr/share/doc/forgetty"

# ── Copy files ──────────────────────────────────────────────────────
info "Copying GTK binary"
install -Dm755 "$BINARY" "$STAGING/usr/bin/forgetty"

info "Copying daemon binary"
install -Dm755 "$DAEMON_BINARY" "$STAGING/usr/bin/forgetty-daemon"

info "Copying shared library"
install -Dm755 "$SO_FILE" "$STAGING/usr/lib/forgetty/libghostty-vt.so.0.1.0"

info "Creating soname symlink"
ln -sf libghostty-vt.so.0.1.0 "$STAGING/usr/lib/forgetty/libghostty-vt.so.0"

info "Copying desktop entry"
install -Dm644 "$SCRIPT_DIR/dev.forgetty.Forgetty.desktop" \
    "$STAGING/usr/share/applications/dev.forgetty.Forgetty.desktop"

info "Copying icon"
install -Dm644 "$PROJECT_ROOT/assets/icons/dev.forgetty.Forgetty.svg" \
    "$STAGING/usr/share/icons/hicolor/scalable/apps/dev.forgetty.Forgetty.svg"

info "Compressing and installing man page"
gzip -9 -n -c "$SCRIPT_DIR/forgetty.1" > "$STAGING/usr/share/man/man1/forgetty.1.gz"
chmod 644 "$STAGING/usr/share/man/man1/forgetty.1.gz"

info "Copying copyright"
install -Dm644 "$SCRIPT_DIR/copyright" \
    "$STAGING/usr/share/doc/forgetty/copyright"

# ── Compute installed size (KB) ─────────────────────────────────────
INSTALLED_SIZE=$(du -s --block-size=1024 "$STAGING" | cut -f1)

# ── Generate DEBIAN/control ─────────────────────────────────────────
info "Generating DEBIAN/control (Installed-Size: ${INSTALLED_SIZE} KB)"
cat > "$STAGING/DEBIAN/control" <<EOF
Package: forgetty
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Depends: libgtk-4-1, libadwaita-1-0, libc6
Installed-Size: ${INSTALLED_SIZE}
Maintainer: Vick <vick@totemlabs.dev>
Homepage: https://forgetty.dev
Description: The AI-first agentic terminal emulator
 Forgetty is a workspace-aware terminal where your AI agents,
 tabs, splits, and sessions persist across reboots and sync
 across devices. Built on GTK4/libadwaita with the Ghostty
 VT engine.
EOF

# ── Build the .deb ──────────────────────────────────────────────────
info "Building .deb package"
dpkg-deb --build --root-owner-group "$STAGING" "$DEB_OUTPUT"

# ── Clean up staging directory ──────────────────────────────────────
rm -rf "$PROJECT_ROOT/target/deb-staging"

# ── Report ──────────────────────────────────────────────────────────
DEB_SIZE=$(du -h "$DEB_OUTPUT" | cut -f1)
printf "\n${BOLD}${GREEN}DEB package built successfully!${RESET}\n"
echo ""
echo "  Output: $DEB_OUTPUT"
echo "  Size:   $DEB_SIZE"
echo ""
echo "  Install:  sudo dpkg -i $DEB_OUTPUT"
echo "  Inspect:  dpkg-deb -I $DEB_OUTPUT"
echo "  Contents: dpkg-deb -c $DEB_OUTPUT"
