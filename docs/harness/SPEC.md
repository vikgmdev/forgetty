# SPEC — T-058: Cold-Start Session Restore (Layout, CWDs, and VT Buffer)

**Task:** T-058
**Date:** 2026-04-02
**Status:** Ready to implement
**Phase 1 complexity:** Low (~80 lines, 3 files)
**Phase 2 complexity:** Medium (~200 lines, 7 files)

---

## 1. Summary

After a system reboot — or any situation where `forgetty-daemon` starts with no live panes — the GTK client must restore the previous session: same tabs in the same order, same split-pane layout per tab, each pane's shell starting in the correct CWD, and (Phase 2) each pane showing the last visible screen content rather than a blank terminal.

Requirements 1-3 (layout, splits, CWDs) are already stored in `default.json` by T-057. The problem is purely in the **cold-start code path**: the `Ok(_)` branch of `dc.list_tabs()` in `app.rs` ignores the session file entirely. Phase 2 adds daemon-side VT snapshot persistence to disk on shutdown and pre-seeding on cold-start pane creation.

---

## 2. Root Cause

### 2a. The broken branch

**File:** `crates/forgetty-gtk/src/app.rs`, ~line 1395:

```rust
Ok(_) => {
    tracing::info!("Daemon has no live panes — creating initial tab via RPC");
}
```

This fires when `list_tabs` returns `Ok(vec![])` — exactly the cold-start state after a reboot. It logs one line and falls through to `!restored`, creating a single fresh tab. The session file is never consulted.

### 2b. CWD gap: `new_tab()` has no CWD parameter

**File:** `crates/forgetty-socket/src/handlers.rs`, `handle_new_tab`:

```rust
match sm.create_pane(size, None, None, None, true) {
```

`SessionManager::create_pane` already accepts `cwd: Option<PathBuf>` but the RPC handler always passes `None`. The fix is to read an optional `cwd` param from the JSON-RPC request.

### 2c. VT buffer gap: no persistence mechanism

The daemon's VT state lives purely in memory. On SIGTERM it calls `kill_all()` without writing any screen state to disk. No load path exists on cold start.

---

## 3. Phase 1: Layout + CWD Restore

### 3a. Extend `new_tab` RPC to accept optional `cwd`

**File:** `crates/forgetty-socket/src/handlers.rs`, `handle_new_tab`:

```rust
fn handle_new_tab(request: &Request, sm: &SessionManager) -> Response {
    let size = PtySize { rows: DEFAULT_ROWS, cols: DEFAULT_COLS, pixel_width: 0, pixel_height: 0 };

    let cwd: Option<PathBuf> = request
        .params
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir()); // silently ignore nonexistent dirs

    match sm.create_pane(size, cwd, None, None, true) { ... }
}
```

Backward compatible: callers that omit `cwd` still get home-directory fallback.

### 3b. Add `new_tab_with_cwd` to `DaemonClient`

**File:** `crates/forgetty-gtk/src/daemon_client.rs`:

```rust
pub fn new_tab_with_cwd(&self, cwd: Option<&Path>) -> Result<PaneId, DaemonError> {
    let params = match cwd {
        Some(p) => serde_json::json!({ "cwd": p.to_string_lossy().as_ref() }),
        None => serde_json::json!({}),
    };
    let result = self.rpc("new_tab", params)?;
    // ... parse tab_id UUID ...
}

pub fn new_tab(&self) -> Result<PaneId, DaemonError> {
    self.new_tab_with_cwd(None)
}
```

### 3c. Update `reconnect_pane_tree` to pass CWD on fresh pane creation

**File:** `crates/forgetty-gtk/src/app.rs`, in the `PaneTreeState::Leaf` arm of `reconnect_pane_tree`, both places that call `dc.new_tab()` for a missing/fresh pane:

```rust
// Before:
match dc.new_tab() { Ok(pid) => (pid, None), ... }

// After:
let leaf_cwd = if cwd.is_dir() { Some(cwd.as_path()) } else { None };
match dc.new_tab_with_cwd(leaf_cwd) { Ok(pid) => (pid, None), ... }
```

Where `cwd` is the `PathBuf` from `PaneTreeState::Leaf { cwd, .. }`.

### 3d. Fix the cold-start branch

**File:** `crates/forgetty-gtk/src/app.rs`, ~line 1395. Replace the no-op `Ok(_)` arm:

```rust
Ok(_) => {
    tracing::info!("Daemon has no live panes — attempting cold-start session restore");
    let Ok(mgr) = workspace_manager.try_borrow() else { return; };
    let ws = &mgr.workspaces[0];

    let session_tabs: Vec<TabState> = forgetty_workspace::load_session()
        .ok().flatten()
        .and_then(|s| s.workspaces.into_iter().next())
        .map(|w| w.tabs)
        .unwrap_or_default();

    if !session_tabs.is_empty() {
        let mut pane_map: HashMap<uuid::Uuid, PaneInfo> = HashMap::new(); // empty — cold start

        for tab in &session_tabs {
            let legacy_pane_id = tab.pane_id;
            let Some((root_widget, first_da)) = reconnect_pane_tree(
                &tab.pane_tree, &mut pane_map, dc, config,
                &ws.tab_states, &ws.focus_tracker, &ws.custom_titles,
                &window, &ws.tab_view, legacy_pane_id,
            ) else {
                tracing::warn!("reconnect_pane_tree failed for tab {:?}", tab.title);
                continue;
            };

            let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            container.set_hexpand(true);
            container.set_vexpand(true);
            container.append(&root_widget);
            let page = ws.tab_view.append(&container);
            page.set_title(if tab.title.is_empty() { "shell" } else { &tab.title });
            ws.tab_view.set_selected_page(&page);
            first_da.grab_focus();
            register_title_timer(&page, &ws.tab_view, &ws.tab_states,
                &ws.focus_tracker, &ws.custom_titles, &window);
            restored = true;
        }
    }
}
```

---

## 4. Phase 2: VT Buffer Persistence

### 4a. Snapshot format

Same as the `get_screen` RPC response body:
```json
{ "lines": ["<ANSI row string>", ...], "cursor": { "row": 2, "col": 5 } }
```

Storage: `~/.local/share/forgetty/sessions/snapshots/<pane-uuid>.json`

### 4b. New persistence helpers in `forgetty-workspace`

**File:** `crates/forgetty-workspace/src/persistence.rs` — add:

- `snapshot_path(pane_id: uuid::Uuid) -> PathBuf`
- `save_vt_snapshot(pane_id, lines, cursor_row, cursor_col) -> io::Result<()>` — atomic write via temp+rename
- `load_vt_snapshot(pane_id) -> Option<(Vec<String>, usize, usize)>` — returns None if absent/corrupt
- `delete_vt_snapshot(pane_id)` — silent if file not found

Re-export all four from `crates/forgetty-workspace/src/lib.rs`.

### 4c. Extract `serialize_row_ansi` helper in `forgetty-socket`

**File:** `crates/forgetty-socket/src/handlers.rs`

Factor the per-row ANSI serialization logic out of `handle_get_screen` into:
```rust
fn serialize_row_ansi(row: &[Cell]) -> String { ... }
```

Both `handle_get_screen` and the new `save_all_snapshots` call this.

### 4d. `save_all_snapshots` — called by daemon on shutdown

**File:** `crates/forgetty-socket/src/handlers.rs`:

```rust
pub fn save_all_snapshots(sm: &SessionManager) -> usize {
    let pane_ids = sm.list_panes();
    let mut saved = 0usize;
    for id in &pane_ids {
        let result = sm.with_vt(*id, |terminal| {
            let screen = terminal.screen();
            let rows = screen.rows();
            let lines: Vec<String> = (0..rows).map(|r| serialize_row_ansi(screen.row(r))).collect();
            let (cur_row, cur_col) = terminal.cursor_position();
            (lines, cur_row, cur_col)
        });
        if let Ok((lines, cur_row, cur_col)) = result {
            if forgetty_workspace::save_vt_snapshot(id.0, &lines, cur_row, cur_col).is_ok() {
                saved += 1;
            }
        }
    }
    saved
}
```

Expose from `crates/forgetty-socket/src/lib.rs`: `pub use handlers::save_all_snapshots;`

### 4e. Wire into daemon SIGTERM shutdown

**File:** `src/daemon.rs`, in the shutdown block, before `kill_all()`:

```rust
let saved = forgetty_socket::save_all_snapshots(&session_manager);
info!("Saved VT snapshots for {saved} pane(s)");
session_manager.kill_all();
```

### 4f. New RPC: `preseed_snapshot`

**File:** `crates/forgetty-socket/src/protocol.rs`:
```rust
pub const PRESEED_SNAPSHOT: &str = "preseed_snapshot";
```

**File:** `crates/forgetty-socket/src/handlers.rs`:

```rust
fn handle_preseed_snapshot(request: &Request, sm: &SessionManager) -> Response {
    // params: { "pane_id": "<new live pane>", "snapshot_id": "<old saved pane>" }
    let new_pane_id = match require_pane_id(request, sm) { Ok(id) => id, Err(e) => return e };
    let snapshot_uuid = match parse_snapshot_id(request) { Ok(u) => u, Err(e) => return e };

    let Some((lines, cur_row, cur_col)) = forgetty_workspace::load_vt_snapshot(snapshot_uuid) else {
        return Response::success(request.id.clone(), serde_json::json!({ "ok": true, "seeded": false }));
    };

    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(b"\x1b[2J\x1b[H"); // clear + home
    for (i, line) in lines.iter().enumerate() {
        payload.extend_from_slice(format!("\x1b[{};1H{}", i + 1, line).as_bytes());
    }
    payload.extend_from_slice(format!("\x1b[{};{}H", cur_row + 1, cur_col + 1).as_bytes());

    match sm.with_vt_mut(new_pane_id, |t| t.feed(&payload)) {
        Ok(()) => {
            forgetty_workspace::delete_vt_snapshot(snapshot_uuid);
            Response::success(request.id.clone(), serde_json::json!({ "ok": true, "seeded": true }))
        }
        Err(e) => Response::error(request.id.clone(), protocol::INTERNAL_ERROR, e.to_string()),
    }
}
```

Wire in `dispatch`: `methods::PRESEED_SNAPSHOT => handle_preseed_snapshot(request, &sm),`

### 4g. `preseed_snapshot` in `DaemonClient`

**File:** `crates/forgetty-gtk/src/daemon_client.rs`:

```rust
pub fn preseed_snapshot(&self, new_pane_id: PaneId, old_uuid: uuid::Uuid) -> Result<bool, DaemonError> {
    let result = self.rpc("preseed_snapshot", serde_json::json!({
        "pane_id": new_pane_id.to_string(),
        "snapshot_id": old_uuid.to_string(),
    }))?;
    Ok(result.get("seeded").and_then(|v| v.as_bool()).unwrap_or(false))
}
```

### 4h. Call `preseed_snapshot` in `reconnect_pane_tree`

**File:** `crates/forgetty-gtk/src/app.rs`, in the Leaf arm after `new_tab_with_cwd` succeeds, before `subscribe_output`:

```rust
// If this leaf had a saved pane UUID, pre-seed the new pane's VT with the snapshot.
if let Some(old_uuid) = uid {
    if let Err(e) = dc.preseed_snapshot(new_pane_id, old_uuid) {
        tracing::warn!("preseed_snapshot failed: {e}");
    }
}
```

`uid` is the UUID that was tried (from `leaf.pane_id` or `legacy_pane_id`). After pre-seeding, the subsequent `dc.get_screen(new_pane_id)` call already in `reconnect_pane_tree` will return the snapshot content and `create_terminal_for_pane` will display it.

### 4i. Delete snapshot on clean pane close

**File:** `crates/forgetty-socket/src/handlers.rs`, `handle_close_tab`, after `sm.close_pane(id)` succeeds:

```rust
forgetty_workspace::delete_vt_snapshot(id.0);
```

---

## 5. Acceptance Criteria

### Phase 1
- **AC-1.1** After reboot (daemon starts fresh), `forgetty` restores the same number of tabs in the same order
- **AC-1.2** Split-pane layout within each tab is restored
- **AC-1.3** Each pane's shell starts at the saved CWD (`pwd` matches `default.json` leaf `cwd`)
- **AC-1.4** Saved CWD that no longer exists → pane starts at `$HOME`, no crash
- **AC-1.5** No `default.json` → single blank tab as before, no panic
- **AC-1.6** `new_tab` RPC with no `cwd` param is backward compatible
- **AC-1.7** `new_tab` RPC with nonexistent `cwd` silently falls back to home

### Phase 2
- **AC-2.1** `systemctl --user stop forgetty-daemon` → snapshot files written to `~/.local/share/forgetty/sessions/snapshots/`
- **AC-2.2** After cold-start restore, each pane displays the content it showed before shutdown
- **AC-2.3** Snapshot content is display-only — no bytes sent to the live PTY (no re-execution)
- **AC-2.4** Closing a tab via UI deletes its snapshot file
- **AC-2.5** Corrupt/missing snapshot → pane opens blank, no crash
- **AC-2.6** `systemctl --user start forgetty-daemon` followed by `forgetty` shows saved screen state

---

## 6. Edge Cases

| Case | Handling |
|------|----------|
| Saved CWD deleted | `p.is_dir()` guard → `None` → `new_tab_with_cwd(None)` → home |
| `new_tab_with_cwd` fails | `reconnect_pane_tree` returns `None` → tab skipped, others restored |
| No session file | `session_tabs` is empty → `!restored` → single blank tab |
| SIGKILL (no graceful shutdown) | Snapshots not written → Phase 1 restores layout+CWD, panes open blank |
| Corrupt snapshot | `load_vt_snapshot` returns `None` → `preseed_snapshot` returns `seeded: false` → blank pane |
| Old session file (no Leaf.pane_id) | `uid = None` → no preseed → blank pane at correct CWD |
| T-055 session file (TabState.pane_id set) | `legacy_pane_id` fallback → `preseed_snapshot(new_id, legacy_uuid)` → snapshot loaded if present |

---

## 7. Files to Change

### Phase 1 (3 files)

| File | Change |
|------|--------|
| `crates/forgetty-socket/src/handlers.rs` | `handle_new_tab`: read optional `cwd` param |
| `crates/forgetty-gtk/src/daemon_client.rs` | Add `new_tab_with_cwd`; refactor `new_tab` as wrapper |
| `crates/forgetty-gtk/src/app.rs` | Fix `Ok(_)` cold-start branch; update `new_tab()` → `new_tab_with_cwd(leaf_cwd)` in `reconnect_pane_tree` |

### Phase 2 (7 files, incremental on Phase 1)

| File | Change |
|------|--------|
| `crates/forgetty-workspace/src/persistence.rs` | Add 4 snapshot helpers |
| `crates/forgetty-workspace/src/lib.rs` | Re-export snapshot helpers |
| `crates/forgetty-socket/src/handlers.rs` | Extract `serialize_row_ansi`; add `save_all_snapshots`, `handle_preseed_snapshot`; wire in dispatch; delete snapshot in `handle_close_tab` |
| `crates/forgetty-socket/src/protocol.rs` | Add `PRESEED_SNAPSHOT` constant |
| `crates/forgetty-socket/src/lib.rs` | Export `save_all_snapshots` |
| `src/daemon.rs` | Call `save_all_snapshots` before `kill_all` on SIGTERM |
| `crates/forgetty-gtk/src/daemon_client.rs` | Add `preseed_snapshot` method |
| `crates/forgetty-gtk/src/app.rs` | Call `preseed_snapshot` in `reconnect_pane_tree` after fresh pane creation |

---

## 8. Implementation Order

1. Phase 1 fully — verify AC-1.1–1.7 with a real reboot
2. Add snapshot helpers to `forgetty-workspace` (unit testable in isolation)
3. Extract `serialize_row_ansi`, add `save_all_snapshots` to `forgetty-socket`
4. Wire daemon SIGTERM hook
5. Add `handle_preseed_snapshot`, wire in dispatch
6. Add `preseed_snapshot` to `DaemonClient`
7. Call in `reconnect_pane_tree`
8. End-to-end Phase 2 test
