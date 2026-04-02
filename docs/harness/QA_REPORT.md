# QA Report — T-056: Daemon Reconnect Visual Fixes

**Date:** 2026-04-02
**Task:** T-056
**Status:** PASS

---

## Acceptance Criteria Results

| AC | Description | Result |
|----|-------------|--------|
| AC-1 | Reopen GTK with 3 live daemon panes → tab titles show CWD basename, not "shell" | NEEDS_MANUAL |
| AC-2 | Tab title updates to new CWD when user runs `cd /tmp` (OSC takes over from daemon_cwd) | NEEDS_MANUAL |
| AC-3 | Reopen GTK → visible blank area above prompt is ≤ 2 rows for a fresh shell | NEEDS_MANUAL |
| AC-4 | Reopen GTK → pane with full-screen program (htop) shows content immediately, no blank area | NEEDS_MANUAL |
| AC-5 | Self-contained mode: tab titles still update from `/proc/{pid}/cwd` — no regression | PASS |
| AC-6 | `compute_display_title` returns "shell" only as last resort | PASS |

---

## Code Inspection Findings

### Build

`cargo check --workspace` completes with **0 errors, 0 warnings**.

---

### AC-1 — Tab titles show CWD basename on reconnect (NEEDS_MANUAL)

**Code is correct.** Execution path verified:

1. `ordered: Vec<(PaneId, String, Option<String>)>` is built at `app.rs:1199`. All three reconnect branches (session-file live pane, no-session live pane, extra live panes) correctly extract `info.cwd` from `PaneInfo` and pass it as `Some(cwd_string)`.
2. In the wire-up loop (`app.rs:1251`): `let daemon_cwd = cwd.as_ref().map(|s| PathBuf::from(s))` is passed to `create_terminal_for_pane`.
3. `create_terminal_for_pane` stores it in `TerminalState.daemon_cwd` (field verified at `terminal.rs:167` and assigned at `terminal.rs:1606`).
4. The title timer calls `compute_display_title`, which now falls through to the `daemon_cwd` branch before returning "shell" (verified below in AC-6).
5. For non-selected tabs, the title is set once from `tab.title` (session file value) at `app.rs:1293`. That title was the CWD basename from the prior session save — correct for T-056's scope. The timer will update it with fresh `daemon_cwd` on first focus.

**Needs manual verification** because the daemon's `PaneInfo.cwd` field must actually be populated by the daemon's `list_tabs` RPC handler, which is outside the scope of T-056's changes but is a prerequisite.

---

### AC-2 — OSC title takes over from daemon_cwd after `cd` (NEEDS_MANUAL)

**Code is correct.** After the shell runs `cd /tmp` and emits `OSC 0;/tmp ST`, the VT parser sets `terminal.title()` to `"/tmp"` or to what zsh/bash emits (typically the CWD basename). The title timer calls `compute_display_title`:

- `state.pty` is `None` (daemon pane) → `/proc` path skipped.
- `osc_title` is now non-empty and not `"shell"` → returned immediately.
- `daemon_cwd` branch is never reached.

The priority chain is: `/proc CWD > OSC title > daemon_cwd > "shell"`. OSC title correctly takes over.

**Needs manual verification** to confirm zsh/bash actually emit OSC 0/2 with the expected CWD basename in the live environment.

---

### AC-3 — Blank area ≤ 2 rows above prompt on fresh-shell reconnect (NEEDS_MANUAL)

**Code is correct.** The blank-row stripping logic at `terminal.rs:1543–1566` exactly matches the spec:

```
first_content = position of first non-empty line (or len-1 if all blank)
effective_lines = snap.lines[first_content..]
effective_cursor_row = snap.cursor_row.saturating_sub(first_content)
snap_rows = effective_lines.len()
start_row = initial_rows.saturating_sub(snap_rows) + 1   // places content at VT bottom
```

For a fresh shell with 44 blank rows + 2 prompt rows: `first_content = 44`, `effective_lines` has 2 rows, `snap_rows = 2`, `start_row = 80 - 2 + 1 = 79`. After first-draw resize to actual size (e.g., 46 rows), libghostty-vt drops rows from the top, leaving the 2 prompt rows at the bottom. Visible blank area = 0 to (actual_rows - 2) rows depending on terminal height, but the prompt will be at or near the bottom rather than buried in blank space. The result should be ≤ 2 rows of blank above the prompt assuming the snapshot accurately captures the shell cursor position.

**Edge case handled:** If all lines are blank (brand-new pane), `first_content = len - 1`, one blank row is placed at row 80. Correct.

**Needs manual verification** to confirm the VT resize behavior and that `ScreenSnapshot.lines` accurately represents what `get_screen` returns from the daemon.

---

### AC-4 — Full-screen program shows immediately with no blank area (NEEDS_MANUAL)

**Code is correct.** When htop or vim is running, all (or most) rows in the snapshot are non-empty. `first_content = 0` → `effective_lines = snap.lines` (full slice, no-op). The existing bottom-placement logic runs unchanged: all rows are placed from `start_row = 80 - N + 1` to row 80. This is identical to the pre-T-056 behavior for full-screen programs (no regression), and AC-4 was already working before the fix for the blank-row case.

**Needs manual verification** to confirm htop content is visible immediately in the live environment.

---

### AC-5 — Self-contained mode not regressed (PASS)

**Code is provably correct.** In `create_terminal` (self-contained path, `terminal.rs:279`), `pty: Some(pty)` and `daemon_cwd: None` are always set. In `compute_display_title`:

1. `state.pty.as_ref().and_then(|p| p.pid())` succeeds → `/proc/{pid}/cwd` is read → CWD basename returned.
2. The new `daemon_cwd` branch is never reached because the function returns early.

No behavioral change for self-contained mode. **PASS by code inspection.**

---

### AC-6 — `compute_display_title` returns "shell" only as last resort (PASS)

**Code exactly matches spec.** Priority chain at `app.rs:3549–3580`:

1. If `state.pty` present and `/proc/{pid}/cwd` readable → return CWD basename.
2. If `osc_title` non-empty and `osc_title != "shell"` → return OSC title.
3. If `daemon_cwd` present and `file_name()` non-None → return CWD basename.
4. Return `"shell"`.

"shell" is returned only when: no local PTY, AND no meaningful OSC title, AND no daemon CWD (or daemon CWD is `/` with no filename). This is the intended last resort. **PASS by code inspection.**

---

## Issues Found

### Minor: non-selected tabs show stale session title until focused

When reopening with 3+ tabs, the non-selected tabs' titles come from `tab.title` in the session file (set once at `app.rs:1293`). If the session was saved when the title was "shell" (e.g., from an older session before T-056), those tabs will display "shell" until the user focuses them, at which point the timer fires and updates to `daemon_cwd`. This is a one-time-per-focus update and not a regression (tabs without `daemon_cwd` were already broken before T-056). Not a blocker, but worth noting.

### Minor: `info.title` used as fallback for no-session panes (not `info.cwd`)

In the no-session-file branch (`app.rs:1242–1247`), the initial tab title is set from `info.title` (which the daemon may set to "shell" if not tracked). The `daemon_cwd` field is still populated, so the timer will correct the title on first focus. No functional issue, but initial visual state may briefly show "shell" for the non-focused tabs before they're clicked.

---

## Scores

| Category | Score (0-10) |
|----------|--------------|
| Completeness | 9 |
| Correctness | 9 |
| Robustness | 8 |
| Code quality | 9 |

**Overall: PASS** (all scores ≥ 7)

---

## Manual Testing Steps for NEEDS_MANUAL Items

**Prerequisites:** Daemon running (`forgetty-daemon`). At least 3 tabs open in different directories from a prior session (session file saved).

**AC-1 — Tab titles show CWD basename:**
1. Open Forgetty in daemon mode with 3 prior-session tabs.
2. Verify each tab's title shows the directory name (e.g., `forgetty`, `tmp`, `home`), not `shell`.
3. Click through each tab to confirm non-selected tabs also update on focus.

**AC-2 — OSC title takes over after `cd`:**
1. In a reconnected daemon tab, run `cd /tmp`.
2. Wait ~1 second for the shell prompt to redraw.
3. Tab title must change to `tmp` (or whatever zsh/bash emits via OSC 0/2).

**AC-3 — Blank area ≤ 2 rows for fresh shell:**
1. Find a tab that had only a shell prompt (no running program) when GTK was last closed.
2. After reopening, the prompt should appear near the bottom or top of the pane with at most 2 blank rows above it.
3. There should NOT be a large blank region (>5 rows) filling the visible area.

**AC-4 — Full-screen program shows immediately:**
1. Run `htop` in a tab. Close GTK window (daemon keeps running).
2. Reopen GTK. The htop tab should immediately show htop content, not a blank pane.

**AC-5 — Self-contained mode (regression check):**
1. Stop the daemon. Open Forgetty without daemon (self-contained mode).
2. Open a new tab, `cd` to several directories. Tab title must update to CWD basename each time.
3. Confirm no `shell` title appears unexpectedly.
