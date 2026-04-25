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
use forgetty_core::PaneId;
use forgetty_pty::PtySize;
use forgetty_session::{SessionEvent, SessionManager};
use forgetty_socket::SocketServer;
use forgetty_sync::{
    identity::load_or_generate, qr::qr_to_ascii, registry::DeviceRegistry, SyncEndpoint,
};

use forgetty_daemon::iroh_terminal::{handle_terminal_stream, FORGETTY_STREAM_ALPN};

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

    /// Session UUID — identifies this daemon instance.
    ///
    /// Determines the socket path (`forgetty-{uuid}.sock`) and the session
    /// file path (`sessions/{uuid}.json`). GTK passes this when spawning the
    /// daemon. If not provided, a new UUID is generated (useful for manual
    /// invocation).
    #[arg(long)]
    session_id: Option<uuid::Uuid>,

    /// Override the Unix socket path.
    ///
    /// Defaults to `$XDG_RUNTIME_DIR/forgetty-{uuid}.sock`, falling back to
    /// `/tmp/forgetty-{uuid}.sock` when `XDG_RUNTIME_DIR` is unset.
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

    // Resolve or generate the session UUID for this daemon instance.
    let session_id = args.session_id.unwrap_or_else(uuid::Uuid::new_v4);

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

    let config: Config = match load_config(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to load config, using defaults: {e}");
            Config::default()
        }
    };

    info!("session_id: {session_id}");

    // Create the platform-agnostic session manager.
    let session_manager = Arc::new(SessionManager::new());

    // V2-007: thread byte-log config values from TOML into SessionManager.
    session_manager.set_byte_log_config(config.byte_log_ring_kb, config.byte_log_max_mb);

    // Cold-start restore: reload the UUID-named session file into the live SessionLayout.
    //
    // This runs synchronously before the socket server starts accepting connections
    // so that by the time GTK connects and calls `get_layout`, the daemon's layout
    // already reflects the last-saved session. Failures are non-fatal — a fresh
    // start (empty layout) is acceptable if the file is absent or corrupt.
    //
    // Split structure is fully reconstructed: each tab is created with its root pane
    // via `create_tab`, then `split_pane_with_ratio` is called recursively to
    // rebuild the split tree with the original ratios preserved.
    {
        let default_size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        match forgetty_workspace::load_session_for(session_id) {
            Ok(Some(state)) if !state.workspaces.is_empty() => {
                let total: usize = state.workspaces.iter().map(|ws| ws.tabs.len()).sum();
                info!(
                    "cold-start restore: found {} workspace(s), {} tab(s) total, pinned={}",
                    state.workspaces.len(),
                    total,
                    state.pinned,
                );

                // Restore pinned state.
                if state.pinned {
                    session_manager.set_pinned(true);
                }

                // Ensure the daemon has enough workspace slots for all saved workspaces.
                // SessionLayout::new_default() creates workspace[0]; create the rest.
                for (ws_idx, saved_ws) in state.workspaces.iter().enumerate() {
                    if ws_idx == 0 {
                        // FIX-001: workspace[0] was seeded by `SessionLayout::new_default()`
                        // with the literal name "Default". The saved name takes precedence —
                        // this is what lets a user's rename of the Default workspace
                        // survive a daemon restart. Ignore errors: a malformed saved
                        // name is non-fatal (restore loop is already tolerant of
                        // per-workspace failures).
                        if let Err(e) = session_manager.rename_workspace(0, &saved_ws.name) {
                            warn!(
                                "cold-start restore: failed to rename workspace 0 to '{}': {e}",
                                saved_ws.name
                            );
                        }
                    } else {
                        let (_, created_idx) = session_manager.create_workspace(&saved_ws.name);
                        debug_assert_eq!(
                            created_idx, ws_idx,
                            "workspace index mismatch during cold-start restore"
                        );
                    }
                    // FIX-010: restore the persisted per-workspace accent colour.
                    // Missing / null colour field in pre-FIX-010 JSON → no call
                    // (deserialises to `None`). Non-fatal if the daemon rejects
                    // the hex — colour is cosmetic.
                    if saved_ws.color.is_some() {
                        if let Err(e) =
                            session_manager.set_workspace_color(ws_idx, saved_ws.color.as_deref())
                        {
                            warn!(
                                "cold-start restore: failed to set colour on workspace {ws_idx}: {e}"
                            );
                        }
                    }
                }

                // Now restore tabs for ALL workspaces, preserving split structure.
                for (ws_idx, workspace) in state.workspaces.iter().enumerate() {
                    for tab in &workspace.tabs {
                        // Create the root pane using the leftmost leaf's CWD.
                        let root_cwd = first_leaf_cwd(&tab.pane_tree);
                        let effective_root_cwd =
                            if root_cwd.is_dir() { Some(root_cwd.to_path_buf()) } else { None };
                        match session_manager.create_tab(
                            ws_idx,
                            effective_root_cwd,
                            default_size,
                            None,
                        ) {
                            Ok((root_pane_id, _tab_id)) => {
                                debug!("cold-start restore: created root pane {root_pane_id} for workspace {ws_idx}");
                                // Restore the full split tree rooted at this pane.
                                restore_subtree(
                                    &session_manager,
                                    root_pane_id,
                                    &tab.pane_tree,
                                    default_size,
                                );
                            }
                            Err(e) => {
                                warn!("cold-start restore: create_tab failed for workspace {ws_idx}: {e}");
                            }
                        }
                    }

                    // FIX-009: heal legacy empty workspaces from pre-FIX-009 sessions.
                    // If a saved workspace has `tabs: []` — either because the user hit
                    // the carcass path before the daemon-side auto-spawn shipped, or
                    // because every restored tab failed to spawn here — give it a
                    // default shell so the user never sees a "0 tabs" sidebar row on
                    // first launch after upgrading.
                    //
                    // Unlike the live-mutation path in `close_tab`, this heal runs for
                    // every empty workspace including workspace 0 — cold restart is a
                    // one-time repair so the user always lands in a usable state.
                    if workspace.tabs.is_empty() {
                        match session_manager.create_tab(ws_idx, None, default_size, None) {
                            Ok(_) => {
                                info!(
                                    "cold-start restore: auto-seeded empty workspace {ws_idx} ({}) with default tab (FIX-009)",
                                    workspace.name
                                );
                            }
                            Err(e) => {
                                warn!(
                                    "cold-start restore: auto-seed for empty workspace {ws_idx} failed: {e}"
                                );
                            }
                        }
                    }

                    // FIX-005A: propagate the saved active_tab into the live SessionLayout.
                    // `create_tab` above intentionally does not advance active_tab (per AD-008 —
                    // that is UI state owned by GTK). We restore it explicitly here as a
                    // control-plane operation, so that `get_layout` returns the correct
                    // active_tab to the GTK client, and `build_widgets_from_layout` honors it.
                    //
                    // `set_active_tab` bounds-checks against the live tab count, which may be
                    // smaller than `workspace.active_tab` if some tabs failed to restore.
                    // Treat any error as a warning and continue — cosmetic restore failure
                    // is strictly better than aborting cold restart.
                    if let Err(e) = session_manager.set_active_tab(ws_idx, workspace.active_tab) {
                        warn!(
                            "cold-start restore: failed to set active_tab={} on workspace {ws_idx}: {e}",
                            workspace.active_tab
                        );
                    }
                }

                // FIX-005A: propagate the saved active_workspace into the live SessionLayout.
                // Matches the active_tab restore above. Bounds-checked; a stale saved index
                // (e.g., active_workspace was the 3rd one but only 2 restored) degrades to
                // a warning.
                if let Err(e) = session_manager.set_active_workspace(state.active_workspace) {
                    warn!(
                        "cold-start restore: failed to set active_workspace={}: {e}",
                        state.active_workspace
                    );
                }
            }
            Ok(Some(_)) => {
                debug!("cold-start restore: session file has no workspaces, starting fresh");
            }
            Ok(None) => {
                debug!("cold-start restore: no session file found, starting fresh");
            }
            Err(e) => {
                warn!("cold-start restore: failed to load session file: {e}");
            }
        }
    }

    // V2-007 AC-10: prune orphan byte-log files (UUID not matching any live pane).
    // Runs AFTER cold-start restore so restored panes' logs are recognised as live,
    // and BEFORE the socket server accepts connections so there is no race.
    //
    // V2-007 fix cycle 1: under AD-001 (one daemon per window) N daemons start
    // concurrently and each only knows about its *own* in-memory panes. Pruning
    // against just this daemon's `list_panes()` would delete every sibling
    // daemon's log file. Instead we pass the **union** of (a) this daemon's
    // freshly-restored in-memory panes (whose UUIDs may not yet be in any
    // saved JSON if a save has not happened since restore) and (b) every
    // pane persisted in any active-or-trashed session JSON on disk. Every
    // legitimate log on disk is in (b) or was just created by (a); the union
    // is therefore a safe superset of all legitimate UUIDs.
    {
        let in_memory: Vec<uuid::Uuid> =
            session_manager.list_panes().into_iter().map(|id| id.0).collect();
        let persisted: Vec<uuid::Uuid> = forgetty_workspace::all_persisted_pane_ids();

        let mut live_ids: Vec<uuid::Uuid> =
            Vec::with_capacity(in_memory.len().saturating_add(persisted.len()));
        live_ids.extend(in_memory.iter().copied());
        live_ids.extend(persisted.iter().copied());
        live_ids.sort();
        live_ids.dedup();

        forgetty_workspace::prune_orphan_logs(&live_ids);
        debug!(
            "prune_orphan_logs: checked against {} union pane(s) ({} in-memory, {} persisted)",
            live_ids.len(),
            in_memory.len(),
            persisted.len(),
        );
    }

    // Resolve the socket path: UUID-based by default, override with --socket-path.
    let socket_path = args.socket_path.unwrap_or_else(|| {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            PathBuf::from(runtime_dir).join(format!("forgetty-{session_id}.sock"))
        } else {
            PathBuf::from(format!("/tmp/forgetty-{session_id}.sock"))
        }
    });

    // Bind the socket server (with session_id so shutdown_save can write the correct file).
    let socket_server = SocketServer::new_with_session(socket_path.clone(), session_id)?;

    info!("forgetty-daemon started, socket at {}", socket_path.display());

    // Per-pane drain tasks are spawned by the SessionManager automatically
    // when each pane is created (via spawn_drain_task inside create_pane /
    // create_tab / split_pane / split_pane_with_ratio). No shared polling loop
    // needed here — each pane wakes its drain task only when PTY bytes arrive.

    // Dirty-flag: set to true when a layout mutation event (PaneCreated /
    // PaneClosed) is observed. Shared between the watcher task and the
    // debounce-save task.
    let dirty_flag = Arc::new(AtomicBool::new(false));

    // Layout-event watcher task: on layout mutations, save immediately AND
    // set the dirty flag so the debounce task continues to coalesce any
    // follow-up changes within its 5-second window. The immediate save is
    // required to close the multi-daemon startup race (V2-007 fix cycle 7)
    // where a sibling daemon's `prune_orphan_logs` runs before the 5-second
    // debounce fires and wrongly deletes the just-created pane's log file
    // (AD-001 × AD-013 interaction). Layout mutations are rare (user-driven
    // open/close/split), so the extra JSON serialize + atomic write per event
    // is cheap (<1 ms on typical WorkspaceState sizes).
    {
        let sm = Arc::clone(&session_manager);
        let dirty = Arc::clone(&dirty_flag);
        tokio::spawn(async move {
            let mut rx = sm.subscribe_output();
            loop {
                match rx.recv().await {
                    Ok(SessionEvent::PaneCreated { .. })
                    | Ok(SessionEvent::PaneClosed { .. })
                    | Ok(SessionEvent::TabCreated { .. })
                    | Ok(SessionEvent::TabClosed { .. })
                    | Ok(SessionEvent::PaneSplit { .. })
                    | Ok(SessionEvent::TabMoved { .. })
                    | Ok(SessionEvent::ActiveTabChanged { .. })
                    | Ok(SessionEvent::ActiveWorkspaceChanged { .. })
                    | Ok(SessionEvent::WorkspaceCreated { .. })
                    | Ok(SessionEvent::WorkspaceRenamed { .. })
                    | Ok(SessionEvent::WorkspaceDeleted { .. })
                    | Ok(SessionEvent::WorkspaceColorChanged { .. })
                    | Ok(SessionEvent::WorkspacesReordered { .. }) => {
                        // Save immediately so sibling daemons' prune passes
                        // see this pane's UUID in the persisted-union (V2-007
                        // fix cycle 7). Leave the dirty flag set as well so
                        // the debounce task coalesces any rapid follow-up
                        // changes within its window (belt-and-suspenders).
                        save_session_from_layout(&sm, session_id);
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
                    save_session_from_layout(&sm, session_id);
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
                save_session_from_layout(&sm, session_id);
            }
        });
    }

    // Load identity and bind iroh endpoint (V2-011 / AD-015).
    //
    // `forgetty-sync` owns only the pairing ALPN. The terminal streaming ALPN
    // is registered here with a closure that forwards to the terminal-side
    // handler in `forgetty_daemon::iroh_terminal`, keeping all terminal-
    // specific behaviour out of the transport crate.
    let secret_key = load_or_generate()?;
    let sm_for_alpn = Arc::clone(&session_manager);
    let sync_endpoint = match SyncEndpoint::builder(secret_key)
        .allow_pairing(args.allow_pairing)
        .register_alpn(
            FORGETTY_STREAM_ALPN,
            Arc::new(move |conn, registry| {
                let sm = Arc::clone(&sm_for_alpn);
                tokio::spawn(async move {
                    handle_terminal_stream(conn, sm, registry).await;
                });
            }),
        )
        .build()
        .await
    {
        Ok(ep) => {
            info!(
                "totem-sync: iroh endpoint bound, node_id={}, alpns=[forgetty/pair/1, forgetty/stream/1]",
                ep.node_id()
            );
            Arc::new(ep)
        }
        Err(e) => {
            warn!("totem-sync: failed to bind iroh endpoint: {e}");
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
    // V2-007 / AD-013: flush per-pane byte-log ring to disk so reconnecting
    // clients (and next cold-start) see up-to-date scrollback.
    session_manager.flush_all_byte_logs().await;
    info!("byte logs flushed");
    save_session_from_layout(&session_manager, session_id);
    session_manager.kill_all();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Snapshot the live session layout and write it to the UUID-named session file.
///
/// This is the single call site used by all three save triggers (SIGTERM/SIGINT
/// shutdown, debounced auto-save, and the periodic safety save). Save failures
/// are non-fatal: a warning is logged but no error is propagated.
fn save_session_from_layout(session_manager: &SessionManager, session_id: uuid::Uuid) {
    let state = session_manager.snapshot_to_workspace_state();
    match forgetty_workspace::save_session_for(session_id, &state) {
        Ok(()) => debug!(session_id = %session_id, "daemon: session layout saved"),
        Err(e) => warn!("daemon: failed to save session layout: {e}"),
    }
}

/// Return the CWD of the leftmost (first) leaf in a `PaneTreeState`.
///
/// Used by cold-start restore to seed the initial `create_tab` call for a tab
/// whose root node may be a `Split`.
fn first_leaf_cwd(tree: &forgetty_workspace::PaneTreeState) -> &std::path::Path {
    match tree {
        forgetty_workspace::PaneTreeState::Leaf { cwd, .. } => cwd,
        forgetty_workspace::PaneTreeState::Split { first, .. } => first_leaf_cwd(first),
    }
}

/// Recursively restore a saved pane sub-tree into the live session manager.
///
/// `anchor_id` is the live pane that corresponds to the root of `tree`. For
/// `Split` nodes the function calls `split_pane_with_ratio` to place the
/// second child, then recurses into both halves. Leaf nodes are no-ops because
/// `anchor_id` is already the live pane for that position.
fn restore_subtree(
    session_manager: &forgetty_session::SessionManager,
    anchor_id: PaneId,
    tree: &forgetty_workspace::PaneTreeState,
    size: forgetty_pty::PtySize,
) {
    match tree {
        forgetty_workspace::PaneTreeState::Leaf { cwd, .. } => {
            // Explicitly set the anchor pane's cached CWD to the saved value
            // so that snapshot_to_workspace_state returns the correct CWD even
            // before the drain loop has refreshed it from /proc/{pid}/cwd.
            if cwd.is_dir() {
                session_manager.set_pane_cwd(anchor_id, cwd.clone());
            }
        }
        forgetty_workspace::PaneTreeState::Split { direction, ratio, first, second } => {
            let second_cwd = first_leaf_cwd(second);
            let effective_cwd =
                if second_cwd.is_dir() { Some(second_cwd.to_path_buf()) } else { None };

            match session_manager.split_pane_with_ratio(
                anchor_id,
                direction,
                *ratio,
                size,
                effective_cwd,
            ) {
                Ok(second_pane_id) => {
                    restore_subtree(session_manager, anchor_id, first, size);
                    restore_subtree(session_manager, second_pane_id, second, size);
                }
                Err(e) => {
                    warn!("cold-start restore: split_pane_with_ratio failed: {e}");
                }
            }
        }
    }
}
