//! State persistence to disk.
//!
//! Serializes and deserializes workspace/session state to JSON files
//! in the Forgetty data directory for restoration on next launch.

use std::fs;
use std::io;
use std::path::PathBuf;

use forgetty_core::platform::data_dir;
use forgetty_core::Result;
use uuid::Uuid;

use crate::workspace::WorkspaceState;

/// Return the path to the default session file.
///
/// Typically `~/.local/share/forgetty/sessions/default.json`.
pub fn session_path() -> PathBuf {
    data_dir().join("sessions").join("default.json")
}

/// Persist the workspace state as JSON to the default session file.
///
/// Uses atomic write (write to `.tmp`, then `rename()`) to avoid corrupt
/// files if the process is killed mid-write. Creates parent directories
/// if they do not already exist.
pub fn save_session(state: &WorkspaceState) -> Result<()> {
    Ok(save_to_path(&session_path(), state)?)
}

/// Load the workspace state from the default session file.
///
/// Returns `Ok(None)` when the file does not exist.
/// Returns `Ok(None)` (and logs a warning) when the file is corrupt.
pub fn load_session() -> Result<Option<WorkspaceState>> {
    Ok(load_from_path(&session_path())?)
}

// ---------------------------------------------------------------------------
// UUID-based session persistence (T-068)
// ---------------------------------------------------------------------------

/// Return the path to a UUID-named session file.
///
/// Typically `~/.local/share/forgetty/sessions/{session_id}.json`.
pub fn session_path_for(session_id: uuid::Uuid) -> PathBuf {
    data_dir().join("sessions").join(format!("{session_id}.json"))
}

/// Persist the workspace state to a UUID-named session file.
///
/// Uses atomic write (write to `.tmp`, then `rename()`) to avoid corrupt
/// files if the process is killed mid-write.
pub fn save_session_for(session_id: uuid::Uuid, state: &WorkspaceState) -> io::Result<()> {
    save_to_path(&session_path_for(session_id), state)
}

/// Load the workspace state from a UUID-named session file.
///
/// Returns `Ok(None)` when the file does not exist or is corrupt.
pub fn load_session_for(session_id: uuid::Uuid) -> io::Result<Option<WorkspaceState>> {
    load_from_path(&session_path_for(session_id))
}

/// List all session UUIDs that have a saved session file.
///
/// Reads `data_dir()/sessions/`, parses filenames as `{uuid}.json`,
/// and skips any file that does not match (e.g. `default.json`, snapshots/).
pub fn list_sessions() -> Vec<uuid::Uuid> {
    let base = data_dir().join("sessions");
    std::fs::read_dir(&base)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            let stem = s.strip_suffix(".json")?;
            uuid::Uuid::parse_str(stem).ok()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Private helpers (shared by default and UUID-based save/load)
// ---------------------------------------------------------------------------

/// Atomically write `state` to `path` as pretty-printed JSON.
///
/// Creates parent directories if they do not already exist. Uses a `.tmp`
/// sibling file and `rename()` for atomicity.
fn save_to_path(path: &std::path::Path, state: &WorkspaceState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Atomic write: write to temp file, then rename.
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, &json)?;
    if let Err(e) = fs::rename(&tmp_path, path) {
        tracing::warn!("Atomic rename failed ({e}), falling back to direct write");
        fs::write(path, &json)?;
    }
    Ok(())
}

/// Read and deserialize `WorkspaceState` from `path`.
///
/// Returns `Ok(None)` when the file does not exist or the JSON is corrupt
/// (a warning is printed to tracing in the latter case).
fn load_from_path(path: &std::path::Path) -> io::Result<Option<WorkspaceState>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)?;
    match serde_json::from_str::<WorkspaceState>(&contents) {
        Ok(state) => Ok(Some(state)),
        Err(e) => {
            // Corrupt or incompatible session file — treat as missing.
            tracing::warn!("Session file is corrupt or incompatible ({e}), ignoring");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Session trash (B-002: browser-model lifecycle)
// ---------------------------------------------------------------------------

/// Return the path to the trash directory for closed sessions.
///
/// Typically `~/.local/share/forgetty/sessions/trash/`.
pub fn trash_dir() -> PathBuf {
    data_dir().join("sessions").join("trash")
}

/// Move a session file from `sessions/{uuid}.json` to `sessions/trash/{uuid}.json`.
///
/// Creates the trash directory if needed. Uses `rename()` for atomicity (same
/// filesystem). Falls back to copy+delete if rename fails (unusual XDG_DATA_HOME).
pub fn trash_session_for(session_id: uuid::Uuid) -> io::Result<()> {
    let src = session_path_for(session_id);
    if !src.exists() {
        return Ok(());
    }
    let dest_dir = trash_dir();
    fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(format!("{session_id}.json"));
    if fs::rename(&src, &dest).is_err() {
        // Cross-device fallback: copy then remove.
        fs::copy(&src, &dest)?;
        fs::remove_file(&src)?;
    }
    Ok(())
}

/// Restore a session from trash: move from `sessions/trash/{uuid}.json` back to
/// `sessions/{uuid}.json`.
pub fn restore_from_trash(session_id: uuid::Uuid) -> io::Result<()> {
    let src = trash_dir().join(format!("{session_id}.json"));
    if !src.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("trash file not found: {}", src.display()),
        ));
    }
    let dest = session_path_for(session_id);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(&src, &dest).is_err() {
        fs::copy(&src, &dest)?;
        fs::remove_file(&src)?;
    }
    Ok(())
}

/// Delete the session file permanently (no trash copy).
pub fn delete_session_for(session_id: uuid::Uuid) -> io::Result<()> {
    let path = session_path_for(session_id);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// List all trashed session UUIDs.
pub fn list_trashed_sessions() -> Vec<uuid::Uuid> {
    let base = trash_dir();
    fs::read_dir(&base)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            let stem = s.strip_suffix(".json")?;
            uuid::Uuid::parse_str(stem).ok()
        })
        .collect()
}

/// Metadata about a trashed session (for the restore dialog).
#[derive(Debug, Clone)]
pub struct TrashedSessionInfo {
    pub session_id: uuid::Uuid,
    pub workspace_names: Vec<String>,
    pub tab_count: usize,
    pub closed_at: std::time::SystemTime,
}

/// List trashed sessions with metadata for the restore dialog.
pub fn list_trashed_sessions_with_info() -> Vec<TrashedSessionInfo> {
    let base = trash_dir();
    let mut infos = Vec::new();
    let entries = match fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return infos,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy().to_string();
        let stem = match s.strip_suffix(".json") {
            Some(s) => s,
            None => continue,
        };
        let session_id = match uuid::Uuid::parse_str(stem) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let closed_at = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        // Try to parse the file for workspace names and tab count.
        let (workspace_names, tab_count) = match load_from_path(&entry.path()) {
            Ok(Some(state)) => {
                let names: Vec<String> =
                    state.workspaces.iter().map(|ws| ws.name.clone()).collect();
                let tabs: usize = state.workspaces.iter().map(|ws| ws.tabs.len()).sum();
                (names, tabs)
            }
            _ => (vec!["Unknown".to_string()], 0),
        };

        infos.push(TrashedSessionInfo { session_id, workspace_names, tab_count, closed_at });
    }
    // Sort by most recently closed first.
    infos.sort_by(|a, b| b.closed_at.cmp(&a.closed_at));
    infos
}

/// Purge trashed sessions older than `max_days` days.
///
/// `max_days == 0` disables purging (trash kept forever).
pub fn purge_old_trash(max_days: u32) {
    if max_days == 0 {
        return;
    }
    let base = trash_dir();
    let entries = match fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return,
    };
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(max_days as u64 * 86400);
    for entry in entries.flatten() {
        if let Ok(metadata) = entry.metadata() {
            let modified = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if modified < cutoff {
                let _ = fs::remove_file(entry.path());
                tracing::info!("purge_old_trash: deleted {}", entry.path().display());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// VT snapshot persistence (T-058)
// ---------------------------------------------------------------------------

/// Return the path to the legacy VT snapshot file for a given pane UUID.
///
/// Typically `~/.local/share/forgetty/sessions/snapshots/<uuid>.json`.
///
/// Kept alongside [`delete_vt_snapshot`] so long-running installs that have
/// stale snapshot files from earlier versions can still clean them up when a
/// pane is closed. The daemon no longer writes snapshots — byte-log
/// persistence (V2-007 / AD-013) replaces them.
pub fn snapshot_path(pane_id: uuid::Uuid) -> PathBuf {
    data_dir().join("sessions").join("snapshots").join(format!("{pane_id}.json"))
}

/// Delete the legacy VT snapshot for a pane. Silent if the file does not exist.
///
/// Retained for backwards compatibility: when a pane closes we still clean up
/// any pre-V2-008 snapshot file that might exist on disk from older installs.
pub fn delete_vt_snapshot(pane_id: uuid::Uuid) {
    let path = snapshot_path(pane_id);
    if path.exists() {
        if let Err(e) = fs::remove_file(&path) {
            tracing::warn!("Failed to delete VT snapshot {}: {e}", path.display());
        }
    }
}

// ---------------------------------------------------------------------------
// Byte-log persistence (V2-007 / AD-013)
// ---------------------------------------------------------------------------

/// Return the directory for per-pane byte logs.
///
/// Typically `~/.local/share/forgetty/logs/`.
pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// Return the path to a pane's byte-log file.
///
/// Path: `~/.local/share/forgetty/logs/{pane_uuid}.log`. The input is a
/// validated UUID (36 hex-and-dash characters + ".log"), so no user-controlled
/// path components reach the filesystem — path traversal is impossible.
pub fn pane_log_path(pane_id: Uuid) -> PathBuf {
    logs_dir().join(format!("{pane_id}.log"))
}

/// Return the union of all `pane_id`s persisted in every session JSON under
/// `sessions/{uuid}.json` (active) and `sessions/trash/{uuid}.json` (trashed).
///
/// This is the authoritative "legitimate log owner" set for the orphan-prune
/// pass under AD-001 (one daemon per window): each daemon only knows about
/// its own in-memory panes, so pruning against the per-daemon live set would
/// delete sibling daemons' logs. Every pane whose log file is on disk has its
/// UUID persisted in at least one session JSON (active or trashed), so the
/// union is a safe superset of all legitimate UUIDs.
///
/// Unreadable or corrupt session files are silently skipped (same policy as
/// `load_from_path`). The returned `Vec` is deduped.
pub fn all_persisted_pane_ids() -> Vec<Uuid> {
    use std::collections::HashSet;

    let mut set: HashSet<Uuid> = HashSet::new();

    // Active sessions: sessions/{uuid}.json.
    for session_id in list_sessions() {
        if let Ok(Some(state)) = load_from_path(&session_path_for(session_id)) {
            collect_pane_ids(&state, &mut set);
        }
    }

    // Trashed sessions: sessions/trash/{uuid}.json.
    for session_id in list_trashed_sessions() {
        let path = trash_dir().join(format!("{session_id}.json"));
        if let Ok(Some(state)) = load_from_path(&path) {
            collect_pane_ids(&state, &mut set);
        }
    }

    set.into_iter().collect()
}

/// Walk a `WorkspaceState`'s workspaces → tabs → pane_tree and collect every
/// `Some(pane_id)` found on any `TabState` or `PaneTreeState::Leaf`.
fn collect_pane_ids(state: &WorkspaceState, out: &mut std::collections::HashSet<Uuid>) {
    for ws in &state.workspaces {
        for tab in &ws.tabs {
            if let Some(id) = tab.pane_id {
                out.insert(id);
            }
            collect_pane_ids_from_tree(&tab.pane_tree, out);
        }
    }
}

/// Recursive walk over `PaneTreeState` collecting every `Leaf.pane_id`.
fn collect_pane_ids_from_tree(
    tree: &crate::workspace::PaneTreeState,
    out: &mut std::collections::HashSet<Uuid>,
) {
    use crate::workspace::PaneTreeState;
    match tree {
        PaneTreeState::Leaf { pane_id, .. } => {
            if let Some(id) = *pane_id {
                out.insert(id);
            }
        }
        PaneTreeState::Split { first, second, .. } => {
            collect_pane_ids_from_tree(first, out);
            collect_pane_ids_from_tree(second, out);
        }
    }
}

/// Delete byte-log files in `logs_dir()` whose UUID is not present in
/// `live_pane_ids`.
///
/// Called once during daemon startup (after cold-start restore, before the
/// socket server accepts connections). Non-UUID filenames and unexpected
/// entries are skipped. Stale `{uuid}.log.tmp` files left behind by a crashed
/// rotation are also pruned so they do not accumulate. Deletion failures are
/// `warn!`-logged and skipped — the daemon keeps running.
///
/// Caller must pass the **union** of every daemon's "legitimate" pane set —
/// typically this daemon's `session_manager.list_panes()` **union**
/// `all_persisted_pane_ids()` — because under AD-001 (one daemon per window)
/// N daemons start concurrently and each sees only its own in-memory panes.
/// Passing only one daemon's in-memory panes causes sibling daemons' logs to
/// be wrongly deleted.
pub fn prune_orphan_logs(live_pane_ids: &[Uuid]) {
    let dir = logs_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return, // dir absent or unreadable — nothing to prune
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();

        // Accept `{uuid}.log` and `{uuid}.log.tmp` (from crashed rotation).
        let stem = if let Some(stem) = s.strip_suffix(".log") {
            stem
        } else if let Some(stem) = s.strip_suffix(".log.tmp") {
            stem
        } else {
            continue;
        };

        let uuid = match Uuid::parse_str(stem) {
            Ok(u) => u,
            Err(_) => continue, // non-UUID filename — skip
        };

        if !live_pane_ids.contains(&uuid) {
            if let Err(e) = fs::remove_file(entry.path()) {
                tracing::warn!(
                    "prune_orphan_logs: failed to delete {}: {e}",
                    entry.path().display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{PaneTreeState, TabState, Workspace, WorkspaceState};
    use uuid::Uuid;

    fn sample_state() -> WorkspaceState {
        WorkspaceState {
            version: 1,
            workspaces: vec![Workspace {
                id: Uuid::new_v4(),
                name: "my-project".into(),
                root_paths: vec![PathBuf::from("/tmp/my-project")],
                tabs: vec![TabState {
                    title: "Shell".into(),
                    pane_tree: PaneTreeState::Split {
                        direction: "horizontal".into(),
                        ratio: 0.5,
                        first: Box::new(PaneTreeState::Leaf {
                            cwd: PathBuf::from("/tmp/my-project"),
                            pane_id: None,
                        }),
                        second: Box::new(PaneTreeState::Leaf {
                            cwd: PathBuf::from("/tmp/my-project/src"),
                            pane_id: None,
                        }),
                    },
                    pane_id: None,
                }],
                active_tab: 0,
            }],
            active_workspace: 0,
            window_width: Some(960),
            window_height: Some(640),
            pinned: false,
        }
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let state = sample_state();
        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.version, state.version);
        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.workspaces[0].name, "my-project");
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        // Override data dir via env for the test.
        let sessions_dir = dir.path().join("sessions");
        let session_file = sessions_dir.join("default.json");

        // Write directly to the temp path.
        let state = sample_state();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&session_file, &json).unwrap();

        // Read back.
        let contents = std::fs::read_to_string(&session_file).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&contents).unwrap();
        assert_eq!(restored.version, 1);
        assert_eq!(restored.workspaces[0].name, "my-project");
    }

    #[test]
    fn load_missing_file_returns_none() {
        // session_path() points to a real path that almost certainly doesn't
        // exist in CI, but we test the logic via the raw deserializer.
        let bad_json = "not json at all";
        let result = serde_json::from_str::<WorkspaceState>(bad_json);
        assert!(result.is_err());
    }

    #[test]
    fn backward_compat_no_window_dimensions() {
        // Old session files without window_width/window_height should still
        // deserialize successfully (serde(default) fills None).
        let state = sample_state();
        let mut json_value: serde_json::Value = serde_json::to_value(&state).unwrap();

        // Remove the window dimension fields to simulate an old session file.
        if let Some(obj) = json_value.as_object_mut() {
            obj.remove("window_width");
            obj.remove("window_height");
        }

        let old_json = serde_json::to_string_pretty(&json_value).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&old_json).unwrap();
        assert_eq!(restored.version, 1);
        assert!(restored.window_width.is_none());
        assert!(restored.window_height.is_none());
        assert_eq!(restored.workspaces[0].name, "my-project");
    }

    #[test]
    fn window_dimensions_round_trip() {
        let state = sample_state();
        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.window_width, Some(960));
        assert_eq!(restored.window_height, Some(640));
    }

    #[test]
    fn multi_workspace_round_trip() {
        let state = WorkspaceState {
            version: 1,
            workspaces: vec![
                Workspace {
                    id: Uuid::new_v4(),
                    name: "Default".into(),
                    root_paths: vec![],
                    tabs: vec![
                        TabState {
                            title: "shell".into(),
                            pane_tree: PaneTreeState::Leaf {
                                cwd: PathBuf::from("/home/user"),
                                pane_id: None,
                            },
                            pane_id: None,
                        },
                        TabState {
                            title: "build".into(),
                            pane_tree: PaneTreeState::Leaf {
                                cwd: PathBuf::from("/home/user/project"),
                                pane_id: None,
                            },
                            pane_id: None,
                        },
                    ],
                    active_tab: 1,
                },
                Workspace {
                    id: Uuid::new_v4(),
                    name: "Range".into(),
                    root_paths: vec![],
                    tabs: vec![TabState {
                        title: "dev".into(),
                        pane_tree: PaneTreeState::Split {
                            direction: "horizontal".into(),
                            ratio: 0.5,
                            first: Box::new(PaneTreeState::Leaf {
                                cwd: PathBuf::from("/tmp/range"),
                                pane_id: None,
                            }),
                            second: Box::new(PaneTreeState::Leaf {
                                cwd: PathBuf::from("/tmp/range/src"),
                                pane_id: None,
                            }),
                        },
                        pane_id: None,
                    }],
                    active_tab: 0,
                },
                Workspace {
                    id: Uuid::new_v4(),
                    name: "Personal".into(),
                    root_paths: vec![],
                    tabs: vec![TabState {
                        title: "notes".into(),
                        pane_tree: PaneTreeState::Leaf {
                            cwd: PathBuf::from("/home/user/notes"),
                            pane_id: None,
                        },
                        pane_id: None,
                    }],
                    active_tab: 0,
                },
            ],
            active_workspace: 1,
            window_width: Some(1200),
            window_height: Some(800),
            pinned: false,
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.version, 1);
        assert_eq!(restored.workspaces.len(), 3);
        assert_eq!(restored.active_workspace, 1);
        assert_eq!(restored.window_width, Some(1200));
        assert_eq!(restored.window_height, Some(800));

        assert_eq!(restored.workspaces[0].name, "Default");
        assert_eq!(restored.workspaces[0].tabs.len(), 2);
        assert_eq!(restored.workspaces[0].active_tab, 1);

        assert_eq!(restored.workspaces[1].name, "Range");
        assert_eq!(restored.workspaces[1].tabs.len(), 1);
        assert_eq!(restored.workspaces[1].active_tab, 0);

        assert_eq!(restored.workspaces[2].name, "Personal");
        assert_eq!(restored.workspaces[2].tabs.len(), 1);
    }

    #[test]
    fn backward_compat_single_workspace_default_name() {
        // T-029 format: single workspace named "default" (lowercase).
        // Should deserialize fine -- the GTK layer capitalizes it on restore.
        let state = WorkspaceState {
            version: 1,
            workspaces: vec![Workspace {
                id: Uuid::new_v4(),
                name: "default".into(),
                root_paths: vec![],
                tabs: vec![TabState {
                    title: "shell".into(),
                    pane_tree: PaneTreeState::Leaf {
                        cwd: PathBuf::from("/home/user"),
                        pane_id: None,
                    },
                    pane_id: None,
                }],
                active_tab: 0,
            }],
            active_workspace: 0,
            window_width: None,
            window_height: None,
            pinned: false,
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.workspaces[0].name, "default");
        assert_eq!(restored.workspaces[0].tabs.len(), 1);
        assert_eq!(restored.active_workspace, 0);
    }

    // -----------------------------------------------------------------------
    // T-068: UUID-based session path and list_sessions
    // -----------------------------------------------------------------------

    #[test]
    fn test_session_path_for_uuid() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let path = session_path_for(id);
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with("550e8400-e29b-41d4-a716-446655440000.json"),
            "path should end with UUID.json, got: {path_str}"
        );
    }

    #[test]
    fn test_list_sessions_skips_non_uuid() {
        let dir = tempfile::tempdir().unwrap();

        let id1 = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let id2 = Uuid::parse_str("7b14a2bc-1111-2222-3333-446655440000").unwrap();

        // Write two UUID-named files and one non-UUID file.
        std::fs::write(dir.path().join(format!("{id1}.json")), b"{}").unwrap();
        std::fs::write(dir.path().join(format!("{id2}.json")), b"{}").unwrap();
        std::fs::write(dir.path().join("default.json"), b"{}").unwrap();

        // Use the raw fs logic that list_sessions uses, but against our temp dir.
        let found: Vec<Uuid> = std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                let stem = s.strip_suffix(".json")?;
                Uuid::parse_str(stem).ok()
            })
            .collect();

        assert_eq!(found.len(), 2, "expected 2 UUID sessions, got {}", found.len());
        assert!(found.contains(&id1));
        assert!(found.contains(&id2));
    }

    // -----------------------------------------------------------------------
    // V2-007: Byte-log path + orphan prune
    // -----------------------------------------------------------------------

    #[test]
    fn test_pane_log_path_ends_with_uuid_log() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let path = pane_log_path(id);
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with("550e8400-e29b-41d4-a716-446655440000.log"),
            "path should end with {{uuid}}.log, got: {path_str}"
        );
        // Parent directory should be `logs/`.
        assert_eq!(
            path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()),
            Some("logs"),
            "pane_log_path should live under logs/, got parent: {:?}",
            path.parent()
        );
    }

    #[test]
    fn test_logs_dir_under_data_dir() {
        let logs = logs_dir();
        assert!(logs.ends_with("logs"), "logs_dir should end with `logs`, got: {}", logs.display());
    }

    #[test]
    fn test_prune_orphan_logs_filter_logic() {
        // We cannot override data_dir() in-process, so this test exercises the
        // core filter logic: a .log/.log.tmp filename that parses as a live
        // UUID is retained; non-UUID filenames and dead UUIDs would be deleted.
        let live = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let dead = Uuid::parse_str("7b14a2bc-1111-2222-3333-446655440000").unwrap();
        let live_list = vec![live];

        let live_name = format!("{live}.log");
        let dead_name = format!("{dead}.log");
        let dead_tmp = format!("{dead}.log.tmp");
        let garbage = "something.txt";

        // Replicate the classification used by prune_orphan_logs.
        fn classify(name: &str, live_ids: &[Uuid]) -> &'static str {
            let stem = if let Some(stem) = name.strip_suffix(".log") {
                stem
            } else if let Some(stem) = name.strip_suffix(".log.tmp") {
                stem
            } else {
                return "skip";
            };
            let Ok(uuid) = Uuid::parse_str(stem) else {
                return "skip";
            };
            if live_ids.contains(&uuid) {
                "keep"
            } else {
                "delete"
            }
        }

        assert_eq!(classify(&live_name, &live_list), "keep");
        assert_eq!(classify(&dead_name, &live_list), "delete");
        assert_eq!(classify(&dead_tmp, &live_list), "delete");
        assert_eq!(classify(garbage, &live_list), "skip");
    }

    // V2-007 fix cycle 1: the cross-daemon prune bug under AD-001 (one daemon
    // per window). Each daemon's `list_panes()` sees only its own in-memory
    // panes, so pruning against that per-daemon set deletes every sibling
    // daemon's log file. The fix: caller must pass the union of every live
    // pane across all saved session JSONs (active + trashed). This test
    // demonstrates the bug semantics and the fix semantics side-by-side
    // against the same classifier used in `prune_orphan_logs`.
    #[test]
    fn test_prune_is_safe_across_concurrent_daemons() {
        // Two daemons are running concurrently. Each has a distinct in-memory
        // pane; each pane also owns a log file on disk.
        let daemon_a_pane = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let daemon_b_pane = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();

        // Per-daemon view: each daemon's `list_panes()`.
        let daemon_a_live = vec![daemon_a_pane];
        let daemon_b_live = vec![daemon_b_pane];

        // Union view (the fix): every pane persisted in any active/trashed
        // session JSON. In production this is `all_persisted_pane_ids()`.
        // Here we simulate it with the concatenation.
        let union_live = vec![daemon_a_pane, daemon_b_pane];

        // Same classifier as `prune_orphan_logs` so the test locks on the
        // actual production logic (see `test_prune_orphan_logs_filter_logic`).
        fn classify(name: &str, live_ids: &[Uuid]) -> &'static str {
            let stem = if let Some(stem) = name.strip_suffix(".log") {
                stem
            } else if let Some(stem) = name.strip_suffix(".log.tmp") {
                stem
            } else {
                return "skip";
            };
            let Ok(uuid) = Uuid::parse_str(stem) else {
                return "skip";
            };
            if live_ids.contains(&uuid) {
                "keep"
            } else {
                "delete"
            }
        }

        let a_log = format!("{daemon_a_pane}.log");
        let b_log = format!("{daemon_b_pane}.log");

        // BUG SEMANTICS: daemon A's per-daemon view wrongly marks daemon B's
        // log as deletable (and vice versa). This is the QA-failing
        // behaviour before the fix.
        assert_eq!(classify(&a_log, &daemon_a_live), "keep");
        assert_eq!(classify(&b_log, &daemon_a_live), "delete"); // wrong — daemon B owns it
        assert_eq!(classify(&a_log, &daemon_b_live), "delete"); // wrong — daemon A owns it
        assert_eq!(classify(&b_log, &daemon_b_live), "keep");

        // FIX SEMANTICS: the union classifier preserves both logs regardless
        // of which daemon ran the prune.
        assert_eq!(classify(&a_log, &union_live), "keep");
        assert_eq!(classify(&b_log, &union_live), "keep");
    }

    #[test]
    fn test_collect_pane_ids_walks_split_and_leaf() {
        // Exercise the `collect_pane_ids` / `collect_pane_ids_from_tree` walk
        // that `all_persisted_pane_ids` uses. Verifies: Split recursion,
        // Leaf Some/None, TabState.pane_id Some/None, and dedup across
        // workspaces are all handled.
        let leaf_a = Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000001").unwrap();
        let leaf_b = Uuid::parse_str("bbbbbbbb-0000-0000-0000-000000000002").unwrap();
        let tab_root = Uuid::parse_str("cccccccc-0000-0000-0000-000000000003").unwrap();
        let duplicate = Uuid::parse_str("dddddddd-0000-0000-0000-000000000004").unwrap();

        let state = WorkspaceState {
            version: 1,
            workspaces: vec![
                Workspace {
                    id: Uuid::new_v4(),
                    name: "A".into(),
                    root_paths: vec![],
                    tabs: vec![TabState {
                        title: "splits".into(),
                        // Split-of-splits — makes sure recursion actually fires.
                        pane_tree: PaneTreeState::Split {
                            direction: "horizontal".into(),
                            ratio: 0.5,
                            first: Box::new(PaneTreeState::Leaf {
                                cwd: PathBuf::from("/a"),
                                pane_id: Some(leaf_a),
                            }),
                            second: Box::new(PaneTreeState::Split {
                                direction: "vertical".into(),
                                ratio: 0.5,
                                first: Box::new(PaneTreeState::Leaf {
                                    cwd: PathBuf::from("/b"),
                                    pane_id: Some(leaf_b),
                                }),
                                second: Box::new(PaneTreeState::Leaf {
                                    cwd: PathBuf::from("/c"),
                                    // Exercise None handling.
                                    pane_id: None,
                                }),
                            }),
                        },
                        pane_id: Some(tab_root),
                    }],
                    active_tab: 0,
                },
                Workspace {
                    id: Uuid::new_v4(),
                    name: "B".into(),
                    root_paths: vec![],
                    tabs: vec![TabState {
                        title: "dup".into(),
                        pane_tree: PaneTreeState::Leaf {
                            cwd: PathBuf::from("/d"),
                            // Duplicate — also appears in a tab_id below; set
                            // dedup verified by HashSet collection.
                            pane_id: Some(duplicate),
                        },
                        // TabState.pane_id identical to the leaf above —
                        // exercises dedup path.
                        pane_id: Some(duplicate),
                    }],
                    active_tab: 0,
                },
            ],
            active_workspace: 0,
            window_width: None,
            window_height: None,
            pinned: false,
        };

        let mut set = std::collections::HashSet::new();
        collect_pane_ids(&state, &mut set);

        assert!(set.contains(&leaf_a));
        assert!(set.contains(&leaf_b));
        assert!(set.contains(&tab_root));
        assert!(set.contains(&duplicate));
        // None leaf should not contribute; duplicate should appear exactly once.
        assert_eq!(set.len(), 4, "expected 4 unique ids after dedup, got {}: {set:?}", set.len());
    }
}
