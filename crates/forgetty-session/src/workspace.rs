//! Workspace layout types and the `snapshot_workspace()` helper.
//!
//! `WorkspaceLayout` is populated by the GTK widget-tree walker and passed to
//! `SessionManager::snapshot_workspace()`, which fills in live CWD and title
//! from the session's pane registry.

use std::path::PathBuf;

use forgetty_core::PaneId;
use forgetty_workspace::{PaneTreeState, TabState, Workspace, WorkspaceState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Layout types (GTK-agnostic description of the widget tree)
// ---------------------------------------------------------------------------

/// Platform-agnostic description of the full workspace layout, as recorded by
/// the GTK (or any other platform) widget-tree walker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceLayout {
    pub workspaces: Vec<WorkspaceLayoutEntry>,
    pub active_workspace: usize,
    pub window_width: Option<i32>,
    pub window_height: Option<i32>,
}

/// One workspace entry in the layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceLayoutEntry {
    pub id: Uuid,
    pub name: String,
    pub tabs: Vec<TabLayoutEntry>,
    pub active_tab: usize,
}

/// One tab entry in the layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabLayoutEntry {
    pub title: String,
    pub pane_tree: PaneTreeLayout,
}

/// Recursive tree describing the pane layout inside a tab (platform-agnostic).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaneTreeLayout {
    /// A single terminal pane.
    Leaf { pane_id: PaneId },
    /// Two panes side-by-side or stacked.
    Split {
        /// `"horizontal"` or `"vertical"`.
        direction: String,
        /// Ratio of the first child (0.0..=1.0).
        ratio: f32,
        first: Box<PaneTreeLayout>,
        second: Box<PaneTreeLayout>,
    },
}

// ---------------------------------------------------------------------------
// snapshot_workspace: build WorkspaceState from layout + live pane data
// ---------------------------------------------------------------------------

/// Build a `WorkspaceState` from a `WorkspaceLayout` by resolving live CWD
/// from each pane's session state.
///
/// The `get_cwd` closure receives a `PaneId` and returns the live CWD for
/// that pane (read from `/proc/{pid}/cwd` or cached in the pane's session
/// record). This keeps the workspace module free of direct `/proc` access.
pub fn build_workspace_state<F>(layout: &WorkspaceLayout, get_cwd: F) -> WorkspaceState
where
    F: Fn(PaneId) -> PathBuf,
{
    let workspaces: Vec<Workspace> = layout
        .workspaces
        .iter()
        .map(|ws_entry| {
            let tabs: Vec<TabState> = ws_entry
                .tabs
                .iter()
                .map(|tab_entry| TabState {
                    title: tab_entry.title.clone(),
                    pane_tree: build_pane_tree_state(&tab_entry.pane_tree, &get_cwd),
                })
                .collect();

            Workspace {
                id: ws_entry.id,
                name: ws_entry.name.clone(),
                root_paths: Vec::new(),
                tabs,
                active_tab: ws_entry.active_tab,
            }
        })
        .collect();

    WorkspaceState {
        version: 1,
        workspaces,
        active_workspace: layout.active_workspace,
        window_width: layout.window_width,
        window_height: layout.window_height,
    }
}

fn build_pane_tree_state<F>(tree: &PaneTreeLayout, get_cwd: &F) -> PaneTreeState
where
    F: Fn(PaneId) -> PathBuf,
{
    match tree {
        PaneTreeLayout::Leaf { pane_id } => {
            let cwd = get_cwd(*pane_id);
            PaneTreeState::Leaf { cwd }
        }
        PaneTreeLayout::Split { direction, ratio, first, second } => PaneTreeState::Split {
            direction: direction.clone(),
            ratio: *ratio,
            first: Box::new(build_pane_tree_state(first, get_cwd)),
            second: Box::new(build_pane_tree_state(second, get_cwd)),
        },
    }
}
