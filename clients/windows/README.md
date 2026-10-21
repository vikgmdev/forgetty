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
- Rendering cost: `windows-latest` has no GPU, so GPUI draws through WARP and every full-window
  quad costs about 8 ms there. The shell therefore paints no redundant full-window backgrounds,
  dims unfocused panes by recoloring their rows (no alpha overlay) and draws the focus ring as four
  1 px bars. Startup also asks for a 1 ms timer tick (`timeBeginPeriod(1)`); the default 15.6 ms
  tick quantizes timed wake-ups. Smoke numbers on CI: key-to-frame p50 about 16.5 ms with 1 and
  with 4 panes (one 60 Hz frame), `frame_cpu_*` and `frame_idle_*` show the frame cost split.
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

## Remote machines (W3)

The same shell now talks to real servers over protocol v2 (`docs/architecture/PROTOCOL_V2.md`).

- `remote/` (crate `forgetty-remote`): `RemoteBackend` implements `SessionBackend` on
  `forgetty-client`. Each trait method is the request of the same name. `attach_pane` yields a
  `PaneSource` over a `PaneStream` (resume offsets are the client's job); a server VT reset becomes
  `PaneEvent::Reset`, and the view starts a fresh `Terminal` and drops terminal replies while the
  replay drains (they would answer old queries a second time). Input goes through the client's input
  stream (64 KiB buffer while disconnected). Resize is `resize_pane`; per AD-020 the latest active
  client wins, so the view sends its size when it changes and again when the window is activated.
  Futures run on a private tokio runtime (2 threads); the UI awaits their `JoinHandle`s.
- Machines: the sidebar lists the local machine and every paired remote with the link state
  (`connected`, `connecting`, `reconnecting` after 1 s, `offline`). The shell keeps the machine on
  screen in its own fields and parks the others (`machines.rs`), so every machine keeps receiving
  events (status, notifications, layout) while it is not shown.
- Pairing: command palette "Pair machine..." (or the sidebar link) opens a dialog. Paste the text
  `forgetty pair` prints (the QR payload JSON may sit among other lines). A confirm step shows the
  machine name, the first 8 characters of the node id and the relay. Nothing is contacted before
  Confirm. `store::parse_offer` strips control characters and bounds lengths.
- Storage in `%APPDATA%\forgetty\` (`FORGETTY_DATA_DIR` overrides): `identity.key`, `client_id`,
  `machines.json`, `addr_cache.json` (the `AddrCacheSnapshot`). Every file is written to a temporary
  name, restricted, then renamed. On Windows "restricted" is a protected DACL with one entry (the
  current user, full access); on Unix mode 0600. The pairing secret is never stored.
- Reliability: the reconnecting overlay covers the machine's panes only after 1 s without a
  connection (`ConnState::show_overlay`). Typing meanwhile is buffered by the client. A
  `NotifyIpInterfaceChange` callback and a `PowerRegisterSuspendResumeNotification` callback (the
  `PBT_APMRESUMEAUTOMATIC` event of `WM_POWERBROADCAST`, without subclassing the GPUI window) call
  `network_changed()` on every remote backend after a 400 ms quiet period (`winsys.rs`).
- Toasts: a WinRT toast for `StatusChanged` waiting, done, failed and for `Notification` when the user
  is not looking at the pane (window in the background, other machine, or other pane). Policy and text
  cleaning are in `shell-core/src/notify.rs` (pane text is untrusted: control and bidi characters are
  removed, length is bounded, at most 1 toast per pane per 3 s and 10 per 30 s). A click focuses the
  machine, tab and pane (the launch argument is `forgetty:focus?m=<uuid>&p=<uuid>`, parsed strictly). The app registers
  `HKCU\Software\Classes\AppUserModelId\Forgetty.Windows` so an unpackaged exe can show toasts.
- Statuses are the server's: `idle`, `working`, `waiting`, `done`, `failed`. A local shell prompt (OSC 133)
  is `idle`: no icon, no toast.
- Pane padding: `pane_padding_x = 6`, `pane_padding_y = 4` in `config.toml` (px between the pane border and the
  text).

W3 smoke (after the W2 phases): the CI starts `v2-test-server.exe` as its own process. The smoke pairs through
`pair_submit_text` + `pair_confirm` (the dialog's code), opens 2 remote panes, types and reads the echo, measures
key-to-frame over localhost (`key_to_frame_p50_ms_remote`), kills the server (overlay after 1 s, typing
buffered), restarts it (same port and key), checks the buffered input arrives and input continues, then drives
the toast paths and checks the Windows hooks registered. Checks: `pane_padding`, `pair_rejects_garbage`,
`pair_dialog_shows_offer`, `pair_waits_for_confirm`, `sidebar_lists_machines`, `remote_two_panes`,
`machine_stored`, `store_files_user_only`, `remote_echo_2_panes`, `remote_echo_isolated`,
`remote_vt_reset_on_attach`, `key_p50_remote_le_25ms`, `remote_resize_reaches_server`, `no_overlay_before_1s`,
`overlay_after_1s`, `sidebar_shows_reconnecting`, `network_poke_reaches_backends`, `reconnected_after_restart`,
`overlay_gone_after_reconnect`, `typing_during_reconnect_buffered`, `vt_reset_after_restart`,
`input_continues_after_restart`, `toast_*`, `no_toast_when_focused`, `net_change_hook_registered`,
`power_resume_hook_registered`, `toast_requests_processed`.

Known limits: the server has no `set_label` yet, so tab rename stays client-side. The in-process test
server answers only part of the method set (no `move_tab`, `set_active_pane`, `update_split_ratios`);
the real server answers all of them.

## Catch-up, labels, real server (W4)

- **Snapshot catch-up (AD-023).** A `PaneItem::Snapshot` always follows a
  `Reset`. The pane's VT is reset, one dim line `-- earlier output not loaded --`
  is pushed into scrollback, then the snapshot paints the screen. The snapshot
  has no scrollback, so nothing above the marker is lost, it was never sent.
- **Labels.** Tab, pane and workspace renames go through `set_label`
  (`Ctrl+Shift+R` for the tab, palette "Rename pane", double click on a
  workspace in the sidebar). Labels show before titles: tab label, then the
  active pane's label, then the program title.
- **Relays.** Every relay in a pairing payload (`relay` and `relays`) is stored
  per machine. A `machines.json` from W3 with one `relay` string still loads.
- **Test server.** `v2-test-server` now answers `focus_tab` (moves the active
  tab), `set_active_pane`, `update_split_ratios` and `set_label`, and takes the
  stdin commands `snapshot <text>` and `drop`. The smoke reads each of them back
  from the layout.
- **Reconnect time.** After a server restart the client was connected again
  about 0.9 s after the new server accepted (CI), against 2.1 s from the loss.
  The 1.1 s the overlay needs and the test server start were most of the gap.
  The client side is the dial attempt: a machine reached directly before gets a
  1 s dial timeout (a fresh first packet every second, instead of QUIC's
  growing retry gaps) and the backoff wait is cut to 400 ms for the first
  minute of an outage.
- **Real server end to end.** CI builds the real Linux `forgetty` and runs it
  in WSL on `windows-latest` (WSL1, the only kind a hosted runner has). The
  Windows client pairs with it through the dialog's code path, opens two panes,
  types, measures key-to-frame, kills and restarts the server, pushes a
  20 MB burst (`seq 1 2500000`) through one pane and compares its final screen
  with `forgetty capture` run inside WSL. Result: artifact
  `forgetty-win-real` (`real-smoke.json`).

## Install and update (W4)

`forgetty-win-<version>-x64.msi` (CI artifact `forgetty-win-msi`) is a
**per-user MSI** built with WiX v4:

- installs to `%LOCALAPPDATA%\Programs\Forgetty`, needs no admin rights and
  shows no UAC prompt, writes nothing to Program Files or HKLM;
- adds a Start menu entry "Forgetty";
- puts the install folder on the user PATH (`HKCU\Environment`). `forgetty.exe`
  there is reserved for the future Windows CLI and starts the app for now;
- uninstall removes all of it and keeps `%APPDATA%\forgetty` (paired machines,
  device key).

Why MSI and not MSIX: an unsigned MSIX only installs after its certificate is
trusted machine-wide (admin) or Developer Mode is on. A per-user MSI installs
with nothing set up. Revisit MSIX when the package is signed.

**SmartScreen.** The installer and the exe are unsigned. Windows shows "Windows
protected your PC, Microsoft Defender SmartScreen prevented an unrecognized app
from starting" with publisher "Unknown publisher". Click "More info", then "Run
anyway". The MSI's own dialog may also say "Unknown publisher". This goes away
once the files are signed with a code signing certificate.

**Updates.** The app never checks on its own. "Check for updates" (sidebar,
or the palette) makes one HTTPS request to the GitHub Releases API of this
repository (WinHTTP, so the system proxy and certificate store apply) and looks
for the newest `win-v<major>.<minor>.<patch>` release. If one is newer, the
sidebar says so, and a click opens its page in the browser. Nothing is
downloaded or installed by the app. No telemetry: the request carries only a
`User-Agent: forgetty-win/<version>`.

## Smoke mode

`--smoke` opens the window, drives `cmd.exe` through ConPTY and prints one JSON object. Exit 0
only if every check in `checks` is true. 10 s is no longer the budget (the throughput test needs
more): hard timeout 420 s, 60 s per phase. On failure the JSON carries the last screen dump.

Checks: `echo_found`, `key_p50_le_18ms`, `damage_only_redraw`, `key_encoder`, `kitty_encoder`,
`wide_direct`, `wide_shell`, `scrollback_direct`, `scrollback_shell`, `resize`, `throughput`,
`throughput_conpty` (25k lines through ConPTY), `throughput_pump` (22.5 MB injected into the real
pump at full speed, gate: no frame gap over 100 ms), `config`, `mouse_encoder`, `paste`, `selection`.
ConPTY itself delivers only a few thousand lines per second, so the 20 MB drain runs through the pump.

Keystrokes in the latency test are injected at `TerminalView::send_keystroke` (same encoder path
as a real key), below the Win32 key event hop.
