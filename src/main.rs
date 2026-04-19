mod cli;

use clap::Parser;
use forgetty_config::{load_config, OnLaunch};
use forgetty_gtk::app::LaunchOptions;
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------------------
// Session liveness probe
// ---------------------------------------------------------------------------

/// Return the daemon socket path for a given session UUID.
/// Mirrors the identical logic in `forgetty-gtk/src/app.rs::socket_path_for`.
fn daemon_socket_path(session_id: uuid::Uuid) -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        std::path::PathBuf::from(dir).join(format!("forgetty-{session_id}.sock"))
    } else {
        std::path::PathBuf::from(format!("/tmp/forgetty-{session_id}.sock"))
    }
}

/// Return `true` if a daemon for this session is currently serving a GUI
/// window (i.e. another `forgetty` client is already attached to it).
///
/// Used by restore logic to skip sessions that are already displayed in
/// another window. Returns `false` if:
///   - the daemon is not running at all (socket connect fails), OR
///   - the daemon is running but no GUI is attached (AD-012 orphaned
///     daemon after V2-005 `disconnect`).
///
/// The distinction matters because AD-012 makes daemons survive window
/// close. Before the V2-007 fix cycle 2, this function tested only socket
/// liveness, which silently broke the restore path after a window was
/// closed with X: the socket was still connectable (daemon alive) but
/// no GUI was attached, so the session was wrongly skipped and a fresh
/// daemon was spawned — orphaning the one holding the user's state.
///
/// Implementation: open a probe connection, issue the `is_attached`
/// JSON-RPC method, parse the boolean result. The daemon's handler
/// excludes our probe connection from its count, so we correctly see
/// `false` when we're the only client.
fn is_session_in_use(session_id: uuid::Uuid) -> bool {
    use std::io::{BufRead, BufReader, Write};
    use std::time::Duration;

    let path = daemon_socket_path(session_id);
    let mut stream = match std::os::unix::net::UnixStream::connect(&path) {
        Ok(s) => s,
        // Daemon not running → no one to attach to, not in use.
        Err(_) => return false,
    };

    // Short timeouts — this is a local blocking probe on the main thread
    // before GTK starts. The daemon is either responsive within
    // microseconds on loopback or it's wedged (treat as not-in-use and
    // let the normal launch path decide).
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    let req = br#"{"jsonrpc":"2.0","method":"is_attached","id":1}"#;
    if stream.write_all(req).is_err() || stream.write_all(b"\n").is_err() {
        return false;
    }

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return false;
    }

    let parsed: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => return false,
    };
    parsed.get("result").and_then(|r| r.get("attached")).and_then(|b| b.as_bool()).unwrap_or(false)
}

fn main() {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let args = cli::Args::parse();
    // --version and --help are handled by clap before we reach here.

    tracing::info!("Starting Forgetty v{}", env!("CARGO_PKG_VERSION"));

    // Resolve --config-file (canonicalize relative paths before anything changes CWD).
    let config_path = args.config_file.map(|p| {
        std::fs::canonicalize(&p).unwrap_or_else(|_| {
            tracing::warn!("Could not canonicalize config path {:?}, using as-is", p);
            p
        })
    });

    let config = match load_config(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to load config, using defaults: {e}");
            forgetty_config::Config::default()
        }
    };

    // Validate --working-directory: must exist and be a directory.
    let working_directory = args.working_directory.and_then(|p| match std::fs::canonicalize(&p) {
        Ok(canonical) if canonical.is_dir() => Some(canonical),
        Ok(canonical) => {
            tracing::warn!(
                "--working-directory {:?} is not a directory, falling back to home",
                canonical
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                "--working-directory {:?} does not exist ({}), falling back to home",
                p,
                e
            );
            None
        }
    });

    // --restore-session <UUID>: move the trashed session back and launch it.
    if let Some(restore_uuid) = args.restore_session {
        match forgetty_workspace::restore_from_trash(restore_uuid) {
            Ok(()) => {
                tracing::info!("--restore-session: restored {restore_uuid} from trash");
            }
            Err(e) => {
                tracing::warn!("--restore-session: failed to restore {restore_uuid}: {e}");
                // Fall through to launch the session anyway — if the file is already
                // in sessions/ (e.g. race with another restore), this still works.
            }
        }
        let launch_opts = LaunchOptions { session_id: Some(restore_uuid), ..Default::default() };
        if let Err(e) = forgetty_gtk::app::run(config, launch_opts) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }

    // --restore-all: enumerate saved sessions and spawn one forgetty process per UUID.
    if args.restore_all {
        // Auto-purge old trashed sessions before restoring.
        forgetty_workspace::purge_old_trash(config.session_trash_days);
        let sessions = forgetty_workspace::list_sessions();
        if sessions.is_empty() {
            tracing::info!("--restore-all: no saved sessions found");
        } else {
            tracing::info!("--restore-all: restoring {} session(s)", sessions.len());
            let current_exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Error: could not determine current executable: {e}");
                    std::process::exit(1);
                }
            };
            for session_uuid in sessions {
                // Skip sessions that already have a GUI attached — that session
                // is already open in another window. Spawning a second window
                // would connect two GTK clients to the same single-tenant daemon
                // (violates AD-001/AD-006). A daemon that's running but
                // orphaned (AD-012: survived a window close) is NOT skipped —
                // the whole point of restore is to re-attach to it.
                if is_session_in_use(session_uuid) {
                    tracing::info!("--restore-all: skipping {session_uuid} — GUI already attached");
                    continue;
                }
                match std::process::Command::new(&current_exe)
                    .arg("--session-id")
                    .arg(session_uuid.to_string())
                    .spawn()
                {
                    Ok(child) => {
                        std::mem::forget(child);
                        tracing::info!("--restore-all: spawned session {session_uuid}");
                    }
                    Err(e) => {
                        tracing::warn!(
                            "--restore-all: failed to spawn session {session_uuid}: {e}"
                        );
                    }
                }
            }
        }
        return;
    }

    // Default restore-by-default logic.
    // A "bare launch" is when none of the per-session override flags are set.
    // In that case, if config says Restore and sessions exist, spawn one window
    // per saved session and exit — identical to --restore-all.
    let is_bare_launch = !args.no_restore
        && !args.temp
        && args.session_id.is_none()
        && working_directory.is_none()
        && args.execute.is_empty();

    if is_bare_launch && config.on_launch == OnLaunch::Restore {
        // Auto-purge old trashed sessions on bare launch.
        forgetty_workspace::purge_old_trash(config.session_trash_days);
        let sessions = forgetty_workspace::list_sessions();
        if !sessions.is_empty() {
            // Filter out sessions that already have a GUI attached before
            // deciding whether to restore. A session whose daemon is running
            // but has no GUI attached (AD-012: survived window close) is
            // eligible for restore — that's the point.
            let to_restore: Vec<_> = sessions
                .into_iter()
                .filter(|&uuid| {
                    if is_session_in_use(uuid) {
                        tracing::info!(
                            "restore-by-default: skipping {uuid} — GUI already attached"
                        );
                        false
                    } else {
                        true
                    }
                })
                .collect();

            if !to_restore.is_empty() {
                tracing::info!("restore-by-default: restoring {} session(s)", to_restore.len());
                let current_exe = match std::env::current_exe() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Error: could not determine current executable: {e}");
                        std::process::exit(1);
                    }
                };
                for session_uuid in to_restore {
                    match std::process::Command::new(&current_exe)
                        .arg("--session-id")
                        .arg(session_uuid.to_string())
                        .spawn()
                    {
                        Ok(child) => {
                            std::mem::forget(child);
                            tracing::info!("restore-by-default: spawned session {session_uuid}");
                        }
                        Err(e) => {
                            tracing::warn!(
                                "restore-by-default: failed to spawn {session_uuid}: {e}"
                            );
                        }
                    }
                }
                return;
            }
            // Every saved session already has a GUI attached — fall through
            // to a fresh window rather than double-opening.
            tracing::info!(
                "restore-by-default: all sessions already have GUIs attached, opening fresh window"
            );
        } else {
            // No saved sessions — fall through to open a fresh window.
            tracing::info!("restore-by-default: no saved sessions, opening fresh window");
        }
    }

    let launch_opts = LaunchOptions {
        working_directory,
        command: if args.execute.is_empty() { None } else { Some(args.execute) },
        class: args.class,
        no_restore: args.no_restore,
        session_id: args.session_id,
        restore_all: args.restore_all,
        temp: args.temp,
    };

    if let Err(e) = forgetty_gtk::app::run(config, launch_opts) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
