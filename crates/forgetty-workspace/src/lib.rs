//! Workspace and session management for Forgetty.
//!
//! Handles saving and restoring terminal sessions, managing project
//! workspaces, and persisting state across application restarts.

pub mod persistence;
pub mod project;
pub mod session;
pub mod workspace;

pub use persistence::{
    delete_vt_snapshot, load_vt_snapshot, save_vt_snapshot, snapshot_path, TrashedSessionInfo,
};
pub use project::{find_project_root, project_name, ProjectType};
pub use session::{
    delete_session_for, list_sessions, list_trashed_sessions, list_trashed_sessions_with_info,
    load_session, load_session_for, purge_old_trash, restore_from_trash, save_session,
    save_session_for, session_path, session_path_for, trash_session_for,
};
pub use workspace::{PaneTreeState, TabState, Workspace, WorkspaceState};
