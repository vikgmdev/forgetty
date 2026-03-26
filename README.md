# Forgetty

**The AI-first agentic terminal emulator.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/totem-labs-forge/forgetty/actions/workflows/ci.yml/badge.svg)](https://github.com/totem-labs-forge/forgetty/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/forgetty.svg)](https://crates.io/crates/forgetty)

Forgetty is a cross-platform, GPU-accelerated terminal emulator built on
[libghostty-vt](https://github.com/ghostty-org/ghostty), designed from the
ground up for AI coding agents and workspace management. It treats AI agents as
first-class citizens — surfacing notifications when they need your attention,
managing multi-session workspaces, and exposing a scriptable JSON-RPC API so
external tools can drive the terminal programmatically.

<!-- TODO: Add screenshot -->

---

## Features

- **GPU-accelerated rendering** — wgpu-powered renderer with libghostty-vt for
  terminal emulation. Smooth scrolling, ligature support, and sub-pixel
  positioning on all platforms.

- **AI agent-aware** — Notification rings alert you when a background agent
  needs attention. Never miss a prompt or error from your AI assistant again.

- **Smart copy/paste** — Automatically strips box-drawing characters, ANSI
  escapes, and trailing whitespace when you copy from the terminal. Paste what
  you actually want.

- **Vertical tabs** — Tabs live on the side, showing the git branch, working
  directory, and running command for each session at a glance.

- **Split panes** — Horizontal and vertical splits with keyboard-driven
  navigation. Resize with the mouse or keybindings.

- **Embedded markdown/image viewer** — Preview markdown files and images
  inline, with a file watcher that auto-refreshes on save.

- **Workspace management** — Group tabs into workspaces. Session restore brings
  back your full layout — splits, working directories, and scroll position —
  after a restart.

- **Scriptable JSON-RPC socket API** — Automate the terminal from scripts,
  editors, or AI agents. Create tabs, send input, read output, manage
  workspaces — all over a Unix domain socket.

- **Cross-platform** — Linux, macOS, and Windows. Android support is on the
  roadmap.

## Installation

### Pre-built binaries

Download the latest release for your platform from
[GitHub Releases](https://github.com/totem-labs-forge/forgetty/releases).

### Homebrew (macOS / Linux)

```sh
brew install totem-labs-forge/tap/forgetty
```

### AUR (Arch Linux)

```sh
yay -S forgetty
```

### Cargo

```sh
cargo install forgetty
```

> **Note:** Building from source requires a [Zig](https://ziglang.org/)
> compiler (0.13+) because libghostty-vt is built with Zig. See
> [Building from Source](#building-from-source) for details.

## Quick Start

```sh
# Launch Forgetty
forgetty

# Open a new tab
Ctrl+Shift+T

# Split the current pane vertically
Ctrl+Shift+D

# Split the current pane horizontally
Ctrl+Shift+E

# Navigate between panes
Ctrl+Shift+Arrow

# Open the command palette
Ctrl+Shift+P
```

Configuration lives in `~/.config/forgetty/config.toml`. Forgetty creates a
default config on first launch. See the [configuration docs](docs/configuration.md)
for the full reference.

## Building from Source

### Prerequisites

| Tool   | Minimum version | Notes                              |
|--------|-----------------|------------------------------------|
| Rust   | 1.85+           | Install via [rustup](https://rustup.rs/) |
| Zig    | 0.13+           | Required for libghostty-vt         |
| Git    | 2.x             | With submodule support             |

Platform-specific dependencies:

- **Linux:** `libx11-dev`, `libxkbcommon-dev`, `libwayland-dev`, `libfontconfig-dev`
- **macOS:** Xcode command-line tools
- **Windows:** Visual Studio Build Tools 2022

### Build

```sh
# Clone with submodules (libghostty-vt is a git submodule)
git clone --recursive https://github.com/totem-labs-forge/forgetty.git
cd forgetty

# Build in release mode
cargo build --release

# Run
cargo run
```

The release binary is written to `target/release/forgetty`.

### Running tests

```sh
cargo test --workspace
```

## Configuration

Forgetty is configured via `~/.config/forgetty/config.toml`. The config file is
created automatically on first launch with sensible defaults.

See [docs/configuration.md](docs/configuration.md) for the full configuration
reference.

## Architecture

Forgetty is organized as a Cargo workspace with the following crates:

| Crate              | Purpose                                      |
|--------------------|----------------------------------------------|
| `forgetty`         | Binary entry point, CLI, and app orchestration |
| `forgetty-config`  | Configuration loading, validation, and defaults |
| `forgetty-vt`      | Zig build + Rust FFI bindings for libghostty-vt |
| `forgetty-pty`     | PTY abstraction (Unix + ConPTY)              |
| `forgetty-render`  | wgpu renderer, glyph rasterization, shaders  |
| `forgetty-ui`      | Window management, tabs, splits, input handling |
| `forgetty-workspace` | Workspace and session persistence           |
| `forgetty-socket`  | JSON-RPC Unix domain socket server           |
| `forgetty-notify`  | Agent notification detection and rendering   |
| `forgetty-common`  | Shared types, error handling, logging         |

See [docs/architecture.md](docs/architecture.md) for the full architecture
overview and data flow diagrams.

## Why Not Fork Ghostty?

Ghostty is an excellent terminal — so why build Forgetty instead of forking it?

**We use Ghostty's brain, not its body.** Forgetty uses
[libghostty-vt](https://github.com/ghostty-org/ghostty) for terminal emulation
(the same SIMD-optimized VT parser that powers Ghostty), but replaces the
rendering layer with [wgpu](https://wgpu.rs/) for true cross-platform support.

| | Ghostty | Forgetty |
|---|---|---|
| **Terminal engine** | libghostty (Zig) | libghostty-vt (same engine) |
| **GPU rendering** | Metal (macOS) + OpenGL (Linux) | wgpu → Vulkan, Metal, DX12, OpenGL, WebGPU |
| **Shader language** | MSL + GLSL (separate per platform) | WGSL (one language, compiled for all) |
| **Font rendering** | CoreText (macOS) + FreeType (Linux) | cosmic-text + glyphon (Rust, all platforms) |
| **Windows** | Limited (OpenGL, no font discovery) | Full (DX12/Vulkan, native fonts) |
| **Android** | No | Yes (Vulkan/GLES) |
| **Browser** | Stubbed WebGL | WebGPU (all major browsers) |
| **UI framework** | GTK (Linux) + AppKit (macOS) | wgpu custom (one codebase) |
| **Language** | Zig | Rust |
| **License** | MIT | MIT |

**Ghostty's renderer is platform-specific by design** — Metal shaders don't run
on Linux, GLSL doesn't run on macOS. Each platform has a separate renderer
codebase. wgpu solves this: one WGSL shader compiles to Metal, HLSL, GLSL, and
SPIR-V automatically via [Naga](https://github.com/gfx-rs/naga).

**The result:** Forgetty gets Ghostty's battle-tested terminal emulation
(including Kitty protocol, SIMD parsing, Unicode support) with a renderer that
runs on every platform from a single codebase. We build AI-first UI features
(workspaces, agent notifications, smart clipboard, sync) on top.

See [docs/adr/004-wgpu-vs-ghostty-renderer.md](docs/adr/004-wgpu-vs-ghostty-renderer.md)
for the full architectural decision record.

## Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) to
get started. Whether it's a bug report, feature request, documentation
improvement, or code change — we appreciate your help.

## License

Forgetty is licensed under the [MIT License](LICENSE).

## Acknowledgments

Forgetty stands on the shoulders of excellent open-source work:

- **[Ghostty](https://ghostty.org/)** by Mitchell Hashimoto — libghostty-vt
  provides the terminal emulation core. Ghostty's correctness and performance
  set a high bar that we're grateful to build upon.
- **[wgpu](https://wgpu.rs/)** — Cross-platform GPU abstraction that makes
  rendering work everywhere.
- **The Rust community** — For the tooling, crates, and ecosystem that make
  projects like this possible.
