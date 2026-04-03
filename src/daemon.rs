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
//!   --show-pairing-qr      Print the device-pairing QR code (ASCII) and exit
//!   --allow-pairing        Auto-accept next pairing request from any unknown device
//!   --list-devices         List all paired devices and exit
//!   --revoke <DEVICE_ID>   Revoke a paired device by device_id and exit
//!   --socket-path <PATH>   Override the Unix socket path
//!   --config-file <PATH>   Override the config file path
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use forgetty_config::{load_config, Config};
use forgetty_session::{SessionEvent, SessionManager};
use forgetty_socket::SocketServer;
use forgetty_sync::{
    identity::load_or_generate, qr::qr_to_ascii, registry::DeviceRegistry, SyncEndpoint,
};

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

    /// Print the device-pairing QR code (ASCII) and exit.
    ///
    /// Outputs an ASCII QR code encoding the iroh node ID, machine hostname,
    /// and relay URL. Scan with the Forgetty Android app to pair.
    #[arg(long)]
    show_pairing_qr: bool,

    /// Auto-accept the next pairing request from any unknown device.
    ///
    /// Use for initial setup. Do NOT run persistently with this flag in
    /// untrusted environments — it accepts any connecting device.
    #[arg(long)]
    allow_pairing: bool,

    /// List all paired devices and exit.
    #[arg(long)]
    list_devices: bool,

    /// Revoke a paired device by device_id and exit.
    #[arg(long)]
    revoke: Option<String>,

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

async fn main_async() -> anyhow::Result<()> {
    let args = DaemonArgs::parse();

    // --- Early-exit modes that don't need logging or session state ---

    // --list-devices: print and exit.
    if args.list_devices {
        let registry = DeviceRegistry::load()?;
        for d in registry.list() {
            println!("{}: {} (paired {})", d.device_id, d.name, d.paired_at);
        }
        return Ok(());
    }

    // --revoke: remove a device and exit.
    if let Some(device_id) = &args.revoke {
        let mut registry = DeviceRegistry::load()?;
        if registry.remove(device_id)? {
            println!("Revoked {device_id}");
        } else {
            eprintln!("Device not found: {device_id}");
        }
        return Ok(());
    }

    // --show-pairing-qr: needs the identity but not the session manager.
    if args.show_pairing_qr {
        let secret_key = load_or_generate()?;
        let node_id = secret_key.public();
        let payload = forgetty_sync::QrPayload::new(node_id.to_string());
        let ascii = qr_to_ascii(&payload)?;
        println!("{ascii}");
        println!("\nnode_id: {node_id}");
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
    let session_manager = Arc::new(SessionManager::new());

    // Resolve the socket path.
    let socket_path = args.socket_path.unwrap_or_else(default_socket_path);

    // Bind the socket server.
    let socket_server = SocketServer::new_with_path(socket_path.clone())?;

    info!("forgetty-daemon started, socket at {}", socket_path.display());

    // Background drain loop: polls all live panes every 20ms (50 Hz).
    // This drives the session-side VT (for get_screen) and fires
    // SessionEvent::PtyOutput on the broadcast channel (for subscribe_output).
    {
        let sm = Arc::clone(&session_manager);
        tokio::spawn(async move {
            loop {
                let pane_ids = sm.list_panes();
                for id in pane_ids {
                    let _ = sm.drain_output(id);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }

    // Dirty-flag: set to true when a layout mutation event (PaneCreated /
    // PaneClosed) is observed. Shared between the watcher task and the
    // debounce-save task.
    let dirty_flag = Arc::new(AtomicBool::new(false));

    // Layout-event watcher task: subscribes to the broadcast channel and sets
    // the dirty flag on PaneCreated / PaneClosed events.
    {
        let sm = Arc::clone(&session_manager);
        let dirty = Arc::clone(&dirty_flag);
        tokio::spawn(async move {
            let mut rx = sm.subscribe_output();
            loop {
                match rx.recv().await {
                    Ok(SessionEvent::PaneCreated { .. }) | Ok(SessionEvent::PaneClosed { .. }) => {
                        dirty.store(true, Ordering::Relaxed);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Fell behind — treat as dirty (many events fired).
                        dirty.store(true, Ordering::Relaxed);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Debounce-save task: if the dirty flag is set, save once every 5 seconds.
    // This coalesces rapid layout mutations (e.g. three tabs opened at once)
    // into a single disk write.
    {
        let sm = Arc::clone(&session_manager);
        let dirty = Arc::clone(&dirty_flag);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if dirty.swap(false, Ordering::Relaxed) {
                    save_session_from_layout(&sm);
                }
            }
        });
    }

    // Periodic safety-save task: unconditionally saves the layout every 60
    // seconds regardless of the dirty flag. Guarantees at most 60 seconds of
    // layout changes are lost even if the event watcher misses something.
    {
        let sm = Arc::clone(&session_manager);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                save_session_from_layout(&sm);
            }
        });
    }

    // Load identity and bind iroh endpoint.
    let secret_key = load_or_generate()?;
    let sync_endpoint = match SyncEndpoint::bind(
        secret_key,
        args.allow_pairing,
        Arc::clone(&session_manager),
    )
    .await
    {
        Ok(ep) => {
            info!("totem-sync: iroh endpoint bound, node_id={}", ep.node_id());
            Arc::new(ep)
        }
        Err(e) => {
            warn!("totem-sync: failed to bind iroh endpoint: {e}");
            // Non-fatal: daemon continues without sync capability.
            // Wrap in a short early return here would require restructuring; instead
            // we pass None to the socket server below.
            return Err(anyhow::anyhow!("iroh bind failed: {e}"));
        }
    };

    // Spawn iroh accept loop.
    {
        let ep = Arc::clone(&sync_endpoint);
        tokio::spawn(async move {
            ep.accept_loop().await;
        });
    }

    // Spawn the socket server with full SessionManager + SyncEndpoint integration.
    let _socket_task = {
        let sm = Arc::clone(&session_manager);
        let se = Arc::clone(&sync_endpoint);
        tokio::spawn(async move {
            if let Err(e) = socket_server.run_with_streaming(sm, Some(se)).await {
                error!("Socket server error: {e}");
            }
        })
    };

    // Wait for SIGTERM (from systemd stop) or SIGINT (Ctrl-C in --foreground mode).
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => { info!("Received SIGTERM"); }
        _ = sigint.recv()  => { info!("Received SIGINT");  }
    }

    info!("forgetty-daemon shutting down");
    sync_endpoint.close().await;
    let saved = forgetty_socket::save_all_snapshots(&session_manager);
    info!("Saved VT snapshots for {saved} pane(s)");
    save_session_from_layout(&session_manager);
    session_manager.kill_all();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Snapshot the live session layout and write it to `default.json`.
///
/// This is the single call site used by all three save triggers (SIGTERM/SIGINT
/// shutdown, debounced auto-save, and the periodic safety save). Save failures
/// are non-fatal: a warning is logged but no error is propagated.
fn save_session_from_layout(session_manager: &SessionManager) {
    let state = session_manager.snapshot_to_workspace_state();
    let ws_count = state.workspaces.len();
    match forgetty_workspace::save_session(&state) {
        Ok(()) => debug!(ws_count, "daemon: session layout saved to default.json"),
        Err(e) => warn!("daemon: failed to save session layout: {e}"),
    }
}

/// Default socket path: `$XDG_RUNTIME_DIR/forgetty.sock` or `/tmp/forgetty.sock`.
fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("forgetty.sock")
    } else {
        PathBuf::from("/tmp/forgetty.sock")
    }
}
