//! Workspace and session management for Forgetty.
//!
//! Handles saving and restoring terminal sessions, managing project
//! workspaces, and persisting state across application restarts.

pub mod persistence;
pub mod project;
pub mod session;
pub mod workspace;

pub use project::{find_project_root, project_name, ProjectType};
pub use session::{load_session, save_session, session_path};
pub use workspace::{PaneTreeState, TabState, Workspace, WorkspaceState};
