# Changelog

All notable changes to Forgetty will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0-beta] - Unreleased

Initial public beta release.

### Terminal

- Native GTK4 + Pango/FreeType rendering — identical text quality to Ghostty on Linux
- libghostty-vt engine: SIMD-optimized VT parsing, Kitty keyboard protocol, Unicode grapheme clustering, text reflow
- Tabs (libadwaita TabBar) with CWD-based titles, auto-updating
- Horizontal and vertical split panes with independent shells per pane
- Pane navigation via `Alt+Arrow`, keyboard-driven focus management
- Per-pane scrollback with configurable buffer size (default 10,000 lines)
- Scrollbar that auto-hides when viewport covers all content, hides on alternate screen
- Mouse text selection: click-drag, double-click word, triple-click line
- Double-click-drag extends selection by word granularity
- Mouse tracking coexistence — clicks go to apps (vim, htop) when mouse tracking is on
- Full keyboard input via ghostty key encoder (176 keys, Kitty protocol levels 1-3)
- Mouse input via ghostty mouse encoder (button mapping, motion dedup, scroll routing)
- Focus reporting (DECSET 1004)

### Search

- Per-pane search (`Ctrl+Shift+F`) across entire scrollback
- All matches collected upfront with accurate "N of M" count
- Match highlighting with warm amber background, brighter orange for focused match
- Enter/Shift+Enter navigate forward/backward with wrap-around
- Viewport auto-scrolls to center matches during navigation

### Themes

- 486 built-in color themes (485 from iTerm2-Color-Schemes + default dark)
- Appearance sidebar (`Ctrl+,`) with live preview theme browser
- Arrow key cycling through themes with instant terminal preview
- Enter to confirm, Escape to revert — close sidebar also reverts if not confirmed
- Color swatches in theme list (background + foreground + 6 ANSI colors)
- Custom themes from `~/.config/forgetty/themes/` override bundled themes
- Theme aliases for common names (Solarized Dark, Tokyo Night, etc.)

### URL Detection

- Hover highlights URLs with underline in cell foreground color
- `Ctrl+Click` opens URL in default browser
- SGR underline dedup — no double underline when cell already has one
- Works in scrollback, after font zoom, at line boundaries

### Cursor

- Block, bar, underline, and hollow block cursor styles
- Blink at ~600ms intervals, stops on keypress
- Reads DECSCUSR from applications (vim insert mode → bar cursor)
- Terminal-provided cursor color (OSC 12) preferred over theme default

### Font

- Font zoom: `Ctrl+=` in, `Ctrl+-` out, `Ctrl+0` reset (per pane)
- Min 6pt / max 72pt with grid reflow on zoom
- Font family and size configurable via config.toml or appearance sidebar

### Bell

- Visual bell: semi-transparent white flash (~150ms)
- Audio bell: system beep
- Configurable: visual, audio, both, or none
- Rate limiting (200ms cooldown) prevents strobe from rapid bells

### UI

- Ghostty-style header: new-tab button, dropdown with split actions, centered window title, hamburger menu
- Hamburger menu with 21 actions across 6 sections
- Right-click context menu: Copy, Paste, Select All, Search, Open URL
- Copy sensitivity — greyed out when no selection
- Command palette (`Ctrl+Shift+P`) with 27 commands
- Keyboard shortcuts window (`F1`) organized by category
- About dialog with version and license

### Smart Clipboard

- Strips box-drawing characters (U+2500-U+257F) from copied text
- Strips trailing whitespace from each line
- Normalizes line endings to LF

### Session Persistence

- Auto-save terminal state: tabs, splits, working directories, scroll position
- Full restore on restart — terminal layout is exactly where you left it

### Workspaces

- Named workspaces for different projects
- Each workspace has its own tabs, splits, and layout
- Workspaces persist across restarts

### Agent Notifications

- Colored ring on pane border when background pane receives output needing attention
- Badge on tab for panes that need attention
- Desktop notifications for agent events

### Socket API

- JSON-RPC 2.0 over Unix domain socket
- Create tabs, send input, read output, manage workspaces programmatically

### Configuration

- TOML config at `~/.config/forgetty/config.toml`
- Hot reload: changes apply instantly to all panes on save
- Malformed config preserves previous working state
- Fields: font_family, font_size, theme, shell, scrollback_lines, cursor_style, bell_mode, keybindings

### Shell Integration

- `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=forgetty`, `TERM_PROGRAM_VERSION`
- Shell exit auto-closes tab/pane
- Signal handling (SIGTERM, SIGHUP, SIGINT) with clean PTY shutdown
- Multi-instance support (each invocation is independent)

### CLI

- `--working-directory` — start in a specific directory
- `-e` / `--execute` — run a command instead of default shell
- `--config-file` — use alternate config file
- `--class` — set WM_CLASS for window manager rules
- `--version`, `--help`

### Packaging

- `.deb` package for Ubuntu/Debian with FHS-compliant layout
- `install.sh` / `uninstall.sh` scripts
- Portable tarball deployment (binary + .so in same directory)
- libghostty-vt.so bundled — no Zig required at runtime
- Desktop entry, SVG icon, man page

### Testing

- 25-scenario stress test suite
- VM test scripts for fresh Ubuntu installs
- Performance benchmarks vs Ghostty

### Known Issues

- Command palette search entry doesn't receive focus when palette opens (typing goes to terminal)
- Concurrent high-throughput output in 2+ split panes can cause UI freeze
- Shift+Click doesn't extend selection (clears instead)
- No auto-scroll while drag-selecting past viewport edge

### Acknowledgments

- [Ghostty](https://ghostty.org/) by Mitchell Hashimoto — libghostty-vt terminal emulation engine
- [iTerm2-Color-Schemes](https://github.com/mbadolato/iTerm2-Color-Schemes) — 485 bundled color themes
- [Claude Code](https://claude.ai/claude-code) by Anthropic — AI coding agent used to build Forgetty
