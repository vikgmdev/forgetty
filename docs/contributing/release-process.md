# Release Process

This document describes how Forgetty releases are cut.

## Versioning

Forgetty follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html):

- **MAJOR** — Incompatible API changes (config format, socket API, CLI flags).
- **MINOR** — New features, backward-compatible.
- **PATCH** — Bug fixes, backward-compatible.

During the `0.x` phase, minor versions may include breaking changes.

## Steps

### 1. Prepare the release

1. Ensure `main` is green on CI.
2. Update `CHANGELOG.md`:
   - Move items from `[Unreleased]` to a new `[X.Y.Z] - YYYY-MM-DD` section.
   - Add a link to the diff at the bottom of the file.
3. Update version numbers:
   - `Cargo.toml` workspace version (`[workspace.package] version`).
   - Verify all crate `Cargo.toml` files inherit the workspace version.
4. Commit: `chore: prepare release vX.Y.Z`
5. Open a PR and get it merged.

### 2. Tag the release

```sh
git tag -a vX.Y.Z -m "Release vX.Y.Z"
git push origin vX.Y.Z
```

### 3. Automated build and publish

Pushing the tag triggers the `release.yml` GitHub Actions workflow, which:

1. Builds release binaries for all targets (Linux x86_64/aarch64, macOS
   x86_64/aarch64, Windows x86_64).
2. Creates a GitHub Release with auto-generated release notes.
3. Uploads the binaries as release assets.

### 4. Publish to crates.io

Crate publishing is done manually to ensure correctness:

```sh
# Publish in dependency order
cargo publish -p forgetty-core
cargo publish -p forgetty-config
cargo publish -p forgetty-vt
cargo publish -p forgetty-pty
cargo publish -p forgetty-renderer
cargo publish -p forgetty-ui
cargo publish -p forgetty-viewer
cargo publish -p forgetty-watcher
cargo publish -p forgetty-workspace
cargo publish -p forgetty-socket
cargo publish -p forgetty
```

### 5. Post-release

1. Update Homebrew tap formula.
2. Update AUR package.
3. Announce on GitHub Discussions.
