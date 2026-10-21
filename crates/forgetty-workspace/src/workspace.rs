//! Workspace management.
//!
//! A workspace represents a collection of terminal sessions associated
//! with a project directory, including window layouts, pane arrangements,
//! and environment state.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Top-level state containing all workspaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Schema version for forward compatibility.
    pub version: u32,
    /// All workspaces.
    pub workspaces: Vec<Workspace>,
    /// Index of the currently active workspace.
    pub active_workspace: usize,
    /// Saved window width (pixels). `None` means use default.
    #[serde(default)]
    pub window_width: Option<i32>,
    /// Saved window height (pixels). `None` means use default.
    #[serde(default)]
    pub window_height: Option<i32>,
    /// Whether this session is pinned. Pinned sessions survive normal close
    /// (they stay in `sessions/` and auto-restore on next launch).
    /// Old session files without this field default to `false`.
    #[serde(default)]
    pub pinned: bool,
    /// Panes created with `remain_on_exit` (`forgetty new --keep`). Such a pane
    /// stays listed after its process exits. Old files default to empty.
    #[serde(default)]
    pub remain_on_exit_panes: Vec<Uuid>,
}

/// A single workspace — a named group of tabs rooted in one or more directories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Unique identifier.
    pub id: Uuid,
    /// Human-readable name (often the project name).
    pub name: String,
    /// Project root directories associated with this workspace.
    pub root_paths: Vec<PathBuf>,
    /// Tabs within this workspace.
    pub tabs: Vec<TabState>,
    /// Index of the currently active tab.
    pub active_tab: usize,
    /// FIX-010: per-workspace accent colour. `"#RRGGBB"` (6 hex digits,
    /// no alpha). Pre-FIX-010 session JSON files omit this field — serde
    /// defaults to `None`. GTK falls back to no custom left border when
    /// `None` (the GTK default row style).
    #[serde(default)]
    pub color: Option<String>,
}

/// The state of a single tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabState {
    /// Tab title shown in the tab bar.
    pub title: String,
    /// Tree of panes within the tab.
    pub pane_tree: PaneTreeState,
    /// Daemon pane ID of the root pane (daemon mode only).
    /// Used to reconnect this tab to the correct live daemon pane after
    /// the GTK window is closed and reopened. `None` in self-contained mode
    /// or for session files written before this field was added.
    #[serde(default)]
    pub pane_id: Option<uuid::Uuid>,
    /// FIX-005B: the pane id of the focused leaf within this tab's pane tree.
    /// Pre-FIX-005B session JSON files omit this field — serde defaults to
    /// `None`. GTK falls back to the leftmost leaf when `None` (the pre-
    /// FIX-005B default behaviour).
    #[serde(default)]
    pub active_pane_id: Option<uuid::Uuid>,
    /// User label of the tab (`set_label`). Empty means none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// User labels of panes in this tab, by pane id (`set_label`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub pane_labels: std::collections::BTreeMap<uuid::Uuid, String>,
}

/// Recursive tree describing the pane layout inside a tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaneTreeState {
    /// A single pane (leaf node).
    Leaf {
        /// Working directory of the shell in this pane.
        cwd: PathBuf,
        /// Daemon pane ID for this leaf. None in self-contained mode or
        /// old session files. serde(default) ensures backward compatibility.
        #[serde(default)]
        pane_id: Option<uuid::Uuid>,
    },
    /// A split containing two children.
    Split {
        /// `"horizontal"` or `"vertical"`.
        direction: String,
        /// Ratio of the first child (0.0 ..= 1.0).
        ratio: f32,
        /// First child.
        first: Box<PaneTreeState>,
        /// Second child.
        second: Box<PaneTreeState>,
    },
}

impl WorkspaceState {
    /// Create a new, empty workspace state.
    pub fn new() -> Self {
        Self {
            version: 1,
            workspaces: Vec::new(),
            active_workspace: 0,
            window_width: None,
            window_height: None,
            pinned: false,
            remain_on_exit_panes: Vec::new(),
        }
    }
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    /// Create a new workspace with the given name and root path.
    pub fn new(name: impl Into<String>, root: PathBuf) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            root_paths: vec![root.clone()],
            tabs: vec![TabState {
                title: String::from("Shell"),
                pane_tree: PaneTreeState::Leaf { cwd: root, pane_id: None },
                pane_id: None,
                active_pane_id: None,
                label: String::new(),
                pane_labels: Default::default(),
            }],
            active_tab: 0,
            color: None,
        }
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn old_session_files_have_no_labels() {
        let old = r#"{"title":"t","pane_tree":{"Leaf":{"cwd":"/","pane_id":null}}}"#;
        let tab: TabState = serde_json::from_str(old).unwrap();
        assert!(tab.label.is_empty() && tab.pane_labels.is_empty());
        // Empty labels are not written, so old readers see the old shape.
        let json = serde_json::to_string(&tab).unwrap();
        assert!(!json.contains("label"), "{json}");
    }

    #[test]
    fn labels_round_trip() {
        let id = uuid::Uuid::new_v4();
        let mut tab: TabState = serde_json::from_str(
            r#"{"title":"t","pane_tree":{"Leaf":{"cwd":"/","pane_id":null}}}"#,
        )
        .unwrap();
        tab.label = "main".into();
        tab.pane_labels.insert(id, "build".into());
        let back: TabState = serde_json::from_str(&serde_json::to_string(&tab).unwrap()).unwrap();
        assert_eq!(back.label, "main");
        assert_eq!(back.pane_labels.get(&id).map(String::as_str), Some("build"));
    }
}
