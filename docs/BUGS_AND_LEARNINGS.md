# Forgetty — Bugs & Engineering Learnings

Cross-platform knowledge base. Every entry here is a real bug we debugged. If you're implementing Forgetty on a new platform (Windows, Android, Web) or touching PTY/input/signal code, read this first.

---

## BUG-001: Ctrl+C does not interrupt foreground processes

**Platforms affected:** Linux (GTK4), likely all platforms  
**Severity:** Critical — basic terminal interaction broken  
**Status:** Fixed (commit `c39e6eb`)

### Symptoms

- Pressing Ctrl+C while `sleep 1000` is running shows `^C` echoed but the process keeps running
- Only Ctrl+X (or other keys) would actually interrupt the process
- Affects any process that puts the PTY in raw mode: Node.js, pm2, any readline-based app

### Root cause

**Two separate failures, both needed fixing:**

**Failure 1: Writing `0x03` alone is not enough**

Writing the INTR byte (`0x03`) to the PTY master only works when the line discipline has `ISIG` enabled (cooked mode). When a child process (Node.js, pm2, etc.) puts the slave PTY into raw mode (`ISIG` disabled), the `0x03` byte just gets passed through as data and echoed as `^C`. SIGINT is never sent.

Fix: after writing `0x03`, also call `kill(-pgid, SIGINT)` directly.

**Failure 2: Getting the foreground pgrp via `/proc/{pid}/fd/0` silently fails**

The first implementation tried to get the foreground process group by opening the slave PTY via the shell's proc fd symlink and calling `tcgetpgrp` on it:

```rust
// BROKEN
let path = format!("/proc/{shell_pid}/fd/0");
let f = std::fs::File::open(&path)?;  // missing O_NOCTTY → can steal controlling terminal
let pgid = libc::tcgetpgrp(f.as_raw_fd());  // fails silently in practice
```

Problems:
- `std::fs::File::open()` on a TTY device without `O_NOCTTY` can steal forgetty's controlling terminal
- Even with `O_NOCTTY` added, `tcgetpgrp` on a freshly-opened slave fd returned unexpected results on Ubuntu in practice

### Fix

Use `portable-pty`'s `MasterPty::process_group_leader()` which calls `tcgetpgrp` on the **master fd we already hold**. The master fd is always correct, always open, and doesn't require any extra files or O_NOCTTY handling.

```rust
// In PtyProcess (forgetty-pty):
pub fn foreground_pgrp(&self) -> Option<i32> {
    self.master.process_group_leader()
}

// In key handler (terminal.rs):
s.pty.write(&[0x03]).ok();
if let Some(pgid) = s.pty.foreground_pgrp() {
    let my_pid = std::process::id() as libc::pid_t;
    if pgid > 0 && pgid != my_pid {
        libc::kill(-(pgid as libc::c_int), libc::SIGINT);
    }
}
```

### Cross-platform notes

- **Windows (ConPTY):** ConPTY handles Ctrl+C natively via `GenerateConsoleCtrlEvent(CTRL_C_EVENT, pgid)`. The same two-path pattern applies: send the byte AND the signal explicitly.
- **Android:** Same issue — PTY raw mode will suppress line discipline SIGINT. Must use the explicit kill path.
- **Web (WebPTY):** In a browser you can't send signals directly; the PTY must be managed server-side. The server process should expose a signal endpoint that does the same `kill(-pgid, SIGINT)`.

---

## BUG-002: GTK accelerator steals Ctrl+C from terminal

**Platforms affected:** Linux (GTK4 only)  
**Severity:** High — copy shortcut breaks raw-mode apps  
**Status:** Fixed

### Symptoms

Registering `<Control>c` as a GTK accel for `win.copy` causes all Ctrl+C presses to be consumed at the window level, before `EventControllerKey` fires on the terminal widget. Raw-mode apps (vim, etc.) never see Ctrl+C.

### Root cause

GTK processes accelerators at the window level before propagating events to focused widgets. A registered `<Control>c` accel intercepts every Ctrl+C regardless of context.

### Fix

Remove `<Control>c` from the accel registration entirely. In the key handler, check for selection and dispatch copy manually:

```rust
// In key handler — Ctrl+C with selection = copy; without = INTR
if ctrl_only && (keyval == gdk::Key::c || keyval == gdk::Key::C) {
    if s.selection.is_some() {
        drop(s);
        da_for_key.activate_action("win.copy", None).ok();
    } else {
        s.pty.write(&[0x03]).ok();
        send_sigint_to_fg_pgrp(&s.pty);
    }
    return glib::Propagation::Stop;
}
```

### Cross-platform notes

- **Windows (WinUI):** Same pattern applies — keyboard accelerators registered in the UI framework will swallow keystrokes before they reach the terminal widget. Handle Ctrl+C in the key handler, not as a global shortcut.
- **Android:** Not applicable (touch-first, no hardware keyboard accelerators for copy).

---

## BUG-003: BEL flash after Ctrl+C (spurious bell)

**Platforms affected:** Linux, likely all platforms with zsh  
**Severity:** Medium — visual noise, misleading  
**Status:** Fixed

### Symptoms

After pressing Ctrl+C, the terminal briefly flashes (visual bell). Happens even when no bell should ring.

### Root cause

zsh sends `\x07` (BEL) as part of readline cleanup when it receives SIGINT. The BEL arrives on the PTY output stream ~10–50ms after the signal, triggering the terminal's visual bell handler.

### Fix

Set a suppression window in `TerminalState` after sending the signal:

```rust
s.suppress_bell_until = Some(Instant::now() + Duration::from_millis(300));
```

In the PTY output reader, skip BEL if inside the suppression window:

```rust
if s.suppress_bell_until.map_or(false, |t| now < t) {
    s.suppress_bell_until = None;
    continue; // skip this BEL
}
```

300ms is enough to absorb zsh's response without suppressing legitimate bells.

### Cross-platform notes

Applies to any platform where zsh (or bash with readline) is the default shell. The suppression logic lives in `forgetty-core`/the shared PTY reader, so it carries over automatically.

---

## BUG-004: Ghostty encoder returns None for Ctrl+C → visual flash

**Platforms affected:** Linux (GTK4), likely all platforms using the ghostty encoder  
**Severity:** Medium — causes incorrect fallback behavior  
**Status:** Fixed

### Symptoms

When the ghostty key encoder returns `None` for a keypress (no bytes to write), the key handler returns `Propagation::Proceed`, which lets GTK process the event. GTK's default handler for unhandled keys triggers a terminal bell → visual flash.

This specifically happened for Ctrl+C in certain keyboard protocol modes where the ghostty encoder decides not to encode the key.

### Fix

Handle Ctrl+C before reaching the encoder. Write `0x03` directly and return `Propagation::Stop` unconditionally:

```rust
// Bypass encoder entirely for Ctrl+C — always write 0x03
if ctrl_only && (keyval == Key::c || keyval == Key::C) {
    // ... handle copy or INTR ...
    return Propagation::Stop;  // never let GTK see it
}
```

### Cross-platform notes

Any platform using the ghostty encoder APIs should treat Ctrl+C as a special case bypassing the encoder. The encoder's behavior for INTR characters is protocol-dependent and not reliable for this use case.

---

## PATTERN: Two-phase Ctrl+C for reliable signal delivery

The definitive pattern for Ctrl+C across all forgetty platforms:

```
1. Write INTR byte (0x03) to PTY master
   → Works for cooked-mode processes (shell prompt, cat, etc.)
   
2. kill(-foreground_pgrp, SIGINT) via the master fd
   → Works for raw-mode processes (Node.js, pm2, vim in certain modes)
   → Use master fd's tcgetpgrp, NOT /proc/{pid}/fd/0
   
3. Suppress BEL for 300ms
   → Prevents zsh readline cleanup bell from triggering visual flash
```

All three steps must happen together. Skipping any one breaks a class of processes.

---

## BUG-002: Split panes restored as flat tabs after daemon restart

**Platforms affected:** Linux (GTK4)  
**Severity:** High — splits are destroyed on every daemon restart  
**Status:** Fixed (cold-start restore in `src/daemon.rs`)

### Symptoms

- Close Forgetty window (daemon restarts on next open)
- Any tab that had panes split horizontally or vertically shows each pane as a separate top-level tab instead

### Root cause

Cold-start restore in `daemon.rs` called `collect_leaf_cwds()` which walked the saved `PaneTreeState` and returned only the leaf CWDs, discarding all `Split` nodes. It then called `create_tab()` once per leaf, producing N flat tabs.

The `PaneTreeState` serialized to JSON correctly (direction, ratio, first/second preserved), but the restore logic never consumed the split structure.

### Fix

Replaced `collect_leaf_cwds` with two helpers:

1. `first_leaf_cwd(tree)` — returns the CWD of the leftmost leaf to seed `create_tab()` for the tab root.
2. `restore_subtree(sm, anchor_id, tree, size)` — recursively walks the saved tree; for each `Split` node calls `split_pane_with_ratio(anchor_id, direction, ratio, ...)` to create the second child pane, then recurses into both halves.

Also added `SessionManager::split_pane_with_ratio()` (and the underlying `replace_leaf_with_ratio()`) so that saved split ratios are preserved instead of always defaulting to 0.5.

### Key insight

After `create_tab()` creates `root_pane_id`, it is the anchor for the first leaf. Calling `split_pane_with_ratio(root_pane_id, direction, ratio, ...)` inserts `Split { first: Leaf(root_pane_id), second: Leaf(new_id) }` in the tree. Recursing into the first subtree can then further split `root_pane_id` inward — the split_pane lookup finds the leaf by ID regardless of tree depth, so nested splits compose correctly.

---

## BUG-005: Daemon PTY stays at 24×80 after first draw (T-076)

**Platforms affected:** Linux (GTK4, daemon mode)
**Severity:** Critical — every full-screen app and prompt using COLUMNS breaks on first launch
**Status:** Fixed (T-076)

### Symptoms

- `tput cols` returns 80 in a freshly opened daemon-mode terminal, regardless of window width
- zsh-autosuggestions render at the wrong column (far right of terminal)
- nano, htop, and other full-screen apps render with display corruption

### Root cause

`draw_terminal` runs an initial cell-measurement block on the first frame (when `cell_measured` flips to `true`). This block resizes the local VT and calls `pty.resize()` for standalone mode — but never called `dc.resize_pane()` for daemon mode. The `connect_resize` callback skips until `cell_measured = true`, so there is no resize event triggered after the first draw. The daemon PTY stayed at its creation-time default of 24×80.

### Fix

Added `dc.resize_pane(pane_id, rows, cols)` immediately after the `pty.resize()` block inside the `if !*cell_measured.borrow()` guard in `draw_terminal` (`crates/forgetty-gtk/src/terminal.rs`).

### Key insight

Daemon mode has TWO resize paths: the `connect_resize` callback (correct, handles subsequent resizes) and the first-draw measurement block (was missing the daemon call). Always keep both paths in sync when adding new resize destinations.

---

## BUG-006: Per-cell Cairo fill() creates visible grid lines in backgrounds (T-076)

**Platforms affected:** Linux (GTK4)
**Severity:** Medium — inverse video, syntax highlighting, and 256-color blocks show visible 1px seams between cells
**Status:** Fixed (T-076)

### Symptoms

- `printf '\e[7m   INVERSE TEXT   \e[0m\n'` shows each character cell as a slightly different shade — no uniform highlight
- 256-color cube (`\e[48;5;Nm`) shows visible grid lines between same-colored adjacent cells
- Any solid-background region (e.g. ncurses header bars) looks "textured" instead of solid

### Root cause

The cell drawing loop called `ctx.fill()` once per cell. Cairo composites each rectangle independently; at fractional cell widths the sub-pixel edges receive different anti-aliasing coverage, leaving a faint 1px seam between adjacent cells. The same artifact had already been documented and fixed for the selection overlay (which uses a single path + single `fill()`), but the cell background drawing still used per-cell fills.

### Fix

Split cell rendering into two passes in `draw_terminal`:

1. **Background pass:** iterate cells with run-length encoding — group consecutive cells sharing the same `Color::Rgb(r, g, b)` into a single wide rectangle, drawn with ONE `fill()` call. Same-color runs have no interior edges for Cairo to anti-alias.
2. **Foreground pass:** draw text, underline, strikethrough per cell as before.

### Key insight

A single `ctx.fill()` over a merged rectangle makes all interior edges invisible to Cairo's anti-aliasing. This is the same principle already documented for the selection overlay. Any solid-fill region spanning multiple cells must be drawn as ONE path + ONE fill to be seam-free.

---

## BUG-008: adw::TabView right-click menu never fires (libadwaita 1.5, Wayland)

**Platforms affected:** Linux (GTK4 + libadwaita 1.5, Wayland compositor)
**Severity:** High — tab context menu feature completely non-functional
**Status:** Fixed (T-M1-extra-009 follow-up)

### Symptoms

- Right-clicking a tab shows nothing
- `adw::TabView::connect_setup_menu` handler never fires
- Capture-phase `GestureClick(button=3)` on `adw::TabBar` fires (coordinates logged), but bubble-phase and `setup-menu` do not fire

### Root cause (triple failure)

**Failure 1: libadwaita 1.5 claims button-3 without emitting `setup-menu`**

`adw::TabView::setup-menu` is the official signal for tab right-click. In libadwaita 1.5 with no `menu-model` set on the `TabView`, the internal `AdwTabButton` still claims button-3 (to suppress the default GTK context menu), but does NOT call `adw_tab_view_setup_menu()`. The signal never fires. This silently breaks any implementation that relies on `setup-menu`.

**Failure 2: `adw::TabBar::pick()` returns None**

`adw::TabBar` wraps its tab buttons in a `GtkScrolledWindow` internally. `gtk4::Widget::pick()` does not traverse into `GtkScrolledWindow` contents (the overflow clip stops traversal). Any code that uses `tab_bar.pick(x, y)` to find the clicked tab button will always get `None`.

**Failure 3: `AdwTabButton` type name mismatch**

Walking the widget tree to find `AdwTabButton` by type name via `widget.type_().name() == "AdwTabButton"` returns zero results on libadwaita 1.5. The actual internal type used is different in this version.

### Fix

**Bypass libadwaita's gesture entirely**:

Attach a `GestureClick(button=3, phase=Capture, Claimed)` on `adw::TabBar`. Claiming in Capture phase prevents libadwaita's `AdwTabButton` gesture from ever firing, so the double-claiming conflict never occurs.

For finding the clicked tab: walk the tab bar widget tree collecting children by bounds (`compute_bounds()`), not by type name. Fall back to `tab_view.selected_page()` if the walk finds nothing (click on empty space, or single-tab hidden bar).

```rust
// In the Capture gesture pressed handler:
gesture.set_state(gtk4::EventSequenceState::Claimed);
let page = tab_bar_find_page_at(&tab_bar, x, y)
    .or_else(|| tab_view.selected_page());
```

`tab_bar_find_page_at()` uses `compute_bounds()` per child widget relative to the tab bar — this works regardless of `GtkScrolledWindow` clipping.

### Cross-platform notes

- **Windows (WinUI/WinRT):** The TabView control also doesn't fire context-menu events by default. Wire `RightTapped` on individual tab items, not on the tab bar parent.
- **Android:** libadwaita is not used. Long-press replaces right-click for context menus on tab-strip items.

---

## BUG-009: GTK4 Popover autohide fails on Wayland when shown inside button-press handler

**Platforms affected:** Linux (GTK4, Wayland)
**Severity:** Medium — click-outside doesn't dismiss popovers
**Status:** Fixed (T-M1-extra-009 follow-up)

### Symptoms

- `gtk4::Popover` with `autohide=true` (the default) does not dismiss when the user clicks outside it on Wayland
- On X11, the same code works correctly

### Root cause

On Wayland, popup autohide is implemented via a compositor input grab (`xdg_popup`). The compositor sets up the grab when the popup surface is created in response to a button event with a valid serial. When `popover.popup()` is called **inside a button-press handler** (while the button is still physically held), the button has not been released yet — the compositor rejects the grab setup. The popup appears but has no dismiss mechanism.

This specifically affects the Forgetty tab context menu, which is shown in a `GestureClick::connect_pressed` callback (button still held).

### Fix

Use `EventControllerFocus::leave` on the popover instead of relying on autohide:

```rust
// WRONG: connect immediately — fires during same event cycle as button-press
let fc = gtk4::EventControllerFocus::new();
fc.connect_leave(move |_| popover.popdown());
popover.add_controller(fc);

// CORRECT: defer to next idle cycle so the button-press event fully completes
let pop = popover.clone();
glib::idle_add_local_once(move || {
    if !pop.is_visible() { return; }
    let p = pop.clone();
    let fc = gtk4::EventControllerFocus::new();
    fc.connect_leave(move |_| p.popdown());
    pop.add_controller(fc);
});
```

The `idle_add_local_once` deferral is critical. If the `EventControllerFocus` is wired before `popup()` returns, GTK fires `leave` during the same button-press event cycle (the popover briefly loses and regains focus as GTK initialises it), immediately closing the popup. Deferring to the next idle cycle lets the focus settle first.

Also set `popover.set_autohide(true)` for Escape-key dismiss (which does work on Wayland).

### Cross-platform notes

- **X11 (Linux):** `autohide=true` works correctly because X11 input grabs are immediate.
- **Windows (WinUI):** `Flyout` / `MenuFlyout` have their own dismiss logic; `LightDismissOverlayMode` handles this natively.
- **macOS:** NSPopover `.transient` behavior handles click-outside.

The `idle_add_local_once` pattern applies anywhere a GTK4 Wayland popup is shown from within an event handler (gesture pressed, key pressed, etc.).

---

## PATTERN: adw::TabView tear-off (drag tab to new window)

libadwaita handles tab tear-off via `AdwTabView::create-window`. The handler must return `Some(new_tab_view)` — returning `None` causes a `CRITICAL` warning and cancels the drag.

```rust
tab_view.connect_create_window(move |_source_tv| {
    Some(open_detached_tab_window(&app))
});
```

Key facts:
- `close-page` does NOT fire on the source when a tab is transferred via `create-window`. PTY processes are safe.
- The `AdwTabPage` child widget (and all descendants including `DrawingArea`, PTY polling timers, Rc'd `TerminalState`) travels with the page unchanged — the terminal keeps working in the new window.
- Wire `create-window` on every `TabView` in the app (initial workspace, each new workspace, and recursively on the detached window's `TabView`).
- The detached window should itself wire `create-window` for further tears.

The minimal receiver window needs: `adw::TabBar` + `adw::TabView` + `close-page` handler (close window when last tab is closed). Full keybindings, workspace sidebar, etc. are optional.

---

## BUG-007: Theme ANSI palette ignored — libghostty-vt uses xterm defaults (T-076)

**Platforms affected:** Linux (GTK4)
**Severity:** Medium — themed terminals don't use the theme's custom ANSI colors
**Status:** Fixed (T-076)

### Symptoms

- `printf '\e[41mRED\e[0m\n'` shows xterm's default dark red (`#800000`) instead of the theme's red
- Applications using ANSI palette colors look inconsistent with the theme

### Root cause

`Terminal::new()` passed a null config pointer to `ghostty_terminal_new`, so libghostty-vt used its built-in xterm palette for color resolution. The theme's `ansi_colors[0..15]` were stored in the config but never passed to the VT layer. `sync_screen` queried the pre-resolved `FG_COLOR`/`BG_COLOR` FFI values which had already been resolved using libghostty-vt's internal palette.

### Fix

1. Added `ansi_palette: [forgetty_core::Rgba; 16]` field to `Terminal`.
2. Changed `Terminal::new()` to accept the palette; GTK call sites pass `config.theme.ansi_colors`.
3. Added `set_ansi_palette()` for runtime theme changes.
4. Added `resolve_style_color()` helper that uses `GhosttyStyle.fg_color`/`bg_color` union fields (which carry the unresolved `Tag::Palette(index)` or `Tag::Rgb(r,g,b)`) instead of the pre-resolved FFI queries. Indices 0–15 use the theme palette; 16–231 use the 6×6×6 cube formula; 232–255 use the grayscale ramp.

### Key insight

`GhosttyStyle` exposes unresolved color tags (`None`, `Palette(u8)`, `Rgb`). Using `style.fg_color`/`style.bg_color` instead of the pre-resolved `FG_COLOR`/`BG_COLOR` FFI queries lets Forgetty own the palette-to-RGB mapping and apply the theme's colors. The pre-resolved queries are a convenience but bypass theme customization.
