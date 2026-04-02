# SPEC — T-057: Fix Split-Pane Session Save and Restore (Daemon Mode)

**Task:** T-057
**Date:** 2026-04-02
**Status:** Ready to implement

---

## 1. Summary

When a GTK tab contains split panes (`gtk::Paned` with multiple `DrawingArea` children), closing and reopening the window restores each split pane as a **separate tab** instead of reconstructing the split layout within the original tab.

Session JSON evidence — 4 flat tabs instead of 2 tabs (one split):
```json
{ "title": "opt",  "pane_tree": { "Leaf": { "cwd": "/home/vick" } }, "pane_id": "uuid-1" },
{ "title": "tmp",  "pane_tree": { "Leaf": { "cwd": "/home/vick" } }, "pane_id": "uuid-2" },
{ "title": "vick", "pane_tree": { "Leaf": { "cwd": "/home/vick" } }, "pane_id": "uuid-3" },
{ "title": "vick", "pane_tree": { "Leaf": { "cwd": "/home/vick" } }, "pane_id": "uuid-4" }
```

Self-contained mode (no daemon) does NOT have this bug — `snapshot_pane_tree` and `build_pane_tree` both handle splits correctly in that path.

---

## 2. Root Cause

### B-1 — `PaneTreeState::Leaf` has no `pane_id` field

**File:** `crates/forgetty-workspace/src/workspace.rs`

```rust
// Current — no pane_id in Leaf
Leaf { cwd: PathBuf }
```

`find_first_daemon_pane_id` walks the widget tree and returns only the **first** leaf's daemon UUID, stored at `TabState.pane_id`. For a split tab with pane A and pane B, only pane A's UUID is saved. Pane B's UUID is discarded entirely.

### B-2 — Daemon reconnect ignores `pane_tree`, uses only `TabState.pane_id`

**File:** `crates/forgetty-gtk/src/app.rs`, daemon reconnect block (~line 1166)

The block builds a flat `Vec<(PaneId, title, cwd)>` from `session_tabs.iter()`, reads `tab.pane_id` (single field), and creates **one flat tab per entry**. `tab.pane_tree` (which contains the correct `Split` structure from `snapshot_pane_tree`) is never read. Even if B-1 were fixed, the reconnect would still create flat tabs.

### How both bugs combine to produce the symptom

1. `snapshot_pane_tree` correctly builds a `Split` tree for a split tab — this part works.
2. `find_first_daemon_pane_id` only captures pane A's UUID → `TabState.pane_id = Some(uuid-A)`.
3. On reopen: pane A is matched via `TabState.pane_id` → one flat tab created.
4. Pane B's UUID was never saved → it lands in the "remaining live panes" pass → a second flat tab.
5. Result: 1 split tab → 2 flat tabs.

---

## 3. Schema Changes

### `PaneTreeState::Leaf` — add `pane_id`

**File:** `crates/forgetty-workspace/src/workspace.rs`

```rust
// Before
Leaf {
    cwd: PathBuf,
},

// After
Leaf {
    cwd: PathBuf,
    /// Daemon pane ID for this leaf. None in self-contained mode or
    /// old session files. serde(default) ensures backward compatibility.
    #[serde(default)]
    pane_id: Option<uuid::Uuid>,
},
```

Correct serialized form for a split tab:
```json
{
  "title": "myproject",
  "pane_tree": {
    "Split": {
      "direction": "horizontal",
      "ratio": 0.5,
      "first":  { "Leaf": { "cwd": "/home/vick/foo", "pane_id": "uuid-A" } },
      "second": { "Leaf": { "cwd": "/home/vick/bar", "pane_id": "uuid-B" } }
    }
  },
  "pane_id": null
}
```

---

## 4. Save Path Fix

### 4a. `snapshot_pane_tree` — embed `daemon_pane_id` in every `Leaf`

**File:** `crates/forgetty-gtk/src/app.rs`
**Function:** `snapshot_pane_tree`

In the `DrawingArea` arm, after reading the `cwd`, also read `daemon_pane_id` from `TerminalState` and store it in the `Leaf`:

```rust
// In the DrawingArea arm of snapshot_pane_tree:
let daemon_pane_id = tab_states
    .try_borrow()
    .ok()
    .and_then(|states| states.get(&widget_name).cloned())
    .and_then(|rc| rc.try_borrow().ok().map(|s| s.daemon_pane_id))
    .flatten()
    .map(|pid| pid.0); // PaneId(uuid) → uuid::Uuid

return Some(PaneTreeState::Leaf { cwd, pane_id: daemon_pane_id });
```

### 4b. `snapshot_single_workspace` — stop writing top-level `pane_id`

**File:** `crates/forgetty-gtk/src/app.rs`
**Function:** `snapshot_single_workspace`

Replace `find_first_daemon_pane_id` call with `pane_id: None`:

```rust
// Before
let pane_id = find_first_daemon_pane_id(&container, &ws.tab_states);
tabs.push(TabState { title, pane_tree, pane_id });

// After — pane_id is now per-Leaf inside pane_tree
tabs.push(TabState { title, pane_tree, pane_id: None });
```

---

## 5. Restore Path Fix

### 5a. New helper: `reconnect_pane_tree`

Add a recursive function to `app.rs` that mirrors `build_pane_tree` (self-contained) but uses daemon pane IDs:

```rust
fn reconnect_pane_tree(
    tree: &PaneTreeState,
    pane_map: &mut HashMap<uuid::Uuid, PaneInfo>,
    dc: &Arc<DaemonClient>,
    config: &Config,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    window: &adw::ApplicationWindow,
    tab_view: &adw::TabView,
    // legacy fallback: TabState.pane_id for T-055-era session files
    legacy_pane_id: Option<uuid::Uuid>,
) -> Option<(gtk4::Widget, gtk4::DrawingArea)>
```

**`Leaf` arm:**
1. Determine the pane UUID: `tree.pane_id` first, then `legacy_pane_id` fallback (T-055 compat).
2. If UUID is `Some(uid)` and `pane_map.remove(&uid)` returns a `PaneInfo`:
   - Subscribe, `create_terminal_for_pane` with that pane's `cwd`.
3. Otherwise (pane gone or no UUID):
   - `dc.new_tab()` → fresh pane, subscribe, `create_terminal_for_pane(cwd=None)`.
4. Wire focus tracking, register pane in `tab_states`.
5. Return `Some((pane_vbox.upcast::<gtk4::Widget>(), drawing_area))`.

**`Split` arm:**
1. Recurse: `reconnect_pane_tree(first, ...)` → `(first_widget, first_da)`.
2. Recurse: `reconnect_pane_tree(second, ...)` → `(second_widget, _)`.
3. Create `gtk::Paned`:
   ```rust
   let orientation = if direction == "horizontal" {
       gtk4::Orientation::Horizontal
   } else {
       gtk4::Orientation::Vertical
   };
   let paned = gtk4::Paned::new(orientation);
   paned.set_start_child(Some(&first_widget));
   paned.set_end_child(Some(&second_widget));
   paned.set_hexpand(true);
   paned.set_vexpand(true);
   ```
4. Schedule ratio restore via `glib::idle_add_local_once` (same pattern as `build_pane_tree`):
   ```rust
   let paned_weak = paned.downgrade();
   let ratio = *ratio;
   glib::idle_add_local_once(move || {
       if let Some(p) = paned_weak.upgrade() {
           let size = if orientation == gtk4::Orientation::Horizontal {
               p.width()
           } else {
               p.height()
           };
           if size > 0 {
               p.set_position((size as f64 * ratio) as i32);
           }
       }
   });
   ```
5. Return `Some((paned.upcast::<gtk4::Widget>(), first_da))`.

### 5b. Replace the flat loop in the daemon reconnect block

**File:** `crates/forgetty-gtk/src/app.rs`, ~line 1166

Replace the `for (pane_id, title, cwd) in &ordered` loop with:

```rust
for tab in &session_tabs {
    let legacy_pane_id = tab.pane_id; // backward compat with T-055 session files
    let Some((root_widget, first_da)) = reconnect_pane_tree(
        &tab.pane_tree,
        &mut pane_map,
        dc,
        config,
        &ws.tab_states,
        &ws.focus_tracker,
        &ws.custom_titles,
        &window,
        &ws.tab_view,
        legacy_pane_id,
    ) else {
        tracing::warn!("reconnect_pane_tree failed for tab {:?}", tab.title);
        continue;
    };

    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    container.set_hexpand(true);
    container.set_vexpand(true);
    container.append(&root_widget);
    let page = ws.tab_view.append(&container);
    let tab_title = if tab.title.is_empty() { "shell" } else { &tab.title };
    page.set_title(tab_title);
    register_title_timer(
        &page, &ws.tab_view, &ws.tab_states,
        &ws.focus_tracker, &ws.custom_titles, &window,
    );
    restored = true;
}

// Append remaining live daemon panes not referenced by any session leaf.
for info in pane_map.into_values() {
    // ... existing orphan-pane tab creation (add_new_tab or equivalent) ...
}
ws.tab_view.set_selected_page(&ws.tab_view.nth_page(0).unwrap_or_else(|| ws.tab_view.nth_page(0).unwrap()));
```

The `ordered` variable and its construction can be removed entirely.

---

## 6. Acceptance Criteria

- **AC-1:** Close GTK with a tab containing a horizontal split of two daemon panes → `default.json` contains a `"Split"` `pane_tree` with two `"Leaf"` children, each with a non-null `"pane_id"`. `TabState.pane_id` at top level is `null`.
- **AC-2:** Reopen GTK → the split tab is restored as **one tab** with a `gtk::Paned` containing two live panes. Not two separate flat tabs.
- **AC-3:** Reopen GTK → split divider position is approximately preserved (within ±5% of saved ratio).
- **AC-4:** Reopen GTK → each pane in the restored split shows the correct CWD from its daemon pane.
- **AC-5:** Single-pane tabs (no split) are unaffected — `Leaf { pane_id: Some(...) }` and restore as before.
- **AC-6:** Self-contained mode (no daemon) session save/restore still works. `Leaf.pane_id` defaults to `None`, `build_pane_tree` ignores it, spawns fresh PTYs.
- **AC-7:** Deeply nested splits (3+ panes) save and restore correctly.
- **AC-8:** Old session file (no `pane_id` in `Leaf`) deserializes without error, `pane_id` defaults to `None`, fresh panes created on reconnect.
- **AC-9:** T-055-era session file (`TabState.pane_id` set, `Leaf.pane_id` absent) reconnects the single pane correctly via the `legacy_pane_id` fallback.

---

## 7. Edge Cases

| Case | Handling |
|------|----------|
| One pane of split closed in daemon before reopen | `pane_map.remove` returns `None` → fresh `dc.new_tab()`. Split layout preserved, one pane is fresh shell. |
| Both panes of split closed | Both fall back to `dc.new_tab()`. Layout preserved, both are fresh shells. |
| Deeply nested splits (3+ panes) | Recursive `reconnect_pane_tree` handles arbitrarily deep trees. |
| Ratio restore timing | `idle_add_local_once` guard: `if size > 0`. If widget not yet realized, defaults to 50/50. |
| `pane_map` orphans after reconnect | Appended as flat tabs (existing behavior). |
| Vertical splits | `direction == "vertical"` → `gtk4::Orientation::Vertical`. Handled by same code path. |

---

## 8. Files to Change

| File | Change |
|------|--------|
| `crates/forgetty-workspace/src/workspace.rs` | Add `pane_id: Option<uuid::Uuid>` with `#[serde(default)]` to `PaneTreeState::Leaf` |
| `crates/forgetty-gtk/src/app.rs` | (1) Embed `daemon_pane_id` in `snapshot_pane_tree` leaf arm; (2) Stop writing `TabState.pane_id` in `snapshot_single_workspace`; (3) Add `reconnect_pane_tree` recursive helper; (4) Replace flat `ordered` loop with `reconnect_pane_tree`-based loop |
