#!/usr/bin/env bash
# Build the forgetty-server release files for one target: deb, rpm, tarball and
# SHA256SUMS. Used by CI (.github/workflows/server-release.yml) and by
# scripts/p6_gate.sh.
#
#   scripts/build-server-dist.sh --target x86_64-unknown-linux-gnu [--out DIR]
#                                [--no-zigbuild] [--skip-deb] [--skip-rpm]
#
# Targets: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu. The binary is
# linked against glibc 2.28 with cargo-zigbuild (see docs/packaging.md for why
# not static musl). --no-zigbuild uses plain cargo: the glibc floor is then
# the glibc of this machine, so use it for local tests only.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="x86_64-unknown-linux-gnu"
OUT="$ROOT/dist-out"
ZIGBUILD=1
DEB=1
RPM=1
GLIBC_FLOOR="2.28"

while [ $# -gt 0 ]; do
  case "$1" in
    --target) TARGET="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --no-zigbuild) ZIGBUILD=0; shift ;;
    --skip-deb) DEB=0; shift ;;
    --skip-rpm) RPM=0; shift ;;
    -h|--help) sed -n '2,13p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

export PATH="$HOME/.cargo/bin:$HOME/.local/zig:$PATH"
case "$TARGET" in
  x86_64-unknown-linux-gnu)  DEB_ARCH=amd64; RPM_ARCH=x86_64;  ZIG_ARCH=x86_64 ;;
  aarch64-unknown-linux-gnu) DEB_ARCH=arm64; RPM_ARCH=aarch64; ZIG_ARCH=aarch64 ;;
  *) echo "unsupported target: $TARGET (use x86_64-unknown-linux-gnu or aarch64-unknown-linux-gnu)" >&2; exit 2 ;;
esac

VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' "$ROOT/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || { echo "cannot read the version from Cargo.toml" >&2; exit 1; }
say() { printf '[dist] %s\n' "$*"; }

cd "$ROOT"
mkdir -p "$OUT"

# 1. Binary and libghostty-vt.
if [ "$ZIGBUILD" = 1 ]; then
  say "cargo zigbuild $TARGET (glibc $GLIBC_FLOOR)"
  FORGETTY_ZIG_TARGET="${ZIG_ARCH}-linux-gnu.${GLIBC_FLOOR}" \
    cargo zigbuild --release --locked --target "${TARGET}.${GLIBC_FLOOR}" -p forgetty-server
else
  say "cargo build $TARGET (host glibc)"
  cargo build --release --locked --target "$TARGET" -p forgetty-server
fi
BIN="$ROOT/target/$TARGET/release/forgetty"
LIB="$ROOT/target/$TARGET/release/lib/libghostty-vt.so.0.1.0"
[ -x "$BIN" ] && [ -f "$LIB" ] || { echo "build output missing: $BIN / $LIB" >&2; exit 1; }

# 2. deb.
if [ "$DEB" = 1 ]; then
  say "deb"
  # Debian: 0.1.0-beta sorts before 0.1.0 as 0.1.0~beta.
  cargo deb -p forgetty-server --no-build --no-strip --target "$TARGET" \
    --deb-version "${VERSION/-/\~}-1" --output "$OUT/" >/dev/null
fi

# 3. rpm.
if [ "$RPM" = 1 ]; then
  say "rpm"
  # RPM versions cannot hold "-beta": 0.1.0-beta becomes 0.1.0~beta (sorts before 0.1.0).
  cargo generate-rpm -p crates/forgetty-server --target "$TARGET" --output "$OUT/" \
    --set-metadata "version = \"${VERSION/-/\~}\""
fi

# GitHub release assets cannot carry "~" in a name: use the plain version in
# the file names. The version inside the packages keeps the "~".
for f in "$OUT"/*"~"*; do
  [ -e "$f" ] || continue
  new="$(dirname "$f")/$(basename "$f" | sed -e 's/~/-/' -e 's/-1_/_/' -e 's/-1\.\(x86_64\|aarch64\)\.rpm$/.\1.rpm/')"
  mv "$f" "$new"
done

# 4. tarball: bin/forgetty, lib/libghostty-vt.so.0, the user unit, docs.
NAME="forgetty-server-${VERSION}-${TARGET}"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/$NAME"/{bin,lib,share/systemd,share/doc}
install -m755 "$BIN" "$STAGE/$NAME/bin/forgetty"
install -m644 "$LIB" "$STAGE/$NAME/lib/libghostty-vt.so.0"
# Tarball unit: the binary lives in ~/.local/bin.
install -m644 "$ROOT/dist/systemd/forgetty-server.service" "$STAGE/$NAME/share/systemd/forgetty-server.service"
install -m644 "$ROOT/docs/server.md" "$ROOT/docs/relay.md" "$ROOT/docs/packaging.md" "$ROOT/README.md" "$ROOT/LICENSE" \
  "$STAGE/$NAME/share/doc/"
install -m644 "$ROOT/dist/relay/iroh-relay.service" "$ROOT/dist/relay/iroh-relay.toml.example" "$STAGE/$NAME/share/doc/"
say "tarball $NAME.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime='2026-01-01 00:00:00' \
  -czf "$OUT/$NAME.tar.gz" -C "$STAGE" "$NAME"

# 5. sums of everything in OUT.
(cd "$OUT" && sha256sum -- *.deb *.rpm *.tar.gz 2>/dev/null > SHA256SUMS || true)
say "done: $OUT"
ls -la "$OUT"
