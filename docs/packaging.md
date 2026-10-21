# Server packages and release builds

What `forgetty-server` ships, and how the files are built.

## What you get

| Format | File | Install |
|--------|------|---------|
| deb | `forgetty-server_<version>_amd64.deb` / `_arm64.deb` | `sudo apt install ./forgetty-server_*.deb` |
| rpm | `forgetty-server-<version>.x86_64.rpm` / `.aarch64.rpm` | `sudo dnf install ./forgetty-server-*.rpm` |
| tarball | `forgetty-server-<version>-<target>.tar.gz` | `scripts/install.sh --tarball FILE` (no root) |

The deb and rpm install:

- `/usr/bin/forgetty` (server and CLI)
- `/usr/lib/forgetty/libghostty-vt.so.0` (found through the RUNPATH `$ORIGIN/../lib/forgetty`)
- `/usr/lib/systemd/user/forgetty-server.service` (a `systemd --user` unit)
- docs and examples in `/usr/share/doc/forgetty-server/`

No system-wide service and no root daemon. The post-install script only prints
the two commands that enable the user service:

```sh
loginctl enable-linger "$USER"
systemctl --user enable --now forgetty-server
```

The packages conflict with the paused GTK package `forgetty`, which also owns
`/usr/bin/forgetty`.

## Install script

```sh
curl -fsSL https://raw.githubusercontent.com/vikgmdev/forgetty/main/scripts/install.sh | bash
bash scripts/install.sh --dry-run                 # show the steps, change nothing
bash scripts/install.sh --channel beta            # stable (default) | beta
bash scripts/install.sh --tarball FILE_OR_URL     # binary into ~/.local/bin, no root
```

Without `--tarball` it picks the package format of the machine: deb on
Debian/Ubuntu (dpkg), rpm on Fedora/RHEL/SUSE, else the tarball. A package
install needs `sudo`; the tarball install never does.

## Release builds: glibc 2.28, not static musl

`libghostty-vt` (the terminal engine, built by Zig) is a shared library:
its static archive has unresolved `simdutf` symbols (see
`crates/forgetty-vt/build.rs`). A fully static musl binary cannot load a
shared library, so static musl does not work with this build.

The release binaries are built with `cargo-zigbuild` against **glibc 2.28**
instead (`x86_64-unknown-linux-gnu.2.28`, `aarch64-unknown-linux-gnu.2.28`).
They run on Debian 10+, Ubuntu 18.10+, RHEL/Rocky/Alma 8+. The `build.rs` of
`forgetty-vt` passes the matching `-Dtarget` to Zig (`FORGETTY_ZIG_TARGET`, or
automatically for a cross build to another CPU). The tarball carries
`bin/forgetty` and `lib/libghostty-vt.so.0`.

```sh
scripts/build-server-dist.sh --target x86_64-unknown-linux-gnu    # deb, rpm, tarball, sha256
scripts/build-server-dist.sh --target aarch64-unknown-linux-gnu
```

Tools: Rust, Zig 0.15, `cargo install cargo-zigbuild cargo-deb cargo-generate-rpm`.

## CI

`.github/workflows/server-release.yml` runs on a tag `server-v*` only. It builds
both architectures, the deb, the rpm, the tarballs and `SHA256SUMS`, and
attaches them to the release of the tag.
