# forgetty-win (V3 P3 spike)

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
  bracketed paste, wheel and Shift+PageUp/PageDown scrolling (pixel deltas accumulate, view snaps
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

## Smoke mode

`--smoke` opens the window, drives `cmd.exe` through ConPTY and prints one JSON object. Exit 0
only if every check in `checks` is true. 10 s is no longer the budget (the throughput test needs
more): hard timeout 120 s, 45 s per phase. On failure the JSON carries the last screen dump.

Checks: `echo_found`, `key_p50_le_18ms`, `damage_only_redraw`, `key_encoder`, `kitty_encoder`,
`wide_direct`, `wide_shell`, `scrollback_direct`, `scrollback_shell`, `resize`, `throughput`,
`config`, `mouse_encoder`, `paste`, `selection`.

Keystrokes in the latency test are injected at `TerminalView::send_keystroke` (same encoder path
as a real key), below the Win32 key event hop.
