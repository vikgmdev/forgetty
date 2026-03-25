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
