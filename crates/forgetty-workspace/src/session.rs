//! Terminal session save / restore.
//!
//! Thin convenience layer over [`crate::persistence`] that works with
//! [`WorkspaceState`] and the default session file.

use std::path::PathBuf;

use forgetty_core::Result;

use crate::persistence;
use crate::workspace::WorkspaceState;

/// Return the path to the default session file.
pub fn session_path() -> PathBuf {
    persistence::session_path()
}

/// Persist the current workspace state to the default session file.
pub fn save_session(state: &WorkspaceState) -> Result<()> {
    persistence::save_session(state)
}

/// Load the workspace state from the default session file.
///
/// Returns `Ok(None)` when the file does not exist or is corrupt
/// (a warning is printed to tracing in the latter case).
pub fn load_session() -> Result<Option<WorkspaceState>> {
    persistence::load_session()
}

// ---------------------------------------------------------------------------
// UUID-based session functions (T-068)
// ---------------------------------------------------------------------------

/// Return the path to a UUID-named session file.
pub fn session_path_for(session_id: uuid::Uuid) -> PathBuf {
    persistence::session_path_for(session_id)
}

/// Save session state to the UUID-named session file.
pub fn save_session_for(
    session_id: uuid::Uuid,
    state: &WorkspaceState,
) -> std::io::Result<()> {
    persistence::save_session_for(session_id, state)
}

/// Load session state from the UUID-named session file.
///
/// Returns `Ok(None)` when the file does not exist or is corrupt.
pub fn load_session_for(
    session_id: uuid::Uuid,
) -> std::io::Result<Option<WorkspaceState>> {
    persistence::load_session_for(session_id)
}

/// List all session UUIDs that have a saved session file.
pub fn list_sessions() -> Vec<uuid::Uuid> {
    persistence::list_sessions()
}
