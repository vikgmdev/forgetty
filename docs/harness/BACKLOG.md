# Forgetty Development Backlog

> **IMPORTANT FOR ALL AGENTS:** Read this entire file before starting any task.
> It contains the project vision, architecture, and reasoning behind every decision.

---

## Project Vision

**Forgetty** is an AI-first terminal emulator built for developers who work with AI coding agents daily. Simple terminals are for the old days — when you spend your day running Claude Code, managing multiple workspaces across projects, and switching between local and remote sessions, you need a terminal built for that workflow.

**Created by:** Vick (TotemLabsForge, LLC)
**License:** MIT
**Started:** 2026-03-25 (built from zero to working terminal in 2 days with Claude Code)

---

## Who This Is For

Vick is user #1. His daily workflow:
- Multiple Ghostty windows, each with different tabs and splits
- Most tabs running Claude Code or other AI agents
- Constantly Ctrl+Tab-ing to find which agent needs attention
- Copy from Claude Code is broken (box-drawing chars, whitespace)
- Screenshot paste to Claude Code doesn't work well on Linux
- Loses entire terminal setup on every reboot (no persistence)
- Uses Termius on Android but wants seamless PC↔phone handoff
- Works across personal projects and work at Range

**Every feature in this backlog solves one of these pain points.**

---

## Competitive Positioning

| | Ghostty | cmux | Forgetty |
|---|---|---|---|
| **Platform** | macOS + Linux | macOS only | **Linux first**, then Windows/Android/Web |
| **Terminal engine** | libghostty | libghostty (full) | libghostty-vt (same VT parser) |
| **Renderer** | Metal (macOS) + OpenGL (Linux) | Metal (via libghostty) | GTK4/Pango (Linux native) |
| **AI features** | None | Notifications, hooks, browser | Notifications, smart copy, viewer, socket API |
| **Session persist** | No | No | **Yes** |
| **Cloud sync** | No | No (Mac/iPhone sync planned) | **Yes** (premium SaaS) |
| **Workspaces** | No | Workspaces with sidebar | **Yes** |

**Strategy:** Match Ghostty on Linux (identical rendering quality), then surpass with AI-native features. Don't compete on macOS (cmux/Ghostty own that).

**Product in one sentence:** Forgetty is a workspace-aware terminal where your AI agents, tabs, splits, and sessions persist across reboots and sync across devices.

---

## Path Analysis — Why GTK4 (decided 2026-03-26)

We evaluated 4 architectural paths for Linux rendering dominance:

| Path | Beats Ghostty on Linux? | Cross-platform later? | Fatal flaw |
|---|---|---|---|
| **A: Pure wgpu** | No — glyphon has no subpixel AA, no Fontconfig, no IME | Yes (that's wgpu's point) | Can NEVER match native Linux text quality |
| **B: Fork Ghostty (Zig)** | Yes — identical quality | No — Zig + GTK locks us to Linux forever | Dead end for cross-platform |
| **C: GTK4 shell + shared core** | **Yes** — Pango/FreeType identical to Ghostty | Yes — write thin shell per platform | Platform-specific shells (~1,300 lines each) |
| **D: Full libghostty rendering** | Yes — pixel-perfect Ghostty | No — API only stable on macOS/iOS | Not ready for Linux |

**We chose Path C** because it's the only path that BOTH beats Ghostty on Linux AND allows future platform expansion.

### Why wgpu failed on Linux (learned from 2 days of debugging)

- **Text quality**: glyphon/cosmic-text renders alpha-only glyphs (no subpixel antialiasing). FreeType with LCD filtering is visually superior — this is what Ghostty uses.
- **Font discovery**: cosmic-text has built-in font discovery but doesn't respect Fontconfig or system font preferences. Pango/Fontconfig is the Linux standard.
- **IME/compose**: winit's input handling is partial — QA found 57 unmapped keys, no CapsLock/NumLock bits, no IME composition events. GTK handles all of this natively.
- **Native features**: Tabs, splits, scrollbars, context menus all had to be hand-built in wgpu (~5,000 lines). GTK has native widgets for all of these.
- **Old hardware**: wgpu needs Vulkan or GL 3.3+. GTK4 works on any system with Cairo (even CPU-only software rendering).

### Honest comparison: Ghostty vs Forgetty on Linux

| Aspect | Ghostty | Forgetty (GTK4) | Winner |
|---|---|---|---|
| Text rendering | FreeType + subpixel AA | Pango + FreeType (identical) | **Tie** |
| Font discovery | Fontconfig | Fontconfig via Pango | **Tie** |
| VT emulation | libghostty | libghostty-vt (same engine) | **Tie** |
| Colors (256 + truecolor) | Yes | Yes (render state FG/BG_COLOR) | **Tie** |
| Kitty keyboard protocol | Yes | Yes (ghostty key encoder) | **Tie** |
| Mouse tracking | Yes | Yes (ghostty mouse encoder) | **Tie** |
| IME / compose input | GTK native | GTK native | **Tie** |
| Accessibility | GTK ATK | GTK ATK | **Tie** |
| Window decorations | GTK CSD | GTK CSD | **Tie** |
| Old hardware compat | OpenGL 4.3 required | GTK4 Cairo fallback (works anywhere) | **Forgetty** |
| Wayland + NVIDIA | Broken (GTK GL threading) | Works (GTK4 fixed this) | **Forgetty** |
| Session persistence | No | **Yes** | **Forgetty** |
| Workspaces | No | **Yes** | **Forgetty** |
| AI notifications | `*` on tab name only | **Rings, badges, desktop notif** | **Forgetty** |
| Smart copy | No | **Yes (strip box-drawing)** | **Forgetty** |
| Cloud sync | No | **Yes (premium SaaS)** | **Forgetty** |
| Socket API | No | **Yes (JSON-RPC)** | **Forgetty** |
| Reliability | Years of production use | Days of development | **Ghostty** |

### GTK4 effort estimate (replacing ~5,000 lines of wgpu/winit)

| GTK4 Feature | Effort | Lines | GTK Widget |
|---|---|---|---|
| Window + app skeleton | Small | ~100 | `adw::Application` + `adw::ApplicationWindow` |
| Terminal text rendering | Medium | ~300 | `gtk::DrawingArea` + Cairo + Pango |
| Tab bar + close buttons | Small | ~50 | `adw::TabBar` + `adw::TabView` |
| Split panes + drag resize | Small | ~100 | `gtk::Paned` (nested) |
| Cursor rendering | Small | ~20 | Cairo rectangle |
| Mouse text selection | Medium | ~150 | DrawingArea event handlers |
| Scrollbar | Small | ~30 | `gtk::Scrollbar` |
| Context menu | Small | ~40 | `gtk::PopoverMenu` |
| Font zoom | Small | ~30 | Pango font size change |
| Search | Medium | ~100 | `gtk::SearchBar` |
| URL detection | Medium | ~100 | Regex + hover handler |
| Config wiring | Small | ~50 | Read existing TOML parser |
| Small features (bell, blink, opacity) | Small | ~30 | GLib timeout, CSS |
| Font discovery, IME, accessibility | **Free** | 0 | GTK handles automatically |
| **Total** | | **~1,300** | |

**vs what wgpu required**: ~5,000 lines of custom rendering code that never achieved native quality.

### Priority tiers

| Tier | Goal | Tasks |
|---|---|---|
| **Tier 1** | Make Forgetty Vick's daily driver on Linux | T-001 → T-016 (match Ghostty) |
| **Tier 2** | Differentiate — features Ghostty doesn't have | T-017 → T-023 (AI-native) |
| **Tier 3** | Platform expansion | T-024 → T-027 (Windows, Android, Web) |
| **Tier 4** | Business | Premium sync, team features, public launch |

---

## Architecture: Native Shell + Shared Rust Core

```
┌──────────────────────────────────────────────────────┐
│  Platform Shell (THIN — ~1,300 lines per platform)   │
│  Linux:   GTK4 + libadwaita (gtk4-rs)                │
│  Windows: winit + wgpu + DirectWrite (future)        │
│  Android: Jetpack Compose + Rust JNI (future)        │
│  Web:     DOM + WebGPU canvas (future)               │
├──────────────────────────────────────────────────────┤
│  Shared Rust Core (THICK — ~10,000+ lines, ~70%)     │
│                                                      │
│  forgetty-vt      libghostty-vt C FFI wrapper        │
│                   - Terminal, Screen, Cell types      │
│                   - Render state API (Ghostling       │
│                     pattern: GRAPHEMES, FG/BG_COLOR)  │
│                   - 176 GhosttyKey constants          │
│                                                      │
│  forgetty-pty     PTY management (portable-pty)      │
│                   - Spawn shell, read/write, resize   │
│                                                      │
│  forgetty-input   Key encoder + mouse encoder        │
│                   - ghostty_key_encoder_* (Kitty)     │
│                   - ghostty_mouse_encoder_* (SGR)     │
│                   - ghostty_focus_encode              │
│                                                      │
│  forgetty-core    Shared types, errors, platform     │
│  forgetty-config  Config schema, theme, defaults     │
│  forgetty-workspace  Session save/restore, projects  │
│  forgetty-socket  JSON-RPC Unix socket API           │
│  forgetty-watcher File watcher (notify crate)        │
│  forgetty-viewer  Markdown/image viewer (wry)        │
│  forgetty-clipboard  Smart copy pipeline             │
└──────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────┐
│  libghostty-vt.so (Zig, C API — NOT our code)       │
│  - SIMD-optimized VT parser (AVX2/NEON)             │
│  - All escape sequences (CSI, OSC, SGR, DCS)        │
│  - Kitty keyboard + graphics protocol               │
│  - Mouse tracking (X10, SGR, URxvt, SGR-Pixels)     │
│  - Unicode grapheme clusters                         │
│  - Text reflow on resize                             │
│  - Scrollback with viewport control                  │
│  - Write-PTY callbacks (DA, XTVERSION, size)         │
│  - Fuzz-tested, Valgrind-tested                      │
│  - Proven by millions of Ghostty users               │
└──────────────────────────────────────────────────────┘
```

### Data Flow (per frame)

```
PTY stdout → terminal_vt_write() → [libghostty-vt internal state]
                                          ↓
                              render_state_update()
                                          ↓
                              ┌─ render_state_colors_get() → default fg/bg/cursor
                              ├─ row_iterator_next() → for each row:
                              │    └─ row_cells_next() → for each cell:
                              │         ├─ GRAPHEMES_LEN + GRAPHEMES_BUF → String
                              │         ├─ FG_COLOR → GhosttyColorRgb (pre-resolved)
                              │         ├─ BG_COLOR → GhosttyColorRgb (pre-resolved)
                              │         └─ STYLE → bold, italic, inverse flags
                              └─ CURSOR_VIEWPORT_X/Y → cursor position
                                          ↓
                              Platform shell draws cells:
                              Linux: Pango layout → Cairo draw → screen
                              Windows: DirectWrite → DX surface → screen
```

### What We Keep From the wgpu Phase (DO NOT REBUILD)

| Crate | Status | Notes |
|---|---|---|
| `forgetty-vt` | **100% keep** | FFI bindings match Ghostling exactly. Render state, graphemes, colors, cursor all working. |
| `forgetty-pty` | **100% keep** | portable-pty works. PTY spawn, read/write, resize, non-blocking. |
| `forgetty-core` | **100% keep** | Types, errors, platform utils. |
| `forgetty-config` | **100% keep** | Config schema, theme (Catppuccin Mocha), defaults. |
| `forgetty-workspace` | **100% keep** | Workspace/session model, JSON persistence, project detection. |
| `forgetty-socket` | **100% keep** | JSON-RPC protocol, server, handlers. |
| `forgetty-watcher` | **100% keep** | File watcher with debouncing. |
| `forgetty-viewer` | **100% keep** | Markdown/image rendering via pulldown-cmark + wry. |
| Key/mouse encoders | **95% keep** | GhosttyKey constants, encoder logic. Only change: input source from winit→GTK. |
| Keybinding logic | **Keep logic** | Action enum, matching. Rebind to GTK key events. |
| Smart clipboard | **Keep logic** | Strip pipeline. Wire to GTK clipboard + mouse selection. |
| `forgetty-renderer` (wgpu) | **Replace on Linux** | GTK4 handles rendering. Possibly keep for Windows/Android/Web. |
| `forgetty-ui` (winit) | **Replace on Linux** | GTK4 replaces winit event loop. Keep pane/tab data structures. |

### Why GTK4 (not keep wgpu)

| Aspect | wgpu (what we had) | GTK4 (what we're switching to) |
|---|---|---|
| Text quality | glyphon/cosmic-text (Rust, no subpixel AA) | Pango/FreeType (identical to Ghostty) |
| Font discovery | cosmic-text built-in (less complete) | Fontconfig (native Linux, respects system config) |
| IME / compose | winit (partial, 57 unmapped keys found) | GTK IMContext (native, complete) |
| Accessibility | None | GTK ATK (automatic) |
| Tabs | Custom wgpu rendering (~250 lines) | libadwaita::TabBar (built-in widget) |
| Splits | Custom wgpu rendering (caused segfaults) | gtk::Paned (native widget with drag handle) |
| Context menu | None | gtk::PopoverMenu (built-in) |
| Scrollbar | None | gtk::Scrollbar (built-in) |
| CSD | winit default | GTK CSD (native Linux look) |
| Old hardware | Needs Vulkan or GL 3.3+ | Works on ANY GTK-supported system |
| Total renderer code | ~5,000 lines | ~1,300 lines |

### Critical FFI Pattern (learned from debugging)

The libghostty-vt FFI bindings follow the "Ghostling pattern" from `/tmp/ghostling/main.c`. **DO NOT deviate from this pattern** — it caused weeks of segfaults when done wrong.

Key rules:
1. Pass `&mut handle as *mut _ as *mut c_void` (pointer TO handle, NOT handle AS pointer)
2. Use `GRAPHEMES_LEN` + `GRAPHEMES_BUF` for cell text (NOT `grid_ref` + `CODEPOINT`)
3. Use `FG_COLOR`/`BG_COLOR` per cell (pre-resolved RGB, NOT manual palette lookup)
4. Call `render_state_update()` every frame before reading cells
5. Use key encoder + mouse encoder APIs for input (NOT raw byte sequences)
6. Register Write-PTY callback: pass function pointer directly as `*const c_void` (NOT `&fn_ptr`)

---

## User's Actual Pain Points → Feature Mapping

| Pain Point | Current Workaround | Feature | Task |
|---|---|---|---|
| Multiple Ghostty windows per workspace | Open separate windows, mentally track | Workspace manager | T-018 |
| Can't tell which Claude Code needs attention | Ctrl+Tab through all tabs | Notification rings | T-019 |
| Copy from Claude Code is broken | Ask Claude to write to file | Smart copy + selection | T-007, T-020 |
| Commands from Claude need manual copy | Copy with garbage, clean up | Code block detection | T-020 |
| Screenshots to Claude Code broken | Workarounds or give up | Clipboard image support | T-023 |
| Lose entire setup on reboot | Never shut down | Session persistence | T-017 |
| Android: separate Termius setup | Open Termius, SSH manually | Cross-device sync | T-026 |
| Can't switch PC → phone seamlessly | Start from scratch | Session handoff | T-026 |

---

## Milestone 1: GTK4 Pivot — Match Ghostty on Linux

Replace winit + wgpu renderer with GTK4 native shell. Use Pango/FreeType for text (identical to Ghostty). Keep all shared core code.

**Goal:** When M1 is complete, Forgetty should be indistinguishable from Ghostty in terminal rendering quality, and Vick can replicate his current Ghostty setup in Forgetty.

### [x] T-001: GTK4 window + application skeleton
**Scope:** Create a GTK4 Application with a single window using `gtk4-rs` and `libadwaita`. Replace winit event loop. Window opens, shows title "Forgetty", can be resized and closed. No terminal rendering yet — just prove GTK4 works with our cargo workspace.
**Crate changes:** Add `gtk4-rs` and `libadwaita` dependencies. Create `crates/forgetty-gtk/` crate for the Linux platform shell. Update `src/main.rs` to launch GTK app instead of winit.
**Keep:** All existing crates except `forgetty-renderer` (wgpu) and the winit parts of `forgetty-ui`.
**GTK approach:** `adw::Application` → `adw::ApplicationWindow` with header bar.
**AC:** Window opens. Window resizes. Window closes cleanly. `cargo build --release` works. Native GTK CSD (client-side decorations).

### [x] T-002: Terminal grid rendering with Pango + Cairo
**Scope:** Inside the GTK window, render the terminal grid using `gtk::DrawingArea` + Cairo + Pango. Read cells from libghostty-vt render state (existing `forgetty-vt` code). Draw each cell's grapheme with correct fg/bg colors. Draw cursor. Spawn PTY and feed terminal.
**Core rendering loop (pseudocode):**
```
fn draw(ctx: &cairo::Context) {
    render_state_update(render_state, terminal);
    let colors = render_state_colors_get();
    for each row (row_iterator_next):
        for each cell (row_cells_next):
            graphemes → Pango layout → cairo draw
            fg/bg colors → cairo set_source_rgb
    draw cursor at CURSOR_VIEWPORT_X/Y
}
```
**AC:** Shell prompt appears with colors. Typing produces visible text. Cursor is at the correct position. Colors match Ghostty (FreeType + Pango = identical rendering stack). `vim`, `htop`, `claude` all render correctly.

### [x] T-003: Keyboard input via ghostty key encoder
**Scope:** Forward GTK key events through the existing ghostty key encoder. Map GDK key codes to GhosttyKey constants (adapt from the winit mapping in `ghostty_input.rs`). Handle press, release, repeat. Use `gtk::EventControllerKey`.
**AC:** Typing works. Arrow keys work in vim. Ctrl+C sends SIGINT. Tab completion works in shell. Kitty keyboard protocol detected by nvim.

### [x] T-004: Mouse input via ghostty mouse encoder
**Scope:** Forward GTK mouse events through the ghostty mouse encoder. Use `gtk::GestureClick`, `gtk::EventControllerMotion`, `gtk::EventControllerScroll`. Check mouse tracking mode for scroll → app vs scrollback.
**AC:** htop responds to mouse clicks. vim mouse mode works. Scroll wheel navigates scrollback in shell. Scroll wheel scrolls in vim.

### [x] T-005: Tab bar with libadwaita TabBar
**Scope:** Replace custom wgpu tab bar with `adw::TabBar` + `adw::TabView`. Each tab gets its own terminal DrawingArea + PTY. Tab titles show CWD basename (existing `display_title()` logic). Close button on tabs. New tab button (+).
**AC:** Tabs appear with titles. Click tab switches terminal. Close button closes tab and kills PTY. Ctrl+Shift+T creates new tab. Tab title updates on `cd`. Last tab close exits app.

### [x] T-006: Split panes with gtk::Paned
**Scope:** Use `gtk::Paned` for split panes. Each split creates a new terminal DrawingArea + PTY in a nested Paned widget. Support horizontal (side-by-side) and vertical (top-bottom) splits. Drag handle to resize. Track focused pane for keyboard input routing.
**AC:** Alt+Shift+= splits right. Alt+Shift+- splits down. Drag divider resizes. Alt+Arrow navigates between panes (changes which pane receives keyboard input). Each pane has its own shell. Ctrl+Shift+W closes focused pane.

### [x] T-007: Mouse text selection
**Scope:** Click-drag in terminal selects text. Map pixel coordinates to cell coordinates. Highlight selected cells with selection color. Release copies to clipboard (or Ctrl+Shift+C copies). Wire to existing smart clipboard pipeline (strip box-drawing chars, trailing whitespace).
**AC:** Click and drag selects text visually. Ctrl+Shift+C copies clean text. Box-drawing characters from Claude Code output are stripped. Double-click selects word. Triple-click selects line.

### [x] T-008: Scrollbar
**Scope:** Add a scrollbar that reflects scrollback position. Query `GHOSTTY_TERMINAL_DATA_SCROLLBAR` for state (total, offset, len). Connect to `ghostty_terminal_scroll_viewport`. Can use `gtk::Scrollbar` or overlay.
**AC:** Scrollbar appears when there's scrollback content. Dragging scrollbar scrolls viewport. Clicking above/below thumb jumps a page. Scrollbar hides when viewport covers all content.

### [x] T-009: Search in terminal (Ctrl+Shift+F)
**Scope:** Search bar using `gtk::SearchBar` that finds text in the terminal. Iterate render state cells to find text matches. Highlight matches. Next/Previous navigation.
**AC:** Ctrl+Shift+F opens search. Typing highlights matches. Enter goes to next match. Shift+Enter goes to previous. Escape closes search.

### [x] T-010: Right-click context menu
**Scope:** Right-click in terminal shows `gtk::PopoverMenu` with: Copy, Paste, Select All, Search, Open URL (if cursor is on a URL).
**AC:** Right-click shows menu. Copy copies selected text (smart copy). Paste inserts clipboard. Select All selects all visible text.

### [x] T-011: Font zoom (Ctrl+Plus/Minus)
**Scope:** Ctrl+= increases font size, Ctrl+- decreases, Ctrl+0 resets to default. Update Pango font description, recalculate cell dimensions, call `ghostty_terminal_resize()` with new grid size.
**AC:** Ctrl+= makes text bigger. Ctrl+- makes text smaller. Ctrl+0 resets. Grid reflows correctly after zoom.

### [x] T-012: URL detection + click
**Scope:** Detect URLs in terminal output (regex for http/https/file). Underline on mouse hover (change cursor to pointer). Ctrl+Click opens in default browser via `gtk::show_uri`.
**AC:** URLs are visually underlined on hover. Ctrl+Click opens browser. Works with `git clone` URLs, web URLs.

### [x] T-013: Cursor blink + style from terminal
**Scope:** Cursor blinks when terminal requests it (GLib timeout toggles visibility). Read cursor style from render state `CURSOR_VISUAL_STYLE` (bar/block/underline/block_hollow). Respect cursor visibility from terminal.
**AC:** Cursor blinks in shell. vim changes cursor to bar in insert mode. Cursor stops blinking when typing (reset timer on keypress).

### [x] T-014: Bell (visual + audio)
**Scope:** Visual bell (briefly flash the terminal background) and/or audio bell (`gdk::Display::beep()`) when BEL character received. Detect via the bell callback already registered in `terminal.rs`. Configurable in config.toml.
**AC:** `echo -e '\a'` triggers visual flash. Bell type configurable (visual/audio/none) in config.toml.

### [x] T-015: Config file loading + hot reload
**Scope:** Read `~/.config/forgetty/config.toml` on launch (existing `forgetty-config` code). Apply: font family, font size, theme colors, keybindings, bell mode. Watch for file changes (existing `forgetty-watcher`) and hot-reload.
**AC:** Changing font_size in config.toml and saving updates the terminal live. Theme color changes apply without restart.

### [x] T-016: Native window decorations + keyboard shortcuts display
**Scope:** Use libadwaita CSD with header bar. Add Ctrl+? or F1 shortcut to show `gtk::ShortcutsWindow` listing all keybindings.
**AC:** Window has native GNOME-style decorations. F1 opens shortcuts help dialog.

### Milestone 1 Extra: UI Polish — Match Ghostty's Chrome

Unplanned tasks based on comparing Forgetty vs Ghostty side-by-side. These must be done before M2.

### [x] T-M1-extra-001: Move tabs to top panel + Ghostty-style header layout
**Scope:** Replicate Ghostty's header bar layout: tabs in the TOP panel (between title bar buttons and hamburger menu), not below it. Ghostty has: `[tab dropdown ▾] [tab1 ×] [tab2 ×] [tab3 ×] ... [title centered] [grid icon] [hamburger ≡] [minimize] [maximize] [close]`. The tab bar should be IN the header bar, not a separate bar below it. The `+` for new tab should be a dropdown menu (like Ghostty's left dropdown) with: New Tab, Split Up, Split Down, Split Left, Split Right.
**Reference:** Ghostty screenshots — tabs live in the CSD title bar itself.
**AC:** Tabs appear in the header bar. No separate tab bar below. Dropdown for new tab/split actions. Clean, compact layout matching Ghostty's density.

### [x] T-M1-extra-002: Hamburger menu matching Ghostty
**Scope:** The hamburger menu (≡) should have all the actions Ghostty has: Copy, Paste, New Window, Close Window, Change Tab Title..., New Tab, Close Tab, Split (submenu: Up, Down, Left, Right), Clear, Reset, Command Palette, Terminal Inspector, Open Configuration, Reload Configuration, About Forgetty, Quit. Each with its keyboard shortcut displayed.
**Reference:** Ghostty's hamburger menu screenshot.
**AC:** Hamburger menu has all listed items. Keyboard shortcuts shown next to each. All actions work when clicked.

### [x] T-M1-extra-003: Appearance sidebar (live config)
**Scope:** A full GTK preferences window (like GNOME Settings style) where users can modify ALL config options visually. Categories: Appearance (theme, font, font size, opacity), Behavior (bell mode, cursor style, cursor blink), Keybindings (customizable shortcuts), Editor (external editor command). Changes write to `~/.config/forgetty/config.toml` and hot-reload takes effect immediately. No need to edit the config file manually.
**AC:** Menu → Preferences opens settings window. Change font size via slider → terminal updates live. Change theme → terminal updates. Change keybinding → works immediately. Changes persist to config.toml.

### [x] T-M1-extra-004: Theme browser with live preview + bundled themes
**Scope:** In the Preferences window, a theme section that lists all available themes. Arrow up/down through the list and the terminal updates in REAL TIME to preview the theme. Press Enter to apply, Escape to cancel. Show a small color swatch preview next to each theme name.

**Bundled themes:** Ship with 200+ themes out of the box from the iTerm2-Color-Schemes collection (MIT licensed, same source Ghostty uses). Convert them to Forgetty's TOML format and bundle in `resources/themes/`. Include at minimum: 0x96f, Catppuccin Mocha/Latte/Frappe/Macchiato, Dracula, Gruvbox Dark/Light, Nord, Solarized Dark/Light, Tokyo Night, One Dark/Light, Monokai, Ayu Dark/Light, Atom One Dark, Material, Palenight, Rosé Pine, Everforest, Kanagawa, and all the themes shown in Ghostty's `+list-themes` screenshot.

**Theme sources:** `resources/themes/` (bundled) + `~/.config/forgetty/themes/` (user custom).

**Key UX difference from Ghostty:** Ghostty requires `ghostty +list-themes` CLI in a separate terminal. Forgetty shows the live preview in your ACTUAL working terminal while you browse — no CLI, no config editing, no window switching.

**AC:** Open Preferences → Themes. See 200+ themes listed. Arrow through them — terminal colors change in real time. Enter applies and saves to config. Escape reverts. User custom themes from `~/.config/forgetty/themes/` also appear in the list.

### [x] T-M1-extra-005: Command Palette (Ctrl+Shift+P) ⚠️ KNOWN ISSUE: SearchEntry focus
**Scope:** A fuzzy-searchable command palette (like VS Code) listing all actions: New Tab, Split Right, Close Tab, Toggle Fullscreen, Open Config, Reload Config, Change Theme, Zoom In, Zoom Out, etc. Type to filter. Enter to execute. Shows keyboard shortcut next to each command.
**Reference:** Ghostty has "Command Palette" in its hamburger menu.
**AC:** Ctrl+Shift+P opens overlay. Type "split" → shows Split Up/Down/Left/Right. Enter executes. Escape closes. All actions from hamburger menu are available here.

### [ ] T-M1-extra-006: Shell Profiles System (Windows Terminal's killer feature)
**Scope:** Named profiles for different shell configurations. Each profile has: shell command (bash, zsh, fish, `ssh user@host`), icon (shown in tab and dropdown), starting directory, per-profile appearance (optional theme/font override). Profiles appear in the new-tab dropdown with icons and Ctrl+Shift+1-9 shortcuts. Profiles stored in `config.toml` under `[[profiles]]` array. Default profile configurable.
**Reference:** Windows Terminal's profile system — each profile is a distinct shell environment with its own identity.
**Example config:**
```toml
[[profiles]]
name = "Local Shell"
command = "/usr/bin/zsh"
directory = "~"
icon = "terminal"

[[profiles]]
name = "Range Work"
command = "/usr/bin/zsh"
directory = "~/work/range"
icon = "folder"

[[profiles]]
name = "Range SSH"
command = "ssh range@luna-04"
icon = "network"
```
**AC:** New-tab dropdown shows all profiles with icons. Click profile → opens tab with that shell/dir. Ctrl+Shift+1 opens first profile. Per-profile starting directory works. Profiles persist in config.toml. Adding a new profile via config → appears in dropdown after hot-reload.

### [ ] T-M1-extra-007: Full keybindings/actions editor (GUI)
**Scope:** A settings page (or sidebar section) listing ALL actions with their current keybindings. Each row: action name, current shortcut, edit button. Click edit → press new key combo → saves. Based on Windows Terminal's Actions page. Include all ~40+ actions: tab management, split management, focus navigation, clipboard, zoom, search, scroll, clear, reset, open config, command palette, etc.
**Reference:** Windows Terminal Settings → Actions — massive searchable list of every action with editable keybinding per row.
**AC:** Open Settings → Keybindings. See all actions listed with search/filter. Click to edit any keybinding (press new combo to capture). New binding takes effect immediately. Saved to config.toml under `[keybindings]`. Conflicts detected (warn if same key combo assigned to two actions). "Reset to default" button per action. "Reset all" button. Must match Windows Terminal's Actions page depth — every single action the terminal supports must be listed and editable.

### [ ] T-M1-extra-008: Paste safety warnings
**Scope:** When pasting text into the terminal, warn the user in these cases: (a) paste is >5 KiB (large paste could be accidental), (b) paste contains newline characters (could execute commands unintentionally). Show a GTK confirmation dialog with the paste preview. Option to "Paste anyway" or "Cancel". Configurable in config.toml (`paste_warn_size = 5120`, `paste_warn_newline = true`).
**Reference:** Windows Terminal Settings → Interaction → Warnings section.
**AC:** Paste >5KiB → warning dialog with size shown. Paste with newlines → warning dialog. Both configurable to disable in config.toml. "Paste anyway" proceeds, "Cancel" aborts.

### [ ] T-M1-extra-009: Tab color + duplicate tab
**Scope:** Two features: (1) Right-click tab → "Change Tab Color" → color picker to set a tint color on the tab background. Helps visually distinguish tabs (e.g., red for production, blue for dev). (2) Right-click tab → "Duplicate Tab" → creates a new tab with the same CWD, profile, and shell. Also add to right-click tab context: "Close Tabs to the Right", "Close Other Tabs".
**Reference:** Windows Terminal right-click tab menu.
**AC:** Right-click tab shows: Change Color, Rename, Duplicate, Move (submenu), Close Tabs to Right, Close Other Tabs, Close Tab. Tab color tint persists visually. Duplicate opens same CWD.

### [ ] T-M1-extra-010: Quake/dropdown mode (global hotkey)
**Scope:** Register a global hotkey (e.g., F12 or configurable) that toggles a dropdown terminal from the top of the screen, like Guake/Yakuake/Tilda on Linux. When triggered: if Forgetty is hidden, slide down from top edge; if visible, slide up and hide. The dropdown terminal is a separate window with auto-hide on focus loss (configurable). Uses `gtk_layer_shell` for Wayland or `_NET_WM_WINDOW_TYPE_DROPDOWN` for X11.
**Reference:** Windows Terminal's "Quake mode", Guake, Yakuake on Linux.
**AC:** Press F12 → terminal drops down from top of screen. Press F12 again → slides back up. Global hotkey works from any application. Auto-hide on focus loss (configurable). Full terminal features in dropdown mode.

### [ ] T-M1-extra-011: Fullscreen mode (F11)
**Scope:** Toggle fullscreen with F11. Hide all window decorations, tab bar expands to full screen. Press F11 again to exit fullscreen. Also accessible from hamburger menu and command palette.
**AC:** F11 toggles fullscreen. No title bar in fullscreen. Tab bar still visible. Terminal content fills screen. F11 again restores windowed mode.

### [ ] T-M1-extra-012: Export terminal text
**Scope:** Right-click or menu action → "Export Text" saves the terminal buffer (visible + scrollback) to a text file. File save dialog lets user choose location. Option to export as plain text or with ANSI escape sequences preserved. Uses `ghostty_formatter_terminal_new()` with `PLAIN` or `VT` format.
**Reference:** Windows Terminal right-click → "Exportar texto".
**AC:** Right-click → Export Text. Save dialog opens. Plain text file contains all terminal output. ANSI option preserves colors for sharing.

---

## Milestone 2: Production Readiness — Actually Installable on Linux

**Goal:** Someone can `sudo apt install forgetty` (or download a .deb) on a fresh Ubuntu machine and use it as their daily terminal. Not "works on my laptop" — works on ANY Linux desktop.

### [x] T-017: Shell exit auto-closes tab/pane
**Scope:** When a shell process exits (`exit`, process terminates, EOF), automatically close the tab/pane. Last tab close exits Forgetty. Detect via PTY EOF in the polling loop.
**AC:** Type `exit` → tab closes. Run `bash -c "exit 0"` → tab closes. Kill shell PID externally → pane closes. Last tab close exits app cleanly.

### [x] T-018: Multi-instance support
**Scope:** Allow multiple Forgetty windows simultaneously. Set `gio::ApplicationFlags::NON_UNIQUE` on the GTK application. Each instance is independent.
**AC:** Run `forgetty` twice → two separate windows. Each has independent tabs/panes. Closing one doesn't affect the other.

### [x] T-019: CLI flags (--working-directory, -e, --version, --help)
**Scope:** Standard terminal CLI flags: `--working-directory /path` (start in dir), `-e "command"` (run command instead of shell), `--version`, `--help`, `--class` (WM class). Use existing clap CLI parsing from `src/cli.rs`.
**AC:** `forgetty --version` prints version. `forgetty -e htop` opens htop directly. `forgetty --working-directory /tmp` starts in /tmp. `forgetty --help` shows all flags.

### [x] T-020: TERM/terminfo + shell integration
**Scope:** Set `TERM=xterm-256color` (or custom `forgetty` terminfo if we ship one), `COLORTERM=truecolor`, `TERM_PROGRAM=forgetty`, `TERM_PROGRAM_VERSION=0.1.0`. Ensure these propagate through SSH. Test that programs (vim, htop, tmux, ssh) detect capabilities correctly.
**AC:** `echo $TERM` shows `xterm-256color`. `echo $COLORTERM` shows `truecolor`. SSH to remote machine → `echo $TERM` still correct. Colors work over SSH. tmux inside Forgetty works.

### [x] T-021: Signal handling + clean shutdown
**Scope:** Handle SIGHUP (terminal closed), SIGTERM (system shutdown), SIGINT gracefully. Save session state before exit. Kill all child PTY processes. No orphan processes left after close.
**AC:** `kill -TERM $(pgrep forgetty)` → saves session, exits cleanly. Close window → no orphan shell processes (check with `ps aux | grep zsh`). System shutdown → session saved.

### [x] T-022: Desktop entry + app icon + install script
**Scope:** Create `forgetty.desktop` file, install script that copies binary to `/usr/local/bin/`, desktop entry to `~/.local/share/applications/`, icon to `~/.local/share/icons/`. Forgetty appears in GNOME app launcher.
**AC:** Run `./install.sh`. Forgetty appears in Activities search. Click launches terminal. Icon visible in taskbar. `which forgetty` returns `/usr/local/bin/forgetty`.

### [x] T-023: libghostty-vt bundling
**Scope:** The .so file must ship with the binary. Options: (a) static link libghostty-vt.a (fix simdutf linking), (b) bundle .so next to binary with RPATH, (c) install .so to /usr/local/lib. The user must NOT need Zig installed to run Forgetty.
**AC:** Fresh Ubuntu machine. Copy the forgetty binary + libs. Run it. Works. No "libghostty-vt.so not found" error. No Zig required.

### [x] T-024: DEB package build
**Scope:** Create a Debian package (.deb) that installs forgetty binary, .so libs, desktop entry, icon, man page. Build with `cargo-deb` or manual `dpkg-deb`. Include dependencies on `libgtk-4-1`, `libadwaita-1-0`.
**AC:** `sudo dpkg -i forgetty_0.1.0_amd64.deb` installs cleanly. `forgetty` launches. `sudo apt remove forgetty` removes cleanly.

### [x] T-025: Stress test suite
**Scope:** Automated and manual tests for robustness: `cat /dev/urandom` (high throughput), `yes` (infinite output), large file cat (100MB), ncurses apps (dialog, whiptail), tmux/screen nested, 256-color test, Unicode edge cases (emoji, CJK, combining marks, zero-width joiners), very long lines (10K+ chars), bracketed paste mode, OSC 8 hyperlinks.
**AC:** All tests documented with expected behavior. No crashes. No visual corruption. Performance doesn't degrade under sustained load.

### [ ] T-026: Test on fresh Ubuntu VM *(deferred — pre-launch)*
### [ ] T-027: Performance benchmarking vs Ghostty *(deferred — pre-launch)*
### [ ] T-028: Memory leak stress test (24h) *(deferred — pre-launch)*

> **T-026, T-027, T-028 on hold.** Moved to after M3, before launch. Full scope preserved below in Pre-Launch section.

---

## Milestone 3: Surpass Ghostty — AI-Native Features

These features don't exist in Ghostty or any other Linux terminal. This is where Forgetty becomes worth switching to.

### [x] T-029: Session persistence (auto-save + restore)
**Scope:** Save all workspaces, tabs, panes, CWDs to JSON on exit (existing `forgetty-workspace` code). Auto-restore on launch. Periodic auto-save (every 30s). Save to `~/.local/share/forgetty/sessions/`.
**Pain point solved:** "Every time I reboot I need to recreate the whole setup"
**AC:** Close Forgetty with 3 tabs and splits. Reopen. Same layout restored with correct CWDs.

### [x] T-030: Workspace manager
**Scope:** Named workspaces (e.g., "Range", "Personal", "Forgetty"). Each workspace has its own set of tabs/panes. Switch with keyboard shortcut. Workspace selector sidebar or overlay.
**Pain point solved:** "I have to open multiple Ghostty windows, each for different projects"
**AC:** Create workspace "Range". Switch to "Personal". Each has completely separate tabs. Ctrl+Alt+1/2/3 switches. Workspaces persist across restart.

### [x] T-031: AI agent notification rings
**Scope:** When a background pane receives OSC 9/99/777 (notification) or BEL, show: colored ring around pane border, badge number on tab, desktop notification (via `libnotify`). Click notification switches to that pane.
**Pain point solved:** "I need to constantly Ctrl+Tab to find which Claude Code needs attention"
**AC:** Run Claude Code in background tab. When it needs input, tab shows badge. Pane border glows. Desktop notification appears.

---

## Milestone 3.5: Daemon Architecture + Android Sync (CRITICAL — Pre-Launch)

> **Why before M4:** The daemon + sync foundation IS the primary product differentiator.
> "Unstoppable terminals that follow you to your phone" requires this.
> T-037 (Windows) and T-038 (Android wgpu) are superseded by this milestone.
> Full spec: `docs/architecture/DAEMON_SYNC_ARCHITECTURE.md`

### [x] T-048: forgetty-session crate — extract SessionManager from GTK
**Scope:** Create `crates/forgetty-session/` that owns all PTY processes, VT state, workspace/tab/pane tree, and session lifecycle. Currently scattered through `forgetty-gtk` as `Rc<RefCell<TerminalState>>` per pane. Extract into platform-agnostic `SessionManager` (Arc<Mutex<>>, tokio-compatible) with no GTK dependency. This is the most critical refactor in the project — everything daemon and Android depends on it.
**Pain point solved:** "GTK owns the PTYs so closing the window kills sessions. Every other product (Android, Web, CLI) can't access sessions."
**AC:** `forgetty-session` crate compiles standalone with zero GTK dep. Contains: `SessionManager`, `PaneState`, `PtyBridge`, `VtInstance` wrappers. GTK still fully functional (imports forgetty-session internally). All existing tests pass. `cargo check --workspace` clean.

### [x] T-049: forgetty-daemon binary — headless, systemd user service
**Scope:** New binary `forgetty-daemon` that starts `SessionManager` headlessly, runs `forgetty-socket` JSON-RPC server (Unix socket), runs totem-sync iroh listener. Includes systemd user service (`dist/linux/forgetty-daemon.service`), `install.sh` updated to enable daemon on install, `--show-pairing-qr` flag for headless server use case, `--foreground` flag for debugging.
**Pain point solved:** "Closing my terminal window kills all my sessions / I can't connect from Android if GTK isn't open."
**UX:** All panes are always daemon panes. No user action needed — sessions live in daemon by default. GTK is just a renderer.
**AC:** `forgetty-daemon` binary exists and starts cleanly. Sessions survive GTK close (PTYs keep running). `systemctl --user status forgetty-daemon` shows running after install. `--show-pairing-qr` prints scannable QR to terminal. Memory under 25MB idle.

### [x] T-050: Wire forgetty-socket to real daemon state
**Scope:** All 8 JSON-RPC handlers in `forgetty-socket` return placeholders. Wire them to `SessionManager` via tokio channels (socket server → channel → session runtime). Implement: `list_tabs` returns real tabs with CWD/PID/metadata; `new_tab` spawns real PTY; `close_tab` kills PTY; `split_pane` creates split PTY; `send_input` writes bytes to PTY master; `get_screen` reads VT cell state; `get_pane_info` returns real rows/cols/title. Add `subscribe_output` streaming method (daemon pushes PTY output events to connected clients).
**Pain point solved:** "Socket API exists but returns fake data — Claude Code hooks, MCP server, and Android all blocked on this."
**AC:** `echo '{"jsonrpc":"2.0","method":"list_tabs","id":1}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/forgetty.sock` returns real tab data. `send_input` causes visible output. `get_screen` returns current viewport. `subscribe_output` streams live PTY bytes to client. All existing unit tests pass.

### [x] T-051: GTK refactor as daemon client
**Scope:** Convert `forgetty-gtk` from "owner of everything" to "renderer that connects to daemon via Unix socket." GTK sends input via `send_input` RPC. GTK renders VT state by subscribing to `subscribe_output` (applies PTY bytes to local VT mirror via forgetty-vt). Tab/pane structure reconciled from daemon on startup. New tab → `new_tab` RPC → daemon spawns PTY → GTK creates DrawingArea. Ensure-daemon pattern: if daemon not running on GTK launch, spawn it as a subprocess before connecting.
**Pain point solved:** "All features are good but the architecture is a dead end — can't share sessions between GTK and Android."
**AC:** Close GTK → sessions alive → reopen GTK → all panes reconnected with current state. Two GTK windows can attach to same daemon simultaneously (same PTY output visible in both). Run `cat /dev/urandom | base64` → close GTK → reopen → command still running, output still flowing.

### [x] T-052: totem-sync / iroh integration — identity + QR pairing
**Scope:** Integrate iroh into `forgetty-daemon`. Persistent Ed25519 identity at `~/.local/share/forgetty/identity.key`. QR code generation (iroh node_id + machine name + relay hint). Pairing handshake: Android dials daemon by node_id, daemon checks if device is in `authorized_devices.json`, if new device → confirmation dialog in GTK (or auto-accept if `--allow-pairing` active). Device registry: list, revoke. Settings panel: paired devices list, pair new device (shows QR), revoke button.
**Pain point solved:** "Setting up phone access requires manual SSH keys and port forwarding."
**AC:** GTK shows "Pair phone" option → QR appears. Scanning from Android phone → iroh QUIC connection established within 3 seconds. `authorized_devices.json` updated. Device listed in Settings → Paired Devices. Revoke disconnects device and removes from list. Re-pair after revoke works. Identity survives daemon restart.

### [x] T-053: Full terminal stream to Android
**Scope:** Stream raw PTY output bytes from daemon to connected Android devices via iroh QUIC (one QUIC stream per active pane). On connect: send `FullSnapshot` (structured cells for viewport only). Ongoing: send raw PTY bytes (Android runs same VT parser via JNI). Scrollback: lazy-loaded on demand. Backpressure: drop intermediate frames → send fresh `FullSnapshot` if Android falls behind. Optional zstd compression for high-throughput output. Message framing: MessagePack.
**Pain point solved:** "Can't see my terminal from my phone."
**AC:** Connected Android displays live terminal output. High-throughput (`cat /dev/urandom | base64`) doesn't disconnect. Network switch (WiFi→cellular) auto-reconnects within 2 seconds via iroh QUIC migration + DERP relay fallback. Scrollback fetches lazily. Output visually identical to desktop rendering.
**Post-ship fixes (2026-04-02):**
- `Cargo.toml`: added `default-run = "forgetty"` — `cargo run --release` was failing because 4 binaries exist.
- `src/stream_test.rs` `--pair-first`: daemon closes QUIC connection with code 0 immediately for known devices (no bi-stream opened). Fixed `pair_with_daemon()` to treat "connected-ok" close as success instead of error.
- Documented: live pane IDs come from `list_tabs` socket RPC, not from the sessions JSON file (workspace IDs ≠ pane IDs).

### [x] T-055: Session file as daemon reconnect source of truth
**Scope:** Make `~/.local/share/forgetty/sessions/default.json` the authoritative record for daemon-mode reconnect. Currently the file is never written on GTK close in daemon mode, and reconnect uses `list_tabs` order (HashMap-based, fixed by T-054 pane_order but still not layout-preserving). This task wires the session file into the full daemon reconnect lifecycle.
**Pain point solved:** "Tabs come back in the wrong order / with wrong content after closing and reopening the window."
**Files:** `crates/forgetty-workspace/src/workspace.rs`, `crates/forgetty-gtk/src/app.rs`

**Schema change — workspace.rs**
Add `pane_id: Option<uuid::Uuid>` to `TabState` (serde `default` for backward compat):
```rust
pub struct TabState {
    pub title: String,
    pub pane_tree: PaneTreeState,
    #[serde(default)]
    pub pane_id: Option<uuid::Uuid>,
}
```

**Save on daemon-mode close — app.rs**
In `connect_close_request` (and SIGTERM/SIGHUP handlers): save the session file in daemon mode too. Remove the `if dc_window_close.is_none()` gate on `save_all_workspaces` — keep it only on `kill_all_workspace_ptys`. Populate `pane_id` when snapshotting: `TerminalState` already holds `pane_id`; pass it through `snapshot_pane_tree` → `TabState`.

**Reconnect from file — app.rs daemon block (~line 1166)**
Replace the current "call list_tabs, create one tab per daemon pane in daemon order" flow:
1. Call `list_tabs` → build `HashMap<PaneId, PaneInfo>` of live panes
2. Load session file
3. For each tab in **session file order**:
   - If `tab.pane_id` is `Some` and that pane exists in the map → reconnect it (`subscribe_output` + `get_screen` + `create_terminal_for_pane`), remove from map
   - Otherwise → create a new daemon pane (`dc.new_tab()`), subscribe, create terminal
4. Append any remaining map entries (daemon panes not in session file) as new tabs
5. If session file missing/empty → fall through to existing "create one new tab" path

**AC:**
- [ ] Close GTK window in daemon mode → session file written with correct `pane_id`s and tab titles in visual order
- [ ] Reopen → tabs appear in the same visual order as before close
- [ ] Reopen with 3 tabs (Tab 1 had `sleep 1000`) → Tab 1 still shows Tab 1's content, no mixing
- [ ] Daemon pane closed between GTK close and reopen → a fresh pane is created in its slot, no crash
- [ ] Session file from before this change (no `pane_id` field) → deserializes cleanly, reconnect falls back gracefully

### [x] T-056: Daemon reconnect visual fixes — tab titles and snapshot blank space

**Scope:** Two visual regressions found during T-055 QA (screenshot evidence, 2026-04-02). Both are in the daemon-mode reconnect path. Neither existed in self-contained mode.

**Pain point solved:** "After reopening the window all tabs are titled 'shell' and there's a huge blank area above the shell prompt."

**Files:** `crates/forgetty-gtk/src/terminal.rs` (both fixes), `crates/forgetty-gtk/src/app.rs` (pass `pane_info.cwd` to terminal state)

---

**Bug 1 — Tab titles always show "shell" in daemon mode**

Root cause chain (verified by code trace):
1. `page.set_title(correct_title)` is called correctly in the reconnect loop.
2. `register_title_timer` fires after 100 ms.
3. `compute_display_title(state)` at `app.rs:3542` is called every tick:
   - `state.pty` → `None` (daemon panes have no local PTY) → skips `/proc/{pid}/cwd`
   - `state.terminal.title()` → `""` (fresh VT; no OSC 0/2 emitted by daemon shell yet)
   - Falls through to `return "shell".to_string()`
4. `page.set_title("shell")` permanently overwrites the correct title.

Fix — add `daemon_cwd: Option<PathBuf>` to `TerminalState` (`terminal.rs`):
- Initialise to `None` in `create_terminal()` (self-contained panes).
- Initialise to `Some(PathBuf::from(&pane_info.cwd))` in `create_terminal_for_pane()`.
- In `compute_display_title`, add a third fallback **after** the OSC title check and **before** the `"shell"` return:
  ```rust
  // Daemon fallback: use CWD basename from pane_info (no local /proc path available).
  if let Some(cwd) = &state.daemon_cwd {
      if let Some(name) = cwd.file_name() {
          return name.to_string_lossy().to_string();
      }
  }
  ```
- The static daemon_cwd is used until the running shell emits OSC 0/2 (via `subscribe_output` stream), at which point `state.terminal.title()` takes over naturally.

---

**Bug 2 — Large blank area above the shell prompt on reconnect**

Root cause chain (verified by code trace):
1. `get_screen` RPC returns all `screen.rows()` rows of the daemon VT (e.g. 46 rows).
2. For a fresh/idle shell, rows 0–43 are **blank**; only rows 44–45 have the prompt.
3. `create_terminal_for_pane` at `terminal.rs:1541` replaces ALL snapshot rows including blank leading rows:
   ```
   start_row = initial_rows(80) − snap_rows(46) + 1 = 35
   Rows 35-78 in the oversized VT → blank snapshot rows
   Rows 79-80                     → shell prompt
   ```
4. On first-draw resize from 80 → 46 rows (shrink removes top 34):
   - Rows 35–78 become rows 1–44 in the new VT → **44 blank rows visible**
   - Rows 79–80 become rows 45–46 → prompt at very bottom
5. User sees ~90 % blank gray area with the prompt crammed at the bottom.

Fix — strip leading empty snapshot lines before replay in `create_terminal_for_pane`:
```rust
// Discard blank leading rows — they produce a large empty region in the
// viewport after the first-draw resize.  Only lines from the first
// non-empty row onward are replayed, so the cursor lands at the same
// relative position within the visible content.
let first_content = snap.lines.iter()
    .position(|l| !l.is_empty())
    .unwrap_or(snap.lines.len().saturating_sub(1)); // keep at least cursor row
let effective_lines = &snap.lines[first_content..];
let effective_cursor_row = snap.cursor_row.saturating_sub(first_content);
// Use effective_lines.len() as snap_rows for start_row calculation.
```

Edge cases:
- All lines empty (brand-new pane): `first_content` saturates to `len-1`, keeps one line (the cursor row), positions it at bottom of initial VT. Fresh shell output will fill in normally.
- Cursor row < first_content (shouldn't happen in practice but if it does): `saturating_sub` keeps cursor at row 0 of the effective slice.

---

**AC:**
- [ ] Reopen GTK with 3 live daemon panes → tab titles show CWD basename (e.g. `forgetty`), not `"shell"`
- [ ] Tab title updates to new CWD when user runs `cd /tmp` in a daemon pane (OSC title path takes over from static CWD)
- [ ] Reopen GTK → visible blank area above prompt is ≤ 2 rows for a fresh shell (no large empty region)
- [ ] Reopen GTK → pane with a full-screen program running (e.g. `htop`) shows content immediately with no blank area
- [ ] Self-contained mode (no daemon): tab titles still update from `/proc/{pid}/cwd` as before — no regression
- [ ] `compute_display_title` returns `"shell"` only as a last resort when both OSC title and daemon_cwd are unavailable

### [x] T-057: Fix split-pane session save and restore in daemon mode
**Scope:** When a GTK tab contains split panes, closing and reopening restored each split pane as a separate tab instead of reconstructing the split layout. Root cause: `PaneTreeState::Leaf` had no `pane_id` field (non-first leaf pane IDs were discarded), and the daemon reconnect loop only read `TabState.pane_id` (flat, single-pane) ignoring `pane_tree` entirely.
**Fix:** Added `pane_id: Option<uuid::Uuid>` (`#[serde(default)]`) to `PaneTreeState::Leaf`; updated `snapshot_pane_tree` to embed each leaf's `daemon_pane_id`; added recursive `reconnect_pane_tree` that rebuilds `gtk::Paned` trees from the session file; replaced the flat `ordered` loop with `reconnect_pane_tree`-based per-tab reconstruction. Legacy T-055 session files handled via `legacy_pane_id` fallback.
**AC:** Split tab closes → session file has `Split` pane_tree with two Leaf children each having `pane_id`. Reopen → split restored as one tab with correct layout and CWDs. Single-pane tabs, self-contained mode, and old session files unaffected.

### [x] T-058: Cold-start session restore — layout, CWDs, and VT buffer persistence
**Scope:** After a reboot (daemon starts fresh, zero live panes), restore the full previous session. Phase 1: wire the ignored `Ok(_)` cold-start branch to load `default.json` and call `reconnect_pane_tree` with an empty pane_map — each leaf spawns a fresh daemon pane at its saved CWD via a new optional `cwd` param on the `new_tab` RPC. Phase 2: daemon writes per-pane VT screen snapshots to `~/.local/share/forgetty/sessions/snapshots/<uuid>.json` on SIGTERM; cold-start restore pre-seeds each new pane's VT with its snapshot via a new `preseed_snapshot` RPC before the GTK client subscribes, so the user sees their last screen content rather than a blank terminal.
**AC:** After reboot: same tabs, same split layout, shells start at saved CWDs. Screen content visible (not blank) after graceful daemon stop+start. Corrupt/missing snapshots open blank without crashing. Closing a tab deletes its snapshot file.

---

## Milestone 4: Daemon Owns Layout (Stateless Clients)

> **Why:** The daemon must be the single source of truth for all session state — tabs, splits, CWDs, ordering. GTK (and future Android/Windows/macOS clients) must be stateless renderers that connect to a daemon and build their UI from the daemon's layout. This ensures: one source of truth, no stale session files, multi-client support (two GTK windows see the same tabs), and the same architecture works for every platform.
>
> **Dependency graph:**
> ```
> T-059 ──→ T-060 ──→ T-061 (daemon saves session)
>               │
>               ├──→ T-062 (RPCs) ──→ T-064 (GTK reads layout from daemon)
>               │                              │
>               └──→ T-063 (events) ───────────┴──→ T-065 (GTK fully stateless)
> ```
>
> After T-065: GTK is a pure renderer. Android/Windows clients connect to their local daemon and get the exact same experience. Two GTK windows see each other's tab changes in real time.

### [ ] T-059: Introduce `SessionLayout` struct in `SessionManager`
**Scope:** Add a `SessionLayout` struct to `SessionManagerInner` that holds the live tab/split tree hierarchy: `workspaces → tabs → PaneTreeLayout` with `PaneId` references at leaves. This is a live, mutable data structure — not a serialization-only type. Populate it from existing `create_pane` (each new pane becomes a new tab with a single leaf in the default workspace). Update it on `close_pane` (remove the leaf, remove the tab if empty). Add `pub fn layout(&self) -> SessionLayout` to `SessionManager`. Add unit tests.

Key types (new file `crates/forgetty-session/src/layout.rs`):
- `SessionLayout { workspaces, active_workspace }`
- `SessionWorkspace { id, name, tabs, active_tab }`
- `SessionTab { id: Uuid, title: String, pane_tree: PaneTreeLayout }`
- Reuse existing `PaneTreeLayout` enum from `workspace.rs`

This task does NOT change any RPC handlers or GTK code — purely internal daemon state.

**Files:** `crates/forgetty-session/src/layout.rs` (new), `crates/forgetty-session/src/manager.rs`, `crates/forgetty-session/src/lib.rs`
**Pain point solved:** "SessionManager has no concept of tabs or splits — it only knows about a flat list of panes."
**AC:**
- [ ] `SessionLayout` struct exists with workspaces/tabs/pane_tree hierarchy
- [ ] `create_pane()` adds a new tab to the default workspace's layout
- [ ] `close_pane()` removes the pane from the layout tree (and its containing tab if now empty)
- [ ] `SessionManager::layout()` returns a consistent snapshot after any sequence of create/close
- [ ] Unit tests: create 3 panes → layout has 3 tabs; close middle → 2 tabs; create after close → 3 tabs in correct order
- [ ] No RPC, GTK, or serialization changes — purely internal

### [ ] T-060: Layout mutation methods on `SessionManager`
**Scope:** Add structured layout mutation methods that modify `SessionLayout` AND spawn/close panes atomically:
- `create_tab(workspace_idx, cwd, size) → (PaneId, Uuid/*tab_id*/)` — creates a pane AND inserts a new tab
- `split_pane(pane_id, direction, size, cwd) → PaneId` — creates a new pane AND replaces the leaf containing `pane_id` with a Split node
- `close_tab(tab_id)` — closes ALL panes in the tab's tree and removes the tab
- `move_tab(tab_id, new_index)` — reorders a tab within its workspace
- `set_active_tab(workspace_idx, tab_idx)` — updates active_tab

These replace the current flat `create_pane`/`close_pane` for layout-aware operations. Existing primitives remain as low-level internals.

**Files:** `crates/forgetty-session/src/manager.rs`, `crates/forgetty-session/src/layout.rs`
**Pain point solved:** "`split_pane` and `focus_tab` are stubs in the socket handlers because the daemon has no layout tree to mutate."
**Dependencies:** T-059
**AC:**
- [ ] `create_tab()` returns tab_id + pane_id; layout reflects new tab
- [ ] `split_pane(pane_id, "horizontal")` replaces leaf with Split node containing two leaves
- [ ] `close_tab(tab_id)` removes tab and kills all panes in its tree
- [ ] `move_tab(tab_id, 0)` moves tab to first position
- [ ] Unit tests for each mutation confirm layout + pane registry consistency
- [ ] Existing code compiles (create_pane/close_pane still public)

### [ ] T-061: Daemon saves `default.json` itself
**Scope:** The daemon becomes responsible for persisting `default.json`. Add `SessionManager::snapshot_to_workspace_state() → WorkspaceState` that converts the live `SessionLayout` into the existing serialization format (resolving live CWD from each pane's `/proc/{pid}/cwd`). Wire into:
1. Daemon SIGTERM/SIGINT shutdown (alongside existing `save_all_snapshots`)
2. Debounced auto-save: after any layout mutation, schedule a save within 5 seconds (coalesce rapid changes)
3. Periodic safety save every 60 seconds

GTK's `save_all_workspaces` remains for now (dual-write) — removed in T-065.

**Files:** `crates/forgetty-session/src/manager.rs`, `crates/forgetty-session/src/layout.rs`, `src/daemon.rs`
**Pain point solved:** "If GTK crashes or is SIGKILL'd, the session file is stale because only GTK writes it."
**Dependencies:** T-059, T-060
**AC:**
- [ ] SIGTERM to daemon → `default.json` written with current layout (workspaces, tabs, CWDs)
- [ ] Create 3 tabs via RPC → wait 6 seconds → `default.json` contains 3 tabs (debounced auto-save)
- [ ] `default.json` written by daemon is loadable by existing `load_session()` (same schema)
- [ ] GTK `save_all_workspaces` still works (not removed yet — dual-write for backward compat)

### [ ] T-062: `get_layout` and layout mutation RPCs
**Scope:** Wire layout methods to the JSON-RPC socket API:
- `get_layout` — returns full `SessionLayout` as JSON
- Rewrite `new_tab` handler to call `sm.create_tab()` (returns tab_id + pane_id)
- Rewrite `close_tab` handler to call `sm.close_tab(tab_id)` (backward compat: accept pane_id too)
- Implement `split_pane` handler (currently a stub) → `sm.split_pane()`
- Implement `focus_tab` handler (currently a stub) → `sm.set_active_tab()`
- Add `move_tab` RPC
- Add `DaemonClient` methods for all new RPCs

**Files:** `crates/forgetty-socket/src/handlers.rs`, `crates/forgetty-socket/src/protocol.rs`, `crates/forgetty-gtk/src/daemon_client.rs`
**Pain point solved:** "`split_pane` and `focus_tab` RPC handlers are stubs that do nothing."
**Dependencies:** T-060
**AC:**
- [ ] `socat` call to `get_layout` returns JSON with workspaces/tabs/pane_tree
- [ ] `new_tab` RPC returns `tab_id` AND `pane_id`; `get_layout` shows new tab
- [ ] `split_pane` RPC creates a split; `get_layout` shows the Split node
- [ ] `close_tab` with tab_id works; legacy `close_tab` with pane_id still works
- [ ] `focus_tab` updates `active_tab`; `move_tab` reorders tabs

### [ ] T-063: Layout change event broadcast
**Scope:** Add layout-specific events to `SessionEvent`:
- `TabCreated { workspace_idx, tab_id, pane_id }`
- `TabClosed { workspace_idx, tab_id }`
- `PaneSplit { tab_id, parent_pane_id, new_pane_id, direction }`
- `TabMoved { workspace_idx, tab_id, new_index }`
- `ActiveTabChanged { workspace_idx, tab_idx }`

Fire from layout mutation methods. Add `subscribe_layout` RPC that streams layout events to connected clients (same pattern as `subscribe_output`). Enables multi-client.

**Files:** `crates/forgetty-session/src/events.rs`, `crates/forgetty-session/src/manager.rs`, `crates/forgetty-socket/src/protocol.rs`, `crates/forgetty-socket/src/server.rs`
**Pain point solved:** "Two GTK windows can't see the same layout changes in real time."
**Dependencies:** T-060
**AC:**
- [ ] `subscribe_layout` RPC starts a streaming connection
- [ ] Create tab → subscriber receives `TabCreated` within 100ms
- [ ] Split pane → subscriber receives `PaneSplit`
- [ ] Close tab → subscriber receives `TabClosed`
- [ ] Existing `subscribe_output` unaffected

### [ ] T-064: GTK calls `get_layout` on connect
**Scope:** Replace the current daemon-mode reconnect path in `app.rs` — which loads `default.json` from disk and cross-references `list_tabs` — with a single `get_layout` RPC call. GTK calls `dc.get_layout()` on startup and builds its entire widget tree from the response. Remove `load_session()` from GTK daemon mode. Add `get_layout()` to `DaemonClient`.

Cold-start: daemon starts → loads `default.json` itself (T-061) → rebuilds layout → creates panes at saved CWDs. GTK connects → calls `get_layout` → gets the restored layout → builds widgets. Clean separation.

**Files:** `crates/forgetty-gtk/src/app.rs`, `crates/forgetty-gtk/src/daemon_client.rs`
**Pain point solved:** "GTK reads a stale session file that may disagree with the daemon's live state."
**Dependencies:** T-062
**AC:**
- [ ] GTK reconnect calls `get_layout` instead of `load_session` + `list_tabs`
- [ ] Daemon with 3 tabs (one split) → open GTK → 3 tabs with correct split layout
- [ ] Daemon with 0 panes (fresh start, no session file) → GTK creates one new tab via RPC
- [ ] `default.json` is NOT read by GTK in daemon mode
- [ ] Self-contained mode (no daemon) unaffected

### [ ] T-065: GTK tab/split actions send RPCs — fully stateless client
**Scope:** In daemon mode, change user-initiated layout actions to send RPCs instead of directly mutating the widget tree:
- `Ctrl+Shift+T` → `new_tab` RPC → daemon creates tab → GTK adds widget
- `Alt+Shift+=` → `split_pane` RPC → daemon splits → GTK creates `gtk::Paned`
- `Ctrl+Shift+W` → `close_tab`/`close_pane` RPC → daemon updates layout → GTK removes widget
- Tab drag-reorder → `move_tab` RPC

Subscribe to `subscribe_layout` so layout changes from other clients (second GTK window, socat, Android) are reflected. Remove `save_all_workspaces` from GTK daemon mode — daemon handles persistence (T-061).

**Files:** `crates/forgetty-gtk/src/app.rs`, `crates/forgetty-gtk/src/daemon_client.rs`
**Pain point solved:** "Layout mutations happen in GTK's widget tree and the daemon doesn't know about them — two GTK windows show different layouts."
**Dependencies:** T-063, T-064
**AC:**
- [ ] Ctrl+Shift+T → `new_tab` RPC → tab appears in GTK
- [ ] Alt+Shift+= → `split_pane` RPC → split appears
- [ ] Ctrl+Shift+W → close RPC → pane removed
- [ ] `save_all_workspaces` no longer called in daemon mode
- [ ] Two GTK windows: create tab in A → appears in B (via `subscribe_layout`)
- [ ] Self-contained mode unaffected

---

### [ ] T-054: Full interactive from Android — bidirectional PTY input *(ON HOLD — resume after Linux GTK client is complete)*
**Scope:** Android sends keystrokes, paste, and control sequences to daemon via iroh QUIC stream. Daemon routes to correct PTY master. Full interactive support: vim works (cursor movement, modes, save/quit), htop works, Claude Code interactive prompts answerable from phone. Key encoding: same logic as desktop key encoder (encode Android key events to correct PTY bytes). Control sequences: Ctrl+C, Ctrl+D, arrow keys, Escape, Tab, function keys.
**Pain point solved:** "I need to step away from my laptop but Claude Code needs a response."
**AC:** Type in Android → appears in desktop PTY within 100ms (LAN) / 300ms (relay). `vim` fully works from Android. `htop` fully works. Ctrl+C kills running process. Claude Code interactive prompt answerable from Android. Paste from Android clipboard → PTY. Input from either Android or GTK both reach same PTY.

---

### [ ] T-032: Smart copy — code block detection
**Scope:** Detect code blocks in Claude Code output (box-drawing borders). One-click copy of code content only, stripped of decorations.
**Pain point solved:** "Copy from Claude Code is broken"
**AC:** Hover over code block → copy icon. Click copies clean code.

### [ ] T-033: "Open in Editor" action per pane
**Scope:** Icon/button + right-click menu + `Ctrl+.` shortcut to open pane's CWD in configured editor (default: `code .`). Configurable in config.toml.
**Pain point:** "Need to open VS Code at the folder where Claude Code is running"
**AC:** `Ctrl+.` opens VS Code at pane's CWD. Configurable editor command.

### [ ] T-034: Socket API (live)
**Scope:** Start JSON-RPC socket server alongside app. Wire to actual app state. External tools can create tabs, send input, read output.
**AC:** `socat` can list tabs, create panes. Claude Code hooks integration works.

### [ ] T-035: Embedded markdown/image viewer
**Scope:** Split viewer pane alongside terminal. Auto-detect .md/.png/.svg files. Render markdown with syntax highlighting.
**AC:** Claude Code generates README.md → viewer auto-opens showing rendered markdown.

### [ ] T-036: Screenshot paste to Claude Code
**Scope:** Paste clipboard images into terminal. Convert to temp file path for Claude Code.
**Pain point solved:** "Screenshots to Claude Code broken on Linux"
**AC:** PrtSc → Ctrl+Shift+V → image available to Claude Code.

---

## Milestone 4: Platform Expansion

### [ ] T-037: Windows + WSL support
**Scope:** Platform shell for Windows (winit + DirectWrite). ConPTY for PTY. Test with WSL2. Note: daemon architecture from M3.5 means Windows shell is just another thin renderer connecting to forgetty-daemon via socket — same pattern as GTK.
**AC:** Forgetty runs on Windows 11. WSL2 shell works. Colors, input, splits all functional.

### [!] T-038: Android app — SUPERSEDED by M3.5
**Scope:** ~~Kotlin Compose shell + Rust core via JNI. wgpu Vulkan rendering. SSH-only initially.~~
**Status:** Superseded. The Android app (`~/Forge/forgetty-android`) is already ~80% built with complete mock UI (MachineListScreen, SessionListScreen, SessionDetailScreen, FileBrowserScreen, SystemStatusScreen, OnboardingScreen). It uses Kotlin Compose + Rust JNI — no wgpu, no SSH-only. The real work is plugging in totem-sync (iroh) and the daemon protocol. See M3.5 tasks T-052–T-054 and forgetty-android backlog (MA-005+).
**AC:** Replaced by M3.5 T-052–T-054 + forgetty-android MA-005 through MA-019.

### [!] T-039: Cross-device sync (premium SaaS) — SUPERSEDED by M3.5
**Scope:** ~~Cloud sync service (separate private repo). E2E encrypted workspace/settings sync.~~
**Status:** Superseded. Sync is built into forgetty-daemon via iroh (XD-002). No separate cloud service needed for core sync — iroh's DERP relay handles NAT traversal for free. Cloud premium tier (T-039-premium, future) adds: cloud scrollback backup, multi-machine (3+), team features. Core P2P sync is MIT free tier.

### [ ] T-040: Web version
**Scope:** WebGPU/WASM terminal in browser connected via WebSocket to sync server.
**AC:** Open browser, see synced workspaces. Connect to active session.

---

## Milestone 5: Advanced Features (Future)

### [ ] T-041: Monarch/Peasant multi-window coordination
**Scope:** Implement Windows Terminal's Monarch/Peasant pattern for multi-window management. The first Forgetty instance becomes the "monarch" — subsequent launches can either: (a) open a new tab in the existing monarch window, (b) open as a new independent "peasant" window coordinated by the monarch. The monarch tracks all windows, enables cross-window tab moves, and handles global hotkeys (like quake mode). If the monarch dies, a peasant promotes itself.
**Reference:** Windows Terminal's `src/cascadia/Remoting/` — Monarch.cpp, Peasant.cpp. Uses COM-based IPC on Windows; on Linux we'd use D-Bus or a Unix socket for coordination.
**Why later:** Requires multi-instance (T-018) to be solid first. More advanced than NON_UNIQUE flag — this is a full window coordination protocol.
**AC:** Launch `forgetty` → monarch. Launch `forgetty` again → opens tab in existing window (default) OR new peasant window (with `--new-window` flag). `forgetty -e htop` from another app → opens in monarch's new tab. Move tab between windows via drag or menu. Monarch crash → peasant auto-promotes.

### [ ] T-042: Forgetty MCP Server for AI agent pane orchestration
**Scope:** Create an MCP (Model Context Protocol) server that Claude Code auto-discovers when running inside Forgetty. The MCP server connects to Forgetty's Socket API (T-034) and exposes tools that let AI agents create, control, and monitor terminal panes.
**MCP Tools exposed:**
- `create_pane(label, command, cwd)` → opens a new split pane labeled "🤖 Managed by Claude" (or custom label). Returns pane_id.
- `send_input(pane_id, text)` → sends text/commands to a managed pane
- `read_output(pane_id, lines?)` → reads recent output from a pane (default last 50 lines)
- `close_pane(pane_id)` → closes a managed pane and kills its process
- `list_panes()` → lists all panes with labels, PIDs, CWDs, and managed-by status
- `create_tab(label, command, cwd)` → opens a new tab instead of split
- `create_workspace(name)` → creates a named workspace for the AI task
- `get_pane_status(pane_id)` → returns running/exited/error + exit code
**Auto-discovery:** Forgetty writes a `.mcp.json` entry or registers the MCP server in Claude Code's config when `TERM_PROGRAM=forgetty` is detected. The MCP server runs as a child process of Forgetty, communicating via the Socket API.
**Managed pane UX:** Managed panes have a small "🤖 Managed by Claude" label/badge in the pane header. The user can click into a managed pane and type (taking over control). The label changes to "🤖 Managed by Claude · User Override" when the user interacts.
**Pain point:** AI agents run blind in a single terminal. The user can't see what the agent is doing across multiple processes, can't watch builds/tests in real time, and can't intervene without interrupting the agent.
**AC:** Run Claude Code in Forgetty. Claude calls `create_pane("Build", "npm run build", ".")` → pane appears with label. User sees build output in real time. Claude calls `read_output(pane_id)` to check status. Claude calls `close_pane()` when done. User can click managed pane and type at any time.

### [ ] T-043: AI agent workflow templates
**Scope:** Pre-built workflow templates that Claude Code (or other AI agents) can invoke via the MCP server to set up common development environments. Templates define multi-pane layouts with labeled panes for different tasks.
**Example templates:**
- "Full Stack Dev" → 3 panes: Editor (main), Server (managed: `npm run dev`), Tests (managed: `npm test --watch`)
- "Claude Code Deep Work" → 2 panes: Claude session (main) + secondary terminal for running commands
- "Multi-Agent" → 4 panes: Orchestrator + 3 worker agents, each in its own pane
**AC:** Claude calls `use_template("full-stack", cwd)` → Forgetty creates the layout with labeled panes. Templates configurable in `~/.config/forgetty/templates/`.

### [ ] T-044: Custom shader/visual effects support
**Scope:** Allow users to apply custom visual effects to the terminal rendering. On GTK/Cairo this could be implemented as post-processing filters (blur, CRT scanlines, transparency overlays). Configurable in config.toml. Reference: Windows Terminal's custom HLSL shader support for retro CRT effects.
**AC:** Config option `shader = "retro"` applies CRT scanline effect. `shader = "none"` disables. At least 2-3 built-in effects. Custom effect API documented for advanced users.

### [ ] T-043: Settings fragments / auto-profile discovery
**Scope:** Support "settings fragments" — TOML files that third-party tools or scripts can drop into `~/.config/forgetty/profiles.d/` to auto-register shell profiles. When Forgetty starts, it scans this directory and merges discovered profiles into the profile dropdown. Use case: installing a WSL distro or SSH config could auto-create a Forgetty profile.
**Reference:** Windows Terminal's settings fragments system where WSL distros and VS Code auto-register profiles.
**AC:** Drop a `.toml` file in `~/.config/forgetty/profiles.d/` → profile appears in dropdown on next launch. Remove the file → profile disappears. Fragments don't override user config, they supplement it.

### [ ] T-045: Inline image rendering (Kitty Graphics Protocol + Sixel)
**Scope:** Parse Kitty Graphics Protocol (`ESC_G<payload>ESC\`) and Sixel (`ESC P...q...ESC\`) escape sequences from PTY output independently of libghostty-vt. Intercept image sequences before feeding remaining data to the VT parser. Decode images (base64 PNG/RGB for Kitty, palette bitmap for Sixel) and store in an image cache keyed by cell position. In the GTK4 Cairo draw callback, render cached images as `cairo::ImageSurface` overlays on top of the text grid. Support image placement (position, size), scrolling with text, and clearing on terminal reset.
**No dependency on libghostty-vt for this** — images are an independent overlay layer we manage ourselves.
**AC:** `chafa image.png` displays image inline. `timg photo.jpg` works. `img2sixel image.png` works. Images scroll with text. Images cleared on reset. Multiple images can be displayed simultaneously. Images don't interfere with text rendering or selection.

### [ ] T-046: Optional GPU-accelerated rendering backend
**Scope:** For high-performance scenarios (very large terminals, rapid output, image-heavy content), offer an optional GPU rendering path using GTK4's `GLArea` widget with OpenGL. Keep Cairo as default (simpler, works everywhere). GPU backend uses a glyph atlas + instanced rendering (similar to our original wgpu proof-of-concept code that still exists in the repo). Configurable in config.toml: `renderer = "cairo"` (default) or `renderer = "opengl"`.
**Prerequisites:** Only needed if Cairo performance becomes a measurable bottleneck. May never be needed.
**AC:** `renderer = "opengl"` in config uses GPU rendering. `renderer = "cairo"` (default) uses current software rendering. Both produce identical visual output. GPU path handles 200+ column, 100+ row terminals at 60fps.

### [ ] T-047: Evaluate libghostty-rs as FFI replacement
**Scope:** Evaluate `libghostty-rs` (github.com/Uzaaft/libghostty-rs) — safe Rust wrapper around libghostty-vt's C API, maintained by Ghostty maintainers and endorsed by Mitchell Hashimoto. Compare against our hand-written `forgetty-vt/src/ffi.rs` (~1,027 lines of unsafe FFI). If stable and covers our needs, migrate to their crate. Eliminates all unsafe FFI, gives closure-based callbacks (no pointer segfaults), auto-builds libghostty-vt from source.
**Prerequisites:** Wait for libghostty-rs to reach ~0.2+ (currently 0.1.1, 8 days old).
**AC:** Replace `forgetty-vt/src/ffi.rs` with `libghostty-rs` dependency. All functionality works. No unsafe code in VT layer. Build auto-fetches libghostty-vt. All tests pass.

---

## Pre-Launch (after M3, before public release)

> These M2 tasks were deferred to focus on M3 AI-native features first. Complete before launch.

### [ ] T-026: Test on fresh Ubuntu VM
**Scope:** Install Forgetty on a fresh Ubuntu 24.04 VM (no development tools installed). Test the .deb package, app launcher, all M1 features, different shells (bash, zsh, fish), SSH, tmux, different display scales (100%, 150%, 200%).
**AC:** Clean Ubuntu 24.04 VM. Install .deb. All M1 features work. Different shells work. SSH works. HiDPI works. Wayland and X11 both work.

### [ ] T-027: Performance benchmarking vs Ghostty
**Scope:** Benchmark Forgetty vs Ghostty on same machine: startup time, `cat large_file` throughput, input latency (keystroke to pixel), idle CPU, memory usage at rest and under load. Document results. Optimize any areas where we're >20% slower.
**AC:** Benchmark results documented in `docs/benchmarks.md`. Startup < 500ms. Throughput within 80% of Ghostty. Idle CPU < 1%. Memory < 100MB at rest.

### [ ] T-028: Memory leak stress test (24h)
**Scope:** Run 3+ Claude Code panes simultaneously for 24+ hours. Monitor RSS every hour. Graph memory over time. Identify and fix any leaks.
**Pain point:** Ghostty had a 209GB memory leak from running Claude Code for weeks. Our users do exactly this.
**AC:** 3 Claude Code panes running for 24h. RSS stays under 500MB. No degradation. No hangs.

---

## Task Status Legend

- `[ ]` — Not started
- `[~]` — In progress (see SESSION_STATE.md)
- `[x]` — Completed and QA passed
- `[!]` — Blocked (see notes)
