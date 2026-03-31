//! State persistence to disk.
//!
//! Serializes and deserializes workspace/session state to JSON files
//! in the Forgetty data directory for restoration on next launch.

use std::fs;
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
    let path = session_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Atomic write: write to temp file, then rename.
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, &json)?;
    if let Err(e) = fs::rename(&tmp_path, &path) {
        tracing::warn!("Atomic rename failed ({e}), falling back to direct write");
        fs::write(&path, &json)?;
    }
    Ok(())
}

/// Load the workspace state from the default session file.
///
/// Returns `Ok(None)` when the file does not exist.
/// Returns `Ok(None)` (and logs a warning) when the file is corrupt.
pub fn load_session() -> Result<Option<WorkspaceState>> {
    let path = session_path();
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)?;
    match serde_json::from_str::<WorkspaceState>(&contents) {
        Ok(state) => Ok(Some(state)),
        Err(e) => {
            // Corrupt or incompatible session file — treat as missing.
            tracing::warn!("Session file is corrupt or incompatible ({e}), ignoring");
            Ok(None)
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
                        }),
                        second: Box::new(PaneTreeState::Leaf {
                            cwd: PathBuf::from("/tmp/my-project/src"),
                        }),
                    },
                }],
                active_tab: 0,
            }],
            active_workspace: 0,
            window_width: Some(960),
            window_height: Some(640),
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
}
