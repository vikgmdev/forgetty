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

## Design

- `src/pty.rs`: portable-pty (ConPTY) spawn, reader thread, async channel to the UI thread.
- `src/view.rs`: `TerminalView` owns `forgetty_vt::Terminal`. Feeds PTY bytes (coalesced),
  writes VT replies (DA, CPR) back to the PTY, caches one shaped line per row and re-shapes
  only rows the VT reports dirty (`Screen::row_dirty_since`). Redraw happens only on
  `cx.notify()`, which only the PTY pump calls.
- `src/element.rs`: custom GPUI `Element`. Paint = background quads (merged runs) + cursor
  block + one `ShapedLine::paint` per row. `shape_line(.., force_width = cell width)` keeps
  the monospace grid exact. GPUI's text system (DirectWrite) provides the glyph atlas.
- `src/keys.rs`: legacy xterm key encoding (Enter, Backspace, arrows, Ctrl+letter, F-keys, Alt prefix).

## IME (out of scope for the spike)

GPUI exposes the Windows IME (TSF/IMM) path through `Window::handle_input` with an
`ElementInputHandler` over an `EntityInputHandler` (`replace_text_in_range`,
`replace_and_mark_text_in_range`, `marked_text_range`, `bounds_for_range`, ...). See
`crates/gpui/examples/input.rs` in the Zed repo. The terminal element would register that
handler in `paint` and report the cursor cell via `bounds_for_range` so the candidate window
anchors at the cursor. Marked text can be drawn with `UnderlineStyle`.

## Smoke mode

`--smoke` opens the window, runs `echo spike-ok` in `cmd.exe`, waits until a screen row
equals `spike-ok` and the next prompt is drawn, then types 100 letters one by one. For each
letter it records the time from the PTY write to the end of the paint pass whose scene
contains the echoed glyph. Prints one JSON object and exits 0. Exit 1 on a 10 s timeout.
Keystrokes are injected at the PTY write, below GPUI's OS key dispatch, so the number covers
PTY round trip + VT parse + vsync-aligned frame + scene build, not the Win32 key event hop.
