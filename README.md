<p align="center">
  <img src="assets/icons/dev.forgetty.Forgetty.svg" width="120" alt="Forgetty icon">
</p>

<h1 align="center">Forgetty</h1>

<p align="center">
  <b>A modern terminal emulator for Linux with workspaces, session persistence, and native rendering quality.</b>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License"></a>
  <img src="https://img.shields.io/badge/platform-Linux-informational" alt="Linux">
  <img src="https://img.shields.io/badge/status-beta-orange" alt="Beta">
  <img src="https://img.shields.io/badge/built%20with-Claude%20Code-blueviolet" alt="Built with Claude Code">
</p>

<p align="center">
  <em>Screenshots coming soon — see <a href="CHANGELOG.md">CHANGELOG.md</a> for what's shipped.</em>
</p>

> ⚠️ **Active development — pre-release.** Forgetty v0.1.0-beta is the first
> public preview. Expect breaking changes, rough edges, and missing features.
> Not recommended for production use. Bug reports very welcome.

Forgetty is a terminal emulator for Linux built on
[Ghostty](https://ghostty.org/)'s VT engine ([libghostty-vt](https://github.com/ghostty-org/ghostty))
and native GTK4 rendering. It matches Ghostty's text quality pixel-for-pixel,
then adds what's missing from every Linux terminal: workspaces that persist
across reboots, session restore, a live theme browser with 486 themes, and
AI-native integrations for developers who work with coding agents daily.

---

## Features

### Native rendering quality

GTK4 + Pango/FreeType — the same text rendering stack as Ghostty on Linux.
Subpixel antialiasing, Fontconfig font discovery, full IME support. Powered by
libghostty-vt: SIMD-optimized VT parsing, Kitty keyboard protocol, Unicode
grapheme clustering, text reflow. No compromises on terminal correctness.

### Tabs and split panes

Tabs with CWD-based titles that update automatically. Horizontal and vertical
splits with independent shells. Navigate panes with `Alt+Arrow`. Each pane has
its own scrollback, search state, zoom level, and cursor.

### 486 color themes with live preview

Browse and switch themes from the appearance sidebar (`Ctrl+,`). All 485 themes
from [iTerm2-Color-Schemes](https://github.com/mbadolato/iTerm2-Color-Schemes)
are bundled, plus you can drop your own into `~/.config/forgetty/themes/`. Arrow
keys cycle through themes with live preview on your actual terminal content —
Enter to keep, Escape to revert.

### Session persistence

Close your laptop, reopen it, and everything is exactly where you left it —
tabs, splits, working directories, scroll position. Auto-saves in the
background. You never lose your terminal layout again.

### Workspaces

Named workspaces for different projects. Each workspace has its own set of tabs,
splits, and layout. Switch between them instantly. Workspaces persist across
restarts just like sessions.

### Search across scrollback

`Ctrl+Shift+F` opens per-pane search. All matches highlighted with an "N of M"
count. Enter/Shift+Enter navigate forward and backward with wrap-around.
Viewport auto-scrolls to center each match.

### AI-native integrations

For developers working with AI coding agents daily:

- **Agent notifications** — colored ring on pane border, badge on the tab, and
  desktop notification when a background agent needs attention. No more hunting
  through tabs.
- **Smart clipboard** — strips box-drawing characters, trailing whitespace, and
  normalizes line endings automatically. Copy from Claude Code or any TUI app
  and paste clean text.
- **Socket API** — JSON-RPC over Unix socket. Automate Forgetty from scripts,
  editors, or AI agents — create tabs, send input, read output, manage
  workspaces programmatically.

### Everything else

- **URL detection** — hover highlights URLs, `Ctrl+Click` opens in browser
- **Font zoom** — `Ctrl+=`/`Ctrl+-` per pane, grid reflows correctly
- **Cursor styles** — block, bar, underline, hollow block; blink; respects DECSCUSR
- **Bell modes** — visual flash, audio beep, both, or none
- **Config hot reload** — edit `config.toml`, changes apply instantly to all panes
- **Right-click context menu** — copy, paste, select all, search, open URL
- **Command palette** — `Ctrl+Shift+P` for quick access to all actions
- **Multi-instance** — each `forgetty` invocation is a fully independent window
- **Desktop integration** — `.desktop` entry, SVG icon, GNOME Activities search
- **CLI flags** — `--working-directory`, `-e` (execute command), `--class`, `--config-file`

## Forgetty vs the rest

| | Ghostty | Warp | Terminator | GNOME Terminal | Forgetty |
|---|---|---|---|---|---|
| **Rendering** | Pango/FreeType | GPU | VTE | VTE | Pango/FreeType |
| **VT engine** | libghostty | Custom | VTE | VTE | libghostty-vt |
| **Tabs + splits** | Yes | Yes | Yes | Tabs only | Yes |
| **Session persistence** | No | Partial | No | No | **Full** |
| **Workspaces** | No | No | Layouts (manual) | No | **Yes** |
| **Themes** | ~20 | Limited | ~10 | ~10 | **486 + live preview** |
| **Agent notifications** | No | No | No | No | **Yes** |
| **Smart copy** | No | Some | No | No | **Yes** |
| **Socket API** | No | No | D-Bus (limited) | No | **JSON-RPC** |
| **Config hot reload** | Yes | N/A | Partial | No | **Yes** |
| **Open source** | Yes | No | Yes | Yes | **Yes (MIT)** |

## Install

### DEB package (Ubuntu/Debian)

```sh
# Download from GitHub Releases
sudo dpkg -i forgetty_0.1.0-beta_amd64.deb
```

### Install script (any Linux)

```sh
git clone https://github.com/vikgmdev/forgetty.git
cd forgetty
cargo build --release
./install.sh
```

Installs binary to `/usr/local/bin/`, shared library to `/usr/local/lib/`,
desktop entry and icon to `~/.local/share/`. Uninstall with `./uninstall.sh`.

## Keyboard shortcuts

| Action | Shortcut |
|--------|----------|
| New tab | `Ctrl+Shift+T` |
| Close pane/tab | `Ctrl+Shift+W` |
| Split right | `Alt+Shift+=` |
| Split down | `Alt+Shift+-` |
| Navigate panes | `Alt+Arrow` |
| Copy | `Ctrl+Shift+C` |
| Paste | `Ctrl+Shift+V` |
| Search | `Ctrl+Shift+F` |
| Zoom in / out / reset | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` |
| Appearance sidebar | `Ctrl+,` |
| Command palette | `Ctrl+Shift+P` |
| Keyboard shortcuts | `F1` |
| Quit | `Ctrl+Shift+Q` |

## Configuration

Forgetty is configured via `~/.config/forgetty/config.toml`. The config file is
created automatically on first launch with sensible defaults. Changes apply
instantly (hot reload).

```toml
font_family = "JetBrains Mono"
font_size = 13.0
theme = "Catppuccin Mocha"
scrollback_lines = 10000
cursor_style = "block"      # block | bar | underline | block_hollow
bell_mode = "visual"         # visual | audio | both | none
# shell = "/bin/zsh"         # default: your login shell
```

Or use the appearance sidebar (`Ctrl+,`) to change theme, font, and size with live preview.

## Building from Source

### Prerequisites

| Tool | Version | Notes |
|------|---------|-------|
| Rust | stable | Install via [rustup.rs](https://rustup.rs/) |
| Zig | 0.15+ | Builds libghostty-vt ([download](https://ziglang.org/download/)) |
| GTK4 | 4.14+ | `sudo apt install libgtk-4-dev libadwaita-1-dev` |

> **Tip:** If Zig isn't on your `$PATH`, set `ZIG_PATH` to its location.

### Build

```sh
git clone --recursive https://github.com/vikgmdev/forgetty.git
cd forgetty
cargo build --release
cargo run --release
```

The release binary is at `target/release/forgetty`.

### Running tests

```sh
cargo test --workspace
```

## Architecture

**Daemon-first design:** Each Forgetty window runs its own `forgetty-daemon`
process that owns all PTYs and session state. The GTK window is a stateless
renderer that communicates via JSON-RPC over a Unix socket. This means:

- **Closing the window doesn't kill your processes** — the daemon keeps them alive
- **Sessions persist** in `~/.local/share/forgetty/sessions/` and restore automatically on next launch
- **Multi-window** — each window is fully independent with its own daemon, socket, and session file
- **Future-proof** — Android, Windows, and web clients can connect to a desktop daemon as remote renderers

**Thin shell + thick core:** The GTK4 platform shell is ~1,300 lines. The
shared Rust core is ~10,000+ lines (~70% of the code) and is
platform-independent. When we add Windows or Android, we write a new thin
shell — the core stays the same.

```
┌──────────────────────────────────────────────────────┐
│  Platform Shell (THIN — native per platform)          │
│  Linux:   GTK4 + libadwaita (gtk4-rs)    ← current   │
│  Windows: native shell                   ← planned    │
│  Android: Jetpack Compose + Rust JNI     ← planned    │
├──────────────────────────────────────────────────────┤
│  Shared Rust Core (THICK — platform-independent)      │
│  forgetty-vt        libghostty-vt FFI bindings        │
│  forgetty-pty       PTY management (portable-pty)     │
│  forgetty-config    Config, 486 themes, defaults      │
│  forgetty-core      Shared types, errors              │
│  forgetty-workspace Session + workspace persistence   │
│  forgetty-watcher   Config file hot reload            │
│  forgetty-socket    JSON-RPC API                      │
│  forgetty-clipboard Smart copy pipeline               │
├──────────────────────────────────────────────────────┤
│  libghostty-vt.so (Zig, C API — Ghostty project)     │
│  SIMD VT parser, Kitty protocol, Unicode graphemes,   │
│  scrollback, text reflow — proven by millions of users │
└──────────────────────────────────────────────────────┘
```

The decision to use GTK4 instead of wgpu came from learning (the hard way) that
GPU-rendered text can never match native Linux quality — no subpixel
antialiasing, no Fontconfig, no IME. Pivoting to GTK4 + Pango gave us rendering
identical to Ghostty on day one.

See `docs/architecture/ARCHITECTURE_DECISIONS.md` for the full design rationale.

## Roadmap

**Next:** Windows + WSL support, Android companion app, cross-device sync,
web version. See the roadmap in [CONTRIBUTING.md](CONTRIBUTING.md) for the full plan.

## Contributing

Contributions welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for build
instructions, code style, and the crate map.

## License

[MIT](LICENSE) &copy; 2026 TotemLabsForge, LLC

## The story

Forgetty is built by one person — [Victor Garcia](https://github.com/vikgmdev),
a self-taught engineer with 10+ years of experience across backend, blockchain,
infrastructure, and security. No company, no funding, no team. Just a developer
who got tired of losing his terminal layout every time he closed a window.

The project started in early 2026, born from a simple frustration: every
terminal emulator treats sessions as disposable. Close the window, lose your
work. I wanted a terminal that *remembers* — tabs, splits, working directories,
scroll position — all restored exactly where I left off. And I wanted it to feel
native on Linux, not an Electron wrapper or a GPU experiment.

I'd never written Rust before this project. I used
[Claude Code](https://claude.ai/claude-code) as a force multiplier — an AI
coding agent that let me move at 10x speed in a language I was learning as I
built. The entire codebase, from the daemon architecture to the GTK4 renderer to
the 486-theme browser, was built this way: one developer + one AI, shipping a
feature-complete terminal in weeks instead of months.

Forgetty is the terminal I wanted to exist. If you work with AI coding agents
daily — running Claude Code, Copilot, or Cursor in split panes for hours — you
need a terminal that's built for that workflow. That's what this is.

No venture capital. No growth metrics. Just a tool that works.

**Follow the journey:** [@vikgmdev](https://twitter.com/vikgmdev)

## Acknowledgments

- **[Ghostty](https://ghostty.org/)** by [Mitchell Hashimoto](https://github.com/mitchellh)
  — libghostty-vt provides the terminal emulation engine. Ghostty's VT
  correctness and SIMD-optimized parsing are what make Forgetty possible.
- **[iTerm2-Color-Schemes](https://github.com/mbadolato/iTerm2-Color-Schemes)**
  — 485 of our 486 bundled themes come from this collection.
- **[Claude Code](https://claude.ai/claude-code)** by
  [Anthropic](https://anthropic.com) — the AI coding agent that made it possible
  for a solo developer to build a full terminal emulator in Rust.
- **GTK4, libadwaita, Pango, FreeType** — the GNOME platform that gives
  Forgetty its native rendering quality.
- **The Rust ecosystem** — gtk4-rs, portable-pty, serde, toml, clap, and the
  many crates Forgetty depends on.
