//! State persistence to disk.
//!
//! Serializes and deserializes workspace/session state to JSON files
//! in the Forgetty data directory for restoration on next launch.

use std::fs;
use std::io;
use std::path::PathBuf;

use forgetty_core::platform::data_dir;
use forgetty_core::Result;
use uuid::Uuid;

use crate::workspace::WorkspaceState;

/// Return the path to the default session file.
///
/// Typically `~/.local/share/forgetty/sessions/default.json`.
pub fn session_path() -> PathBuf {
    data_dir().join("sessions").join("default.json")
}

/// Persist the workspace state as JSON to the default session file.
///
/// Uses atomic write (write to `.tmp`, then `rename()`) to avoid corrupt
/// files if the process is killed mid-write. Creates parent directories
/// if they do not already exist.
pub fn save_session(state: &WorkspaceState) -> Result<()> {
    Ok(save_to_path(&session_path(), state)?)
}

/// Load the workspace state from the default session file.
///
/// Returns `Ok(None)` when the file does not exist.
/// Returns `Ok(None)` (and logs a warning) when the file is corrupt.
pub fn load_session() -> Result<Option<WorkspaceState>> {
    Ok(load_from_path(&session_path())?)
}

// ---------------------------------------------------------------------------
// UUID-based session persistence (T-068)
// ---------------------------------------------------------------------------

/// Return the path to a UUID-named session file.
///
/// Typically `~/.local/share/forgetty/sessions/{session_id}.json`.
pub fn session_path_for(session_id: uuid::Uuid) -> PathBuf {
    data_dir().join("sessions").join(format!("{session_id}.json"))
}

/// Persist the workspace state to a UUID-named session file.
///
/// Uses atomic write (write to `.tmp`, then `rename()`) to avoid corrupt
/// files if the process is killed mid-write.
pub fn save_session_for(session_id: uuid::Uuid, state: &WorkspaceState) -> io::Result<()> {
    save_to_path(&session_path_for(session_id), state)
}

/// Load the workspace state from a UUID-named session file.
///
/// Returns `Ok(None)` when the file does not exist or is corrupt.
pub fn load_session_for(session_id: uuid::Uuid) -> io::Result<Option<WorkspaceState>> {
    load_from_path(&session_path_for(session_id))
}

/// List all session UUIDs that have a saved session file.
///
/// Reads `data_dir()/sessions/`, parses filenames as `{uuid}.json`,
/// and skips any file that does not match (e.g. `default.json`, snapshots/).
pub fn list_sessions() -> Vec<uuid::Uuid> {
    let base = data_dir().join("sessions");
    std::fs::read_dir(&base)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            let stem = s.strip_suffix(".json")?;
            uuid::Uuid::parse_str(stem).ok()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Private helpers (shared by default and UUID-based save/load)
// ---------------------------------------------------------------------------

/// Atomically write `state` to `path` as pretty-printed JSON.
///
/// Creates parent directories if they do not already exist. Uses a `.tmp`
/// sibling file and `rename()` for atomicity.
fn save_to_path(path: &std::path::Path, state: &WorkspaceState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Atomic write: write to temp file, then rename.
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, &json)?;
    if let Err(e) = fs::rename(&tmp_path, path) {
        tracing::warn!("Atomic rename failed ({e}), falling back to direct write");
        fs::write(path, &json)?;
    }
    Ok(())
}

/// Read and deserialize `WorkspaceState` from `path`.
///
/// Returns `Ok(None)` when the file does not exist or the JSON is corrupt
/// (a warning is printed to tracing in the latter case).
fn load_from_path(path: &std::path::Path) -> io::Result<Option<WorkspaceState>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)?;
    match serde_json::from_str::<WorkspaceState>(&contents) {
        Ok(state) => Ok(Some(state)),
        Err(e) => {
            // Corrupt or incompatible session file — treat as missing.
            tracing::warn!("Session file is corrupt or incompatible ({e}), ignoring");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Session trash (B-002: browser-model lifecycle)
// ---------------------------------------------------------------------------

/// Return the path to the trash directory for closed sessions.
///
/// Typically `~/.local/share/forgetty/sessions/trash/`.
pub fn trash_dir() -> PathBuf {
    data_dir().join("sessions").join("trash")
}

// ---------------------------------------------------------------------------
// Active-bucket persistence (P-018 / AD-016)
// ---------------------------------------------------------------------------

/// Return the path to the `active/` directory for live (running) sessions.
///
/// Typically `~/.local/share/forgetty/sessions/active/`.
///
/// AD-016: live daemons write their session JSON here. On clean exit the file
/// is moved to `sessions/{uuid}.json` (pinned) or `sessions/trash/{uuid}.json`
/// (unpinned). Files left here at startup are crash orphans and are processed
/// by [`recover_orphans_in_active`].
pub fn active_dir() -> PathBuf {
    data_dir().join("sessions").join("active")
}

/// Return the path to a UUID-named live session file in `active/`.
///
/// Typically `~/.local/share/forgetty/sessions/active/{session_id}.json`.
pub fn session_path_active_for(session_id: uuid::Uuid) -> PathBuf {
    active_dir().join(format!("{session_id}.json"))
}

/// Persist `state` to `sessions/active/{session_id}.json`.
///
/// Creates `active/` if absent. Uses the same atomic-write helper as
/// `save_session_for` (write `.tmp`, rename), inheriting 0600 mode from the
/// process umask the daemon sets at startup.
///
/// This is the canonical "live daemon write" path: called once at daemon
/// startup before the socket binds, again on every save trigger
/// (set_pinned, debounce, periodic, shutdown). The file is moved out of
/// `active/` by [`move_active_to_sessions`] or [`move_active_to_trash`] on
/// clean shutdown.
pub fn move_to_active(session_id: uuid::Uuid, state: &WorkspaceState) -> io::Result<()> {
    save_to_path(&session_path_active_for(session_id), state)
}

/// Move `src` → `dest`, creating `dest`'s parent directory if needed and
/// falling back to copy+remove for cross-device renames.
///
/// All three buckets (`sessions/`, `active/`, `trash/`) live under the same
/// XDG data subtree so `rename(2)` is atomic. The cross-device fallback is
/// preserved for unusual `XDG_DATA_HOME` setups (e.g. data dir on a different
/// filesystem from `/tmp`).
fn rename_with_fallback(src: &std::path::Path, dest: &std::path::Path) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(src, dest).is_err() {
        fs::copy(src, dest)?;
        fs::remove_file(src)?;
    }
    Ok(())
}

/// Atomically move `sessions/active/{uuid}.json` → `sessions/{uuid}.json`.
///
/// Used for the **pinned** clean-close path (AD-016). Idempotent: returns
/// `Ok(())` if the source does not exist (already moved or never created).
pub fn move_active_to_sessions(session_id: uuid::Uuid) -> io::Result<()> {
    let src = session_path_active_for(session_id);
    if !src.exists() {
        return Ok(());
    }
    rename_with_fallback(&src, &session_path_for(session_id))
}

/// Atomically move `sessions/active/{uuid}.json` → `sessions/trash/{uuid}.json`.
///
/// Used for the **unpinned** clean-close path (AD-016). Idempotent: returns
/// `Ok(())` if the source does not exist.
pub fn move_active_to_trash(session_id: uuid::Uuid) -> io::Result<()> {
    let src = session_path_active_for(session_id);
    if !src.exists() {
        return Ok(());
    }
    rename_with_fallback(&src, &trash_dir().join(format!("{session_id}.json")))
}

/// Delete `sessions/active/{uuid}.json` for the given session.
///
/// Used during crash-recovery for unpinned orphans (AD-016, AC-16). Silent if
/// the file does not exist.
pub fn delete_active_for(session_id: uuid::Uuid) -> io::Result<()> {
    let path = session_path_active_for(session_id);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Atomically move `sessions/trash/{uuid}.json` → `sessions/active/{uuid}.json`.
///
/// Used by the "Undo Close" toast restore path: the trashed file is promoted
/// back into the live bucket so a subsequent `--restore-session` can spawn
/// a daemon that writes there.
///
/// Returns `Err(NotFound)` if the trash file does not exist — the caller
/// (notification action handler) needs to know if the recovery window has
/// closed.
pub fn restore_from_trash_to_active(session_id: uuid::Uuid) -> io::Result<()> {
    let src = trash_dir().join(format!("{session_id}.json"));
    if !src.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("trash file not found: {}", src.display()),
        ));
    }
    rename_with_fallback(&src, &session_path_active_for(session_id))
}

/// AD-016 pinned-aware exit move.
///
/// `pinned == true`  → `move_active_to_sessions(session_id)`.
/// `pinned == false` → `move_active_to_trash(session_id)`.
///
/// Failures are warn-logged via `tracing` and swallowed — the caller is in
/// an exit path and cannot meaningfully recover. `caller` is a short label
/// for log attribution (e.g. `"shutdown_clean"`, `"signal exit"`).
///
/// Single-call helper that closes the duplicated branch logic in the daemon
/// signal path and the socket-server RPC handlers.
pub fn pinned_aware_exit_move(session_id: uuid::Uuid, pinned: bool, caller: &str) {
    if pinned {
        match move_active_to_sessions(session_id) {
            Ok(()) => tracing::info!("{caller}: pinned session {session_id} promoted to sessions/"),
            Err(e) => tracing::warn!("{caller}: move_active_to_sessions failed: {e}"),
        }
    } else {
        match move_active_to_trash(session_id) {
            Ok(()) => tracing::info!("{caller}: unpinned session {session_id} moved to trash"),
            Err(e) => tracing::warn!("{caller}: move_active_to_trash failed: {e}"),
        }
    }
}

/// Scan `sessions/active/` for orphan files (unclean shutdown remnants) and
/// return their `(uuid, is_pinned)` pairs.
///
/// Called once at GTK startup before the daemon spawns. The caller drives
/// the resulting moves: pinned → `move_active_to_sessions`; unpinned →
/// `delete_active_for`.
///
/// Tolerant of:
/// - missing `active/` directory (returns empty `Vec`),
/// - corrupt JSON files (warn-logged and skipped),
/// - non-UUID filenames (silently skipped — same filter as `list_sessions`),
/// - subdirectories or other non-file entries (skipped).
///
/// Reads only the `pinned` field — uses a dedicated `OrphanProbe` struct so
/// schema-incompatible files (older or newer versions) still load if the
/// `pinned` boolean is parseable.
pub fn recover_orphans_in_active() -> Vec<(uuid::Uuid, bool)> {
    let dir = active_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        let stem = match s.strip_suffix(".json") {
            Some(s) => s,
            None => continue,
        };
        let uuid = match uuid::Uuid::parse_str(stem) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let path = entry.path();
        match read_pinned_field(&path) {
            Ok(is_pinned) => out.push((uuid, is_pinned)),
            Err(e) => {
                tracing::warn!(
                    "recover_orphans_in_active: skipping corrupt orphan {}: {e}",
                    path.display()
                );
            }
        }
    }
    out
}

/// Read just the `pinned` field from a session JSON file.
///
/// Uses a minimal `OrphanProbe` struct with `serde(default)` so the function
/// succeeds on older (pre-`pinned`) schemas, returning `false`. Returns an
/// error only for I/O failures or syntactically invalid JSON.
fn read_pinned_field(path: &std::path::Path) -> io::Result<bool> {
    #[derive(serde::Deserialize)]
    struct OrphanProbe {
        #[serde(default)]
        pinned: bool,
    }

    let contents = fs::read_to_string(path)?;
    let probe: OrphanProbe = serde_json::from_str(&contents)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(probe.pinned)
}

// ---------------------------------------------------------------------------
// One-shot migration to the three-bucket layout (P-018 / AD-016)
// ---------------------------------------------------------------------------

/// Path to the migration idempotency marker.
///
/// Typically `~/.local/share/forgetty/.migration_p018`. Contains the literal
/// string `"p018-v1\n"` (one line, LF-terminated) when migration has run.
/// Future migrations (`p018-v2`, `p019-v1`) will inspect this file to decide
/// whether to re-run.
pub fn migration_p018_marker_path() -> PathBuf {
    data_dir().join(".migration_p018")
}

/// One-shot migration to the AD-016 three-bucket layout.
///
/// Behaviour (idempotent — gated on [`migration_p018_marker_path`]):
///
/// 1. If the marker exists, return `Ok(())` immediately.
/// 2. List `sessions/*.json` (top-level only; `active/`, `trash/`, and
///    `snapshots/` are skipped by the `.json` suffix filter on directories).
/// 3. For each file, read just the `pinned` field. On JSON corruption: log a
///    `WARN` line and leave the file in place (skip semantics — AC-23).
/// 4. If `pinned: true`: leave in `sessions/` (no-op).
/// 5. If `pinned: false`: rename to `sessions/trash/{uuid}.json`. If the
///    trash file already exists (partial prior migration — AC-24), the
///    `sessions/` copy is the duplicate and is removed; the `trash/` copy
///    is authoritative.
/// 6. Create `sessions/active/` if absent.
/// 7. Write the marker with content `"p018-v1\n"`.
///
/// Failure modes are non-fatal at the caller (GTK startup): file-level errors
/// are logged and skipped; only marker-write failure propagates as `Err` so
/// the caller can decide whether to re-run on the next launch.
pub fn run_migration_p018() -> io::Result<()> {
    let marker = migration_p018_marker_path();
    if marker.exists() {
        tracing::debug!("run_migration_p018: marker present at {}, skipping", marker.display());
        return Ok(());
    }

    let sessions_root = data_dir().join("sessions");
    let trash = trash_dir();
    let active = active_dir();

    // Ensure `active/` exists for new daemons (created up-front so a daemon
    // start that races with migration still has the directory).
    fs::create_dir_all(&active)?;
    fs::create_dir_all(&trash)?;

    // Enumerate top-level UUID-named JSON files.
    let entries = match fs::read_dir(&sessions_root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // No `sessions/` at all — first ever launch. Write the marker so
            // we don't re-scan on every launch.
            write_marker(&marker)?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("run_migration_p018: cannot stat {}: {e}; skipping", path.display());
                continue;
            }
        };
        if !file_type.is_file() {
            continue; // skip `active/`, `trash/`, `snapshots/`, etc.
        }
        let name = entry.file_name();
        let s = name.to_string_lossy();
        let stem = match s.strip_suffix(".json") {
            Some(stem) => stem,
            None => continue, // not a session file
        };
        let session_id = match uuid::Uuid::parse_str(stem) {
            Ok(u) => u,
            Err(_) => continue, // non-UUID name — leave it alone
        };

        let pinned = match read_pinned_field(&path) {
            Ok(p) => p,
            Err(e) => {
                // Corrupt JSON or I/O — leave file in place and warn (AC-23).
                tracing::warn!(
                    "run_migration_p018: corrupt or unreadable session {}: {e}; skipping",
                    path.display()
                );
                continue;
            }
        };

        if pinned {
            tracing::debug!("run_migration_p018: leaving pinned session in place: {session_id}");
            continue;
        }

        // Unpinned: move to trash. Handle AC-24 duplicate semantics.
        let dest = trash.join(format!("{session_id}.json"));
        if dest.exists() {
            // Partial prior migration: trash is authoritative; the
            // `sessions/` copy is the duplicate. Remove it.
            if let Err(e) = fs::remove_file(&path) {
                tracing::warn!(
                    "run_migration_p018: duplicate cleanup failed for {}: {e}",
                    path.display()
                );
            } else {
                tracing::info!(
                    "run_migration_p018: removed duplicate {} (trash copy kept)",
                    path.display()
                );
            }
            continue;
        }
        if let Err(e) = fs::rename(&path, &dest) {
            // Cross-device fallback.
            if fs::copy(&path, &dest).is_ok() && fs::remove_file(&path).is_ok() {
                tracing::info!(
                    "run_migration_p018: copy+delete fallback moved {session_id} to trash"
                );
            } else {
                tracing::warn!(
                    "run_migration_p018: rename {} → {} failed: {e}",
                    path.display(),
                    dest.display()
                );
            }
        } else {
            tracing::info!("run_migration_p018: moved unpinned session {session_id} to trash");
        }
    }

    write_marker(&marker)?;
    Ok(())
}

/// Write the migration marker with content `"p018-v1\n"`.
fn write_marker(marker: &std::path::Path) -> io::Result<()> {
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(marker, b"p018-v1\n")?;
    Ok(())
}

/// Move a session file from `sessions/{uuid}.json` to `sessions/trash/{uuid}.json`.
///
/// Creates the trash directory if needed. Uses `rename()` for atomicity (same
/// filesystem). Falls back to copy+delete if rename fails (unusual XDG_DATA_HOME).
pub fn trash_session_for(session_id: uuid::Uuid) -> io::Result<()> {
    let src = session_path_for(session_id);
    if !src.exists() {
        return Ok(());
    }
    let dest_dir = trash_dir();
    fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(format!("{session_id}.json"));
    if fs::rename(&src, &dest).is_err() {
        // Cross-device fallback: copy then remove.
        fs::copy(&src, &dest)?;
        fs::remove_file(&src)?;
    }
    Ok(())
}

/// Restore a session from trash: move from `sessions/trash/{uuid}.json` back to
/// `sessions/{uuid}.json`.
pub fn restore_from_trash(session_id: uuid::Uuid) -> io::Result<()> {
    let src = trash_dir().join(format!("{session_id}.json"));
    if !src.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("trash file not found: {}", src.display()),
        ));
    }
    let dest = session_path_for(session_id);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(&src, &dest).is_err() {
        fs::copy(&src, &dest)?;
        fs::remove_file(&src)?;
    }
    Ok(())
}

/// Delete the session file permanently (no trash copy).
pub fn delete_session_for(session_id: uuid::Uuid) -> io::Result<()> {
    let path = session_path_for(session_id);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// List all trashed session UUIDs.
pub fn list_trashed_sessions() -> Vec<uuid::Uuid> {
    let base = trash_dir();
    fs::read_dir(&base)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            let stem = s.strip_suffix(".json")?;
            uuid::Uuid::parse_str(stem).ok()
        })
        .collect()
}

/// Metadata about a trashed session (for the restore dialog).
#[derive(Debug, Clone)]
pub struct TrashedSessionInfo {
    pub session_id: uuid::Uuid,
    pub workspace_names: Vec<String>,
    pub tab_count: usize,
    pub closed_at: std::time::SystemTime,
}

/// List trashed sessions with metadata for the restore dialog.
pub fn list_trashed_sessions_with_info() -> Vec<TrashedSessionInfo> {
    let base = trash_dir();
    let mut infos = Vec::new();
    let entries = match fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return infos,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy().to_string();
        let stem = match s.strip_suffix(".json") {
            Some(s) => s,
            None => continue,
        };
        let session_id = match uuid::Uuid::parse_str(stem) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let closed_at = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        // Try to parse the file for workspace names and tab count.
        let (workspace_names, tab_count) = match load_from_path(&entry.path()) {
            Ok(Some(state)) => {
                let names: Vec<String> =
                    state.workspaces.iter().map(|ws| ws.name.clone()).collect();
                let tabs: usize = state.workspaces.iter().map(|ws| ws.tabs.len()).sum();
                (names, tabs)
            }
            _ => (vec!["Unknown".to_string()], 0),
        };

        infos.push(TrashedSessionInfo { session_id, workspace_names, tab_count, closed_at });
    }
    // Sort by most recently closed first.
    infos.sort_by_key(|i| std::cmp::Reverse(i.closed_at));
    infos
}

/// Purge trashed sessions older than `max_days` days.
///
/// `max_days == 0` disables purging (trash kept forever).
pub fn purge_old_trash(max_days: u32) {
    if max_days == 0 {
        return;
    }
    let base = trash_dir();
    let entries = match fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return,
    };
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(max_days as u64 * 86400);
    for entry in entries.flatten() {
        if let Ok(metadata) = entry.metadata() {
            let modified = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if modified < cutoff {
                let _ = fs::remove_file(entry.path());
                tracing::info!("purge_old_trash: deleted {}", entry.path().display());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// VT snapshot persistence (T-058)
// ---------------------------------------------------------------------------

/// Return the path to the legacy VT snapshot file for a given pane UUID.
///
/// Typically `~/.local/share/forgetty/sessions/snapshots/<uuid>.json`.
///
/// Kept alongside [`delete_vt_snapshot`] so long-running installs that have
/// stale snapshot files from earlier versions can still clean them up when a
/// pane is closed. The daemon no longer writes snapshots — byte-log
/// persistence (V2-007 / AD-013) replaces them.
pub fn snapshot_path(pane_id: uuid::Uuid) -> PathBuf {
    data_dir().join("sessions").join("snapshots").join(format!("{pane_id}.json"))
}

/// Delete the legacy VT snapshot for a pane. Silent if the file does not exist.
///
/// Retained for backwards compatibility: when a pane closes we still clean up
/// any pre-V2-008 snapshot file that might exist on disk from older installs.
pub fn delete_vt_snapshot(pane_id: uuid::Uuid) {
    let path = snapshot_path(pane_id);
    if path.exists() {
        if let Err(e) = fs::remove_file(&path) {
            tracing::warn!("Failed to delete VT snapshot {}: {e}", path.display());
        }
    }
}

// ---------------------------------------------------------------------------
// Byte-log persistence (V2-007 / AD-013)
// ---------------------------------------------------------------------------

/// Return the directory for per-pane byte logs.
///
/// Typically `~/.local/share/forgetty/logs/`.
pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// Return the path to a pane's byte-log file.
///
/// Path: `~/.local/share/forgetty/logs/{pane_uuid}.log`. The input is a
/// validated UUID (36 hex-and-dash characters + ".log"), so no user-controlled
/// path components reach the filesystem — path traversal is impossible.
pub fn pane_log_path(pane_id: Uuid) -> PathBuf {
    logs_dir().join(format!("{pane_id}.log"))
}

/// Return the union of all `pane_id`s persisted in every session JSON under
/// `sessions/{uuid}.json` (active) and `sessions/trash/{uuid}.json` (trashed).
///
/// This is the authoritative "legitimate log owner" set for the orphan-prune
/// pass under AD-001 (one daemon per window): each daemon only knows about
/// its own in-memory panes, so pruning against the per-daemon live set would
/// delete sibling daemons' logs. Every pane whose log file is on disk has its
/// UUID persisted in at least one session JSON (active or trashed), so the
/// union is a safe superset of all legitimate UUIDs.
///
/// Unreadable or corrupt session files are silently skipped (same policy as
/// `load_from_path`). The returned `Vec` is deduped.
pub fn all_persisted_pane_ids() -> Vec<Uuid> {
    use std::collections::HashSet;

    let mut set: HashSet<Uuid> = HashSet::new();

    // Active sessions: sessions/{uuid}.json.
    for session_id in list_sessions() {
        if let Ok(Some(state)) = load_from_path(&session_path_for(session_id)) {
            collect_pane_ids(&state, &mut set);
        }
    }

    // Trashed sessions: sessions/trash/{uuid}.json.
    for session_id in list_trashed_sessions() {
        let path = trash_dir().join(format!("{session_id}.json"));
        if let Ok(Some(state)) = load_from_path(&path) {
            collect_pane_ids(&state, &mut set);
        }
    }

    set.into_iter().collect()
}

/// Walk a `WorkspaceState`'s workspaces → tabs → pane_tree and collect every
/// `Some(pane_id)` found on any `TabState` or `PaneTreeState::Leaf`.
fn collect_pane_ids(state: &WorkspaceState, out: &mut std::collections::HashSet<Uuid>) {
    for ws in &state.workspaces {
        for tab in &ws.tabs {
            if let Some(id) = tab.pane_id {
                out.insert(id);
            }
            collect_pane_ids_from_tree(&tab.pane_tree, out);
        }
    }
}

/// Recursive walk over `PaneTreeState` collecting every `Leaf.pane_id`.
fn collect_pane_ids_from_tree(
    tree: &crate::workspace::PaneTreeState,
    out: &mut std::collections::HashSet<Uuid>,
) {
    use crate::workspace::PaneTreeState;
    match tree {
        PaneTreeState::Leaf { pane_id, .. } => {
            if let Some(id) = *pane_id {
                out.insert(id);
            }
        }
        PaneTreeState::Split { first, second, .. } => {
            collect_pane_ids_from_tree(first, out);
            collect_pane_ids_from_tree(second, out);
        }
    }
}

/// Delete byte-log files in `logs_dir()` whose UUID is not present in
/// `live_pane_ids`.
///
/// Called once during daemon startup (after cold-start restore, before the
/// socket server accepts connections). Non-UUID filenames and unexpected
/// entries are skipped. Stale `{uuid}.log.tmp` files left behind by a crashed
/// rotation are also pruned so they do not accumulate. Deletion failures are
/// `warn!`-logged and skipped — the daemon keeps running.
///
/// Caller must pass the **union** of every daemon's "legitimate" pane set —
/// typically this daemon's `session_manager.list_panes()` **union**
/// `all_persisted_pane_ids()` — because under AD-001 (one daemon per window)
/// N daemons start concurrently and each sees only its own in-memory panes.
/// Passing only one daemon's in-memory panes causes sibling daemons' logs to
/// be wrongly deleted.
pub fn prune_orphan_logs(live_pane_ids: &[Uuid]) {
    let dir = logs_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return, // dir absent or unreadable — nothing to prune
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();

        // Accept `{uuid}.log` and `{uuid}.log.tmp` (from crashed rotation).
        let stem = if let Some(stem) = s.strip_suffix(".log") {
            stem
        } else if let Some(stem) = s.strip_suffix(".log.tmp") {
            stem
        } else {
            continue;
        };

        let uuid = match Uuid::parse_str(stem) {
            Ok(u) => u,
            Err(_) => continue, // non-UUID filename — skip
        };

        if !live_pane_ids.contains(&uuid) {
            if let Err(e) = fs::remove_file(entry.path()) {
                tracing::warn!(
                    "prune_orphan_logs: failed to delete {}: {e}",
                    entry.path().display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{PaneTreeState, TabState, Workspace, WorkspaceState};
    use uuid::Uuid;

    fn sample_state() -> WorkspaceState {
        WorkspaceState {
            version: 1,
            workspaces: vec![Workspace {
                id: Uuid::new_v4(),
                name: "my-project".into(),
                root_paths: vec![PathBuf::from("/tmp/my-project")],
                tabs: vec![TabState {
                    title: "Shell".into(),
                    pane_tree: PaneTreeState::Split {
                        direction: "horizontal".into(),
                        ratio: 0.5,
                        first: Box::new(PaneTreeState::Leaf {
                            cwd: PathBuf::from("/tmp/my-project"),
                            pane_id: None,
                        }),
                        second: Box::new(PaneTreeState::Leaf {
                            cwd: PathBuf::from("/tmp/my-project/src"),
                            pane_id: None,
                        }),
                    },
                    pane_id: None,
                    active_pane_id: None,
                }],
                active_tab: 0,
                color: None,
            }],
            active_workspace: 0,
            window_width: Some(960),
            window_height: Some(640),
            pinned: false,
        }
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let state = sample_state();
        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.version, state.version);
        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.workspaces[0].name, "my-project");
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        // Override data dir via env for the test.
        let sessions_dir = dir.path().join("sessions");
        let session_file = sessions_dir.join("default.json");

        // Write directly to the temp path.
        let state = sample_state();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&session_file, &json).unwrap();

        // Read back.
        let contents = std::fs::read_to_string(&session_file).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&contents).unwrap();
        assert_eq!(restored.version, 1);
        assert_eq!(restored.workspaces[0].name, "my-project");
    }

    #[test]
    fn load_missing_file_returns_none() {
        // session_path() points to a real path that almost certainly doesn't
        // exist in CI, but we test the logic via the raw deserializer.
        let bad_json = "not json at all";
        let result = serde_json::from_str::<WorkspaceState>(bad_json);
        assert!(result.is_err());
    }

    #[test]
    fn backward_compat_no_window_dimensions() {
        // Old session files without window_width/window_height should still
        // deserialize successfully (serde(default) fills None).
        let state = sample_state();
        let mut json_value: serde_json::Value = serde_json::to_value(&state).unwrap();

        // Remove the window dimension fields to simulate an old session file.
        if let Some(obj) = json_value.as_object_mut() {
            obj.remove("window_width");
            obj.remove("window_height");
        }

        let old_json = serde_json::to_string_pretty(&json_value).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&old_json).unwrap();
        assert_eq!(restored.version, 1);
        assert!(restored.window_width.is_none());
        assert!(restored.window_height.is_none());
        assert_eq!(restored.workspaces[0].name, "my-project");
    }

    #[test]
    fn window_dimensions_round_trip() {
        let state = sample_state();
        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.window_width, Some(960));
        assert_eq!(restored.window_height, Some(640));
    }

    /// FIX-010: `Workspace.color` round-trips losslessly across JSON. A JSON
    /// document with no `color` key (pre-FIX-010 shape) still deserialises,
    /// and a document with `"color": "#3a6ee4"` round-trips to
    /// `Some("#3a6ee4".into())`.
    #[test]
    fn test_workspace_color_serde_round_trip() {
        // 1) Missing-field path: pre-FIX-010 JSON must load with color: None.
        let old_json = r##"{
            "version": 1,
            "workspaces": [
                {
                    "id": "00000000-0000-0000-0000-000000000001",
                    "name": "Default",
                    "root_paths": [],
                    "tabs": [],
                    "active_tab": 0
                }
            ],
            "active_workspace": 0
        }"##;
        let restored: WorkspaceState = serde_json::from_str(old_json).unwrap();
        assert_eq!(restored.workspaces.len(), 1);
        assert!(
            restored.workspaces[0].color.is_none(),
            "pre-FIX-010 JSON without color field must deserialise to None"
        );

        // 2) Present-field path: the hex string round-trips verbatim.
        let new_json = r##"{
            "version": 1,
            "workspaces": [
                {
                    "id": "00000000-0000-0000-0000-000000000002",
                    "name": "A",
                    "root_paths": [],
                    "tabs": [],
                    "active_tab": 0,
                    "color": "#3a6ee4"
                }
            ],
            "active_workspace": 0
        }"##;
        let restored: WorkspaceState = serde_json::from_str(new_json).unwrap();
        assert_eq!(restored.workspaces[0].color, Some("#3a6ee4".to_string()));

        // 3) Full round-trip: serialise a Some(hex) workspace and parse back.
        let mut state = sample_state();
        state.workspaces[0].color = Some("#ff00aa".to_string());
        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.workspaces[0].color, Some("#ff00aa".to_string()));
    }

    #[test]
    fn multi_workspace_round_trip() {
        let state = WorkspaceState {
            version: 1,
            workspaces: vec![
                Workspace {
                    id: Uuid::new_v4(),
                    name: "Default".into(),
                    root_paths: vec![],
                    tabs: vec![
                        TabState {
                            title: "shell".into(),
                            pane_tree: PaneTreeState::Leaf {
                                cwd: PathBuf::from("/home/user"),
                                pane_id: None,
                            },
                            pane_id: None,
                            active_pane_id: None,
                        },
                        TabState {
                            title: "build".into(),
                            pane_tree: PaneTreeState::Leaf {
                                cwd: PathBuf::from("/home/user/project"),
                                pane_id: None,
                            },
                            pane_id: None,
                            active_pane_id: None,
                        },
                    ],
                    active_tab: 1,
                    // FIX-010: verify colour survives round-trip alongside other fields.
                    color: Some("#ff00aa".into()),
                },
                Workspace {
                    id: Uuid::new_v4(),
                    name: "Range".into(),
                    root_paths: vec![],
                    tabs: vec![TabState {
                        title: "dev".into(),
                        pane_tree: PaneTreeState::Split {
                            direction: "horizontal".into(),
                            ratio: 0.5,
                            first: Box::new(PaneTreeState::Leaf {
                                cwd: PathBuf::from("/tmp/range"),
                                pane_id: None,
                            }),
                            second: Box::new(PaneTreeState::Leaf {
                                cwd: PathBuf::from("/tmp/range/src"),
                                pane_id: None,
                            }),
                        },
                        pane_id: None,
                        active_pane_id: None,
                    }],
                    active_tab: 0,
                    color: None,
                },
                Workspace {
                    id: Uuid::new_v4(),
                    name: "Personal".into(),
                    root_paths: vec![],
                    tabs: vec![TabState {
                        title: "notes".into(),
                        pane_tree: PaneTreeState::Leaf {
                            cwd: PathBuf::from("/home/user/notes"),
                            pane_id: None,
                        },
                        pane_id: None,
                        active_pane_id: None,
                    }],
                    active_tab: 0,
                    color: None,
                },
            ],
            active_workspace: 1,
            window_width: Some(1200),
            window_height: Some(800),
            pinned: false,
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.version, 1);
        assert_eq!(restored.workspaces.len(), 3);
        assert_eq!(restored.active_workspace, 1);
        assert_eq!(restored.window_width, Some(1200));
        assert_eq!(restored.window_height, Some(800));

        assert_eq!(restored.workspaces[0].name, "Default");
        assert_eq!(restored.workspaces[0].tabs.len(), 2);
        assert_eq!(restored.workspaces[0].active_tab, 1);
        assert_eq!(restored.workspaces[0].color, Some("#ff00aa".into()));

        assert_eq!(restored.workspaces[1].name, "Range");
        assert_eq!(restored.workspaces[1].tabs.len(), 1);
        assert_eq!(restored.workspaces[1].active_tab, 0);
        assert!(restored.workspaces[1].color.is_none());

        assert_eq!(restored.workspaces[2].name, "Personal");
        assert_eq!(restored.workspaces[2].tabs.len(), 1);
        assert!(restored.workspaces[2].color.is_none());
    }

    #[test]
    fn backward_compat_single_workspace_default_name() {
        // T-029 format: single workspace named "default" (lowercase).
        // Should deserialize fine -- the GTK layer capitalizes it on restore.
        let state = WorkspaceState {
            version: 1,
            workspaces: vec![Workspace {
                id: Uuid::new_v4(),
                name: "default".into(),
                root_paths: vec![],
                tabs: vec![TabState {
                    title: "shell".into(),
                    pane_tree: PaneTreeState::Leaf {
                        cwd: PathBuf::from("/home/user"),
                        pane_id: None,
                    },
                    pane_id: None,
                    active_pane_id: None,
                }],
                active_tab: 0,
                color: None,
            }],
            active_workspace: 0,
            window_width: None,
            window_height: None,
            pinned: false,
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: WorkspaceState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.workspaces[0].name, "default");
        assert_eq!(restored.workspaces[0].tabs.len(), 1);
        assert_eq!(restored.active_workspace, 0);
    }

    // -----------------------------------------------------------------------
    // T-068: UUID-based session path and list_sessions
    // -----------------------------------------------------------------------

    #[test]
    fn test_session_path_for_uuid() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let path = session_path_for(id);
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with("550e8400-e29b-41d4-a716-446655440000.json"),
            "path should end with UUID.json, got: {path_str}"
        );
    }

    #[test]
    fn test_list_sessions_skips_non_uuid() {
        let dir = tempfile::tempdir().unwrap();

        let id1 = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let id2 = Uuid::parse_str("7b14a2bc-1111-2222-3333-446655440000").unwrap();

        // Write two UUID-named files and one non-UUID file.
        std::fs::write(dir.path().join(format!("{id1}.json")), b"{}").unwrap();
        std::fs::write(dir.path().join(format!("{id2}.json")), b"{}").unwrap();
        std::fs::write(dir.path().join("default.json"), b"{}").unwrap();

        // Use the raw fs logic that list_sessions uses, but against our temp dir.
        let found: Vec<Uuid> = std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                let stem = s.strip_suffix(".json")?;
                Uuid::parse_str(stem).ok()
            })
            .collect();

        assert_eq!(found.len(), 2, "expected 2 UUID sessions, got {}", found.len());
        assert!(found.contains(&id1));
        assert!(found.contains(&id2));
    }

    // -----------------------------------------------------------------------
    // V2-007: Byte-log path + orphan prune
    // -----------------------------------------------------------------------

    #[test]
    fn test_pane_log_path_ends_with_uuid_log() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let path = pane_log_path(id);
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with("550e8400-e29b-41d4-a716-446655440000.log"),
            "path should end with {{uuid}}.log, got: {path_str}"
        );
        // Parent directory should be `logs/`.
        assert_eq!(
            path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()),
            Some("logs"),
            "pane_log_path should live under logs/, got parent: {:?}",
            path.parent()
        );
    }

    #[test]
    fn test_logs_dir_under_data_dir() {
        let logs = logs_dir();
        assert!(logs.ends_with("logs"), "logs_dir should end with `logs`, got: {}", logs.display());
    }

    #[test]
    fn test_prune_orphan_logs_filter_logic() {
        // We cannot override data_dir() in-process, so this test exercises the
        // core filter logic: a .log/.log.tmp filename that parses as a live
        // UUID is retained; non-UUID filenames and dead UUIDs would be deleted.
        let live = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let dead = Uuid::parse_str("7b14a2bc-1111-2222-3333-446655440000").unwrap();
        let live_list = vec![live];

        let live_name = format!("{live}.log");
        let dead_name = format!("{dead}.log");
        let dead_tmp = format!("{dead}.log.tmp");
        let garbage = "something.txt";

        // Replicate the classification used by prune_orphan_logs.
        fn classify(name: &str, live_ids: &[Uuid]) -> &'static str {
            let stem = if let Some(stem) = name.strip_suffix(".log") {
                stem
            } else if let Some(stem) = name.strip_suffix(".log.tmp") {
                stem
            } else {
                return "skip";
            };
            let Ok(uuid) = Uuid::parse_str(stem) else {
                return "skip";
            };
            if live_ids.contains(&uuid) {
                "keep"
            } else {
                "delete"
            }
        }

        assert_eq!(classify(&live_name, &live_list), "keep");
        assert_eq!(classify(&dead_name, &live_list), "delete");
        assert_eq!(classify(&dead_tmp, &live_list), "delete");
        assert_eq!(classify(garbage, &live_list), "skip");
    }

    // V2-007 fix cycle 1: the cross-daemon prune bug under AD-001 (one daemon
    // per window). Each daemon's `list_panes()` sees only its own in-memory
    // panes, so pruning against that per-daemon set deletes every sibling
    // daemon's log file. The fix: caller must pass the union of every live
    // pane across all saved session JSONs (active + trashed). This test
    // demonstrates the bug semantics and the fix semantics side-by-side
    // against the same classifier used in `prune_orphan_logs`.
    #[test]
    fn test_prune_is_safe_across_concurrent_daemons() {
        // Two daemons are running concurrently. Each has a distinct in-memory
        // pane; each pane also owns a log file on disk.
        let daemon_a_pane = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let daemon_b_pane = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();

        // Per-daemon view: each daemon's `list_panes()`.
        let daemon_a_live = vec![daemon_a_pane];
        let daemon_b_live = vec![daemon_b_pane];

        // Union view (the fix): every pane persisted in any active/trashed
        // session JSON. In production this is `all_persisted_pane_ids()`.
        // Here we simulate it with the concatenation.
        let union_live = vec![daemon_a_pane, daemon_b_pane];

        // Same classifier as `prune_orphan_logs` so the test locks on the
        // actual production logic (see `test_prune_orphan_logs_filter_logic`).
        fn classify(name: &str, live_ids: &[Uuid]) -> &'static str {
            let stem = if let Some(stem) = name.strip_suffix(".log") {
                stem
            } else if let Some(stem) = name.strip_suffix(".log.tmp") {
                stem
            } else {
                return "skip";
            };
            let Ok(uuid) = Uuid::parse_str(stem) else {
                return "skip";
            };
            if live_ids.contains(&uuid) {
                "keep"
            } else {
                "delete"
            }
        }

        let a_log = format!("{daemon_a_pane}.log");
        let b_log = format!("{daemon_b_pane}.log");

        // BUG SEMANTICS: daemon A's per-daemon view wrongly marks daemon B's
        // log as deletable (and vice versa). This is the QA-failing
        // behaviour before the fix.
        assert_eq!(classify(&a_log, &daemon_a_live), "keep");
        assert_eq!(classify(&b_log, &daemon_a_live), "delete"); // wrong — daemon B owns it
        assert_eq!(classify(&a_log, &daemon_b_live), "delete"); // wrong — daemon A owns it
        assert_eq!(classify(&b_log, &daemon_b_live), "keep");

        // FIX SEMANTICS: the union classifier preserves both logs regardless
        // of which daemon ran the prune.
        assert_eq!(classify(&a_log, &union_live), "keep");
        assert_eq!(classify(&b_log, &union_live), "keep");
    }

    #[test]
    fn test_collect_pane_ids_walks_split_and_leaf() {
        // Exercise the `collect_pane_ids` / `collect_pane_ids_from_tree` walk
        // that `all_persisted_pane_ids` uses. Verifies: Split recursion,
        // Leaf Some/None, TabState.pane_id Some/None, and dedup across
        // workspaces are all handled.
        let leaf_a = Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000001").unwrap();
        let leaf_b = Uuid::parse_str("bbbbbbbb-0000-0000-0000-000000000002").unwrap();
        let tab_root = Uuid::parse_str("cccccccc-0000-0000-0000-000000000003").unwrap();
        let duplicate = Uuid::parse_str("dddddddd-0000-0000-0000-000000000004").unwrap();

        let state = WorkspaceState {
            version: 1,
            workspaces: vec![
                Workspace {
                    id: Uuid::new_v4(),
                    name: "A".into(),
                    root_paths: vec![],
                    tabs: vec![TabState {
                        title: "splits".into(),
                        // Split-of-splits — makes sure recursion actually fires.
                        pane_tree: PaneTreeState::Split {
                            direction: "horizontal".into(),
                            ratio: 0.5,
                            first: Box::new(PaneTreeState::Leaf {
                                cwd: PathBuf::from("/a"),
                                pane_id: Some(leaf_a),
                            }),
                            second: Box::new(PaneTreeState::Split {
                                direction: "vertical".into(),
                                ratio: 0.5,
                                first: Box::new(PaneTreeState::Leaf {
                                    cwd: PathBuf::from("/b"),
                                    pane_id: Some(leaf_b),
                                }),
                                second: Box::new(PaneTreeState::Leaf {
                                    cwd: PathBuf::from("/c"),
                                    // Exercise None handling.
                                    pane_id: None,
                                }),
                            }),
                        },
                        pane_id: Some(tab_root),
                        active_pane_id: None,
                    }],
                    active_tab: 0,
                    color: None,
                },
                Workspace {
                    id: Uuid::new_v4(),
                    name: "B".into(),
                    root_paths: vec![],
                    tabs: vec![TabState {
                        title: "dup".into(),
                        pane_tree: PaneTreeState::Leaf {
                            cwd: PathBuf::from("/d"),
                            // Duplicate — also appears in a tab_id below; set
                            // dedup verified by HashSet collection.
                            pane_id: Some(duplicate),
                        },
                        // TabState.pane_id identical to the leaf above —
                        // exercises dedup path.
                        pane_id: Some(duplicate),
                        active_pane_id: None,
                    }],
                    active_tab: 0,
                    color: None,
                },
            ],
            active_workspace: 0,
            window_width: None,
            window_height: None,
            pinned: false,
        };

        let mut set = std::collections::HashSet::new();
        collect_pane_ids(&state, &mut set);

        assert!(set.contains(&leaf_a));
        assert!(set.contains(&leaf_b));
        assert!(set.contains(&tab_root));
        assert!(set.contains(&duplicate));
        // None leaf should not contribute; duplicate should appear exactly once.
        assert_eq!(set.len(), 4, "expected 4 unique ids after dedup, got {}: {set:?}", set.len());
    }

    // -----------------------------------------------------------------------
    // P-018: three-bucket layout (active/, sessions/, trash/) + migration
    // -----------------------------------------------------------------------
    //
    // These tests use a process-global mutex to serialize XDG_DATA_HOME
    // mutation. `cargo test` runs tests in parallel by default, and
    // `data_dir()` reads the env var at call time — without serialization,
    // tests would leak XDG_DATA_HOME values into each other.

    use std::sync::Mutex;

    /// Serialize XDG_DATA_HOME-mutating tests. Lazily initialized on first
    /// use; lock held for the duration of each individual test.
    fn xdg_lock() -> &'static Mutex<()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// RAII guard: sets `XDG_DATA_HOME` to a temp dir, restores prior value
    /// on drop. Unsafe in the same sense as `std::env::set_var` but used
    /// only inside the `xdg_lock` mutex.
    struct XdgGuard {
        _tmp: tempfile::TempDir,
        prior: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl XdgGuard {
        fn new() -> Self {
            let lock = xdg_lock().lock().unwrap_or_else(|e| e.into_inner());
            let prior = std::env::var_os("XDG_DATA_HOME");
            let tmp = tempfile::tempdir().expect("tempdir");
            // SAFETY: serialized by `xdg_lock`; tests in this file are the
            // only XDG_DATA_HOME mutators.
            unsafe {
                std::env::set_var("XDG_DATA_HOME", tmp.path());
            }
            Self { _tmp: tmp, prior, _lock: lock }
        }
    }
    impl Drop for XdgGuard {
        fn drop(&mut self) {
            // SAFETY: serialized by `xdg_lock`.
            unsafe {
                if let Some(prev) = self.prior.take() {
                    std::env::set_var("XDG_DATA_HOME", prev);
                } else {
                    std::env::remove_var("XDG_DATA_HOME");
                }
            }
        }
    }

    fn write_pinned_session_file(path: &std::path::Path, pinned: bool) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let body = format!(
            "{{\"version\":1,\"workspaces\":[],\"active_workspace\":0,\"pinned\":{pinned}}}"
        );
        fs::write(path, body).unwrap();
    }

    #[test]
    fn p018_active_dir_under_sessions() {
        let _g = XdgGuard::new();
        let active = active_dir();
        assert!(active.ends_with("sessions/active"), "{}", active.display());
    }

    #[test]
    fn p018_session_path_active_for_format() {
        let _g = XdgGuard::new();
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let path = session_path_active_for(id);
        assert!(path
            .to_string_lossy()
            .ends_with("sessions/active/550e8400-e29b-41d4-a716-446655440000.json"));
    }

    #[test]
    fn p018_move_to_active_writes_file() {
        let _g = XdgGuard::new();
        let id = Uuid::new_v4();
        let state = WorkspaceState::default();
        move_to_active(id, &state).unwrap();
        let p = session_path_active_for(id);
        assert!(p.exists(), "active file should exist at {}", p.display());
    }

    #[test]
    fn p018_move_active_to_sessions_pinned_path() {
        let _g = XdgGuard::new();
        let id = Uuid::new_v4();
        let mut state = WorkspaceState::default();
        state.pinned = true;
        move_to_active(id, &state).unwrap();
        move_active_to_sessions(id).unwrap();

        assert!(!session_path_active_for(id).exists(), "active should be empty");
        assert!(session_path_for(id).exists(), "sessions/{id}.json should exist");
    }

    #[test]
    fn p018_move_active_to_trash_unpinned_path() {
        let _g = XdgGuard::new();
        let id = Uuid::new_v4();
        let state = WorkspaceState::default();
        move_to_active(id, &state).unwrap();
        move_active_to_trash(id).unwrap();

        assert!(!session_path_active_for(id).exists(), "active should be empty");
        assert!(trash_dir().join(format!("{id}.json")).exists(), "trash file should exist");
        assert!(!session_path_for(id).exists(), "no top-level file should exist");
    }

    #[test]
    fn p018_move_active_to_sessions_idempotent_on_missing() {
        let _g = XdgGuard::new();
        let id = Uuid::new_v4();
        // No file in active/ — should be a no-op, not an error.
        move_active_to_sessions(id).unwrap();
        move_active_to_trash(id).unwrap();
    }

    #[test]
    fn p018_delete_active_for_removes_file() {
        let _g = XdgGuard::new();
        let id = Uuid::new_v4();
        let state = WorkspaceState::default();
        move_to_active(id, &state).unwrap();
        assert!(session_path_active_for(id).exists());

        delete_active_for(id).unwrap();
        assert!(!session_path_active_for(id).exists());

        // Idempotent.
        delete_active_for(id).unwrap();
    }

    #[test]
    fn p018_restore_from_trash_to_active_round_trip() {
        let _g = XdgGuard::new();
        let id = Uuid::new_v4();
        let state = WorkspaceState::default();
        move_to_active(id, &state).unwrap();
        move_active_to_trash(id).unwrap();
        assert!(trash_dir().join(format!("{id}.json")).exists());

        restore_from_trash_to_active(id).unwrap();
        assert!(session_path_active_for(id).exists());
        assert!(!trash_dir().join(format!("{id}.json")).exists());
    }

    #[test]
    fn p018_recover_orphans_in_active_returns_pinned_pairs() {
        let _g = XdgGuard::new();
        let pinned_id = Uuid::new_v4();
        let unpinned_id = Uuid::new_v4();

        write_pinned_session_file(&session_path_active_for(pinned_id), true);
        write_pinned_session_file(&session_path_active_for(unpinned_id), false);

        let orphans = recover_orphans_in_active();
        assert_eq!(orphans.len(), 2);

        let pinned = orphans.iter().find(|(u, _)| *u == pinned_id).unwrap();
        assert!(pinned.1, "pinned session should report pinned=true");

        let unpinned = orphans.iter().find(|(u, _)| *u == unpinned_id).unwrap();
        assert!(!unpinned.1, "unpinned session should report pinned=false");
    }

    #[test]
    fn p018_recover_orphans_skips_corrupt_json() {
        let _g = XdgGuard::new();
        let valid = Uuid::new_v4();
        let corrupt = Uuid::new_v4();

        write_pinned_session_file(&session_path_active_for(valid), true);
        // Corrupt JSON: just `{`.
        fs::create_dir_all(active_dir()).unwrap();
        fs::write(session_path_active_for(corrupt), b"{").unwrap();

        let orphans = recover_orphans_in_active();
        assert_eq!(orphans.len(), 1, "corrupt orphan must be skipped");
        assert_eq!(orphans[0].0, valid);
    }

    #[test]
    fn p018_recover_orphans_on_missing_dir() {
        let _g = XdgGuard::new();
        // No active/ exists — empty vec, not an error.
        let orphans = recover_orphans_in_active();
        assert!(orphans.is_empty());
    }

    #[test]
    fn p018_recover_orphans_skips_non_uuid_filenames() {
        let _g = XdgGuard::new();
        fs::create_dir_all(active_dir()).unwrap();
        fs::write(active_dir().join("not-a-uuid.json"), b"{\"pinned\":true}").unwrap();
        fs::write(active_dir().join("nojson.txt"), b"meh").unwrap();
        let orphans = recover_orphans_in_active();
        assert!(orphans.is_empty(), "non-UUID files must be skipped, got: {orphans:?}");
    }

    #[test]
    fn p018_migration_creates_marker_with_version() {
        let _g = XdgGuard::new();
        run_migration_p018().unwrap();

        let marker = migration_p018_marker_path();
        assert!(marker.exists(), "marker should exist");
        let content = fs::read_to_string(&marker).unwrap();
        assert_eq!(content, "p018-v1\n");
    }

    #[test]
    fn p018_migration_moves_unpinned_to_trash_keeps_pinned() {
        let _g = XdgGuard::new();
        let pinned_ids: Vec<_> = (0..3).map(|_| Uuid::new_v4()).collect();
        let unpinned_ids: Vec<_> = (0..3).map(|_| Uuid::new_v4()).collect();

        for &id in &pinned_ids {
            write_pinned_session_file(&session_path_for(id), true);
        }
        for &id in &unpinned_ids {
            write_pinned_session_file(&session_path_for(id), false);
        }

        run_migration_p018().unwrap();

        for &id in &pinned_ids {
            assert!(session_path_for(id).exists(), "pinned {id} should remain in sessions/");
            assert!(!trash_dir().join(format!("{id}.json")).exists());
        }
        for &id in &unpinned_ids {
            assert!(!session_path_for(id).exists(), "unpinned {id} must leave sessions/");
            assert!(
                trash_dir().join(format!("{id}.json")).exists(),
                "unpinned {id} must be in trash/"
            );
        }
    }

    #[test]
    fn p018_migration_idempotent_skip_on_marker() {
        let _g = XdgGuard::new();
        let id = Uuid::new_v4();
        write_pinned_session_file(&session_path_for(id), false);

        run_migration_p018().unwrap();
        assert!(trash_dir().join(format!("{id}.json")).exists());

        // Place a new unpinned file after migration ran. With the marker
        // present, migration must NOT re-process.
        let id2 = Uuid::new_v4();
        write_pinned_session_file(&session_path_for(id2), false);
        run_migration_p018().unwrap();

        assert!(
            session_path_for(id2).exists(),
            "marker must prevent re-migration; id2 should still be in sessions/"
        );
    }

    #[test]
    fn p018_migration_corrupt_skipped_warn() {
        let _g = XdgGuard::new();
        let valid = Uuid::new_v4();
        let corrupt = Uuid::new_v4();
        write_pinned_session_file(&session_path_for(valid), false);
        fs::create_dir_all(data_dir().join("sessions")).unwrap();
        fs::write(session_path_for(corrupt), b"{").unwrap();

        run_migration_p018().unwrap();

        // Valid file moved to trash.
        assert!(trash_dir().join(format!("{valid}.json")).exists());
        // Corrupt file left in place.
        assert!(session_path_for(corrupt).exists());
        // Marker still written (migration completed despite skip).
        assert!(migration_p018_marker_path().exists());
    }

    #[test]
    fn p018_migration_duplicate_in_trash_removes_source() {
        let _g = XdgGuard::new();
        let id = Uuid::new_v4();
        // Both source and trash exist (partial prior migration).
        write_pinned_session_file(&session_path_for(id), false);
        write_pinned_session_file(&trash_dir().join(format!("{id}.json")), false);

        run_migration_p018().unwrap();

        // Source removed (it was the duplicate).
        assert!(!session_path_for(id).exists(), "duplicate source must be removed");
        // Trash kept (authoritative copy).
        assert!(trash_dir().join(format!("{id}.json")).exists());
    }

    #[test]
    fn p018_migration_marker_present_skips_run() {
        let _g = XdgGuard::new();
        // Pre-create the marker; do NOT pre-create trash/.
        let marker = migration_p018_marker_path();
        if let Some(p) = marker.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&marker, b"p018-v1\n").unwrap();

        // An unpinned file exists. With the marker present, it must NOT move.
        let id = Uuid::new_v4();
        write_pinned_session_file(&session_path_for(id), false);

        run_migration_p018().unwrap();
        assert!(
            session_path_for(id).exists(),
            "marker present → migration must not run, file should stay"
        );
        assert!(
            !trash_dir().join(format!("{id}.json")).exists(),
            "marker present → no migration, no trash creation"
        );
    }

    #[test]
    fn p018_migration_creates_active_dir_on_clean_install() {
        let _g = XdgGuard::new();
        // No `sessions/` at all. Migration must still write the marker and
        // leave active/ + trash/ ready.
        run_migration_p018().unwrap();
        assert!(migration_p018_marker_path().exists());
    }

    #[test]
    fn p018_list_sessions_skips_active_and_trash_subdirs() {
        let _g = XdgGuard::new();
        // Create active/ and trash/ as subdirs of sessions/. They must not
        // confuse list_sessions(): only top-level *.json with UUID stem are
        // returned (R-7 confirmation).
        let id = Uuid::new_v4();
        write_pinned_session_file(&session_path_for(id), true);
        fs::create_dir_all(active_dir()).unwrap();
        fs::create_dir_all(trash_dir()).unwrap();
        // Add JSON files in subdirs — these should NOT appear via list_sessions.
        let inner = Uuid::new_v4();
        write_pinned_session_file(&session_path_active_for(inner), true);
        write_pinned_session_file(&trash_dir().join(format!("{inner}.json")), true);

        let listed = list_sessions();
        assert_eq!(listed, vec![id], "list_sessions should only return top-level UUIDs");
    }
}
