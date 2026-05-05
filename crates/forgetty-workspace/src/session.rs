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
pub fn save_session_for(session_id: uuid::Uuid, state: &WorkspaceState) -> std::io::Result<()> {
    persistence::save_session_for(session_id, state)
}

/// Load session state from the UUID-named session file.
///
/// Returns `Ok(None)` when the file does not exist or is corrupt.
pub fn load_session_for(session_id: uuid::Uuid) -> std::io::Result<Option<WorkspaceState>> {
    persistence::load_session_for(session_id)
}

/// List all session UUIDs that have a saved session file.
pub fn list_sessions() -> Vec<uuid::Uuid> {
    persistence::list_sessions()
}

// ---------------------------------------------------------------------------
// Session trash functions (B-002)
// ---------------------------------------------------------------------------

/// Move a session file to trash (browser-model close).
pub fn trash_session_for(session_id: uuid::Uuid) -> std::io::Result<()> {
    persistence::trash_session_for(session_id)
}

/// Restore a session from trash back to the active sessions directory.
pub fn restore_from_trash(session_id: uuid::Uuid) -> std::io::Result<()> {
    persistence::restore_from_trash(session_id)
}

/// Delete a session file permanently (no trash copy).
pub fn delete_session_for(session_id: uuid::Uuid) -> std::io::Result<()> {
    persistence::delete_session_for(session_id)
}

/// List all trashed session UUIDs.
pub fn list_trashed_sessions() -> Vec<uuid::Uuid> {
    persistence::list_trashed_sessions()
}

/// List trashed sessions with metadata.
pub fn list_trashed_sessions_with_info() -> Vec<persistence::TrashedSessionInfo> {
    persistence::list_trashed_sessions_with_info()
}

/// Purge trashed sessions older than `max_days` days.
pub fn purge_old_trash(max_days: u32) {
    persistence::purge_old_trash(max_days)
}

// ---------------------------------------------------------------------------
// P-018 / AD-016: three-bucket lifecycle (active/, sessions/, trash/)
// ---------------------------------------------------------------------------

/// Persist `state` to `sessions/active/{session_id}.json`.
pub fn move_to_active(session_id: uuid::Uuid, state: &WorkspaceState) -> std::io::Result<()> {
    persistence::move_to_active(session_id, state)
}

/// Move the live (active) session file to `sessions/{uuid}.json` (pinned close).
pub fn move_active_to_sessions(session_id: uuid::Uuid) -> std::io::Result<()> {
    persistence::move_active_to_sessions(session_id)
}

/// Move the live (active) session file to `sessions/trash/{uuid}.json` (unpinned close).
pub fn move_active_to_trash(session_id: uuid::Uuid) -> std::io::Result<()> {
    persistence::move_active_to_trash(session_id)
}

/// Delete the live (active) session file (orphan cleanup for unpinned crashes).
pub fn delete_active_for(session_id: uuid::Uuid) -> std::io::Result<()> {
    persistence::delete_active_for(session_id)
}

/// Restore a trashed session back into the active bucket (Undo Close).
pub fn restore_from_trash_to_active(session_id: uuid::Uuid) -> std::io::Result<()> {
    persistence::restore_from_trash_to_active(session_id)
}

/// Scan `active/` for crash orphans. Returns `(uuid, is_pinned)` pairs.
pub fn recover_orphans_in_active() -> Vec<(uuid::Uuid, bool)> {
    persistence::recover_orphans_in_active()
}

/// Run the one-shot P-018 migration to the three-bucket layout.
pub fn run_migration_p018() -> std::io::Result<()> {
    persistence::run_migration_p018()
}

/// Pinned-aware exit move: pinned → `sessions/`, unpinned → `sessions/trash/`.
pub fn pinned_aware_exit_move(session_id: uuid::Uuid, pinned: bool, caller: &str) {
    persistence::pinned_aware_exit_move(session_id, pinned, caller);
}
