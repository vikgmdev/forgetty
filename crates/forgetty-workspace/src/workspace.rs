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
}

/// The state of a single tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabState {
    /// Tab title shown in the tab bar.
    pub title: String,
    /// Tree of panes within the tab.
    pub pane_tree: PaneTreeState,
}

/// Recursive tree describing the pane layout inside a tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaneTreeState {
    /// A single pane (leaf node).
    Leaf {
        /// Working directory of the shell in this pane.
        cwd: PathBuf,
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
        Self { version: 1, workspaces: Vec::new(), active_workspace: 0 }
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
                pane_tree: PaneTreeState::Leaf { cwd: root },
            }],
            active_tab: 0,
        }
    }
}
