# QA Report — T-058: Cold-Start Session Restore

**Task:** T-058  
**Date:** 2026-04-02  
**Phase:** Initial implementation (commit `fce3487`)  
**QA method:** Code inspection + `cargo check --workspace` + `cargo test --workspace`

---

## Build & Test Results

```
cargo check --workspace   →  CLEAN (1 dead_code warning, pre-existing)
cargo test --workspace    →  87 tests, 0 failures
```

The only warning (`find_first_daemon_pane_id` is never used) is pre-existing dead code unrelated to T-058.

---

## Acceptance Criteria Results

### Phase 1 — Tab/layout/CWD restore

---

#### AC-1.1: After reboot (daemon starts fresh), forgetty restores same number of tabs in same order

**Result: PASS**

**Code trace:**  
`app.rs:1395` — `Ok(_)` branch (no live panes returned by `list_tabs`).  
`app.rs:1400–1405` — `forgetty_workspace::load_session()` retrieves the session file, extracts the first workspace's `tabs` vector in order.  
`app.rs:1410–1448` — iterates `for tab in &session_tabs` and calls `reconnect_pane_tree` per tab in iteration order, appending each to `ws.tab_view`. Order is preserved by the `Vec<TabState>` iteration.

---

#### AC-1.2: Split-pane layout within each tab is restored

**Result: PASS**

**Code trace:**  
`app.rs:1972–2023` — `reconnect_pane_tree` `Split` arm: reads `direction`, creates `gtk::Paned` with matching `gtk4::Orientation`, recursively wires first/second children, defers `set_position` via `idle_add_local_once` using the saved `ratio`. The full tree is reconstructed depth-first, identical to the live-reconnect path.

---

#### AC-1.3: Each pane's shell starts at the saved CWD

**Result: PASS**

**Code trace:**  
Full chain confirmed:

1. `app.rs:1886` — `let leaf_cwd = if cwd.is_dir() { Some(cwd.as_path()) } else { None };`
2. `app.rs:1904` — `dc.new_tab_with_cwd(leaf_cwd)` — passes `Some(path)` to daemon client
3. `daemon_client.rs:182–185` — `new_tab_with_cwd` sends `{"cwd": "/path/..."}` in the JSON-RPC params
4. `handlers.rs:146–152` — `handle_new_tab` reads `cwd` param, validates `p.is_dir()`, converts to `PathBuf`
5. `handlers.rs:154` — `sm.create_pane(size, cwd, None, None, true)` — `cwd` passed directly to session manager
6. `manager.rs:86–93` — `create_pane` takes `cwd: Option<PathBuf>` and passes it to `PtyBridge::spawn`

---

#### AC-1.4: Saved CWD that no longer exists → pane starts at $HOME, no crash

**Result: PASS**

**Code trace:**  
`app.rs:1886` — `let leaf_cwd = if cwd.is_dir() { Some(cwd.as_path()) } else { None };`  
When `cwd` does not exist, `is_dir()` returns `false`, so `leaf_cwd = None`.  
`app.rs:1904` — `dc.new_tab_with_cwd(None)` is called.  
`handlers.rs:146–152` — `cwd` evaluates to `None` (no `cwd` key in params, or empty string filtered by `.filter(|s| !s.is_empty())`).  
`sm.create_pane(size, None, ...)` — PTY spawns in the shell's default directory (typically `$HOME`).  
No crash path: the `None` branch is handled cleanly throughout the chain.

---

#### AC-1.5: No default.json → single blank tab, no panic

**Result: PASS**

**Code trace:**  
`app.rs:1400–1405` — `forgetty_workspace::load_session().ok().flatten()...unwrap_or_default()` — any `Err` or `None` from `load_session` returns an empty `Vec<TabState>`.  
`app.rs:1407` — `if !session_tabs.is_empty()` — the entire restore block is skipped when the vec is empty.  
`app.rs:1487–1492` — `if !restored { ... }` — the fallback path creates a single blank tab.  
`persistence.rs:50–53` — `load_session()` returns `Ok(None)` on missing file (not an error).

---

#### AC-1.6: `new_tab` RPC with no `cwd` param remains backward compatible

**Result: PASS**

**Code trace:**  
`daemon_client.rs:197–199` — `pub fn new_tab(&self) -> Result<PaneId> { self.new_tab_with_cwd(None) }` — wraps `new_tab_with_cwd(None)`.  
`handlers.rs:146–152` — `request.params.get("cwd")` returns `None` when param is absent → `cwd = None` → falls through cleanly to `sm.create_pane(size, None, ...)`.  
No `required` param semantics: absent `cwd` is treated as `None` throughout.

---

#### AC-1.7: `new_tab` RPC with nonexistent `cwd` silently falls back to home

**Result: PASS**

**Code trace:**  
`handlers.rs:146–152`:
```rust
let cwd: Option<PathBuf> = request
    .params
    .get("cwd")
    .and_then(|v| v.as_str())
    .filter(|s| !s.is_empty())
    .map(PathBuf::from)
    .filter(|p| p.is_dir()); // silently ignore nonexistent dirs
```
`p.is_dir()` returns `false` for nonexistent paths → `cwd = None` → `sm.create_pane(size, None, ...)`.  
No error returned, no log at error level (silent fallback as required).

---

### Phase 2 — VT snapshot save/restore

---

#### AC-2.1: `systemctl --user stop forgetty-daemon` → snapshot files written to `~/.local/share/forgetty/sessions/snapshots/`

**Result: PASS (code); NEEDS_MANUAL for filesystem verification**

**Code trace:**  
`daemon.rs:250–262` — shutdown block:
```rust
tokio::select! {
    _ = sigterm.recv() => { info!("Received SIGTERM"); }
    _ = sigint.recv()  => { info!("Received SIGINT");  }
}
info!("forgetty-daemon shutting down");
sync_endpoint.close().await;
let saved = forgetty_socket::save_all_snapshots(&session_manager);
info!("Saved VT snapshots for {saved} pane(s)");
session_manager.kill_all();
```
`save_all_snapshots` is called BEFORE `kill_all()` — correct ordering confirmed.

`handlers.rs:312–329` — `save_all_snapshots`: calls `sm.list_panes()`, for each pane calls `sm.with_vt`, serializes rows via `serialize_row_ansi`, calls `forgetty_workspace::save_vt_snapshot(id.0, &lines, cur_row, cur_col)`.

`persistence.rs:72–73` — `snapshot_path`: `data_dir().join("sessions").join("snapshots").join(format!("{pane_id}.json"))` — correct path.

`persistence.rs:80–102` — `save_vt_snapshot`: atomic write via temp+rename. Creates parent dirs with `fs::create_dir_all`.

**Manual step:** Run `systemctl --user stop forgetty-daemon` with active panes and verify files appear in `~/.local/share/forgetty/sessions/snapshots/*.json`.

---

#### AC-2.2: After cold-start restore, each pane displays content it showed before shutdown

**Result: PASS (code logic); NEEDS_MANUAL for visual verification**

**Code trace:**  
In cold-start `reconnect_pane_tree` (`app.rs:1888–1935`):
- `pane_map` is empty → `pane_map.remove(&uid)` returns `None` → fresh pane branch → `fresh_pane = true`
- `app.rs:1929–1934`: `if fresh_pane { if let Some(old_uuid) = uid { dc.preseed_snapshot(daemon_pane_id, old_uuid) } }`
- `preseed_snapshot` is called BEFORE `subscribe_output` (line 1938) — correct ordering

`handlers.rs:480–508` — `handle_preseed_snapshot`:
- Loads snapshot via `forgetty_workspace::load_vt_snapshot(snapshot_uuid)`
- Builds ANSI payload: `\x1b[2J\x1b[H` (clear+home), then `\x1b[{r};1H{line}` per row, then final `\x1b[{cur_row+1};{cur_col+1}H`
- Feeds payload into VT via `sm.with_vt_mut(new_pane_id, |t| t.feed(&payload))`
- Deletes snapshot file on success

**Manual step:** After daemon restart + `forgetty`, verify pane content matches pre-shutdown state.

---

#### AC-2.3: Snapshot content is display-only — no bytes sent to live PTY

**Result: PASS**

**Code trace:**  
`handlers.rs:495` — `sm.with_vt_mut(new_pane_id, |t| t.feed(&payload))` — calls `with_vt_mut` on the session manager.

`manager.rs:287–297` — `with_vt_mut` operates exclusively on `pane.vt.terminal` (the session-side VT struct). It holds `self.inner.lock()` and calls `f(&mut pane.vt.terminal)`.

`manager.rs:151–158` — `write_pty` is a separate function that calls `pane.pty_bridge.pty.write(data)` — this is the PTY master fd path.

`with_vt_mut` never touches `pty_bridge`; `write_pty` is never called from `handle_preseed_snapshot`. The snapshot bytes are injected into the in-memory VT buffer only.

---

#### AC-2.4: Closing a tab via UI deletes its snapshot file

**Result: PASS**

**Code trace:**  
`handlers.rs:166–183` — `handle_close_tab`:
```rust
match sm.close_pane(id) {
    Ok(()) => {
        forgetty_workspace::delete_vt_snapshot(id.0);
        Response::success(...)
    }
    ...
}
```
`delete_vt_snapshot(id.0)` is called immediately after `close_pane` succeeds.

`persistence.rs:129–136` — `delete_vt_snapshot`: checks `path.exists()` before calling `fs::remove_file`, logs a warning on failure but does not propagate the error (silent on missing).

The UI close path goes through `handle_close_tab` via the `close_tab` RPC — confirmed by `daemon_client.rs:202–205`.

---

#### AC-2.5: Corrupt/missing snapshot → pane opens blank, no crash

**Result: PASS**

**Code trace:**  
`persistence.rs:108–126` — `load_vt_snapshot`: returns `None` on any `fs::read_to_string` error (missing file) or `serde_json::from_str` error (corrupt JSON). Uses `?` with `.ok()` chaining — no panics possible.

`handlers.rs:480–486` — `handle_preseed_snapshot`:
```rust
let Some((lines, cur_row, cur_col)) = forgetty_workspace::load_vt_snapshot(snapshot_uuid)
else {
    return Response::success(
        request.id.clone(),
        serde_json::json!({ "ok": true, "seeded": false }),
    );
};
```
Returns `{ "seeded": false }` — graceful success response. The caller in `daemon_client.rs:270–283` returns `Ok(false)` which `app.rs:1931` ignores via `if let Err(e)` (the `Ok(false)` path continues normally). Pane opens blank.

---

#### AC-2.6: After restart + forgetty, saved screen state visible

**Result: NEEDS_MANUAL**

This is the end-to-end behavioral verification of AC-2.1 + AC-2.2 combined. Code path is confirmed correct; visual result requires manual testing.

**Manual step:** See manual testing steps below.

---

## Code Quality Notes

**`serialize_row_ansi` (handlers.rs:238–306):**  
Correctly skips trailing blank cells (`content_end` scan). Handles all SGR attributes (bold, dim, italic, underline, inverse, strikethrough, RGB fg/bg). Resets SGR at end of each line to prevent bleed between rows. Handles `Color::Default` correctly (no escape emitted for default colors — terminals render default correctly without SGR). Well commented.

**`save_all_snapshots` (handlers.rs:312–329):**  
Uses `sm.with_vt` (read-only) — correct choice for snapshot serialization. Counts and logs saved panes. Handles per-pane errors gracefully (continues loop, doesn't abort).

**`handle_preseed_snapshot` (handlers.rs:460–509):**  
Clear separation between snapshot loading, payload construction, VT injection, and cleanup. The `delete_vt_snapshot` call after successful injection ensures no stale snapshots accumulate. The `seeded: false` path handles missing/corrupt snapshots gracefully.

**One dead_code warning:** `find_first_daemon_pane_id` (app.rs:1683) is unused. Not introduced by T-058, pre-existing.

---

## Scores

| Dimension | Score | Notes |
|---|---|---|
| Completeness | 9/10 | All 11 ACs implemented; no skeleton/stub paths in T-058 code |
| Correctness | 9/10 | All traces confirm correct logic; snapshot/PTY isolation verified |
| Robustness | 9/10 | Missing file, corrupt JSON, nonexistent CWD all handled gracefully; atomic writes |
| Code quality | 8/10 | Clean, well-commented; one pre-existing dead_code warning; no new issues |

**Overall: PASS** (all scores >= 7)

---

## Manual Testing Steps (for NEEDS_MANUAL items)

These require a running daemon and GTK build.

### AC-2.1 + AC-2.6: Snapshot files written on shutdown / visible after restart

```bash
# 1. Start daemon
systemctl --user start forgetty-daemon

# 2. Launch forgetty, open 2-3 tabs, run some commands in each
#    (e.g., ls -la, echo "hello world", htop)

# 3. Stop daemon via systemd
systemctl --user stop forgetty-daemon

# 4. Verify snapshot files exist
ls ~/.local/share/forgetty/sessions/snapshots/
# Expected: one .json file per pane UUID

# 5. Inspect a snapshot file
cat ~/.local/share/forgetty/sessions/snapshots/<uuid>.json
# Expected: {"lines":["...ANSI encoded rows..."],"cursor":{"row":N,"col":M}}

# 6. Restart daemon
systemctl --user start forgetty-daemon

# 7. Launch forgetty
# Expected: each pane shows the content it had before shutdown
#           (static display — no interactive replay to shell)
```

### AC-2.2: Content visible, not replayed to PTY

```bash
# After AC-2.6 verification above:
# 8. In a restored pane, press Enter or type a command
# Expected: shell responds normally (CWD is correct per AC-1.3)
#           The snapshot content does not "interfere" — it is purely cosmetic
#           and the shell prompt is drawn fresh by the PTY on first output
```

### AC-2.4: Snapshot deleted on tab close

```bash
# After daemon restart + forgetty open:
# 9. Note the UUID of a restored pane (from journalctl or snapshot filename)
# 10. Close that tab via Ctrl+Shift+W or the tab close button
# 11. Verify the snapshot file is gone
ls ~/.local/share/forgetty/sessions/snapshots/
# Expected: corresponding .json file no longer present
```

### AC-1.4: Nonexistent CWD falls back to $HOME

```bash
# 12. Manually edit the session file to set a cwd that doesn't exist:
#     ~/.local/share/forgetty/sessions/default.json
#     Set a leaf's "cwd" to "/tmp/nonexistent_path_xyz"

# 13. Stop and restart daemon, then launch forgetty
# Expected: pane opens at $HOME (not /tmp/nonexistent_path_xyz)
#           No crash, no error dialog
```
