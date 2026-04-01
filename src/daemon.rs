//! `forgetty-daemon` — headless session daemon.
//!
//! Starts `SessionManager` and the JSON-RPC socket server without any GTK or
//! display-server dependency. Intended to run as a systemd user service so
//! that terminal sessions survive GTK window closures.
//!
//! # Usage
//!
//! ```text
//! forgetty-daemon [OPTIONS]
//!
//! Options:
//!   --foreground           Stay in foreground; compact log to stderr
//!   --show-pairing-qr      Print pairing QR placeholder and exit
//!   --socket-path <PATH>   Override the Unix socket path
//!   --config-file <PATH>   Override the config file path
//! ```

use std::path::PathBuf;

use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use forgetty_config::{load_config, Config};
use forgetty_session::SessionManager;
use forgetty_socket::{handlers, SocketServer};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "forgetty-daemon")]
#[command(about = "Forgetty headless daemon — keeps sessions alive, runs socket server")]
#[command(version)]
struct DaemonArgs {
    /// Stay in foreground and log to stderr (useful for debugging).
    ///
    /// By default the daemon logs without ANSI colours and without timestamps
    /// so that systemd's journal can add its own. With `--foreground` the log
    /// output is compact and coloured for interactive use.
    #[arg(long)]
    foreground: bool,

    /// Print the device-pairing QR code and exit.
    ///
    /// iroh integration is not yet available (T-052). This flag currently
    /// prints a placeholder message.
    #[arg(long)]
    show_pairing_qr: bool,

    /// Override the Unix socket path.
    ///
    /// Defaults to `$XDG_RUNTIME_DIR/forgetty.sock`, falling back to
    /// `/tmp/forgetty.sock` when `XDG_RUNTIME_DIR` is unset.
    #[arg(long)]
    socket_path: Option<PathBuf>,

    /// Path to the config file.
    ///
    /// Defaults to `~/.config/forgetty/config.toml`. If the file does not
    /// exist the daemon warns and continues with built-in defaults.
    #[arg(long)]
    config_file: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(e) = runtime.block_on(main_async()) {
        eprintln!("forgetty-daemon error: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Async main
// ---------------------------------------------------------------------------

async fn main_async() -> std::io::Result<()> {
    let args = DaemonArgs::parse();

    // --show-pairing-qr: print placeholder and exit before anything else starts.
    if args.show_pairing_qr {
        println!("Forgetty pairing QR\n");
        println!("  Not yet available — iroh integration is not configured (T-052).");
        println!("  Once T-052 is complete, scanning this QR will pair your Android device.");
        println!();
        println!("  To pair manually when T-052 is done:");
        println!("    forgetty-daemon --show-pairing-qr");
        return Ok(());
    }

    // Configure tracing.
    //
    // When --foreground: compact + colour output to stderr for interactive debugging.
    // Otherwise: no ANSI, no timestamps — systemd's journal adds its own metadata.
    if args.foreground {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_target(false)
            .compact()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_target(false)
            .with_ansi(false)
            .without_time()
            .init();
    }

    // Load config (canonicalize path first so relative paths survive CWD changes).
    let config_path = args.config_file.map(|p| {
        std::fs::canonicalize(&p).unwrap_or_else(|_| {
            warn!("Could not canonicalize config path {:?}, using as-is", p);
            p
        })
    });

    let _config: Config = match load_config(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to load config, using defaults: {e}");
            Config::default()
        }
    };

    // Create the platform-agnostic session manager.
    let session_manager = SessionManager::new();

    // Resolve the socket path.
    let socket_path = args.socket_path.unwrap_or_else(default_socket_path);

    // Bind the socket server.
    let socket_server = SocketServer::new_with_path(socket_path.clone())?;

    info!("forgetty-daemon started, socket at {}", socket_path.display());

    // Spawn the socket server on the tokio executor.
    let _socket_task = tokio::spawn(async move {
        if let Err(e) = socket_server.run(|req| handlers::dispatch(&req)).await {
            error!("Socket server error: {e}");
        }
    });

    // Wait for SIGTERM (from systemd stop) or SIGINT (Ctrl-C in --foreground mode).
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => { info!("Received SIGTERM"); }
        _ = sigint.recv()  => { info!("Received SIGINT");  }
    }

    info!("forgetty-daemon shutting down");
    session_manager.kill_all();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Default socket path: `$XDG_RUNTIME_DIR/forgetty.sock` or `/tmp/forgetty.sock`.
fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("forgetty.sock")
    } else {
        PathBuf::from("/tmp/forgetty.sock")
    }
}
