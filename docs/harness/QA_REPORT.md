# QA Report — T-058 Fix Cycle 1: Session File Corruption on Daemon Death

**Task:** T-058 Fix Cycle 1  
**Date:** 2026-04-03  
**Phase:** Fix cycle (commit `b6c35d8`)  
**QA method:** Code inspection + `cargo check --workspace`

---

## Build Results

```
cargo check --workspace   →  CLEAN (1 dead_code warning, pre-existing)
```

The only warning (`find_first_daemon_pane_id` is never used in `app.rs:1683`) is pre-existing and unrelated to this fix.

---

## Acceptance Criteria Results

### AC-fix.1: Kill daemon with GTK open → tabs stay, no cascade

**Result: PASS**

**Code trace:**  
`terminal.rs:1802–1833` — When `pty_exited` is true in the daemon poll timer:

1. Line 1807: `state.try_borrow()` acquires a shared borrow (mutable borrow `s` already dropped — see borrow safety below)
2. Line 1808–1810: `s.daemon_client.as_ref().map(|dc| dc.list_tabs().is_ok()).unwrap_or(true)`
3. When daemon is dead, `list_tabs()` calls `rpc("list_tabs", ...)` which calls `UnixStream::connect(&self.socket_path)` — this fails immediately with `ECONNREFUSED` or `ENOENT` → `Err` → `is_ok()` returns `false` → `daemon_alive = false`
4. Line 1826–1831: `daemon_alive == false` branch: logs "Daemon died — keeping pane open to preserve session" and does NOT call `on_exit`
5. Line 1832: returns `Break` — timer stops, but tab remains in `tab_view`

The on_exit cascade is fully suppressed. No `close_pane_by_name` calls, no `window.close()`, no `save_all_workspaces` with partial state.

---

### AC-fix.2: Session file not corrupted after daemon death

**Result: PASS**

**Code trace:**  
Since `on_exit` is never fired when the daemon dies (AC-fix.1), the cascade that leads to `window.close()` → `connect_close_request` → `save_all_workspaces` with fewer tabs never triggers. The session file (`default.json`) remains at its last-saved state — the full session with all tabs.

When the user later closes GTK manually (after daemon death), `save_all_workspaces` runs with all tabs still present in the `tab_view`, saving the complete session.

---

### AC-fix.3: Full cold-start restore works after daemon death + manual GTK close + restart

**Result: PASS**

**Code trace:**  
With the cascade suppressed (AC-fix.1, AC-fix.2):

1. Daemon dies → tabs stay open → user closes GTK manually → full session saved to `default.json`
2. User restarts daemon + GTK
3. `app.rs:1395` — cold-start path: `list_tabs()` returns empty (fresh daemon) → loads `default.json` → gets full tab list
4. `app.rs:1410–1448` — iterates tabs, calls `reconnect_pane_tree` per tab → all tabs restored at saved CWDs

This path was already verified correct in the original T-058 QA (AC-1.1 through AC-1.7). The fix ensures the session file is not corrupted before this path is exercised.

---

### AC-fix.4: `exit` in one pane (daemon alive) → pane closes normally

**Result: PASS**

**Code trace:**  
`terminal.rs:1802–1825` — When `pty_exited` is true and daemon is alive:

1. Line 1808–1809: `dc.list_tabs().is_ok()` → daemon is alive → returns `Ok(Vec<PaneInfo>)` → `is_ok()` returns `true` → `daemon_alive = true`
2. Line 1813–1825: `daemon_alive == true` branch: fires `on_exit` via `glib::idle_add_local_once` exactly as before the fix

Individual shell exits are unaffected. The on_exit callback runs, `close_pane_by_name` removes just that pane.

---

### AC-fix.5: `list_tabs` check doesn't block GTK

**Result: PASS**

**Code trace:**  
`daemon_client.rs:112–125` — `rpc()` method:

1. Line 120: `UnixStream::connect(&self.socket_path)` — synchronous Unix socket connect
2. When daemon is dead: socket file either missing (`ENOENT`, instant) or connection refused (`ECONNREFUSED`, instant). No TCP-style timeout.
3. Line 122–123: `set_read_timeout(Some(Duration::from_millis(500)))` — safety net for the read phase. But when connect fails, we never reach the read phase.
4. Dead daemon: total latency is microseconds (one failed syscall)
5. Live daemon: `list_tabs` is a tiny JSON-RPC round-trip over a Unix socket — sub-millisecond

The 8ms poll timer will not accumulate meaningful latency. GTK main loop remains responsive.

---

## Borrow Safety Analysis

**Critical verification:** The mutable borrow `s` (from `state.try_borrow_mut()` at line 1646) must be dropped before the shared borrow `state.try_borrow()` at line 1807.

**`had_data == true` path (line 1700–1754):**  
Line 1753: `drop(s);` — explicit drop before `da.queue_draw()`. Borrow ends.

**`had_data == false` path (line 1755–1785):**  
Line 1779: `if needs_redraw || bell_active || ring_changed` → line 1780: `drop(s);`  
Line 1782: `else` → line 1783: `drop(s);`  
Both branches explicitly drop `s`. This was added as part of the fix (the original code only dropped in the `true` branch of the inner `if`, leaving `s` alive if no redraw was needed).

**After line 1785:** `s` is unconditionally dead. Lines 1787–1800 (notification callbacks) hold no borrow. Line 1807 (`state.try_borrow()`) is safe.

**Edge case — `try_borrow()` fails at line 1807:**  
Returns `Break` immediately. Timer stops, no on_exit fired. Tab stays open. Correct behavior — errs on the side of preserving the session.

---

## Self-Contained Mode (No Daemon)

**Code trace:**  
`terminal.rs:1810` — `.unwrap_or(true)`: when `daemon_client` is `None` (self-contained/local pane), `daemon_alive` evaluates to `true`, and `on_exit` fires normally.

However, self-contained panes use a different code path entirely — the local poll timer at line 395–609 which does NOT have the daemon check. The daemon check only exists in the daemon poll timer (line 1641–1836). This is correct: local panes never connect to a daemon, so there is no bulk-disconnect scenario to guard against.

---

## Scores

| Dimension | Score | Notes |
|---|---|---|
| Completeness | 9/10 | All 5 fix ACs addressed; root cause (cascade close) fully eliminated |
| Correctness | 10/10 | Borrow safety verified across all branches; daemon alive/dead distinction correct; self-contained mode unaffected |
| Robustness | 9/10 | Failed `try_borrow` → safe fallback; dead daemon → instant failure (no hang); temporarily unresponsive daemon → treated as dead (conservative, session-preserving) |
| Code quality | 8/10 | Clean, well-commented change; explicit `drop(s)` in both branches of `else` path eliminates borrow ambiguity; one pre-existing dead_code warning |

**Overall: PASS** (all scores >= 7)

---

## Edge Cases Verified

| Case | Expected | Verified |
|------|----------|----------|
| Daemon killed by SIGKILL (instant death) | Socket gone → `connect` fails → `daemon_alive = false` → no cascade | Yes (code trace) |
| Daemon killed by SIGTERM (graceful) | Socket removed during shutdown → same as SIGKILL | Yes (code trace) |
| `try_borrow()` contention at line 1807 | Returns `Break` → timer stops, tab stays | Yes (code trace) |
| Self-contained mode (no daemon_client) | `unwrap_or(true)` → on_exit fires normally | Yes (code trace) |
| Single shell exit (daemon alive) | `list_tabs` succeeds → on_exit fires → pane closes | Yes (code trace) |
| All panes disconnect simultaneously | Each timer independently checks daemon → all see dead → none fire on_exit | Yes (code trace) |
