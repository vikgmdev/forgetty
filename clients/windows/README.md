# forgetty-win (V3 Windows client)

Rust native Windows client on GPUI + libghostty-vt + a local ConPTY shell.
Separate Cargo workspace; the root workspace is untouched.

## Pins

- GPUI: `zed-industries/zed` tag `v1.24.4` (newest stable release tag, no `-pre`),
  rev `aed37547d8543000494fb69101c05daec5891068`. Crates used: `gpui`, `gpui_platform`
  (v1.24 split the platform backends into `gpui_windows` etc).
- Zig 0.15.2 builds libghostty-vt. `forgetty-vt/build.rs` passes
  `-Dtarget=x86_64-windows-gnu` when the Rust target OS is Windows. The DLL only
  exports a C ABI, so it links into the MSVC Rust binary through the import lib
  `ghostty-vt.lib`. `ghostty-vt.dll` is copied next to the exe.
- Zed patches some crates.io crates in its own root manifest. Patches do not apply to
  dependents, so `async-task` and `calloop` are repeated in `Cargo.toml` `[patch.crates-io]`.

## Build and run

```
cd clients/windows
cargo build --release
target\release\forgetty-win.exe            # interactive window, cmd.exe (or $FORGETTY_SHELL)
target\release\forgetty-win.exe --smoke    # headless measurement, JSON on stdout
```

CI: `.github/workflows/windows-client.yml` (branch `v3/win-spike`).

## Design (W1)

- `src/source.rs`: `PaneSource` trait (events channel with `Output`/`Closed`, `write_input`,
  `resize`, `shutdown`) and `LocalConPty` (portable-pty). The view only sees the trait, so the
  remote `forgetty-client` pane plugs in without view changes. Output channel is bounded
  (backpressure), writes go through a writer thread (never block the UI).
- `src/view.rs`: `TerminalView` owns `forgetty_vt::Terminal`. Output is fed in <=256 KB batches
  with a yield between batches. One resize per frame (grid follows the window, VT + PTY).
  Shaped lines are cached per row by content hash (rows the VT did not touch are not even hashed).
  Selection (drag, double-click word, triple-click line), copy on select, Ctrl+Shift+C/V,
  bracketed paste with pastejacking defense (`forgetty_vt::paste`: ESC and C1 CSI stripped in bracketed mode, C0/C1 stripped when unbracketed, newline or stripped bytes show an Enter/Esc confirm overlay with a preview), wheel and Shift+PageUp/PageDown scrolling (pixel deltas accumulate, view snaps
  to rows, no re-parse), DECSET 1004 focus reports, window title from OSC.
- `src/element.rs`: custom GPUI `Element`: background quads, selection, cursor (block, bar,
  underline, hollow, DECSCUSR blink), shaped text per row, IME preedit at the cursor.
- `src/encoder.rs`: libghostty-vt key encoder (DECCKM, Kitty keyboard protocol, release events)
  and mouse encoder (1000/1002/1003, SGR/UTF-8/URxvt) through `forgetty_vt::ffi`. Used directly:
  no fallback to hand-written key tables.
- `src/config.rs`: `%APPDATA%\forgetty\config.toml` (`FORGETTY_CONFIG` overrides the path).
  Defaults: Cascadia Mono 11pt (falls back to Cascadia Code, Consolas, Courier New if not
  installed), Campbell theme, 10000 lines scrollback.
- `forgetty-vt` additions: `CellAttributes::{wide, spacer}`, `Terminal::with_scrollback`.
  NOTE: libghostty-vt counts `max_scrollback` in BYTES. `Terminal::new` passes 10_000, which keeps
  only about 1000 rows. The GTK client has the same limit.

## IME (out of scope for the spike)

GPUI exposes the Windows IME (TSF/IMM) path through `Window::handle_input` with an
`ElementInputHandler` over an `EntityInputHandler` (`replace_text_in_range`,
`replace_and_mark_text_in_range`, `marked_text_range`, `bounds_for_range`, ...). See
`crates/gpui/examples/input.rs` in the Zed repo. The terminal element would register that
handler in `paint` and report the cursor cell via `bounds_for_range` so the candidate window
anchors at the cursor. Marked text can be drawn with `UnderlineStyle`.

## App shell (W2)

Layers, top to bottom:

- `src/shell.rs`, `src/chrome.rs`: `AppShell` root view. Custom titlebar with tabs (client-side
  decorations, `window_control_area` caption buttons so Windows 11 snap layouts work), sidebar
  (machines, workspaces, tabs), split panes with draggable dividers, command palette. It talks only
  to a `SessionBackend` and learns state only from `BackendEvent`s.
- `shell-core/` (crate `forgetty-shell-core`, no UI code, unit-tested on Linux):
  `layout.rs` (workspaces, tabs, pane tree with ratios; serde shape equals the server's `SessionLayout`),
  `backend.rs` (`SessionBackend` trait, `BackendEvent`, requests/replies), `local.rs` (`LocalBackend`
  on ConPTY), `keys.rs` (actions, chords, configurable keymap), `fuzzy.rs`, `osc.rs`.
- `src/view.rs`, `src/element.rs`: one `TerminalView` per pane (libghostty-vt, damage-only row shaping).

`SessionBackend` mirrors PROTOCOL_V2: `get_layout`, `new_tab`, `close_tab`, `focus_tab`, `move_tab`,
`create_workspace`, `set_active_workspace`, `rename_workspace`, `split_pane`, `close_pane`,
`set_active_pane`, `resize_pane`, `update_split_ratios`, `get_pane_info`, `attach_pane`, plus
`subscribe()` for `LayoutChanged`, `PaneClosed`, `TitleChanged`, `CwdChanged`, `PaneResized`,
`StatusChanged`. Every method returns a future; `LocalBackend` returns ready ones. A `RemoteBackend`
on `forgetty-client` implements the same trait; the shell does not change.

Notes and decisions:

- Split direction is the server's: `horizontal` = side by side (Alt+Shift+Plus), `vertical` = stacked
  (Alt+Shift+Minus). `update_split_ratios` is keyed by the leftmost leaf of the split's first child.
- The protocol has no `rename_tab`: tab rename is a client-side title override (not persisted).
  Pane zoom and the focused pane per tab are client UI state (focus is also sent via `set_active_pane`).
- `StatusChanged.status` strings are not specified yet (P4). The client accepts `working`, `waiting`,
  `done`, `failed`. `LocalBackend` derives them from OSC 133 marks (C = working, D;0 = done, D;n = failed,
  A = waiting) and takes title from OSC 0/2, cwd from OSC 7 and OSC 9;9.
- Security: a cwd from escape codes is a display hint only. New tabs and splits start in the parent
  pane's spawn directory (else `default_cwd`, else the user profile). UNC, device (`\\?\`, `\\.\`)
  paths and non-local `file://` hosts are dropped, so program output cannot make the client touch a
  network share. The OSC scanner decodes on bytes and runs under `catch_unwind`.
- Mica is used on Windows 11 22H2+ (`backdrop = "mica"`); older builds and `backdrop = "none"` get a solid
  themed background. Panes always paint opaque.
- Scrollback is 10 MiB of libghostty bytes per pane by default (`Terminal::new` too);
  `scrollback_bytes` in `config.toml` changes it.

Default keys (all in `[keybindings]` of `config.toml`, action name = value; `"none"` unbinds):
Ctrl+Shift+T new tab, Ctrl+Shift+Q close tab, Ctrl+Tab / Ctrl+Shift+Tab cycle, Ctrl+1..9 go to tab,
Ctrl+Shift+PageUp/Down move tab, Ctrl+Shift+R rename tab (also double-click), Alt+Shift+Plus / Minus split,
Ctrl+Shift+W close pane, Alt+Arrows focus, Ctrl+Shift+Z zoom, Ctrl+B sidebar, Ctrl+Shift+P palette,
Ctrl+Shift+N new workspace. Middle-click closes a tab; tabs and dividers drag.

W2 smoke checks (on top of the W1 list): `tabs_4`, `tab_nav`, `split_4_panes`, `zoom_toggle`,
`echo_per_pane`, `echo_isolated`, `key_p50_4panes_le_18ms`, `damage_only_4panes` (re-shaped rows in
unfocused panes must stay 0), `divider_drag_ratios`, `divider_pty_follow`, `status_events`, `screenshot`
(writes `layout-4pane.png`, uploaded as artifact `forgetty-win-layout-4pane`), `close_pane_pty_exit`,
`close_tab_pty_exit`, `palette_opens`, `palette_fuzzy_split_right`, `palette_runs_action`, `tab_rename`,
`config_w2`, `vt_default_scrollback`. Shortcuts in the smoke go through `AppShell::handle_key`, the same
function the window's key handler calls.

## Smoke mode

`--smoke` opens the window, drives `cmd.exe` through ConPTY and prints one JSON object. Exit 0
only if every check in `checks` is true. 10 s is no longer the budget (the throughput test needs
more): hard timeout 300 s, 60 s per phase. On failure the JSON carries the last screen dump.

Checks: `echo_found`, `key_p50_le_18ms`, `damage_only_redraw`, `key_encoder`, `kitty_encoder`,
`wide_direct`, `wide_shell`, `scrollback_direct`, `scrollback_shell`, `resize`, `throughput`,
`throughput_conpty` (25k lines through ConPTY), `throughput_pump` (22.5 MB injected into the real
pump at full speed, gate: no frame gap over 100 ms), `config`, `mouse_encoder`, `paste`, `selection`.
ConPTY itself delivers only a few thousand lines per second, so the 20 MB drain runs through the pump.

Keystrokes in the latency test are injected at `TerminalView::send_keystroke` (same encoder path
as a real key), below the Win32 key event hop.
