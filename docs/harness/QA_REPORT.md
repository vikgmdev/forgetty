# QA Report — T-057: Fix Split-Pane Session Save and Restore (Daemon Mode)

**Task:** T-057
**Date:** 2026-04-02
**Commit:** 2279d27
**Fix cycle:** Initial
**QA method:** Code inspection + cargo check + cargo test --workspace

---

## Summary

T-057 fixes the root bug where split-pane tabs were restored as multiple flat tabs instead of
a single tab with `gtk::Paned` layout. The fix adds `pane_id` to `PaneTreeState::Leaf`, embeds
daemon pane UUIDs at the per-leaf level during snapshot, and replaces the old flat reconnect loop
with a recursive `reconnect_pane_tree` function that mirrors `build_pane_tree`.

**Overall verdict: PASS** (all scored criteria >= 7; runtime ACs need manual verification).

---

## Build Results

```
cargo check --workspace
  warning: function `find_first_daemon_pane_id` is never used
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.26s

cargo test --workspace
  All 15 forgetty-workspace tests pass (including 2 backward-compat tests).
  All other crate tests pass. 0 failures.
```

**One compiler warning:** `find_first_daemon_pane_id` is dead code -- it was superseded by the
per-leaf approach but not removed. The warning is non-blocking but should be cleaned up.

---

## AC Results

### AC-1 -- Session file has `"Split"` pane_tree with non-null `pane_id` per Leaf; TabState.pane_id is null

**PASS (code inspection)**

Trace:
1. `snapshot_pane_tree` DrawingArea arm (app.rs:1686-1693): reads `daemon_pane_id` from
   `TerminalState.daemon_pane_id`, maps `PaneId(uuid)` -> `uuid::Uuid`, returns
   `PaneTreeState::Leaf { cwd, pane_id: daemon_pane_id }`.
2. Paned arm (app.rs:1696-1719): recurses into both children, assembles `Split { first, second }`.
3. `snapshot_single_workspace` (app.rs:1754-1755): writes `pane_id: None` at `TabState` level.

The resulting JSON for a split tab will be:
```json
{
  "title": "myproject",
  "pane_tree": {
    "Split": {
      "first":  { "Leaf": { "cwd": "...", "pane_id": "<uuid-A>" } },
      "second": { "Leaf": { "cwd": "...", "pane_id": "<uuid-B>" } }
    }
  },
  "pane_id": null
}
```

### AC-2 -- Reopen: split tab restored as ONE tab with gtk::Paned, not two flat tabs

**PASS (code inspection) / NEEDS_MANUAL (runtime)**

Trace:
1. Daemon reconnect block (app.rs:1198-1241): iterates `session_tabs`, calls
   `reconnect_pane_tree` per tab.
2. `reconnect_pane_tree` Split arm (app.rs:1908-1960): recurses into both children, creates
   one `gtk4::Paned`, returns `(paned.upcast::<gtk4::Widget>(), first_da)`.
3. The container `gtk4::Box` wrapping `root_widget` is appended as a single tab page
   (app.rs:1224-1230). One tab page per session tab entry.
4. Pane B's UUID is in its own `Leaf.pane_id`, so it is consumed by the recursive call --
   not left in `pane_map` as an orphan.

NEEDS_MANUAL: Runtime verification that the `gtk::Paned` widget renders visibly and both halves
are live terminals.

### AC-3 -- Reopen: split divider position approximately preserved (+-5% of saved ratio)

**PASS (code inspection) / NEEDS_MANUAL (runtime)**

In `reconnect_pane_tree` Split arm (app.rs:1942-1956): `saved_ratio` is captured, then
`glib::idle_add_local_once` defers `paned.set_position((size * saved_ratio) as i32)` until
after widget realization. The `if size > 0` guard prevents zero-size division. This is identical
to the pattern in `build_pane_tree` which works in self-contained mode.

NEEDS_MANUAL: Runtime verification that divider lands within +-5% of saved ratio.

### AC-4 -- Reopen: each pane shows correct CWD from its daemon pane

**PASS (code inspection) / NEEDS_MANUAL (runtime)**

In `reconnect_pane_tree` Leaf arm (app.rs:1833-1871): when `pane_map.remove(&uid)` succeeds,
`daemon_cwd` is set from `info.cwd`. The effective CWD logic (app.rs:1869-1871) uses
`daemon_cwd` first (live CWD from daemon), falls back to saved `cwd` from session file.
`create_terminal_for_pane` receives `effective_cwd` as the starting directory.

NEEDS_MANUAL: Runtime verification that each pane's shell prompt shows the correct CWD.

### AC-5 -- Single-pane tabs unaffected

**PASS (code inspection + automated tests)**

For a single-pane tab, `pane_tree` is `PaneTreeState::Leaf { cwd, pane_id: Some(uid) }`.
`reconnect_pane_tree` Leaf arm handles this identically to the pre-T-057 path (pane_map lookup,
subscribe, create_terminal_for_pane). The `pane_id` value now comes from `Leaf.pane_id` rather
than `TabState.pane_id`, but the lookup and result are the same.

All 15 `forgetty-workspace` tests pass including `backward_compat_*` tests.

### AC-6 -- Self-contained mode save/restore still works

**PASS (code inspection + automated tests)**

`build_pane_tree` Leaf arm (app.rs:1978): `PaneTreeState::Leaf { cwd, .. }` -- the `..` pattern
explicitly ignores `pane_id`. The function spawns a fresh PTY from `cwd`. No regression.

`#[serde(default)]` on `Leaf.pane_id` means self-contained session files written before T-057
(no `pane_id` field in JSON) deserialize with `pane_id: None`. All persistence tests pass.

### AC-7 -- Deeply nested splits (3+ panes) save and restore correctly

**PASS (code inspection) / NEEDS_MANUAL (runtime)**

Both `snapshot_pane_tree` and `reconnect_pane_tree` are purely recursive with no depth limit.
A `Split` whose `first` or `second` child is itself a `Split` is handled identically by pattern
matching on `PaneTreeState`. Stack depth is bounded only by actual nesting depth (typically <= 8
in practice).

No automated unit test exists for 3-level nested serialization. Code inspection is the basis for
this pass.

NEEDS_MANUAL: Runtime test with 3+ pane split.

### AC-8 -- Old session file (no pane_id in Leaf) deserializes without error, fresh panes created

**PASS (code inspection + automated tests)**

`#[serde(default)]` on `Leaf.pane_id` (workspace.rs:68-69) causes serde to fill `None` when the
JSON field is absent. The `backward_compat_no_window_dimensions` test exercises the serde default
path. The `load_session` function returns `Ok(None)` on parse error as a safety net.

In `reconnect_pane_tree` (app.rs:1857-1865): when `uid` is `None`, falls through to
`dc.new_tab()` to create a fresh pane -- no crash, graceful degradation.

### AC-9 -- T-055-era session file (TabState.pane_id set, Leaf.pane_id absent) reconnects via legacy fallback

**PASS (code inspection)**

In the daemon reconnect block (app.rs:1204): `let legacy_pane_id = tab.pane_id;`
In `reconnect_pane_tree` signature: `legacy_pane_id: Option<uuid::Uuid>` parameter.
In Leaf arm (app.rs:1831): `let uid = pane_id.or(legacy_pane_id);`

For a T-055 file: `Leaf.pane_id = None`, `TabState.pane_id = Some(uuid-X)`.
`uid = None.or(Some(uuid-X)) = Some(uuid-X)` -> `pane_map.remove(&uuid-X)` -> reconnects correctly.

For a T-057 file: `Leaf.pane_id = Some(uuid-A)`.
`uid = Some(uuid-A).or(legacy_pane_id) = Some(uuid-A)` -> correct, `legacy_pane_id` not used.

Split children receive `legacy_pane_id: None` (app.rs:1917, 1921) -- correct, because T-055 files
never had splits with per-leaf IDs, and passing the tab-level ID to both split children would cause
a double-consume on a single UUID.

---

## Code Quality Notes

### Dead code: `find_first_daemon_pane_id`

The function at app.rs:1630 is no longer called from outside itself (only recursive self-calls).
The compiler emits `warning: function find_first_daemon_pane_id is never used`.
Its doc comment still says it is "used to populate `TabState.pane_id`" -- which is now incorrect
since `snapshot_single_workspace` writes `pane_id: None`.

This is not a correctness issue but the dead function adds ~40 lines of confusion and a build
warning. It should be removed in a follow-up.

### No unit test for 3-level nested split

AC-7 is covered by code inspection (pure recursion, no depth limit) but has no automated test.
A persistence unit test for 3-level nested `PaneTreeState` serialization would be easy to add.

### `reconnect_pane_tree` has 10 parameters

The `#[allow(clippy::too_many_arguments)]` annotation acknowledges this. It mirrors
`build_pane_tree`'s signature style and is appropriate given the current architecture.

---

## Scores

| Dimension      | Score | Notes |
|----------------|-------|-------|
| Completeness   | 9/10  | All 9 ACs addressed; dead code is only gap |
| Correctness    | 9/10  | Save/restore logic and legacy fallback correct; dead-code comment misleads but does not break |
| Robustness     | 8/10  | Missing-pane fallback (`dc.new_tab()`) and `idle_add_local_once` guard both solid; ratio restore could fail silently on first frame if widget not yet realized |
| Code quality   | 7/10  | Dead code + stale doc comment drag down an otherwise clean recursive design |

---

## Overall Verdict

**PASS** -- All 9 ACs are satisfied by code inspection and automated tests. Runtime ACs (AC-2,
AC-3, AC-4, AC-7) require manual testing to close fully.

---

## Manual Testing Steps (for NEEDS_MANUAL items)

### Setup

1. Start the daemon: run `forgetty` in daemon mode (background).
2. Launch GTK: `cargo run --release`.

### AC-2: Split tab restores as one gtk::Paned

1. Open a new tab. Split it horizontally via the split pane action.
2. Run `pwd` in each half to confirm two live panes.
3. Close the GTK window (daemon keeps running).
4. Reopen: `cargo run --release`.
5. **Expected:** one tab with a visible divider between two live terminal halves.
6. **Failure sign:** two separate tabs instead of one split tab.

### AC-3: Divider position preserved

1. In the split tab, drag the divider to ~30% from the left.
2. Close and reopen GTK.
3. **Expected:** divider lands at roughly 30% (within a few percent).
4. **Failure sign:** divider snaps to 50% or to an extreme.

### AC-4: Correct CWD in each pane

1. In the split, `cd /tmp` in the first pane and `cd /var` in the second.
2. Close and reopen GTK.
3. **Expected:** first pane shell prompt shows `/tmp`, second shows `/var`.
4. **Failure sign:** both panes open in the same CWD or in `$HOME`.

### AC-7: 3-pane split

1. Split a tab twice (horizontal, then vertical in one half) to get 3 panes.
2. Close and reopen GTK.
3. **Expected:** one tab with two `gtk::Paned` widgets nesting 3 live panes.
4. **Failure sign:** 3 flat tabs, or partial layout with one pane as a fresh shell unexpectedly.

---

## Recommended Follow-up (not blocking)

- Remove `find_first_daemon_pane_id` function (dead code, misleading doc comment).
- Add a persistence unit test for 3-level nested `PaneTreeState` serialization.
