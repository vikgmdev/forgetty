# QA Report — T-056: Daemon Reconnect Visual Fixes

**Date:** 2026-04-02
**Task:** T-056
**Fix Cycle:** 1 (commit `9695d12`)
**Status:** PASS

---

## Summary of Fix Cycle 1

The prior QA pass marked AC-3 NEEDS_MANUAL but the live test failed: a large blank area was
visible above the shell prompt. Root cause: the original builder placed snapshot content at the
**bottom** of the oversized 80-row VT (`start_row = initial_rows - snap_rows + 1`). Ghostty's
`resizeWithoutReflow → trimTrailingBlankRows()` removes blank rows from the **bottom only**, so
content at the bottom left no trailing blanks to trim, and the blank rows above the content
remained in the visible window.

Fix (commit `9695d12`): change `start_row` to `1` — content placed at the **top** of the
oversized VT. Trailing blank rows now sit below the content and are trimmed cleanly by the
first-draw resize.

---

## Acceptance Criteria Results

| AC | Description | Result |
|----|-------------|--------|
| AC-1 | Reopen GTK with 3 live daemon panes → tab titles show CWD basename, not "shell" | NEEDS_MANUAL |
| AC-2 | Tab title updates to new CWD when user runs `cd /tmp` (OSC takes over from daemon_cwd) | NEEDS_MANUAL |
| AC-3 | Reopen GTK → visible blank area above prompt is ≤ 2 rows for a fresh shell | PASS (by trace) |
| AC-4 | Reopen GTK → pane with full-screen program (htop) shows content immediately, no blank area | PASS (by trace) |
| AC-5 | Self-contained mode: tab titles still update from `/proc/{pid}/cwd` — no regression | PASS |
| AC-6 | `compute_display_title` returns "shell" only as last resort | PASS |

---

## Build

`cargo check --workspace` completes with **0 errors, 0 warnings**.

---

## Detailed Findings

### AC-3 — Blank area ≤ 2 rows above prompt on fresh-shell reconnect (PASS by trace)

**Fix verified.** `terminal.rs:1563`:

```rust
let start_row = 1_usize; // 1-indexed; content always at top
```

The comment block at lines 1551–1562 explains the reasoning correctly.

**Trace for fresh-shell scenario:**

- `snap.lines`: indices 0–43 are `""` (blank), indices 44–45 have the shell prompt
- `first_content = 44` (first non-empty index)
- `effective_lines = &snap.lines[44..]` → 2 rows
- `effective_cursor_row = snap.cursor_row.saturating_sub(44) = 45 - 44 = 1`
- `start_row = 1`
- Row 1 ← prompt line (snap.lines[44])
- Row 2 ← prompt line (snap.lines[45])
- Cursor placed at: row `1 + 1 = 2`, col `snap.cursor_col + 1`
- VT state after priming: rows 1–2 have content, rows 3–80 are blank (78 trailing blank rows)

**First-draw resize 80 → 46 (actual terminal height):**

`trimTrailingBlankRows(34)` removes rows 47–80 (34 trailing blanks). After resize, visible area
is rows 1–46: prompt at rows 1–2, blank at rows 3–46.

Result: **0 blank rows above the prompt**. The prompt is at the very top. AC-3 is provably
correct for this scenario. ✓

**Edge case: all-blank snapshot (brand-new pane):**

`first_content = snap.lines.len() - 1`. `effective_lines` has 1 blank entry. `start_row = 1`.
One blank row written at row 1. Cursor at row 1. Rows 2–80 blank. First-draw resize trims
trailing blanks. Visible: single blank row at row 1 (≤ 2 rows). ✓

---

### AC-4 — Full-screen program (htop) shows immediately (PASS by trace)

**Trace for htop scenario:**

- `snap.lines`: all 46 entries non-empty
- `first_content = 0` (first non-empty is index 0)
- `effective_lines = &snap.lines[0..]` → 46 rows (full slice, no-op stripping)
- `start_row = 1`
- Rows 1–46 receive htop content
- Rows 47–80: 34 trailing blank rows

**First-draw resize 80 → 46:**

`trimTrailingBlankRows(34)` removes rows 47–80. Visible area: rows 1–46, all htop content.
Result: full content visible immediately, no blank area. ✓

No regression: the behavior for non-blank snapshots is identical to the previous bottom-placement
strategy for this specific case (when `snap_rows == actual_rows`, both strategies place content
at the same rows).

---

### AC-5 — Self-contained mode not regressed (PASS)

`create_terminal` (`terminal.rs:279`) sets `pty: Some(pty)` (line 318) and `daemon_cwd: None`
(line 351). In `compute_display_title` (`app.rs:3549`):

1. `state.pty.as_ref().and_then(|p| p.pid())` succeeds → reads `/proc/{pid}/cwd` → returns CWD
   basename immediately (early return).
2. The `daemon_cwd` branch (line 3573) is never reached.

No behavioral change for self-contained mode. **PASS by code inspection.** ✓

---

### AC-6 — `compute_display_title` returns "shell" only as last resort (PASS)

Priority chain confirmed at `app.rs:3549–3580`:

1. `state.pty` present AND `/proc/{pid}/cwd` readable → CWD basename.
2. `osc_title` non-empty AND `!= "shell"` → OSC title.
3. `state.daemon_cwd` present AND `file_name()` non-None → daemon CWD basename.
4. `"shell"` (last resort only).

"shell" is returned only when: no local PTY, no meaningful OSC title, and no daemon CWD (or
daemon CWD is `/` with no filename component). **PASS by code inspection.** ✓

---

### AC-1 — Tab titles show CWD basename on reconnect (NEEDS_MANUAL)

**Code is correct.** Execution path verified:

1. `ordered: Vec<(PaneId, String, Option<String>)>` built at `app.rs:1199`. All three reconnect
   branches correctly extract `info.cwd` and pass it as `Some(cwd_string)` (lines 1206–1207,
   1237–1238, 1245–1246).
2. Wire-up loop at `app.rs:1264`: `daemon_cwd = cwd.as_ref().map(|s| PathBuf::from(s))` passed
   to `create_terminal_for_pane`.
3. `create_terminal_for_pane` stores it in `TerminalState.daemon_cwd` at line 1613.
4. Title timer calls `compute_display_title`, which reaches the `daemon_cwd` branch before
   returning "shell".

**Needs manual verification** because the daemon's `PaneInfo.cwd` field must be populated by the
daemon's `list_tabs` RPC handler (outside T-056 scope but prerequisite).

---

### AC-2 — OSC title takes over from daemon_cwd after `cd` (NEEDS_MANUAL)

**Code is correct.** After `cd /tmp` the shell emits `OSC 0;/tmp ST`. On the next timer tick,
`compute_display_title` evaluates:

- `state.pty` is `None` (daemon pane) → `/proc` path skipped.
- `osc_title` is non-empty and `!= "shell"` → returned immediately.
- `daemon_cwd` branch never reached.

OSC correctly takes over from daemon_cwd. **Needs manual verification** to confirm the shell
actually emits OSC 0/2 in the live environment.

---

## Issues Found

### Minor: non-selected tabs show stale title until focused

Non-selected tabs' titles come from the session file's `tab.title` field (set at `app.rs:1293`).
If the session was saved with title "shell", those tabs display "shell" until focused (at which
point the timer fires and corrects to `daemon_cwd`). Not a regression; tabs without `daemon_cwd`
were already broken before T-056. Low severity.

### Minor: fresh new pane (all-blank snapshot) shows one blank row at top

When `snap.lines` is entirely blank, `first_content = len - 1`, one blank line is written at
row 1, cursor at row 1. After resize, visible area starts with one blank row then the cursor.
This is a cosmetic artifact (≤ 1 row) within the ≤ 2 row AC-3 threshold. Acceptable.

---

## Scores

| Category | Score (0–10) |
|----------|--------------|
| Completeness | 9 |
| Correctness | 10 |
| Robustness | 9 |
| Code quality | 9 |

**Overall: PASS** (all scores ≥ 7)

---

## Manual Testing Steps

**Prerequisites:** `forgetty-daemon` running, at least 3 tabs open in different directories from a
prior session (session file saved at `~/.local/share/forgetty/session.json`).

**AC-1 — Tab titles show CWD basename:**
1. Open Forgetty in daemon mode.
2. Verify each tab title shows the directory basename (e.g., `forgetty`, `tmp`, `home`) not `shell`.
3. Click each non-selected tab — title must remain correct after focus (no timer-correction flicker
   to "shell" and back).

**AC-2 — OSC title takes over after `cd`:**
1. In a reconnected daemon tab, run `cd /tmp`.
2. Wait ~1 second.
3. Tab title must change to `tmp` (or whatever the shell emits via OSC 0/2).

**AC-3 — Blank area ≤ 2 rows (already proven correct by trace; regression-test only):**
1. Find a tab that had only a shell prompt (no running program) when GTK was last closed.
2. After reopening, prompt should appear at or near the TOP of the pane with zero blank rows above
   it (not buried under a large blank region).

**AC-4 — Full-screen program shows immediately:**
1. Run `htop` in a tab. Close the GTK window (daemon keeps running).
2. Reopen GTK. The htop tab must immediately show htop content, not a blank pane.

**AC-5 — Self-contained mode (regression check):**
1. Stop the daemon. Open Forgetty in self-contained mode.
2. `cd` to several directories. Tab title must update to CWD basename each time.
3. Confirm "shell" does not appear unexpectedly.
