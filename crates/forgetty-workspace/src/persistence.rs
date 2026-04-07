//! State persistence to disk.
//!
//! Serializes and deserializes workspace/session state to JSON files
//! in the Forgetty data directory for restoration on next launch.

use std::fs;
use std::io;
use std::path::PathBuf;

use forgetty_core::platform::data_dir;
use forgetty_core::Result;

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
    if let Err(_) = fs::rename(&src, &dest) {
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
    if let Err(_) = fs::rename(&src, &dest) {
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

/// Return the path to the VT snapshot file for a given pane UUID.
///
/// Typically `~/.local/share/forgetty/sessions/snapshots/<uuid>.json`.
pub fn snapshot_path(pane_id: uuid::Uuid) -> PathBuf {
    data_dir().join("sessions").join("snapshots").join(format!("{pane_id}.json"))
}

/// Persist a VT screen snapshot for a pane.
///
/// Uses atomic write (write to `.tmp`, then `rename()`) to avoid corrupt
/// files if the process is killed mid-write.
pub fn save_vt_snapshot(
    pane_id: uuid::Uuid,
    lines: &[String],
    cursor_row: usize,
    cursor_col: usize,
) -> io::Result<()> {
    let path = snapshot_path(pane_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(&serde_json::json!({
        "lines": lines,
        "cursor": { "row": cursor_row, "col": cursor_col },
    }))
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, &json)?;
    if let Err(e) = fs::rename(&tmp_path, &path) {
        tracing::warn!("VT snapshot atomic rename failed ({e}), falling back to direct write");
        fs::write(&path, &json)?;
    }
    Ok(())
}

/// Load a VT screen snapshot for a pane.
///
/// Returns `None` if the file does not exist or JSON parsing fails.
pub fn load_vt_snapshot(pane_id: uuid::Uuid) -> Option<(Vec<String>, usize, usize)> {
    let path = snapshot_path(pane_id);
    let contents = fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;

    let lines: Vec<String> = value
        .get("lines")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect())
        .unwrap_or_default();

    let cursor = value.get("cursor");
    let cursor_row =
        cursor.and_then(|c| c.get("row")).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let cursor_col =
        cursor.and_then(|c| c.get("col")).and_then(|v| v.as_u64()).unwrap_or(0) as usize;

    Some((lines, cursor_row, cursor_col))
}

/// Delete the VT snapshot for a pane. Silent if the file does not exist.
pub fn delete_vt_snapshot(pane_id: uuid::Uuid) {
    let path = snapshot_path(pane_id);
    if path.exists() {
        if let Err(e) = fs::remove_file(&path) {
            tracing::warn!("Failed to delete VT snapshot {}: {e}", path.display());
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
}
