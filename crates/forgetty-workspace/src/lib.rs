//! Workspace and session management for Forgetty.
//!
//! Handles saving and restoring terminal sessions, managing project
//! workspaces, and persisting state across application restarts.

pub mod persistence;
pub mod project;
pub mod session;
pub mod workspace;

pub use persistence::{
    active_dir, all_persisted_pane_ids, delete_active_for, delete_vt_snapshot, logs_dir,
    migration_p018_marker_path, move_active_to_sessions, move_active_to_trash, move_to_active,
    pane_log_path, pinned_aware_exit_move, prune_orphan_logs, recover_orphans_in_active,
    restore_from_trash_to_active, run_migration_p018, session_path_active_for, snapshot_path,
    TrashedSessionInfo,
};
pub use project::{find_project_root, project_name, ProjectType};
pub use session::{
    delete_session_for, list_sessions, list_trashed_sessions, list_trashed_sessions_with_info,
    load_session, load_session_for, purge_old_trash, restore_from_trash, save_session,
    save_session_for, session_path, session_path_for, trash_session_for,
};
pub use workspace::{PaneTreeState, TabState, Workspace, WorkspaceState};
