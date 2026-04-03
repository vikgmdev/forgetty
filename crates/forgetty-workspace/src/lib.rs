//! Workspace and session management for Forgetty.
//!
//! Handles saving and restoring terminal sessions, managing project
//! workspaces, and persisting state across application restarts.

pub mod persistence;
pub mod project;
pub mod session;
pub mod workspace;

pub use persistence::{delete_vt_snapshot, load_vt_snapshot, save_vt_snapshot, snapshot_path};
pub use project::{find_project_root, project_name, ProjectType};
pub use session::{
    list_sessions, load_session, load_session_for, save_session, save_session_for, session_path,
    session_path_for,
};
pub use workspace::{PaneTreeState, TabState, Workspace, WorkspaceState};
