# SPEC — T-058 Fix Cycle: Session File Corruption on Daemon Death

**Task:** T-058 Fix Cycle 1
**Date:** 2026-04-03
**Status:** Ready to implement

---

## Root Cause

When `forgetty-daemon` dies while GTK is open (Ctrl+C, SIGTERM, crash), ALL `subscribe_output` channels disconnect simultaneously. The 8ms poll timer in each daemon pane detects `pty_exited = true` and fires `on_exit` via `glib::idle_add_local_once`. These callbacks run sequentially:

1. on_exit for pane 1 → `close_pane_by_name` → `tab_view.close_page(page1)` → 2 tabs remain
2. on_exit for pane 2 → `close_pane_by_name` → `tab_view.close_page(page2)` → 1 tab remains
3. on_exit for pane 3 → `close_pane_by_name` → `n_pages <= 1` → `window.close()`
4. `connect_close_request` fires → `save_all_workspaces` → saves with 1 tab (pane 3 still in tab_view)
5. Session file overwritten with 1 tab instead of 3

On next GTK launch: cold-start loads the corrupted 1-tab session → only 1 tab restored.

**Key distinction:** When a single shell exits (`exit` in one pane), the daemon is still alive and only that pane's channel disconnects. When the daemon dies, ALL pane channels disconnect at once. The fix must distinguish these two cases.

---

## Fix

**File:** `crates/forgetty-gtk/src/terminal.rs`, daemon poll timer (~line 1800)

When `pty_exited` is detected for a daemon-backed pane, check if the daemon is still alive by attempting a quick RPC. If the daemon is dead → do NOT fire `on_exit` (keep the tab open, break the timer). If alive → fire `on_exit` as before (individual shell exit).

### Current code (broken):

```rust
// Daemon panes don't exit via pty_exited normally, but handle it gracefully.
if pty_exited {
    tracing::debug!(
        "Daemon pane channel closed for {:?}, scheduling close",
        da.widget_name()
    );
    if let Some(ref exit_cell) = on_exit {
        if let Some(cb) = exit_cell.take() {
            let pane_name = da.widget_name().to_string();
            glib::idle_add_local_once(move || {
                cb(pane_name);
            });
        }
    }
    return glib::ControlFlow::Break;
}
```

### Fixed code:

```rust
if pty_exited {
    // Check if the daemon is still alive. If it died (bulk disconnect),
    // do NOT fire on_exit — the cascade would close all tabs and corrupt
    // the session file before save_all_workspaces runs.
    let daemon_alive = {
        let Ok(s) = state.try_borrow() else { return glib::ControlFlow::Break; };
        s.daemon_client.as_ref()
            .map(|dc| dc.list_tabs().is_ok())
            .unwrap_or(true) // No daemon_client = self-contained mode → treat as alive
    };

    if daemon_alive {
        tracing::debug!(
            "Daemon pane {:?} exited (daemon alive), scheduling close",
            da.widget_name()
        );
        if let Some(ref exit_cell) = on_exit {
            if let Some(cb) = exit_cell.take() {
                let pane_name = da.widget_name().to_string();
                glib::idle_add_local_once(move || {
                    cb(pane_name);
                });
            }
        }
    } else {
        tracing::info!(
            "Daemon died — keeping pane {:?} open to preserve session",
            da.widget_name()
        );
    }
    return glib::ControlFlow::Break;
}
```

### Borrow safety

At line 1800, the mutable borrow `s` from `state.try_borrow_mut()` (line 1646) may still be alive (in the `!had_data` branch without a redraw, `s` is not explicitly dropped). The fix must ensure `s` is dropped before `state.try_borrow()`.

**Add explicit `drop(s)` in the else branch before the notification callbacks:**

In the `else` branch (the `!had_data` path), after the `if needs_redraw || bell_active || ring_changed` block, add an `else { drop(s); }`:

```rust
if needs_redraw || bell_active || ring_changed {
    drop(s);
    da.queue_draw();
} else {
    drop(s);
}
```

This ensures `s` is always dropped before line 1785 (notification callbacks) and line 1800 (pty_exited check), regardless of which branch was taken.

---

## Secondary concern: cold-start path correctness

The cold-start path (`Ok(_)` branch in `app.rs` ~line 1395) is correct but was never exercised because the session file was always corrupted by the on_exit cascade. Once the cascade is suppressed, the cold-start path will receive a correct session file and should work. No changes needed to the cold-start code.

---

## Acceptance Criteria

All original T-058 ACs remain, plus:

- **AC-fix.1:** Kill daemon (Ctrl+C / SIGTERM) while GTK has 3 tabs open → GTK window stays open, all 3 tabs remain visible (no cascade close)
- **AC-fix.2:** After daemon dies, session file (`default.json`) is NOT overwritten with fewer tabs. User can close GTK manually and the full session is saved.
- **AC-fix.3:** After daemon dies + GTK manual close + daemon restart + GTK reopen → all 3 tabs restored at correct CWDs (cold-start works)
- **AC-fix.4:** Run `exit` in one pane while daemon is alive → that pane/tab is closed normally (on_exit still fires for individual exits)
- **AC-fix.5:** `list_tabs` RPC call for the alive/dead check does not block GTK for more than 100ms (Unix socket connect to a dead daemon fails fast)

---

## Edge Cases

| Case | Handling |
|------|----------|
| `state.try_borrow()` fails at line 1800 | Return `Break` — stops timer, doesn't fire on_exit. Tab stays open. |
| Daemon killed by SIGKILL (instant death) | Socket gone immediately → `list_tabs()` fails → daemon_alive = false → no cascade |
| Daemon temporarily unresponsive (not dead) | `list_tabs()` might time out → treated as dead → tab stays open. Acceptable — better than corrupting session. |
| Self-contained mode (no daemon_client) | `daemon_client` is None → `unwrap_or(true)` → treated as alive → on_exit fires normally |

---

## Files to change

| File | Change |
|------|--------|
| `crates/forgetty-gtk/src/terminal.rs` | (1) Add `else { drop(s); }` in the `!had_data` branch to ensure mutable borrow is dropped. (2) In the `pty_exited` block, check daemon aliveness via `list_tabs` before firing on_exit. |
