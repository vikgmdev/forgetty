# SPEC — T-056: Daemon Reconnect Visual Fixes

**Task:** T-056
**Date:** 2026-04-02
**Status:** Implementation exists (commit `181a61d`). Builder must verify against this spec and fix any gaps.

---

## Summary

T-056 fixes two visual regressions in the daemon-mode reconnect path after T-055. Neither bug exists in self-contained (non-daemon) mode.

**Bug 1 — Tab titles always show "shell":** The title timer fires 100 ms after pane creation and calls `compute_display_title`, which returns `"shell"` for daemon panes because there is no local PTY (`/proc/{pid}/cwd` unavailable) and the shell has not yet emitted OSC 0/2. The correct title set in the reconnect loop is immediately overwritten.

**Bug 2 — Large blank area above the shell prompt:** `get_screen` returns all N rows of the daemon VT. For an idle shell, rows 0–43 are blank; only the last few contain the prompt. The snapshot replay places all rows into an oversized 80-row VT, then the first-draw resize removes top rows — but the blank rows fill the visible area, pushing the prompt to the bottom.

---

## Root Cause Analysis

### Bug 1: Tab titles show "shell"

**File:** `crates/forgetty-gtk/src/app.rs`

1. Reconnect loop sets `page.set_title(correct_title)` — correct.
2. `register_title_timer` installs a 100 ms GLib timer.
3. Timer calls `compute_display_title(state)` every tick:
   - `state.pty` → `None` (no local PTY for daemon panes) → skips `/proc/{pid}/cwd`
   - `state.terminal.title()` → `""` (fresh VT, no OSC 0/2 yet)
   - Falls through to `return "shell".to_string()`
4. `page.set_title("shell")` permanently overwrites the correct title within 100 ms.

**Missing:** `TerminalState` has no field to store the daemon-supplied CWD as a title fallback.

### Bug 2: Blank area above shell prompt

**File:** `crates/forgetty-gtk/src/terminal.rs`, `create_terminal_for_pane`

1. `get_screen` returns `ScreenSnapshot` with `lines.len() == screen.rows()` (e.g. 46).
2. For idle shell: lines 0–43 are `""`, lines 44–45 contain the prompt.
3. `snap_rows = 46`, `start_row = 80 - 46 + 1 = 35`.
4. All 46 rows (44 blank + 2 prompt) replayed into rows 35–80 of the 80-row VT.
5. First-draw resize 80→46 removes top 34 rows.
6. Post-resize: rows 1–44 are blank, rows 45–46 have the prompt. User sees ~90% blank area.

**Missing:** No stripping of leading blank rows before computing `start_row`.

---

## Implementation Plan

### Fix 1 — `daemon_cwd` field + title fallback

**`crates/forgetty-gtk/src/terminal.rs`**

1. Change `use std::path::Path;` → `use std::path::{Path, PathBuf};`

2. Add field to `TerminalState` struct (after `daemon_client`):
   ```rust
   /// For daemon-backed panes: the CWD from `PaneInfo` at connect time.
   /// Used as a fallback tab title until the shell emits OSC 0/2.
   pub daemon_cwd: Option<PathBuf>,
   ```

3. In `create_terminal()` struct literal: `daemon_cwd: None,`

4. Add `cwd: Option<PathBuf>` parameter to `create_terminal_for_pane` (between `snapshot` and `on_exit`):
   ```rust
   pub fn create_terminal_for_pane(
       config: &Config,
       pane_id: forgetty_core::PaneId,
       daemon_client: Arc<DaemonClient>,
       daemon_rx: mpsc::Receiver<Vec<u8>>,
       snapshot: Option<&crate::daemon_client::ScreenSnapshot>,
       cwd: Option<PathBuf>,
       on_exit: Option<Rc<dyn Fn(String)>>,
       on_notify: Option<Rc<dyn Fn(NotificationPayload)>>,
   )
   ```

5. In `create_terminal_for_pane` struct literal: `daemon_cwd: cwd,`

**`crates/forgetty-gtk/src/app.rs`**

6. In `compute_display_title`, add daemon_cwd fallback after OSC check, before `"shell"` return:
   ```rust
   // Daemon fallback: use CWD basename from pane_info until shell emits OSC 0/2.
   if let Some(cwd) = &state.daemon_cwd {
       if let Some(name) = cwd.file_name() {
           return name.to_string_lossy().to_string();
       }
   }
   ```
   Also update the doc comment priority line to:
   `/// Priority: /proc CWD basename > OSC title > daemon_cwd basename > "shell".`

7. Change `ordered` from `Vec<(PaneId, String)>` to `Vec<(PaneId, String, Option<String>)>`.
   - Live pane reconnect: `cwd = if info.cwd.is_empty() { None } else { Some(info.cwd.clone()) }`
   - Fresh/new panes (closed between sessions): `None`
   - Remaining live panes (not in session file): same as live pane reconnect

8. In the wire-up loop: `let daemon_cwd = cwd.as_ref().map(|s| PathBuf::from(s));` and pass to `create_terminal_for_pane`.

9. All other call sites (`add_new_tab`, `split_pane`): pass `None` for `cwd`.

### Fix 2 — Strip leading blank snapshot lines

**`crates/forgetty-gtk/src/terminal.rs`**, inside `if let Some(snap) = snapshot` block:

```rust
// Discard blank leading rows — they produce a large empty region in the
// viewport after the first-draw resize.
let first_content = snap.lines.iter()
    .position(|l| !l.is_empty())
    .unwrap_or(snap.lines.len().saturating_sub(1)); // keep at least cursor row
let effective_lines = &snap.lines[first_content..];
let effective_cursor_row = snap.cursor_row.saturating_sub(first_content);

let snap_rows = effective_lines.len();
let start_row = initial_rows.saturating_sub(snap_rows) + 1;
for (i, line) in effective_lines.iter().enumerate() {
    let row = start_row + i;
    terminal.feed(format!("\x1b[{row};1H").as_bytes());
    terminal.feed(line.as_bytes());
}
let cur_row = start_row + effective_cursor_row;
let cur_col = snap.cursor_col + 1;
terminal.feed(format!("\x1b[{cur_row};{cur_col}H").as_bytes());
```

---

## Acceptance Criteria (verbatim from BACKLOG)

- [ ] Reopen GTK with 3 live daemon panes → tab titles show CWD basename (e.g. `forgetty`), not `"shell"`
- [ ] Tab title updates to new CWD when user runs `cd /tmp` in a daemon pane (OSC title path takes over from static CWD)
- [ ] Reopen GTK → visible blank area above prompt is ≤ 2 rows for a fresh shell (no large empty region)
- [ ] Reopen GTK → pane with a full-screen program running (e.g. `htop`) shows content immediately with no blank area
- [ ] Self-contained mode (no daemon): tab titles still update from `/proc/{pid}/cwd` as before — no regression
- [ ] `compute_display_title` returns `"shell"` only as a last resort when both OSC title and daemon_cwd are unavailable

---

## Risks and Edge Cases

| Case | Handling |
|------|----------|
| `daemon_cwd` is `/` (root path) | `file_name()` returns `None`; falls through to `"shell"`. Acceptable. |
| `info.cwd` is `""` | Mapped to `None` before `PathBuf::from`. Branch skipped cleanly. |
| OSC 0/2 emits literal `"shell"` | Guard `osc_title != "shell"` prevents it; `daemon_cwd` fallback fires. |
| All snapshot lines empty (brand-new pane) | `first_content = len - 1`; one-row slice placed at row 80. Correct. |
| Cursor row < `first_content` | `saturating_sub` clamps to 0; cursor at top of effective slice. Safe. |
| Full-screen program (htop, vim) | All rows non-empty → `first_content = 0` → no-op. AC-4 satisfied. |

---

## Files Changed

| File | Change |
|------|--------|
| `crates/forgetty-gtk/src/terminal.rs` | `PathBuf` import; `daemon_cwd` field; `cwd` param in `create_terminal_for_pane`; blank-row stripping |
| `crates/forgetty-gtk/src/app.rs` | `daemon_cwd` fallback in `compute_display_title`; `ordered` carries CWD; `cwd` threaded to call sites |

---

## QA Testing Instructions

**Prerequisites:** Daemon running. At least 2–3 tabs open from a prior session.

**AC-1 — Titles show CWD, not "shell":**
1. Open Forgetty (daemon mode) with prior session tabs.
2. Close GTK window (daemon keeps running). Reopen.
3. Tab titles must show directory name, not `"shell"`.

**AC-2 — OSC title takes over after `cd`:**
1. In a reconnected tab, run `cd /tmp`.
2. After prompt appears (~1 s), tab title must change to `tmp`.

**AC-3 — Blank area ≤ 2 rows:**
1. In a tab with only a shell prompt, close and reopen GTK.
2. Prompt must appear near top — at most 2 blank rows above it.

**AC-4 — Full-screen program shows immediately:**
1. Run `htop` in a tab. Close GTK. Reopen.
2. Tab must show `htop` content, not a blank screen.

**AC-5 — Self-contained mode not regressed:**
1. Stop daemon. Open Forgetty in self-contained mode.
2. `cd` to directories; titles must update from `/proc/{pid}/cwd` as before.

**AC-6 — `"shell"` only as last resort:**
Verify by code inspection: `compute_display_title` returns `"shell"` only when `/proc` CWD, OSC title, and `daemon_cwd.file_name()` all fail.
