# Session State

**Last updated:** 2026-04-03
**Current task:** T-059 (not started)
**Current phase:** Starting M4: Daemon Owns Layout (T-059→T-065). Architectural refactor — daemon becomes single source of truth for session state, GTK becomes a stateless renderer.

## What's been completed

### Milestone 0 — proof of concept with wgpu
- Working terminal with libghostty-vt + wgpu renderer (being replaced with GTK4)
- libghostty-vt FFI matching Ghostling patterns (render state, graphemes, colors)
- Key encoder (176 keys), mouse encoder, focus reporting, viewport scroll
- Write-PTY callbacks (DA responses — vim/tmux work)
- Visual polish (Catppuccin Mocha theme, 16pt font, tab bar with CWD)
- Architecture decision: pivot to GTK4 native shell for Linux
- Full backlog created with 27 tasks across 3 milestones

### T-001: GTK4 window + application skeleton ✓
- Created `crates/forgetty-gtk/` crate with GTK4 + libadwaita
- `adw::Application` + `adw::ApplicationWindow` with CSD header bar
- Window opens with title "Forgetty", default size 960x640
- Wired into `src/main.rs` replacing winit launch path
- All existing crates still compile (`cargo check --workspace` passes)
- QA passed: Completeness 10, Functionality 9, Code quality 9, Robustness 9

### T-002: Terminal grid rendering with Pango + Cairo ✓
- DrawingArea + Cairo + Pango rendering pipeline
- PTY bridge: spawn shell, reader thread, glib polling for data
- Full cell rendering with FG/BG colors, bold, italic, underline, strikethrough, dim
- Block/bar/underline cursor rendering at correct position
- Minimal keyboard input (printable chars, Enter, Backspace, Tab, Escape, Ctrl+A-Z, arrows, F-keys)
- Window resize recalculates grid and reflows terminal
- Shell prompt, vim, htop, colors all render correctly
- QA passed: Completeness 10, Functionality 10, Code quality 9, Robustness 9

### T-003: Keyboard input via ghostty key encoder ✓
- Replaced minimal `key_to_pty_bytes()` with full ghostty key encoder API
- `GhosttyInput` struct owns encoder + event handles + pressed_keys set
- GDK keyval-to-GhosttyKey mapping (letters, digits, symbols, named keys, modifiers, numpad)
- GDK modifier-to-GhosttyMods mapping (Shift, Ctrl, Alt, Super, CapsLock, side bits)
- Key repeat detection via pressed_keys HashSet
- Kitty keyboard protocol support (levels 1-3, key release events, modifier reporting)
- Focus reporting (DECSET 1004) via EventControllerFocus
- Fallback path for edge cases where encoder returns nothing
- FFI side-bit constants added (bits 6-9)
- QA passed: Completeness 10, Functionality 10, Code quality 8, Robustness 9

### T-004: Mouse input via ghostty mouse encoder ✓
- GestureClick, EventControllerMotion, EventControllerScroll controllers
- Mouse encoder via ghostty mouse encoder API (button mapping, motion dedup, scroll routing)
- Scroll routing: mouse tracking mode forwards to encoder, otherwise scrolls viewport
- Scroll-to-bottom on new output (AC-9)
- Fix cycle 1: changed all `state.borrow_mut()` to `state.try_borrow_mut()` to prevent RefCell borrow conflicts
- QA passed: Completeness 10, Functionality 10, Code quality 8, Robustness 9

### T-005: Tab bar with libadwaita TabBar ✓
- `adw::TabBar` + `adw::TabView` replacing single-terminal layout
- Each tab gets its own DrawingArea + TerminalState (independent PTY, VT, input)
- Tab titles show CWD basename via /proc/{pid}/cwd polling (1500ms interval)
- New tab via Ctrl+Shift+T (GTK action `win.new-tab`) and "+" button
- Close tab kills PTY, last tab close exits app
- Focus management via connect_selected_page_notify + grab_focus
- TabStateMap: safe HashMap lookup instead of unsafe GTK set_data/data
- Timer lifetime via weak references (ControlFlow::Break when widget destroyed)
- Fix applied: tab_bar.set_autohide(false) to keep tab bar visible with 1 tab
- QA passed: Completeness 9, Functionality 9, Code quality 9, Robustness 9

### T-006: Split panes with gtk::Paned ✓
- `gtk::Paned` for split panes within tabs (nested tree of Paned + DrawingArea leaves)
- Split right (Alt+Shift+=) and split down (Alt+Shift+-) with independent shells
- Pane navigation via Alt+Arrow (geometric nearest-neighbor) and mouse click
- Close focused pane (Ctrl+Shift+W) with proper sibling promotion
- Visual focus indicator (2px blue border on focused pane)
- Tab title reflects focused pane's CWD with immediate update on focus change
- Fix cycle: shortcut bypass for ghostty encoder, Paned reparenting via set_start/end_child(None), draw order for focus border, mouse click focus, immediate title update
- QA passed: Completeness 10, Functionality 9, Code quality 8, Robustness 9

### T-007: Mouse text selection ✓
- Click-drag selects text with semi-transparent overlay (theme selection color)
- Ctrl+Shift+C copies via smart clipboard pipeline (strip box-drawing, trailing whitespace)
- Double-click selects word (delimiter-bounded), triple-click selects line
- Double-click-drag extends by word granularity
- Mouse tracking coexistence: clicks go to app (vim/htop) when tracking on, selection when off
- Shift+Click overrides mouse tracking for forced selection (stretch AC-15)
- Escape clears selection, single click clears, per-pane independent selection
- Selection clears on new terminal output (AC-17)
- Fix cycle 1: deferred selection creation (drag_origin) to eliminate click flicker
- Fix cycle 2: suppress_selection_clear_ticks counter for resize grace period
- QA passed: Completeness 10, Functionality 9, Code quality 8, Robustness 9

### T-008: Scrollbar ✓
- gtk::Scrollbar + gtk::Adjustment wired to GHOSTTY_TERMINAL_DATA_SCROLLBAR FFI
- Scrollbar appears when scrollback exists, hides when viewport covers all content
- Drag thumb scrolls viewport in real time, click above/below thumb jumps a page
- Scrollbar syncs with mouse wheel scrolling and auto-scroll-to-bottom
- Per-pane independent scrollbars in split layouts
- Scrollbar hides on alternate screen (vim), reappears on exit
- Return type change: create_terminal() returns (gtk::Box, DrawingArea, State) triple
- Fix cycle 1: smart auto-scroll (only when already at bottom), absolute selection coordinates
- QA passed: Completeness 10, Functionality 9, Code quality 8, Robustness 9

### T-009: Search in terminal (Ctrl+Shift+F) ✓
- gtk::SearchBar + gtk::SearchEntry per pane, revealed by Ctrl+Shift+F
- Case-insensitive literal string matching across entire scrollback
- All matches collected upfront with absolute row positions for accurate count
- Match count label shows "N of M" with correct totals
- Warm amber highlight for matches, brighter orange for focused match
- Enter/Shift+Enter navigate forward/backward with wrap-around
- Viewport scrolls to center matches during navigation
- Per-pane search (independent in split layouts)
- Escape closes search, clears highlights, returns focus to terminal
- No PTY input leak during search (focus on SearchEntry, not DrawingArea)
- Fix cycle 1: added scrollback page-by-page scanning for navigation
- Fix cycle 2: refactored to scan entire scrollback upfront for accurate match count
- QA passed: Completeness 10, Functionality 9, Code quality 8, Robustness 9

### T-010: Right-click context menu ✓
- Plain `gtk4::Popover` with manual `gtk4::Box` of flat buttons (not PopoverMenu — avoids GTK's internal ScrolledWindow height constraint)
- Menu items: Copy (with sensitivity), Paste, Select All, Search, conditional "Open URL"
- Keyboard shortcut hints displayed in dimmed text (Shift+Ctrl+C, Shift+Ctrl+V, Shift+Ctrl+F)
- Copy greyed out when no selection, enabled when text selected
- Right-click preserves existing selection (AC-9)
- Right-click always opens menu, even during mouse tracking (matches Ghostty behavior)
- Paste via Ctrl+Shift+V keyboard shortcut + menu action (async clipboard read)
- Select All selects entire visible viewport
- URL detection via row scan for http/https patterns, "Open URL" opens default browser
- Per-pane context menu in split layouts
- Fix cycle 1-3: PopoverMenu scrollbar issue (CSS, connect_map, NESTED flag all failed)
- Fix cycle 4: replaced PopoverMenu with manual Popover+Box — resolved all issues
- QA passed: Completeness 10, Functionality 10, Code quality 9, Robustness 9

### T-011: Font zoom (Ctrl+Plus/Minus) ✓
- Ctrl+= / Ctrl+Shift+= zoom in (1pt per step), Ctrl+Minus zoom out, Ctrl+0 reset to default
- font_size and default_font_size fields in TerminalState (mutable per-pane state)
- apply_font_zoom() helper follows connect_resize pattern (cell dims, cols/rows, terminal.resize, pty.resize)
- font_description_with_size() reads s.font_size from mutable state (not cloned config)
- is_app_shortcut() intercepts all zoom keys before ghostty encoder
- Min 6pt / max 72pt with epsilon no-op check
- Grid reflow, scrollback, cursor, bold/italic all correct after zoom
- Per-pane independent zoom in split layouts
- Fix cycle 1: default font size corrected from 16pt to 12pt (matching Ghostty)
- Fix cycle 2: search highlights fixed on zoom via recompute_all_search_matches() in apply_font_zoom()
- Selection stays visible on zoom (partial after reflow) — better than Ghostty which blocks zoom during search
- All RefCell access uses try_borrow_mut (no unwraps)
- QA passed: Completeness 10, Functionality 10, Code quality 9, Robustness 9

### T-012: URL detection + click ✓
- HoverUrl struct tracks URL string, screen row, and column span for underline rendering
- detect_url_at() refactored to return column span alongside URL string (single function, two consumers)
- Hover detection in motion handler: pointer cursor on URL, text cursor off URL
- Ctrl+Click opens URL in default browser via UriLauncher (reuses open_url_in_browser)
- Hover underline drawn in cell foreground color (respects SGR colors, no hardcoded color)
- SGR underline dedup: skip hover underline when cell already has attrs.underline
- Scroll clears hover state and resets cursor (prevents stale hover after content shift)
- Ctrl+Click suppressed during mouse tracking (hover is passive, click respects app ownership)
- Per-pane independent hover state in split layouts
- Works in scrollback, after font zoom, at line boundaries
- All 24 ACs passed, no fix cycles needed
- Minor observation: slight mouse lag near URLs (detect_url_at on every motion event / queue_draw frequency)
- QA passed: Completeness 10, Functionality 9, Code quality 9, Robustness 8

### T-013: Cursor blink + style from terminal ✓
- Cursor blinks at ~600ms intervals via blink phase toggle in 8ms PTY poll timer
- Blink stops on keypress (cursor_blink_visible = true, last_blink_toggle reset)
- Cursor style read from render state CURSOR_VISUAL_STYLE (bar/block/underline/block_hollow)
- vim insert mode → bar cursor, normal mode → block cursor (immediate transitions)
- DECSCUSR steady (2/4/6) stops blinking, blinking (1/3/5) resumes
- Cursor hidden in unfocused panes (only focused pane shows cursor)
- Initial DECSCUSR 1 fed to VT parser at terminal creation (defaults cursor_blinking to true)
- Alternate screen transition detection (is_alternate_screen FFI) re-feeds DECSCUSR 1 on alt→primary
- Terminal-provided cursor color (OSC 12) preferred over theme default
- BlockHollow variant added to CursorStyle config enum
- 3 fix cycles: default blink state, unfocused pane hiding, alternate screen recovery
- QA passed: Completeness 10, Functionality 9, Code quality 9, Robustness 9

### T-014: Bell (visual + audio) ✓
- Visual bell: semi-transparent white flash (~150ms) over terminal drawing area on BEL
- Audio bell: system beep via gdk::Display::beep() on BEL
- BellMode config enum: Visual (default), Audio, Both, None
- Rate limiting: 200ms cooldown prevents strobe from rapid bells
- Per-pane flash in split layouts (only triggered pane flashes)
- Event draining: terminal.drain_events() called in 8ms tick loop
- Bell flash redraws in no-data branch (ensures flash renders without PTY activity)
- No fix cycles needed
- QA passed: Completeness 10, Functionality 10, Code quality 9, Robustness 9

### T-015: Config file loading + hot reload ✓
- Config loaded on startup via `load_config(None)` with graceful fallback to defaults
- `ConfigWatcher` in `forgetty-watcher` watches config directory (not file) for inotify compatibility with editor write-rename patterns
- `apply_config_change()` propagates font, theme, and bell changes to all panes across all tabs
- `Rc<RefCell<Config>>` at app level ensures new tabs/splits use latest config
- Hot reload is immediate (sub-frame, faster than ~1s target)
- Font zoom interaction: config reload resets default_font_size, clears zoom delta
- Malformed config preserves previous working config (no crash, no revert to hardcoded defaults)
- Recovery works after fixing a bad config (watcher does not give up)
- Rapid saves debounced, no flicker storms
- No fix cycles needed
- QA passed: Completeness 10, Functionality 10, Code quality 9, Robustness 10

### T-016: Native window decorations + keyboard shortcuts display ✓
- Hamburger menu button in header bar with "Keyboard Shortcuts" and "About Forgetty" entries
- `gtk::ShortcutsWindow` with all keybindings organized by category (Tabs, Panes, Clipboard, Search, Zoom, Navigation, Help)
- F1 and Ctrl+? (Ctrl+Shift+/) both open shortcuts window via `win.show-shortcuts` action
- F1 intercepted by `is_app_shortcut()` so it never reaches PTY
- `adw::AboutDialog` with application name, version, description, MIT license
- Standard `gio::Menu` + `gtk::MenuButton` pattern for hamburger menu
- No fix cycles needed
- QA passed: Completeness 10, Functionality 10, Code quality 9, Robustness 9

**MILESTONE 1 COMPLETE.** All 16 tasks (T-001 through T-016) are done. Forgetty matches Ghostty on Linux for terminal rendering, input, tabs, splits, selection, scrollbar, search, context menu, font zoom, URL detection, cursor styles, bell, config hot reload, and discoverable keyboard shortcuts.

### T-M1-extra-001: Move tabs to top panel + Ghostty-style header layout ✓
- Ghostty-style header: [new-tab icon] [dropdown ▾] ... [user@host:~/path centered] ... [hamburger ≡] [window controls]
- Tab bar as separate row below header, autohides with 1 tab (matching Ghostty)
- Two-button left side: direct new-tab button + dropdown chevron with split actions
- Dropdown menu: New Tab, separator, Split Up, Split Down, Split Left, Split Right (with shortcut hints)
- Split Left and Split Up added as new actions (dropdown-only)
- Dynamic window title: `user@host:~/path` updates immediately + 100ms poll
- QA passed: Completeness 10, Functionality 10, Code quality 9, Robustness 9

### T-M1-extra-002: Hamburger menu matching Ghostty ✓
- Full hamburger menu with 21 actions across 6 sections matching Ghostty's discoverability
- New actions: Copy, Paste, New Window, Close Window, Change Tab Title, Close Tab, Split submenu, Clear, Reset, Command Palette (placeholder), Terminal Inspector (placeholder), Open Configuration, Reload Configuration, Quit
- Keyboard shortcut hints on all applicable items via GTK accelerator rendering
- Custom tab title mechanism via `CustomTitles` HashSet (title polling timer respects user-set titles)
- Clear writes Ctrl+L to PTY (shell handles prompt redraw); Reset uses `ghostty_terminal_reset()` API
- Quit action (Ctrl+Shift+Q) closes all windows and exits cleanly
- Placeholders: Command Palette and Terminal Inspector greyed out with `set_enabled(false)`
- Shortcuts window updated with Terminal group (Clear, Reset) and Quit
- 3 fix cycles for Clear/Reset behavior (escape sequence feeding replaced with PTY write and ghostty API)
- QA passed: Completeness 10, Functionality 9, Code quality 9, Robustness 9

### T-M1-extra-003: Appearance sidebar (live config) ✓
- Appearance sidebar slides in from right via `gtk4::Revealer` (instant show/hide)
- Three dropdowns: Theme (Default Dark + custom .toml), Font Family (monospace via Pango), Font Size (8–72)
- Immediate apply: mutates SharedConfig → apply_config_change() on all panes → save_config() in background
- Ctrl+, keyboard shortcut toggles sidebar, close button (X) in header, Escape closes
- Theme file parsing: ThemeFile intermediary + hex string Rgba deserialization for [colors] format
- Inline arrow-key cycling on all dropdowns (Up/Down changes value without opening popup)
- Tab/Shift+Tab navigates between dropdowns
- 2 fix cycles: theme hex parsing + keyboard nav; sidebar space allocation + close animation jitter
- QA passed: Completeness 9, Functionality 9, Code quality 8, Robustness 9

### T-M1-extra-004: Theme browser with live preview + bundled themes ✓
- 486 bundled themes (485 from iTerm2-Color-Schemes + default-dark) embedded via `include_str!()`
- Conversion script `scripts/convert-iterm-themes.py` generates TOML files + Rust registry
- Scrollable `gtk4::ListBox` with color swatches (background + fg + 6 ANSI) per row
- Live preview on arrow key / mouse selection via `apply_to_all_panes()`
- Enter confirms (saves `theme = "Name"` to config.toml), Escape reverts to original
- Close sidebar / X button also reverts if no Enter pressed
- Config schema: `theme = "Name"` string reference + backward-compat inline `[theme]` table parsing
- Theme aliases for 9 commonly-requested names (Solarized Dark, Tokyo Night, etc.)
- Custom themes from `~/.config/forgetty/themes/` override bundled, marked with "(custom)" italic
- Focus grab on sidebar open/reopen for immediate arrow-key browsing
- QA: 8/10 → fix cycle applied (focus grab, theme aliases, close button visible fix) → PASS

### T-M1-extra-005: Command Palette (Ctrl+Shift+P) ⚠️ KNOWN ISSUE
- Palette UI built: 27 commands, Overlay card, SearchEntry + ListBox, click-outside-to-close
- Ctrl+Shift+P opens/closes (toggle), hamburger menu entry enabled
- Card styling, scrollable list, shortcut labels all working
- **KNOWN ISSUE:** SearchEntry does not receive keyboard focus when palette opens
  - Typing goes to terminal pane instead of search entry
  - Escape handler also doesn't fire (same root cause)
  - Two fix attempts failed (idle_add_local_once grab_focus, GtkWindowExt::set_focus)
  - Deferred: will be resolved during upcoming UI work
- QA: Completeness 6, Functionality 5, Code quality 8, Robustness 7
- Closed with known issue per user decision

### T-017: Shell exit auto-closes tab/pane ✓
- PTY exit detection via `DrainResult` struct in `drain_pty_output()` (channel disconnect + `is_alive()` fallback for orphan children)
- `on_exit` callback plumbed through `create_terminal()` with one-shot `Cell<Option<Rc>>` semantics
- `close_pane_by_name()` extracted from `close_focused_pane()` -- searches all tabs, handles splits, idempotent
- Deferred close via `glib::idle_add_local_once` avoids reentrancy
- All 4 split call sites updated with `window` parameter
- Fix cycle: added `pty.is_alive()` check for orphan child processes keeping PTY slave fd open
- QA passed: Completeness 9, Functionality 10, Code quality 9, Robustness 9

### T-018: Multi-instance support ✓
- Added `.flags(gio::ApplicationFlags::NON_UNIQUE)` to `adw::Application::builder()` in app.rs
- Each `forgetty` invocation now launches a fully independent window with its own tabs, panes, and terminal state
- Single-line change, all 12 ACs passed
- Observation: taskbar grouping shows separate icons (risk flag, will be addressed by T-022 desktop entry)
- QA passed: Completeness 10, Functionality 10, Code quality 10, Robustness 9

### T-019: CLI flags (--working-directory, -e, --version, --help) ✓
- Rewrote `src/cli.rs` clap struct: `--working-directory`, `-e`/`--execute` (trailing_var_arg), `--class`, `--config-file`, `--version`/`-V`
- `LaunchOptions` struct in `forgetty-gtk::app` carries CLI overrides separate from Config
- `main.rs` validates working directory (exists + is_dir, fallback to home), canonicalizes config path
- Plumbed working_dir + command through `app::run()` -> `build_ui()` -> initial `add_new_tab()` -> `create_terminal()` -> `spawn_pty_bridge()`
- New tabs and splits correctly pass `None, None` (AC-19: CLI flags only affect initial pane)
- `--class` maps to GTK `application_id()` for WM_CLASS
- 1 fix cycle: added `allow_hyphen_values = true` to `-e` arg so `ls -la` and `bash -c` work
- QA passed: Completeness 9, Functionality 9, Code quality 9, Robustness 8

### T-020: TERM/terminfo + shell integration ✓
- Added `TERM_PROGRAM_VERSION` env var to `PtyProcess::spawn()` using `env!("CARGO_PKG_VERSION")` compile-time macro
- All four env vars set unconditionally on every PTY spawn: `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=forgetty`, `TERM_PROGRAM_VERSION=0.1.0`
- New `test_env_vars_set` unit test verifies all four vars reach the child process
- SSH propagation works for `TERM` (automatic). `COLORTERM`/`TERM_PROGRAM`/`TERM_PROGRAM_VERSION` over SSH deferred to shell integration wrapper (separate feature)
- Explicit decision: no custom `forgetty` terminfo entry (xterm-256color is correct and universal)
- 0 fix cycles, 16/16 ACs passed
- QA passed: Completeness 10, Functionality 10, Code quality 10, Robustness 9

### T-021: Signal handling + clean shutdown ✓
- Centralized `kill_all_ptys()` function iterates `TabStateMap` directly (data-driven, not widget-driven)
- `connect_close_request` on `adw::ApplicationWindow` kills all PTYs before window destroy
- Unix signal handlers via `glib::unix_signal_add_local` for SIGTERM, SIGHUP, SIGINT
- Signal handlers registered before `window.present()` (early registration)
- Quit action (Ctrl+Shift+Q) calls `kill_all_ptys()` before `app.quit()`
- `PtyProcess::Drop` safety net kills child if still alive (last resort)
- Defensive `try_borrow`/`try_borrow_mut` throughout shutdown path (no panics)
- No new dependencies (POSIX signal constants defined as named `const i32`)
- Logging: `info!` for shutdown count/reason, `warn!` for individual kill failures
- 0 fix cycles, 15/15 ACs passed
- QA passed: Completeness 10, Functionality 10, Code quality 9, Robustness 9

### T-022: Desktop entry + app icon + install script ✓
- SVG app icon (`assets/icons/dev.forgetty.Forgetty.svg`): Catppuccin Mocha palette, `>_` prompt motif, 128x128 viewbox
- Freedesktop.org `.desktop` entry (`dist/linux/dev.forgetty.Forgetty.desktop`): Terminal=false, StartupWMClass, X-TerminalArgDir/Exec hints, new-window action
- `install.sh`: binary to `/usr/local/bin/`, .so to `/usr/local/lib/` with symlinks + ldconfig, desktop entry + icon to `~/.local/share/`
- `uninstall.sh`: idempotent removal of all installed files with ldconfig + desktop database cleanup
- Smart .so discovery: globs `target/release/build/forgetty-vt-*/` sorted by mtime, fallback to `crates/forgetty-vt/ghostty-out/lib/`
- Proper sudo scope: elevated only for system paths (`/usr/local/`), user-local paths without sudo
- GNOME Activities search, click-to-launch, taskbar icon all verified working
- `desktop-file-validate` passed with zero errors
- 0 fix cycles, 25/25 ACs passed
- QA passed: Completeness 10, Functionality 10, Code quality 9, Robustness 9

### T-023: libghostty-vt bundling ✓
- Top-level `build.rs` sets RUNPATH with `$ORIGIN/lib`, `$ORIGIN/../lib`, `$ORIGIN`, `/usr/local/lib` via `--enable-new-dtags`
- Library crate `build.rs` copies .so + soname symlink to `target/<profile>/lib/` post-build (atomic copy-then-rename)
- Removed broken RPATH directive from library crate build.rs (Cargo ignores `rustc-link-arg` from lib crates)
- `install.sh` updated to check canonical `target/release/lib/` first, fallback to deep build output
- Portable tarball works: `bin/forgetty` + `lib/libghostty-vt.so.0*` runs with no ldconfig, no Zig, no sudo
- Flat deployment works: binary + .so in same directory
- All deployment scenarios covered: cargo run, install.sh, DEB package, portable tarball, flat layout
- 0 fix cycles, 17/17 ACs passed
- QA passed: Completeness 10, Functionality 10, Code quality 9, Robustness 9

**MILESTONE 2 COMPLETE.** All 7 tasks (T-017 through T-023) are done. Forgetty is a self-contained, portable Linux terminal with CLI flags, env vars, signal handling, desktop integration, and bundled shared library. No Zig or ldconfig required at runtime.

### T-024: DEB package build ✓
- `dist/linux/build-deb.sh` produces `forgetty_0.1.0_amd64.deb` from release artifacts
- Version extracted from Cargo.toml (not hardcoded), workspace reference fallback
- FHS-compliant layout: binary at `/usr/bin/`, .so at `/usr/lib/forgetty/` (private dir), desktop entry, icon, man page, copyright
- Dependencies declared: libgtk-4-1, libadwaita-1-0, libc6
- Man page (`dist/linux/forgetty.1`) with NAME, SYNOPSIS, DESCRIPTION, OPTIONS, ENVIRONMENT, FILES, AUTHORS, SEE ALSO
- Debian DEP-5 copyright file with separate Ghostty .so attribution
- `dpkg -i` installs cleanly, `apt remove` removes cleanly, coexists with install.sh
- 1 fix cycle: moved .so from `/usr/lib/` to `/usr/lib/forgetty/` to avoid conflict with Ghostty PPA package; added `$ORIGIN/../lib/forgetty` RUNPATH in `build.rs`; added `umask 022` for correct directory permissions
- QA passed: Completeness 9, Functionality 9, Code quality 9, Robustness 9

### T-025: Stress test suite ✓
- `tests/stress/run-stress-tests.sh` (902 lines) covers all 25 stress scenarios
- `tests/stress/STRESS_TESTS.md` (507 lines) reference documentation
- Supports `--test <name>` for single test, `--auto-only` for automated subset
- PASS/FAIL/SKIP tracking with summary output
- Human testing results: 22 PASS, 1 FAIL (AC-23), 2 NOT TESTED (blocked by AC-23)
- **BUG FOUND:** AC-23 (concurrent high-throughput splits) freezes terminal -- new backlog item
- AC-24 and AC-25 could not be reached because AC-23 froze the terminal
- QA passed: Completeness 10, Functionality 9, Code quality 9, Robustness 9

### T-029: Session persistence (auto-save + restore) ✓
- `forgetty-workspace` crate wired into `forgetty-gtk` for save/restore
- `WorkspaceState` extended with `window_width`/`window_height` (backward-compatible via `#[serde(default)]`)
- `snapshot_workspace_state()`: recursive GTK widget tree walker (TabView → Paned → DrawingArea), reads CWD from `/proc/{pid}/cwd`
- `restore_session()`: rebuilds tabs/splits from saved `WorkspaceState`, deferred `set_position()` for split ratios
- Atomic write: `.tmp` + `fs::rename()` prevents corruption on crash
- Save wired into all 3 exit paths (X button, Ctrl+Shift+Q, SIGTERM/SIGHUP/SIGINT) — BEFORE `kill_all_ptys()`
- 30-second auto-save timer via `glib::timeout_add_local` with weak window reference
- Session-aware launch: skips restore on `--working-directory`, `-e`, or `--no-restore` CLI flags
- Edge cases: corrupt JSON → warning + default tab; deleted CWD → `$HOME` fallback; empty session → default tab
- `try_borrow()` throughout snapshot code for signal-handler safety
- 2 new unit tests (backward compat, window dimension round-trip), 13/13 total pass
- 0 fix cycles, all 22 ACs passed (16 human tests + 6 automated/code review)
- QA passed: Completeness 9, Functionality 10, Code quality 9, Robustness 9

### T-030: Workspace manager ✓
- `WorkspaceView` and `WorkspaceManagerInner` types for per-workspace GTK state (TabView, TabStateMap, FocusTracker, CustomTitles)
- Named workspaces: create (Ctrl+Alt+N with name dialog), rename (hamburger menu), delete (hamburger menu, disabled when 1 left)
- Workspace switching: Ctrl+Alt+1-9 by index, Ctrl+Alt+PageUp/PageDown for prev/next cycling with wrap
- Workspace selector overlay (Ctrl+Alt+W) with ListBox, keyboard nav, click/Enter to switch
- Window title: single workspace shows `user@host:~/path`, multiple shows `WorkspaceName — user@host:~/path`
- Title timer guard: only active workspace's timer updates window title (prevents flickering)
- Session persistence: `save_all_workspaces()` snapshots all workspaces on close/quit/signal/auto-save
- `restore_all_workspaces()` rebuilds all workspaces from saved session
- Backward compat: T-029 lowercase "default" promoted to "Default"
- CLI flag session protection: `--working-directory`, `-e`, `--no-restore` skip session save (don't overwrite real session)
- Short-lived borrow pattern on action handlers (clone Rc, drop manager borrow, then call function)
- Hamburger menu: New/Rename/Delete Workspace + Workspace Selector entries
- Shortcuts window: Workspaces group with all new shortcuts
- Command palette: 4 new workspace commands
- 7 fix cycles (dialog close, shortcut bypass, selector ListBox discovery, GNOME conflict, title loop, copy/paste borrow, CLI session protection)
- QA passed: Completeness 9, Functionality 9, Code quality 8, Robustness 8

### T-031: AI agent notification rings ✓
- OSC 9/99/777 and BEL scanner (`scan_osc_notification()`) with BEL terminator and ST terminator support; OSC 9;4 progress-bar variant skipped
- Amber notification ring (3px, rgba(1.0, 0.78, 0.0, 0.9)) drawn around unfocused panes that receive a notification
- Tab attention badge via `adw::TabPage::set_needs_attention(true)` — clears when all pane rings clear
- Desktop notification via `notify-rust` on background thread (never blocks GTK); `notify-send` title/body for OSC 777
- `on_notify` callback plumbed through all 3 `create_terminal()` call sites
- `NotificationMode` config enum: All (default), RingOnly, None; propagated via `apply_config_change()`
- Rate limiting: 2-second cooldown per pane on desktop notifications; ring still fires every time
- Click-to-focus: desktop notification action routes focus to correct pane + tab via glib channel + polling timer
- BUG-001 (dismiss triggers focus) caught in code review and fixed before testing — only `action == "focus"` triggers focus path
- All 9 human tests passed; 0 fix cycles after BUG-001 pre-test fix
- QA passed: Completeness 9, Functionality 9, Code quality 8, Robustness 8

### T-048: forgetty-session crate — extract SessionManager from GTK ✓
- New `crates/forgetty-session/` crate with zero GTK dependencies
- `SessionManager` (Arc<Mutex<>>, Send+Sync) owns PTY processes and VT instances
- Full public API: `create_pane`, `close_pane`, `write_pty`, `resize_pane`, `drain_output`, `with_vt`, `with_vt_mut`, `pane_info`, `list_panes`, `subscribe_output`, `snapshot_workspace`, `kill_all`
- `NotificationPayload`, `NotificationSource`, `DrainResult`, `scan_osc_notification` moved from `forgetty-gtk` to `forgetty-session`
- `SessionEvent` broadcast channel (capacity 1024) for future daemon/Android consumers
- `WorkspaceLayout` types for GTK-agnostic layout description
- GTK imports from `forgetty_session` instead of defining types locally; all GTK behavior preserved
- 9 unit tests covering AC-3/4/5/7/11 — all pass without GTK/display server
- Known scope: GTK TerminalState still owns its own PTY (dual-VT approach); full transfer is T-051
- Known pre-existing bugs (not T-048 regressions): Ctrl+C not sending SIGINT reliably; OSC notification ring not firing in unfocused split pane
- QA passed: Completeness 9, Functionality 7, Code quality 7, Robustness 8

### T-049: forgetty-daemon binary — headless, systemd user service ✓
- New `forgetty-daemon` binary in `src/daemon.rs` — zero GTK/display-server dependency
- `DaemonArgs` (clap): `--foreground`, `--show-pairing-qr`, `--socket-path`, `--config-file`
- Startup: loads config, creates `SessionManager`, binds `SocketServer`, awaits SIGTERM/SIGINT
- Graceful shutdown: logs "forgetty-daemon shutting down", calls `session_manager.kill_all()`
- `--show-pairing-qr` prints iroh placeholder (wired in T-052)
- `--foreground`: compact colour logs to stderr; default: no-ansi/no-time (for systemd journal)
- `SocketServer::new_with_path()` added for explicit socket path override
- `dist/linux/forgetty-daemon.service`: Type=simple, WantedBy=default.target, MemoryHigh=30M
- Verified: idle RSS ~5 MB (target <25 MB); `ldd` shows no GTK/wayland/X11 symbols
- Socket round-trip verified: `list_tabs` returns `{"tabs":[]}` (stub; wired in T-050)
- `cargo check --workspace` clean; `cargo build --release --bin forgetty-daemon` clean
- QA: Completeness 10, Functionality 10, Code quality 9, Robustness 9

### T-050 + T-051: Wire forgetty-socket + GTK refactor as daemon client ✓
- All 8 JSON-RPC handlers (`list_tabs`, `new_tab`, `close_tab`, `send_input`, `get_screen`, `get_pane_info`, `focus_tab` stub, `split_pane` stub) wired to real `SessionManager`
- Added `subscribe_output` streaming method: client receives live `{"jsonrpc":"2.0","method":"output","params":{"pane_id":"...","data":"<base64>"}}` notifications, connection closes when pane exits
- `forgetty-socket` now depends on `forgetty-session`; `base64 = "0.22"` added for PTY byte encoding
- `handlers::dispatch` accepts `Arc<SessionManager>`; `SocketServer::run_with_streaming` added (existing `run` preserved for backward compat)
- Background drain loop in `daemon.rs`: tokio task polls `sm.drain_output(id)` at 20 ms for all live panes — keeps VT current and fires broadcast events for subscribers
- Three-stage pane validation helper (`require_pane_id`): distinct `-32602` errors for missing param, invalid UUID, pane not found
- `send_input.data` is base64-decoded; invalid base64 returns `-32602`
- `subscribe_output` validation errors close the connection (not continue)
- `handle_get_pane_info`: graceful `-32602` on concurrent pane close (no panic)
- 22 `forgetty-socket` tests pass; all 89 workspace tests pass
- QA: Completeness 9, Functionality 9, Code quality 10, Robustness 8 — PASS

### T-051: GTK refactor as daemon client ✓
- `daemon_client.rs` (new): `DaemonClient` struct — `list_tabs`, `new_tab`, `close_tab`, `resize_pane`, `send_input`, `send_sigint`, `get_screen`, `subscribe_output`
- `subscribe_output` uses `std::sync::mpsc::channel` (tokio background task → GTK main thread, no glib channel dependency)
- `TerminalState.pty: Option<PtyProcess>` — `None` for daemon-backed panes, `Some(pty)` for local
- `TerminalState.daemon_pane_id: Option<PaneId>` — routes input/resize/kill through RPC
- `TerminalState.daemon_client: Option<Arc<DaemonClient>>` — routes VT write-pty responses to daemon
- `create_terminal_for_pane()` (new): daemon-backed terminal, no local PTY spawn
- GTK startup: `ensure_daemon()` connects or spawns daemon; `list_tabs()` reconnects live panes
- GTK close/signal handlers skip killing PTYs in daemon mode (sessions survive window close)
- Input/paste/resize all route through daemon RPC for daemon panes
- T-050 QA fixes included: `send_sigint` in SessionManager, `resize_pane`/`send_sigint` socket handlers
- `cargo check --workspace --release` clean; 112 tests pass, 0 failures
- QA: Completeness 9, Functionality 9, Code quality 9, Robustness 8 — PASS

### T-052: totem-sync / iroh integration — identity + QR pairing ✓
- `forgetty-sync` crate (new): `SyncEndpoint`, `DeviceRegistry`, `QrPayload`, identity module
- Ed25519 identity persisted at `~/.local/share/forgetty/identity.key` (32 raw bytes, chmod 0600)
- iroh 0.97 QUIC endpoint bound on daemon startup (IPv4 + IPv6 UDP sockets)
- Pairing handshake: bi-directional QUIC stream, daemon sends greeting → client sends `{name}` → daemon writes `authorized_devices.json`
- Auto-accept with `--allow-pairing`; unknown device rejected without it
- `authorized_devices.json` atomic writes (`.tmp` → `rename`)
- QR payload: `{v, node_id, machine, relay}` as JSON → `qrcode` lib → ASCII (terminal) + PNG (GTK)
- Daemon CLI: `--show-pairing-qr`, `--allow-pairing`, `--list-devices`, `--revoke <device_id>`
- Socket RPC: `list_devices`, `revoke_device`, `get_pairing_info` methods
- GTK Settings sidebar: "Paired Devices" section with device list, "Pair new device" QR view, Revoke buttons
- Fix cycle: pair-test persistent identity (`pair-test.key`), `qa-tools` feature gate, GTK 2s refresh polling during QR, graceful `SyncEndpoint::close()` on shutdown, fixed revoke row removal
- `cargo build --workspace` clean; 112 tests pass, 0 failures
- QA: Completeness 10, Functionality 9, Code quality 9, Robustness 9 — PASS

### T-053: Full terminal stream to Android ✓
- New `forgetty/stream/1` ALPN alongside `forgetty/pair/1`; ALPN routing via `Accepting::alpn().await` (iroh 0.97)
- `forgetty-sync/src/stream.rs`: `ClientMsg` (Subscribe/Unsubscribe/RequestScrollback) + `DaemonMsg` (FullSnapshot/PtyBytes/ScrollbackPage/PaneGone/Error)
- Frame format: `[u32 BE length][MessagePack payload]` using `rmp-serde` + `serde_bytes` for efficient binary
- `handle_stream_connection`: verifies device auth, reads Subscribe, sends FullSnapshot, streams PtyOutput broadcast events as PtyBytes
- Backpressure: `RecvError::Lagged` → send fresh FullSnapshot instead of disconnecting
- Reader task pattern: recv side in spawned task + mpsc channel (cancellation-safe select!)
- Scrollback: `RequestScrollback` → reads `terminal.scrollback()` slice, returns `ScrollbackPage`
- `SyncEndpoint::bind()` now takes `Arc<SessionManager>`; passes into streaming connection handlers
- `forgetty-stream-test` QA binary: `--stress` for throughput test, `--pair-first` for auto-pairing
- `cargo check --workspace` clean; 116 tests pass, 4 new (stream serialization roundtrips)
- QA: Completeness 9, Functionality 9, Code quality 9, Robustness 8 — PASS

### Post-T-053 desktop fix: on-demand pairing window (2026-04-02)
`allow_pairing` was a static `bool` set at daemon startup — without `--allow-pairing`, new devices were always rejected and the GTK "Pair new device" button was broken by default.

Fix: made `allow_pairing` dynamically togglable via `Arc<AtomicBool>`. Added `SyncEndpoint::enable_pairing(secs)` method that opens a timed window and auto-closes it. Wired through the full stack:
- `forgetty-sync/src/endpoint.rs` — `Arc<AtomicBool>`, `enable_pairing(secs)` method
- `forgetty-socket/src/protocol.rs` — `ENABLE_PAIRING` constant
- `forgetty-socket/src/handlers.rs` — `handle_enable_pairing` handler
- `forgetty-gtk/src/daemon_client.rs` — `enable_pairing()` RPC wrapper
- `forgetty-gtk/src/preferences.rs` — "Pair new device" calls `enable_pairing(120)` before showing QR

No `--allow-pairing` flag needed for normal UI flow. Flag still works for headless/scripted use.

### Post-T-053 manual QA session (2026-04-02)
Manual testing of all four binaries revealed two bugs, both fixed:
- **`default-run` missing**: `cargo run --release` failed with "could not determine which binary to run" (4 binaries exist). Fixed: added `default-run = "forgetty"` to `[package]` in `Cargo.toml`.
- **`--pair-first` fails for already-paired devices** (`src/stream_test.rs`): Daemon's known-device pairing path closes the QUIC connection immediately with code 0 (`"connected-ok"`) without opening a bi-stream. `pair_with_daemon()` was calling `conn.accept_bi()` which failed. Fixed: added graceful handling — if `accept_bi` fails with "connected-ok" in the error, treat it as "already paired, success" and return `Ok(())`.
- **Pane ID confusion**: Live pane IDs (runtime-assigned UUIDs from `SessionManager::create_pane()`) are not the same as workspace IDs saved in `sessions/default.json`. Must use `list_tabs` socket RPC to get real pane IDs. Documented in manual testing guide.
- End-to-end stream test confirmed working: `FullSnapshot` delivered live terminal viewport, then `PtyBytes` streamed in real time (`ping localhost` output).

### GTK daemon wiring audit + fixes (2026-04-02)
Full audit of `create_terminal_for_pane` against `create_terminal` revealed 5 gaps. All fixed and manually verified:
- **Resize** (`connect_resize` stub → real RPC): `tput cols/lines` now updates after window resize; vim `:set columns?` matches; `list_tabs` cols/rows reflect new size. All 3 resize tests PASS.
- **Mouse scroll** (`EventControllerScroll` missing): Mouse wheel scrolls scrollback in normal mode; vim/htop respond to scroll in mouse-tracking mode. PASS.
- **Mouse motion** (`EventControllerMotion` missing): URL hover underline + pointer cursor working; vim/htop mouse tracking motion working. PASS.
- **Right-click context menu** (popover created but never populated or shown): Copy/Paste/Search/Open URL all working. PASS.
- **Ctrl+Left click URL** (handler missing): Opens URL in browser. PASS.
- `man bash` scroll: not a code bug — `less` doesn't enable mouse tracking by default. Fixed user-side with `export LESS="--mouse"` in `~/.zshrc`. PASS.

All 9 manual QA tests passing. GTK daemon client is fully wired.

### T-055: Session file as daemon reconnect source of truth ✓

- Added `pane_id: Option<uuid::Uuid>` to `TabState` (serde default, backward-compat with old session files).
- Added `find_first_daemon_pane_id()` widget walker in `app.rs` — extracts `daemon_pane_id` from the first leaf `DrawingArea`'s `TerminalState` when snapshotting.
- Snapshot (`snapshot_single_workspace`) now populates `TabState.pane_id` from the walker.
- Removed the `if dc_window_close.is_none()` gate on `save_all_workspaces` in both the close-request handler and SIGTERM/SIGHUP signal handlers — session file is now written on GTK close in daemon mode too.
- Replaced flat `list_tabs` reconnect loop with session-file-ordered algorithm: match session tab → live pane by UUID, create fresh daemon pane for gone slots, append remaining live panes at the end.
- Fixed `forgetty-session` crate: `TabState` constructor updated with `pane_id: None`.

### T-056: Daemon reconnect visual fixes — tab titles and snapshot blank space ✓

- Added `daemon_cwd: Option<PathBuf>` field to `TerminalState` in `terminal.rs`.
- `create_terminal_for_pane` gains a `cwd: Option<PathBuf>` parameter; the `ordered` list in the reconnect loop now carries `(PaneId, title, Option<cwd>)` sourced from `pane_info.cwd`.
- `compute_display_title` has a third fallback (after `/proc` CWD and OSC title) that returns `daemon_cwd.file_name()` basename before falling back to `"shell"`.
- Blank-area fix: `create_terminal_for_pane` now strips leading empty snapshot lines before replay and adjusts `effective_cursor_row` accordingly, eliminating the large gray area above the shell prompt on reconnect.

## What's next

**Milestone 4: Daemon Owns Layout (T-059→T-065) — ACTIVE**

Architectural refactor: daemon becomes single source of truth for all session state. GTK becomes a stateless renderer. This ensures consistent state, multi-client support, and the same architecture for every platform (Android, Windows, macOS).

1. **T-059** — `SessionLayout` struct in `SessionManager` ← NEXT
2. **T-060** — Layout mutation methods (create_tab, split_pane, close_tab, move_tab)
3. **T-061** — Daemon saves `default.json` itself (shutdown + debounced + periodic)
4. **T-062** — `get_layout` + layout mutation RPCs
5. **T-063** — Layout change event broadcast (`subscribe_layout`)
6. **T-064** — GTK calls `get_layout` on connect (replaces `load_session` + `list_tabs`)
7. **T-065** — GTK tab/split actions send RPCs (fully stateless, `save_all_workspaces` removed)

**After M4:** Resume M3 AI features (T-032→T-036), known issues, pre-launch testing, then Android (T-054).

M3.5 daemon chain (all done):
- ~~T-048~~ ✓ ~~T-049~~ ✓ ~~T-050/051~~ ✓ ~~T-052~~ ✓ ~~T-053~~ ✓ ~~T-055~~ ✓ ~~T-056~~ ✓ ~~T-057~~ ✓ ~~T-058~~ ✓

## Known issues for future tasks

- **Concurrent high-throughput splits freeze terminal** — Running `cat /dev/urandom | base64` in 2+ split panes simultaneously causes the terminal to become unresponsive. Likely cause: 8ms PTY poll timer processes all panes sequentially on the GTK main thread, saturating the event loop under concurrent high-throughput load. Found by T-025 stress test AC-23. Should be addressed before T-027 benchmarking or as a standalone bug fix.
- **Command palette focus** — SearchEntry in command palette overlay doesn't receive focus. Needs investigation of GTK4 Overlay focus propagation vs DrawingArea 8ms polling timer.
- **Search match positions can go stale in edge cases** — matches are cleared on new PTY data, but rapid scrollback changes or resize during active search may briefly show stale highlights (minor, not blocking)
- **Shift+Click doesn't extend selection** — clears instead (selection improvement)
- **No auto-scroll while drag-selecting past viewport edge** — dragging to top/bottom during selection should auto-scroll (selection improvement)

## Instructions for new sessions

1. Read `CLAUDE.md` — project overview, architecture, workflow rules
2. Read `docs/harness/METHODOLOGY.md` — the 3-phase workflow (Plan → Build → QA)
3. Read `docs/harness/BACKLOG.md` — full backlog with context (READ THE WHOLE THING)
4. Read this file — where we left off
5. Pick the next `[ ]` task from BACKLOG.md
6. Follow METHODOLOGY.md exactly: Planner agent → Builder agent → QA agent

## Key files to preserve (shared core)

- `crates/forgetty-vt/` — libghostty-vt FFI (STABLE, DO NOT MODIFY without cause)
- `crates/forgetty-pty/` — PTY management
- `crates/forgetty-core/` — shared types
- `crates/forgetty-config/` — config/theme
- `crates/forgetty-workspace/` — session persistence
- `crates/forgetty-socket/` — JSON-RPC API
- `crates/forgetty-watcher/` — file watcher
- `crates/forgetty-viewer/` — markdown/image viewer
- `crates/forgetty-gtk/` — GTK4 platform shell (terminal rendering, PTY bridge, input)

## Build requirements

- Rust stable
- Zig 0.15+ at `/home/vick/.local/zig/zig`
- GTK4 + libadwaita dev packages: `sudo apt install libgtk-4-dev libadwaita-1-dev`
- libghostty-vt.so at `crates/forgetty-vt/ghostty-out/lib/`
