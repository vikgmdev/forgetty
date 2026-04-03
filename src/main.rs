mod cli;

use clap::Parser;
use forgetty_config::{load_config, OnLaunch};
use forgetty_gtk::app::LaunchOptions;
use tracing_subscriber::EnvFilter;

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

    // --restore-all: enumerate saved sessions and spawn one forgetty process per UUID.
    if args.restore_all {
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
        let sessions = forgetty_workspace::list_sessions();
        if !sessions.is_empty() {
            tracing::info!("restore-by-default: restoring {} session(s)", sessions.len());
            let current_exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Error: could not determine current executable: {e}");
                    std::process::exit(1);
                }
            };
            for session_uuid in sessions {
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
                            "restore-by-default: failed to spawn session {session_uuid}: {e}"
                        );
                    }
                }
            }
            return;
        }
        // No saved sessions — fall through to open a fresh window.
        tracing::info!("restore-by-default: no saved sessions, opening fresh window");
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
