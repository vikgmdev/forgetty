# SPEC — T-056: Daemon Reconnect Visual Fixes (Fix Cycle — 2026-04-02)

**Task:** T-056
**Date:** 2026-04-02 (original); Fix cycle opened 2026-04-02 after AC-3 failure in live testing.
**Status:** Prior fix attempt (commit `181a61d`) incomplete — AC-3 still fails in live testing.

---

## Summary

T-056 fixes two visual regressions in the daemon-mode reconnect path after T-055. Neither bug exists in self-contained (non-daemon) mode.

**Bug 1 — Tab titles always show "shell":** Fixed in commit `181a61d`. Verified correct by code inspection. No change needed.

**Bug 2 — Large blank area above the shell prompt:** Commit `181a61d` contains a fix attempt that strips leading blank snapshot lines before replay. The fix does not work. Live testing (AC-3) still shows a large blank area above the shell prompt.

---

## Root Cause Analysis — Bug 2 (Revised)

### What the daemon serializes for blank rows

**File:** `crates/forgetty-socket/src/handlers.rs`, `handle_get_screen`

For each row, `content_end` is computed as:
```rust
let content_end = row
    .iter()
    .rposition(|c| c.grapheme != " " || c.attrs != CellAttributes::default())
    .map(|i| i + 1)
    .unwrap_or(0);
```
For a completely blank row every cell is the default, `rposition` returns `None`, `content_end = 0`, and the loop runs zero times. The resulting line string is `""` (empty). **Blank rows are serialized as `""` — the `is_empty()` check in the prior fix was correct.**

### Why the prior fix didn't work

The prior fix strips leading blank rows correctly. The bug is in the **placement strategy**, not the blank-row detection:

```rust
// BROKEN — places content at the BOTTOM of the 80-row VT
let start_row = initial_rows.saturating_sub(snap_rows) + 1; // e.g. 80 - 2 + 1 = 79
```

The comment in the prior code said: *"libghostty-vt removes rows from the TOP on a shrink"*. **This is wrong.**

### Actual Ghostty resize behavior

**File:** `crates/forgetty-vt/ghostty/src/terminal/PageList.zig`, `resizeWithoutReflow` (~line 2064)

```zig
// Making rows smaller:
// If our rows are shrinking, we prefer to trim trailing
// blank lines from the active area instead of creating
// history if we can.
const trimmed = self.trimTrailingBlankRows(self.rows - rows);
```

`trimTrailingBlankRows` removes blank rows from the **bottom** of the active area. Only when there are insufficient trailing blank rows does content get pushed to scrollback/history.

### Why bottom-placement fails

With `start_row = 79` for a 2-row prompt:
- VT layout: rows 1–78 blank, rows 79–80 have the prompt
- First-draw resize 80 → 46 (delta = 34): `trimTrailingBlankRows(34)` scans up from row 80 — row 80 has the prompt (content). Stops immediately. `trimmed = 0`
- Ghostty pushes rows 1–34 into history to satisfy the row count reduction
- Visible screen becomes rows 35–80: rows 35–78 blank, rows 79–80 prompt
- **User sees 44 blank rows above the prompt — AC-3 fails**

### Why top-placement is correct

With `start_row = 1` for a 2-row prompt:
- VT layout: rows 1–2 have the prompt, rows 3–80 blank (78 trailing blanks)
- First-draw resize 80 → 46 (delta = 34): `trimTrailingBlankRows(34)` removes rows 47–80 cleanly
- Visible screen: rows 1–46, prompt at rows 1–2, blank rows 3–46
- **No blank area above the prompt — AC-3 passes**

### Why self-contained mode never had this problem

`create_terminal()` starts a completely empty VT (no snapshot replay). The shell draws its prompt starting at row 1. Blank rows are naturally below it. Resize trims trailing blanks. Prompt stays at top. No blank area.

---

## Correct Fix

**File:** `crates/forgetty-gtk/src/terminal.rs`
**Location:** `create_terminal_for_pane`, inside `if let Some(snap) = snapshot` block

**Change:** `start_row = initial_rows.saturating_sub(snap_rows) + 1` → `start_row = 1_usize`

### Full replacement block (after)

```rust
if let Some(snap) = snapshot {
    // Strip leading blank rows.  Blank rows from the daemon are serialized as
    // "" (empty string): handle_get_screen only emits bytes up to the last
    // non-default cell, so an all-blank row produces zero bytes.
    let first_content = snap.lines.iter()
        .position(|l| !l.is_empty())
        .unwrap_or(snap.lines.len().saturating_sub(1)); // keep at least cursor row
    let effective_lines = &snap.lines[first_content..];
    let effective_cursor_row = snap.cursor_row.saturating_sub(first_content);

    // Place content at the TOP of the oversized initial VT (row 1).
    //
    // libghostty-vt (PageList::resizeWithoutReflow) shrinks by calling
    // trimTrailingBlankRows(), which removes blank rows from the BOTTOM of
    // the active area — NOT the top.  Placing content at row 1 means the
    // trailing blank rows sit below it; the first-draw resize trims them
    // cleanly and content stays visible at the top.
    //
    // The prior strategy (place at bottom, start_row = initial_rows - snap_rows + 1)
    // was wrong: with content at the bottom there are no trailing blank rows,
    // nothing gets trimmed, and blank rows above the content are pushed into
    // the visible window instead of history.
    let start_row = 1_usize; // 1-indexed; content always at top
    for (i, line) in effective_lines.iter().enumerate() {
        let row = start_row + i;
        // Explicit CUP per row avoids accidental scrolling at the boundary.
        terminal.feed(format!("\x1b[{row};1H").as_bytes());
        terminal.feed(line.as_bytes());
    }
    // Restore cursor to its position within the effective content slice.
    let cur_row = start_row + effective_cursor_row; // absolute 1-indexed row in oversized VT
    let cur_col = snap.cursor_col + 1;
    terminal.feed(format!("\x1b[{cur_row};{cur_col}H").as_bytes());
}
```

Also update the `initial_rows` comment block just above the snapshot block:

```rust
// Over-estimate rows so the first-draw resize is always a SHRINK.
// libghostty-vt shrinks by trimming trailing blank rows from the BOTTOM;
// snapshot content is placed at row 1 (top) so the blank rows sit below
// it and get trimmed cleanly on the first resize.
// 80 rows covers any realistic monitor+font combination.
let initial_rows: usize = 80;
let initial_cols: usize = 240;
```

---

## Files Changed

| File | Change |
|------|--------|
| `crates/forgetty-gtk/src/terminal.rs` | One semantic change: `start_row = 1` instead of `initial_rows - snap_rows + 1`; updated comments |

Bug 1 fixes (tab titles, `daemon_cwd`) from commit `181a61d` are correct — no changes needed there.

---

## Acceptance Criteria (verbatim from BACKLOG)

- [ ] Reopen GTK with 3 live daemon panes → tab titles show CWD basename (e.g. `forgetty`), not `"shell"`
- [ ] Tab title updates to new CWD when user runs `cd /tmp` in a daemon pane (OSC title path takes over from static CWD)
- [ ] Reopen GTK → visible blank area above prompt is ≤ 2 rows for a fresh shell (no large empty region)
- [ ] Reopen GTK → pane with a full-screen program running (e.g. `htop`) shows content immediately with no blank area
- [ ] Self-contained mode (no daemon): tab titles still update from `/proc/{pid}/cwd` as before — no regression
- [ ] `compute_display_title` returns `"shell"` only as a last resort when both OSC title and daemon_cwd are unavailable

---

## Edge Cases

| Case | Analysis |
|------|----------|
| Fresh shell — all blank rows | `first_content = len-1`; one blank row at row 1; cursor at row 1. Trailing rows 2–80 trimmed. Prompt at row 1. |
| Full-screen program — all rows non-blank | `first_content = 0`; rows 1–N have content; rows N+1–80 blank and trimmed. AC-4 satisfied. |
| `snap_rows > 80` (very tall daemon terminal) | Rows fed beyond row 80 cause VT to scroll; early rows go to history. Visible content = last `actual_rows` of snapshot. Acceptable — 80 covers all realistic monitors. |
| Cursor row < `first_content` | `saturating_sub` clamps to 0; cursor at row 1. Safe — does not occur in practice. |
| Empty snapshot | `effective_lines = &[]`; no VT writes; cursor at `\x1b[1;1H`. Clean no-op. |

---

## Manual Testing Instructions

**Prerequisites:** Daemon running. At least 2–3 tabs from a prior session: one with only a shell prompt, one with `htop` running.

**AC-1 — Titles show CWD, not "shell":**
1. Open Forgetty (daemon mode) with prior session tabs. Close GTK. Reopen.
2. Tab titles must show directory name (e.g. `forgetty`), not `"shell"`.

**AC-2 — OSC title takes over after `cd`:**
1. In a reconnected tab, run `cd /tmp`. Wait ~1s for prompt.
2. Tab title must change to `tmp`.

**AC-3 — Blank area ≤ 2 rows (KEY TEST):**
1. Reopen GTK with a tab that had only a shell prompt when closed.
2. Prompt must appear near the top of the pane — at most 2 blank rows above it.
3. FAIL if prompt is in the bottom half of the window with large blank area above.

**AC-4 — Full-screen program shows immediately:**
1. Run `htop` in a tab. Close GTK. Reopen.
2. Tab must show htop content immediately, not a blank pane.

**AC-5 — Self-contained mode not regressed:**
1. Stop daemon. Open Forgetty in self-contained mode.
2. `cd` to directories — tab titles must update from `/proc/{pid}/cwd` as before.

**AC-6 — `"shell"` only as last resort:**
Code inspection: `compute_display_title` in `app.rs` returns `"shell"` only after `/proc CWD`, OSC title, and `daemon_cwd.file_name()` all fail.
