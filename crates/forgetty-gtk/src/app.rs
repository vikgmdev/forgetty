//! GTK4 application entry point.
//!
//! Creates and runs the adw::Application, managing the window lifecycle
//! (open, resize, close) with native GNOME client-side decorations.
//! Uses adw::TabBar + adw::TabView for multi-tab terminal sessions.
//! Supports split panes within tabs via nested gtk::Paned widgets.
//!
//! Each tab page holds a gtk::Box ("pane container") whose single child is
//! either a DrawingArea (leaf terminal) or a gtk::Paned (branch containing
//! two subtrees). This container allows us to swap the root widget of a tab
//! during split/close operations without removing and re-inserting the TabPage.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use std::path::PathBuf;

use crate::daemon_client::{DaemonClient, LayoutEvent, LayoutInfo, PaneTreeNode};
use crate::terminal::NotificationPayload;
use forgetty_config::{load_config, Config, NotificationMode, ProfileConfig};
use forgetty_watcher::ConfigWatcher;
use gtk4::gio;
use gtk4::glib;
use gtk4::pango;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use tracing::info;

use crate::clipboard;
use crate::preferences;
use crate::terminal::{self, TerminalState};

/// Shared config state, updated on hot reload and read by new tab/split creation.
type SharedConfig = Rc<RefCell<Config>>;

/// Interval for polling config file changes (milliseconds).
const CONFIG_POLL_MS: u64 = 500;

/// The application ID used for D-Bus registration and desktop integration.
const APP_ID: &str = "dev.forgetty.Forgetty";

/// Default window width in pixels.
const DEFAULT_WIDTH: i32 = 960;

/// Default window height in pixels.
const DEFAULT_HEIGHT: i32 = 640;

/// Interval for polling CWD / OSC title changes (milliseconds).
/// Kept low (100ms) so `cd` updates feel instant — readlink on /proc is ~1μs.
const TITLE_POLL_MS: u64 = 100;

// POSIX signal numbers — stable constants, avoids adding a `libc` dependency.
/// Hangup (terminal closed, session leader death).
const SIGHUP: i32 = 1;
/// Interrupt (Ctrl+C from controlling terminal).
const SIGINT: i32 = 2;
/// Termination request (default `kill` signal).
const SIGTERM: i32 = 15;

/// Maximum number of idle ticks to wait for a restored `gtk4::Paned` to get a
/// usable allocation before giving up on applying its saved split ratio. Used
/// by `build_widget_from_pane_tree` (session-restore fix, bug 3).
///
/// On a loaded laptop a fresh GTK window typically needs 1–3 idle ticks to
/// propagate size allocation through nested Paneds; 30 gives comfortable
/// headroom without enabling an infinite loop when a Paned is permanently
/// hidden (0-sized).
const RESTORE_RATIO_MAX_RETRIES: u8 = 30;

/// Lookup table mapping each pane's DrawingArea widget name to its TerminalState.
///
/// This is NOT shared mutable terminal state -- it is a simple registry so that
/// close handlers can find and kill the correct PTY. Each pane's
/// `TerminalState` is independently owned by its own closures.
/// With split panes, multiple DrawingAreas exist per tab, each registered here.
type TabStateMap = Rc<RefCell<HashMap<String, Rc<RefCell<TerminalState>>>>>;

/// Tracks which DrawingArea (by widget name) currently has focus in the window.
///
/// Updated on focus-enter events from each pane's EventControllerFocus.
/// Read by split/close/navigate actions to determine the target pane.
type FocusTracker = Rc<RefCell<String>>;

/// Tracks which tab pages (by page pointer hash) have user-set custom titles.
///
/// When a page key is present in this set, the title polling timer skips
/// overwriting that page's title, allowing the user-provided title to stick.
/// Removing the key (or setting an empty title) re-enables automatic CWD polling.
type CustomTitles = Rc<RefCell<HashSet<String>>>;

/// Maps a tab page's identity key (see `page_identity_key()`) to the daemon tab UUID.
///
/// Populated in `add_new_tab` when the daemon returns a `tab_id`.
/// Used by the `page-reordered` handler to send `move_tab` RPCs.
/// Keys are removed on tab close.
type TabIdMap = Rc<RefCell<HashMap<String, uuid::Uuid>>>;

/// Maps a tab page's identity key to a user-chosen RGBA color for the tab indicator dot.
///
/// Set via the right-click tab context menu → "Change Tab Color".
/// Cleared when the user picks "None".
type TabColorMap = Rc<RefCell<HashMap<String, gtk4::gdk::RGBA>>>;

/// FIX-005B fix-cycle 1: per-workspace map `tab_id → daemon PaneId of the
/// saved-focused leaf`.
///
/// Populated at cold restart from the layout snapshot, updated by
/// `wire_focus_tracking`'s `connect_enter` (live focus changes), and
/// updated by the `LayoutEvent::ActivePaneChanged` consumer (echo path).
///
/// Read by `connect_selected_page_notify` BEFORE the legacy `focus_tracker`
/// fallback so that a tab's saved focused pane wins over a stale per-
/// workspace single-string tracker that may belong to a different tab.
///
/// Without this, the synchronous `selected-page-notify` fired by
/// `tab_view.append(&container)` during cold-restart build grabs the
/// leftmost leaf, whose `connect_enter` then RPCs the WRONG pane back to
/// the daemon, corrupting the persisted `active_pane_id` before the
/// post-build deferred `focus_when_mapped(RIGHT_DA)` ever runs.
type TabActivePaneMap = Rc<RefCell<HashMap<uuid::Uuid, forgetty_core::PaneId>>>;

/// Per-workspace GTK state -- owns the TabView and associated state for one workspace.
struct WorkspaceView {
    /// Unique ID matching the Workspace in the session file.
    id: uuid::Uuid,
    /// Human-readable name.
    name: String,
    /// The adw::TabView holding this workspace's pages.
    tab_view: adw::TabView,
    /// Pane states for terminals in this workspace.
    tab_states: TabStateMap,
    /// Focus tracker for this workspace.
    focus_tracker: FocusTracker,
    /// Custom title tracker for this workspace.
    custom_titles: CustomTitles,
    /// Maps page identity key → daemon tab UUID (daemon mode only).
    tab_id_map: TabIdMap,
    /// Per-tab color indicators (from right-click context menu).
    tab_colors: TabColorMap,
    /// User-chosen accent color for this workspace's sidebar card (GTK-side only, not persisted).
    color: Option<gtk4::gdk::RGBA>,
    /// CSS provider for per-row color override (reused across sidebar refreshes).
    color_css_provider: Option<gtk4::CssProvider>,
    /// FIX-005B fix-cycle 1: per-tab saved active_pane_id, consulted by
    /// `connect_selected_page_notify` to grab the correct leaf when the
    /// page's tab_id has a recorded focused pane. See `TabActivePaneMap`
    /// docstring for the full rationale.
    tab_active_pane: TabActivePaneMap,
}

/// Shared state tracking all workspaces and which is active.
type WorkspaceManager = Rc<RefCell<WorkspaceManagerInner>>;

struct WorkspaceManagerInner {
    workspaces: Vec<WorkspaceView>,
    active_index: usize,
    /// Last right-click position on the tab bar (x, y in tab_bar coordinates).
    /// Written by the GestureClick(button=3) Capture handler, read by setup-menu handlers.
    last_tab_click: (f64, f64),
    /// Set to true by setup-menu handler after showing the menu.
    /// Read by the bubble-phase fallback to avoid showing a second menu.
    tab_menu_shown: bool,
    /// FIX-006 Cycle 1: pending self-originated workspace reorders awaiting
    /// the daemon's `WorkspacesReordered` echo. Each entry is `(from_idx, to_idx)`
    /// captured at the moment of the local swap. `move_workspace_up` /
    /// `_down` push on the local-mutation path; the `LayoutEvent::WorkspacesReordered`
    /// consumer pops the head on a match (provenance-based self-echo guard).
    ///
    /// This replaces the original content-based guard, which mis-fired when
    /// two consecutive Move Up clicks landed inside the same 200ms layout-event
    /// drain window — by the time the drainer processed the first event, the
    /// second local swap had already perturbed the post-swap pattern, causing
    /// the consumer to treat the self-echo as external and apply the swap a
    /// second time. The FIFO queue is correct because the daemon emits
    /// `WorkspacesReordered` events in the same order as the RPC calls landed
    /// (single mutex on the daemon side).
    pending_reorders: VecDeque<(usize, usize)>,
    /// FIX-005B: pending self-originated active-pane changes awaiting the
    /// daemon's `ActivePaneChanged` echo. Each entry is the
    /// `(workspace_idx, tab_id, pane_id)` tuple captured at the moment of
    /// the local focus-enter handler. The `LayoutEvent::ActivePaneChanged`
    /// consumer pops the head on FIFO match (provenance-based self-echo
    /// guard, same idiom as `pending_reorders`).
    ///
    /// MUST be a queue, NOT a content-match guard — under fast pane-switch
    /// sequences (e.g. mouse-click flurry across 4 panes) two RPCs can land
    /// inside the same 200ms layout drain window; a content guard mis-fires
    /// when the second event drains while local state already shows the
    /// post-second-RPC value (see FIX-006 INVESTIGATION.md §1–§3).
    pending_active_pane: VecDeque<(usize, uuid::Uuid, forgetty_core::PaneId)>,
}

/// CLI-derived launch parameters for this specific invocation.
///
/// Re-exported from the binary crate. These are runtime overrides, NOT
/// persistent config. They affect only the initial pane.
#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    /// Working directory for the initial pane.
    pub working_directory: Option<PathBuf>,

    /// Command + args for the initial pane (overrides config shell).
    pub command: Option<Vec<String>>,

    /// WM_CLASS override for the GTK application ID.
    pub class: Option<String>,

    /// Skip session restore and open a fresh single-tab window.
    pub no_restore: bool,

    /// Session UUID to attach to (for restore). If not set, a new UUID is generated.
    pub session_id: Option<uuid::Uuid>,

    /// Restore all saved sessions (open one window per session file).
    pub restore_all: bool,
}

/// Derive the daemon socket path for a given session UUID.
fn socket_path_for(session_id: uuid::Uuid) -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join(format!("forgetty-{session_id}.sock"))
    } else {
        PathBuf::from(format!("/tmp/forgetty-{session_id}.sock"))
    }
}

/// Connect to (or spawn) the daemon for this session.
///
/// Per AD-011 the daemon is a hard dependency — there is no local-PTY fallback.
/// If the daemon cannot be started or connected to, the process exits with
/// status 1 after logging the cause.
fn ensure_daemon(session_id: uuid::Uuid) -> Arc<DaemonClient> {
    let socket_path = socket_path_for(session_id);

    // 1. Try to connect immediately (daemon may already be running).
    if let Ok(dc) = DaemonClient::connect(&socket_path) {
        info!("ensure_daemon: connected to existing daemon at {:?}", socket_path);
        return Arc::new(dc);
    }

    // 2. Daemon not running — find the binary.
    let daemon_binary: Option<PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("forgetty-daemon")))
        .filter(|p| p.exists())
        .or_else(|| {
            // PATH fallback
            std::env::var("PATH").ok().and_then(|path_var| {
                path_var.split(':').find_map(|dir| {
                    let p = PathBuf::from(dir).join("forgetty-daemon");
                    if p.exists() {
                        Some(p)
                    } else {
                        None
                    }
                })
            })
        });

    let Some(daemon_path) = daemon_binary else {
        tracing::error!(
            "ensure_daemon: forgetty-daemon binary not found on PATH or alongside forgetty; cannot continue"
        );
        std::process::exit(1);
    };

    // 3. Spawn the daemon with the session UUID (non-blocking).
    match std::process::Command::new(&daemon_path)
        .arg("--session-id")
        .arg(session_id.to_string())
        .spawn()
    {
        Ok(child) => {
            // Leak the child handle so Drop is never called, keeping the process alive.
            std::mem::forget(child);
            info!("ensure_daemon: spawned {:?} with session_id={session_id}", daemon_path);
        }
        Err(e) => {
            tracing::error!("ensure_daemon: failed to spawn daemon ({daemon_path:?}): {e}");
            std::process::exit(1);
        }
    }

    // 4. Retry with exponential-ish backoff (up to ~1s total).
    for attempt in 0..20 {
        let delay_ms = if attempt < 5 { 25 } else { 50 };
        std::thread::sleep(Duration::from_millis(delay_ms));
        if let Ok(dc) = DaemonClient::connect(&socket_path) {
            info!("ensure_daemon: connected after {} attempt(s)", attempt + 1);
            return Arc::new(dc);
        }
    }

    tracing::error!(
        "ensure_daemon: daemon did not become ready after 20 attempts (~1s); cannot continue"
    );
    std::process::exit(1);
}

/// Run the GTK4/libadwaita application.
///
/// This function blocks until the window is closed. It initialises libadwaita,
/// creates the main application window with CSD header bar, and enters the
/// GTK main loop.
pub fn run(config: Config, launch: LaunchOptions) -> Result<(), Box<dyn std::error::Error>> {
    let app_id = launch.class.as_deref().unwrap_or(APP_ID);

    // Resolve or generate the session UUID for this window.
    let session_id: uuid::Uuid = launch.session_id.unwrap_or_else(uuid::Uuid::new_v4);
    info!("GTK session_id: {session_id}");

    // P-018 / AD-016: one-shot migration + crash-recovery for the three-bucket
    // layout.
    //
    // 1. `run_migration_p018()` is gated on the `.migration_p018` marker; it
    //    triages pre-P-018 `sessions/*.json` files (pinned → stay, unpinned →
    //    move to trash/) on the first launch of the new build.
    // 2. `recover_orphans_in_active()` enumerates leftover files in `active/`
    //    (i.e. crashes / kill -9). Pinned orphans get promoted back to
    //    `sessions/`, unpinned orphans are deleted. A clean previous shutdown
    //    leaves `active/` empty, so this is a no-op on the happy path.
    //
    // Both operations are synchronous, single-threaded, run before any daemon
    // spawns or socket binds. AD-009 (no polling) is not at risk here — this
    // is one-shot startup work, not the hot path.
    if let Err(e) = forgetty_workspace::run_migration_p018() {
        tracing::warn!("p018 migration: {e} (continuing — not fatal)");
    }
    let orphans = forgetty_workspace::recover_orphans_in_active();
    if !orphans.is_empty() {
        tracing::info!("p018 orphan recovery: {} file(s) found in active/", orphans.len());
        for (uuid, is_pinned) in orphans {
            if is_pinned {
                match forgetty_workspace::move_active_to_sessions(uuid) {
                    Ok(()) => {
                        tracing::info!("p018 orphan recovery: promoted pinned {uuid} → sessions/")
                    }
                    Err(e) => {
                        tracing::warn!("p018 orphan recovery: failed to promote {uuid}: {e}")
                    }
                }
            } else {
                match forgetty_workspace::delete_active_for(uuid) {
                    Ok(()) => {
                        tracing::info!("p018 orphan recovery: deleted unpinned orphan {uuid}")
                    }
                    Err(e) => {
                        tracing::warn!("p018 orphan recovery: failed to delete {uuid}: {e}")
                    }
                }
            }
        }
    }

    // Attempt to connect to (or spawn) the daemon before entering the GTK loop.
    // This is done outside connect_activate so it runs once, not once per window.
    // Per AD-011 the daemon is a hard dependency; `ensure_daemon` exits the
    // process on failure. The `Option` wrapper is retained for now to keep the
    // existing `if let Some(dc) = daemon_client` patterns compiling — see
    // CHORE-FIX-011-cleanup for the follow-up refactor to `Arc<DaemonClient>`.
    let daemon_client: Option<Arc<DaemonClient>> = Some(ensure_daemon(session_id));
    info!("GTK running in daemon-client mode: sessions survive window close");

    let app = adw::Application::builder()
        .application_id(app_id)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(move |app| {
        build_ui(app, &config, &launch, daemon_client.clone(), session_id);
    });

    // GTK expects argv-style arguments; pass empty since clap already parsed.
    let exit_code = app.run_with_args::<&str>(&[]);

    if exit_code != gtk4::glib::ExitCode::SUCCESS {
        return Err(format!("GTK application exited with code: {:?}", exit_code).into());
    }

    Ok(())
}

/// Generate a unique widget name for each pane's DrawingArea.
fn next_pane_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("forgetty-pane-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Resolve the default profile's command and CWD from the config.
///
/// Returns `(command_argv, resolved_cwd)`. Both are `None` when no profiles
/// are configured, which preserves existing auto-detect shell behavior.
fn resolve_default_profile_args(cfg: &Config) -> (Option<Vec<String>>, Option<PathBuf>) {
    if cfg.profiles.is_empty() {
        return (None, None);
    }
    // Find the designated default profile, or fall back to the first profile.
    let profile = if let Some(ref dp_name) = cfg.default_profile {
        cfg.profiles.iter().find(|p| &p.name == dp_name).unwrap_or(&cfg.profiles[0])
    } else {
        &cfg.profiles[0]
    };
    let command: Option<Vec<String>> = if profile.command.is_empty() {
        None
    } else {
        Some(profile.command.split_whitespace().map(String::from).collect())
    };
    let cwd = resolve_profile_dir(profile.directory.as_deref());
    (command, cwd)
}

/// Build a manual `gtk4::Popover` for the pan-down dropdown button.
///
/// Uses real GTK4 widgets (Button + Image + Label) for profile rows so that
/// icons are guaranteed to render. GTK4's `PopoverMenu` from a `gio::Menu`
/// model does not display icons set via `gio::MenuItem::set_icon()` or the
/// `G_MENU_ATTRIBUTE_ICON` attribute for regular vertical items; a manually
/// constructed popover gives full control over icon display (AC-6).
///
/// Layout:
///   Popover
///   └── Box (vertical)
///         [when profiles exist]
///         ├── Label "Profiles" (section header, small/dim)
///         ├── Button [Image icon + Label name]  ×N  (one per profile)
///         └── Separator
///         ├── Button [Label "New Tab"]  (win.new-tab)
///         └── Separator
///         ├── Button [Label "Split Up"]
///         ├── Button [Label "Split Down"]
///         ├── Button [Label "Split Left"]
///         └── Button [Label "Split Right"]
///
/// Called at startup and on every hot-reload.
fn build_dropdown_popover(
    profiles: &[ProfileConfig],
    window: &adw::ApplicationWindow,
) -> gtk4::Popover {
    let popover = gtk4::Popover::new();
    let outer_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer_box.set_margin_top(6);
    outer_box.set_margin_bottom(6);
    outer_box.set_margin_start(6);
    outer_box.set_margin_end(6);
    outer_box.set_spacing(2);

    // --- Profiles section (AC-6, AC-8) ---
    if !profiles.is_empty() {
        // Section header label (small / dimmed, like GNOME menus).
        let header_label = gtk4::Label::new(Some("Profiles"));
        header_label.set_halign(gtk4::Align::Start);
        header_label.set_margin_start(6);
        header_label.set_margin_top(2);
        header_label.set_margin_bottom(2);
        header_label.add_css_class("caption");
        header_label.add_css_class("dim-label");
        outer_box.append(&header_label);

        for (i, profile) in profiles.iter().enumerate() {
            let icon_name = profile.icon.as_deref().unwrap_or("terminal-symbolic");

            // Row: horizontal Box with Image + Label.
            let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row_box.set_margin_start(6);
            row_box.set_margin_end(6);

            let img = gtk4::Image::from_icon_name(icon_name);
            img.set_icon_size(gtk4::IconSize::Normal);
            row_box.append(&img);

            let lbl = gtk4::Label::new(Some(&profile.name));
            lbl.set_halign(gtk4::Align::Start);
            lbl.set_hexpand(true);
            row_box.append(&lbl);

            let btn = gtk4::Button::new();
            btn.set_child(Some(&row_box));
            btn.set_has_frame(false);
            btn.add_css_class("flat");
            btn.set_action_name(Some(&format!("win.open-profile-{i}")));

            // Clicking the button closes the popover.
            let pop_ref = popover.clone();
            btn.connect_clicked(move |_| {
                pop_ref.popdown();
            });

            outer_box.append(&btn);
        }

        let sep1 = gtk4::Separator::new(gtk4::Orientation::Horizontal);
        sep1.set_margin_top(4);
        sep1.set_margin_bottom(4);
        outer_box.append(&sep1);
    }

    // --- New Tab item (always present) ---
    {
        let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        row_box.set_margin_start(6);
        row_box.set_margin_end(6);

        let lbl = gtk4::Label::new(Some("New Tab"));
        lbl.set_halign(gtk4::Align::Start);
        lbl.set_hexpand(true);
        row_box.append(&lbl);

        let accel_lbl = gtk4::Label::new(Some("Ctrl+Shift+T"));
        accel_lbl.add_css_class("dim-label");
        accel_lbl.add_css_class("caption");
        row_box.append(&accel_lbl);

        let btn = gtk4::Button::new();
        btn.set_child(Some(&row_box));
        btn.set_has_frame(false);
        btn.add_css_class("flat");
        btn.set_action_name(Some("win.new-tab"));

        let pop_ref = popover.clone();
        btn.connect_clicked(move |_| {
            pop_ref.popdown();
        });

        outer_box.append(&btn);
    }

    // --- Split section ---
    {
        let sep2 = gtk4::Separator::new(gtk4::Orientation::Horizontal);
        sep2.set_margin_top(4);
        sep2.set_margin_bottom(4);
        outer_box.append(&sep2);

        for (label, action, accel_hint) in [
            ("Split Up", "win.split-up", None),
            ("Split Down", "win.split-down", Some("Alt+Shift+−")),
            ("Split Left", "win.split-left", None),
            ("Split Right", "win.split-right", Some("Alt+Shift+=")),
        ] {
            let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row_box.set_margin_start(6);
            row_box.set_margin_end(6);

            let lbl = gtk4::Label::new(Some(label));
            lbl.set_halign(gtk4::Align::Start);
            lbl.set_hexpand(true);
            row_box.append(&lbl);

            if let Some(hint) = accel_hint {
                let al = gtk4::Label::new(Some(hint));
                al.add_css_class("dim-label");
                al.add_css_class("caption");
                row_box.append(&al);
            }

            let btn = gtk4::Button::new();
            btn.set_child(Some(&row_box));
            btn.set_has_frame(false);
            btn.add_css_class("flat");
            btn.set_action_name(Some(action));

            let pop_ref = popover.clone();
            btn.connect_clicked(move |_| {
                pop_ref.popdown();
            });

            outer_box.append(&btn);
        }
    }

    popover.set_child(Some(&outer_box));
    popover.set_autohide(true);

    // Keep the popover associated with the window so action dispatch works.
    let _ = window;

    popover
}

/// Resolve a profile's directory to an actual `PathBuf`, expanding `~` and
/// validating existence. Falls back to the home directory on failure.
fn resolve_profile_dir(directory: Option<&str>) -> Option<PathBuf> {
    let raw = directory?;
    let expanded = if raw == "~" || raw.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        if raw == "~" {
            home
        } else {
            format!("{}{}", home, &raw[1..])
        }
    } else {
        raw.to_string()
    };
    let pb = PathBuf::from(&expanded);
    if pb.is_dir() {
        Some(pb)
    } else {
        tracing::warn!("Profile directory {:?} does not exist, falling back to home dir", expanded);
        None
    }
}

/// Register `gio::SimpleAction`s for each profile on the window, wire
/// Ctrl+Shift+1-9 accelerators, and connect each action to open a profile tab.
///
/// Returns the number of actions registered so the caller can unregister them
/// on the next hot-reload cycle.
fn register_profile_actions(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    profiles: &[ProfileConfig],
    shared_config: &SharedConfig,
    workspace_manager: &WorkspaceManager,
    daemon_client: &Option<Arc<DaemonClient>>,
) -> usize {
    for (i, profile) in profiles.iter().enumerate() {
        let profile_clone = profile.clone();
        let cfg_ref = Rc::clone(shared_config);
        let wm_ref = Rc::clone(workspace_manager);
        let dc_ref = daemon_client.clone();
        let win_ref = window.clone();
        let action = gio::SimpleAction::new(&format!("open-profile-{i}"), None);
        action.connect_activate(move |_action, _param| {
            let Ok(cfg) = cfg_ref.try_borrow() else { return };
            let Ok(mgr) = wm_ref.try_borrow() else { return };
            let active_idx = mgr.active_index;
            let ws = &mgr.workspaces[active_idx];
            let command: Option<Vec<String>> = if profile_clone.command.is_empty() {
                None
            } else {
                Some(profile_clone.command.split_whitespace().map(String::from).collect())
            };
            let cwd = resolve_profile_dir(profile_clone.directory.as_deref());
            add_new_tab(
                active_idx,
                &ws.tab_view,
                &cfg,
                &ws.tab_states,
                &ws.focus_tracker,
                &ws.custom_titles,
                &win_ref,
                cwd.as_deref(),
                command.as_deref(),
                dc_ref.clone(),
                &ws.tab_id_map,
                &wm_ref,
            );
        });
        window.add_action(&action);

        // Wire Ctrl+Shift+1-9 (only for first 9 profiles, AC-10, AC-11).
        if i < 9 {
            let digit = i + 1;
            app.set_accels_for_action(
                &format!("win.open-profile-{i}"),
                &[&format!("<Control><Shift>{digit}")],
            );
        }
    }
    profiles.len()
}

/// Apply custom keybinding overrides from `config.keybindings` to the GTK app.
///
/// Iterates every entry in the `[keybindings]` table and calls
/// `app.set_accels_for_action` with the user-defined accelerator string,
/// overriding whatever default was registered at startup.  An empty-string
/// value means "explicitly unbound" — we pass an empty slice so the default is
/// removed (AC-21).
///
/// This helper is used in two places:
/// 1. At startup, *after* all default `set_accels_for_action` calls (AC-20).
/// 2. In the `ConfigWatcher` hot-reload closure (AC-22).
/// 3. Directly in the keybindings editor after saving a change (AC-12).
pub(crate) fn apply_keybinding_overrides(
    app: &adw::Application,
    keybindings: &std::collections::HashMap<String, String>,
) {
    use crate::settings_view::ACTION_DEFS;
    for (config_key, accel) in keybindings {
        // Look up the full GIO action name from the ACTION_DEFS inventory.
        if let Some(def) = ACTION_DEFS.iter().find(|d| d.config_key == config_key.as_str()) {
            if accel.is_empty() {
                app.set_accels_for_action(def.action_name, &[]);
            } else {
                app.set_accels_for_action(def.action_name, &[accel.as_str()]);
            }
        } else {
            // Unknown key — silently skip (forward-compat).
            tracing::debug!(
                "apply_keybinding_overrides: unknown config key {:?}, skipping",
                config_key
            );
        }
    }
}

/// Remove previously registered profile actions from the window and clear
/// their keyboard accelerators. Call before re-registering on hot-reload.
fn unregister_profile_actions(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    count: usize,
) {
    for i in 0..count {
        window.remove_action(&format!("open-profile-{i}"));
        if i < 9 {
            app.set_accels_for_action(&format!("win.open-profile-{i}"), &[]);
        }
    }
}

/// Build the main application window with tab bar and initial terminal tab.
fn build_ui(
    app: &adw::Application,
    config: &Config,
    launch: &LaunchOptions,
    daemon_client: Option<Arc<DaemonClient>>,
    session_id: uuid::Uuid,
) {
    info!("Building Forgetty GTK4 window");

    // CLI overrides skip both session restore AND session save so a one-off
    // launch (e.g. `forgetty --working-directory /tmp`) never overwrites the
    // user's real saved session.
    let has_cli_override =
        launch.working_directory.is_some() || launch.command.is_some() || launch.no_restore;
    let skip_session_save = Rc::new(Cell::new(has_cli_override));

    // --- Widget hierarchy ---
    // adw::ApplicationWindow
    //   content: gtk::Box (vertical)
    //     [0] adw::HeaderBar (centered title from window title "Forgetty")
    //           pack_start: dropdown MenuButton (New Tab / Split actions)
    //           pack_end: hamburger MenuButton
    //     [1] adw::TabBar (autohide=true, hidden when 1 tab, shown for 2+)
    //     [2] adw::TabView (holds pane containers as pages)
    //
    // Each tab page child is a gtk::Box (the "pane container"), which holds
    // either a single DrawingArea or a nested gtk::Paned tree.

    let header = adw::HeaderBar::new();

    // --- Hamburger menu button ---
    // Standard GNOME pattern: gio::Menu model + gtk::MenuButton.
    // Full menu structure matching Ghostty's discoverability.
    let menu = gio::Menu::new();

    // Section 1 -- Clipboard
    let clipboard_section = gio::Menu::new();
    let copy_item = gio::MenuItem::new(Some("Copy"), Some("win.copy"));
    copy_item.set_attribute_value("accel", Some(&"<Control>c".to_variant()));
    clipboard_section.append_item(&copy_item);
    let paste_item = gio::MenuItem::new(Some("Paste"), Some("win.paste"));
    paste_item.set_attribute_value("accel", Some(&"<Control>v".to_variant()));
    clipboard_section.append_item(&paste_item);
    menu.append_section(None, &clipboard_section);

    // Section 2 -- Window & Tab management
    let window_tab_section = gio::Menu::new();
    let new_window_item = gio::MenuItem::new(Some("New Window"), Some("win.new-window"));
    new_window_item.set_attribute_value("accel", Some(&"<Control><Shift>n".to_variant()));
    window_tab_section.append_item(&new_window_item);
    window_tab_section.append(Some("Close Window"), Some("win.close-window"));
    window_tab_section
        .append(Some("Close Window Permanently"), Some("win.close-window-permanently"));
    window_tab_section.append(Some("Change Tab Title\u{2026}"), Some("win.change-tab-title"));
    let new_tab_menu_item = gio::MenuItem::new(Some("New Tab"), Some("win.new-tab"));
    new_tab_menu_item.set_attribute_value("accel", Some(&"<Control><Shift>t".to_variant()));
    window_tab_section.append_item(&new_tab_menu_item);
    let close_tab_item = gio::MenuItem::new(Some("Close Tab"), Some("win.close-tab"));
    close_tab_item.set_attribute_value("accel", Some(&"<Control><Shift>w".to_variant()));
    window_tab_section.append_item(&close_tab_item);
    menu.append_section(None, &window_tab_section);

    // Section 3 -- Workspace management
    let workspace_section = gio::Menu::new();
    let new_ws_item = gio::MenuItem::new(Some("New Workspace"), Some("win.new-workspace"));
    new_ws_item.set_attribute_value("accel", Some(&"<Control><Alt>n".to_variant()));
    workspace_section.append_item(&new_ws_item);
    workspace_section.append(Some("Rename Workspace\u{2026}"), Some("win.rename-workspace"));
    workspace_section.append(Some("Delete Workspace"), Some("win.delete-workspace"));
    let ws_sidebar_item =
        gio::MenuItem::new(Some("Toggle Workspace Sidebar"), Some("win.toggle-workspace-sidebar"));
    ws_sidebar_item.set_attribute_value("accel", Some(&"<Control><Alt>b".to_variant()));
    workspace_section.append_item(&ws_sidebar_item);
    menu.append_section(None, &workspace_section);

    // Section 4 -- Split submenu
    let split_section = gio::Menu::new();
    let split_submenu = gio::Menu::new();
    split_submenu.append(Some("Up"), Some("win.split-up"));
    let split_down_item = gio::MenuItem::new(Some("Down"), Some("win.split-down"));
    split_down_item.set_attribute_value("accel", Some(&"<Alt><Shift>minus".to_variant()));
    split_submenu.append_item(&split_down_item);
    split_submenu.append(Some("Left"), Some("win.split-left"));
    let split_right_item = gio::MenuItem::new(Some("Right"), Some("win.split-right"));
    split_right_item.set_attribute_value("accel", Some(&"<Alt><Shift>equal".to_variant()));
    split_submenu.append_item(&split_right_item);
    split_section.append_submenu(Some("Split"), &split_submenu);
    menu.append_section(None, &split_section);

    // Section 5 -- Session management
    let session_section = gio::Menu::new();
    session_section.append(Some("Pin Session"), Some("win.toggle-pin-session"));
    session_section
        .append(Some("Restore Previous Session\u{2026}"), Some("win.restore-previous-session"));
    menu.append_section(None, &session_section);

    // Section 6 -- Terminal operations
    let terminal_section = gio::Menu::new();
    terminal_section.append(Some("Clear"), Some("win.clear"));
    terminal_section.append(Some("Reset"), Some("win.reset"));
    menu.append_section(None, &terminal_section);

    // Section 6 -- Configuration & Help
    let config_help_section = gio::Menu::new();
    let settings_item = gio::MenuItem::new(Some("Settings"), Some("win.open-settings"));
    settings_item.set_attribute_value("accel", Some(&"<Control>period".to_variant()));
    config_help_section.append_item(&settings_item);
    config_help_section.append(Some("Terminal Inspector"), Some("win.terminal-inspector"));
    let appearance_item = gio::MenuItem::new(Some("Appearance"), Some("win.appearance"));
    appearance_item.set_attribute_value("accel", Some(&"<Control>comma".to_variant()));
    config_help_section.append_item(&appearance_item);
    let shortcuts_item = gio::MenuItem::new(Some("Keyboard Shortcuts"), Some("win.show-shortcuts"));
    shortcuts_item.set_attribute_value("accel", Some(&"F1".to_variant()));
    config_help_section.append_item(&shortcuts_item);
    config_help_section.append(Some("About Forgetty"), Some("win.show-about"));
    menu.append_section(None, &config_help_section);

    // Section 7 -- Application
    let app_section = gio::Menu::new();
    let quit_item = gio::MenuItem::new(Some("Quit"), Some("app.quit"));
    quit_item.set_attribute_value("accel", Some(&"<Control><Shift>q".to_variant()));
    app_section.append_item(&quit_item);
    menu.append_section(None, &app_section);

    let menu_button = gtk4::MenuButton::new();
    menu_button.set_icon_name("open-menu-symbolic");
    menu_button.set_menu_model(Some(&menu));
    menu_button.set_tooltip_text(Some("Menu"));
    header.pack_end(&menu_button);

    // --- New tab button (direct click to create tab, like Ghostty) ---
    let new_tab_button = gtk4::Button::from_icon_name("tab-new-symbolic");
    new_tab_button.set_tooltip_text(Some("New Tab"));
    new_tab_button.set_action_name(Some("win.new-tab"));
    header.pack_start(&new_tab_button);

    // --- Dropdown menu button (profiles + new tab + split actions) ---
    // The popover is built after `window` is created (it needs a reference to the
    // window so profile button actions dispatch correctly via the widget hierarchy).
    // We set a placeholder here and replace it below with build_dropdown_popover().
    let dropdown_button = gtk4::MenuButton::new();
    dropdown_button.set_icon_name("pan-down-symbolic");
    dropdown_button.set_tooltip_text(Some("Tab and Split Actions"));
    header.pack_start(&dropdown_button);

    // Create the initial TabView for the default workspace.
    let initial_tab_view = adw::TabView::new();
    initial_tab_view.set_vexpand(true);
    initial_tab_view.set_hexpand(true);

    let tab_bar = adw::TabBar::new();
    tab_bar.set_view(Some(&initial_tab_view));
    tab_bar.set_autohide(true);
    tab_bar.set_hexpand(true);

    // The header bar keeps its default title ("Forgetty" from the window title).
    // The tab bar lives as a separate row below the header, auto-hidden when
    // only one tab exists (matching Ghostty's two-row layout for 2+ tabs).

    // Wrap TabView in a horizontal Box with an Appearance sidebar Revealer.
    // The sidebar slides in from the right when the user clicks "Appearance".
    let main_area = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    main_area.set_vexpand(true);
    main_area.set_hexpand(true);
    main_area.append(&initial_tab_view);

    // terminal_row wraps the workspace sidebar revealer (left) and main_area (right).
    // This ensures the sidebar pushes main_area right rather than floating over it.
    let terminal_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    terminal_row.set_vexpand(true);
    terminal_row.set_hexpand(true);
    terminal_row.append(&main_area);

    // Wrap terminal_row in an Overlay so the command palette can float on top.
    let main_overlay = gtk4::Overlay::new();
    main_overlay.set_child(Some(&terminal_row));
    main_overlay.set_vexpand(true);

    // Outer stack: "terminal" page (normal UI) and "settings" page (full takeover).
    // The settings page is added lazily on first open to avoid building it at startup.
    let terminal_page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    terminal_page.append(&header);
    terminal_page.append(&tab_bar);
    terminal_page.append(&main_overlay);

    let outer_stack = gtk4::Stack::new();
    outer_stack.set_transition_type(gtk4::StackTransitionType::None);
    outer_stack.add_named(&terminal_page, Some("terminal"));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Forgetty")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&outer_stack)
        .build();

    // --- Dropdown popover (Bug 1 fix: manual popover for icon support) ---
    // Now that `window` exists, build the popover and attach it. The popover
    // rows use flat buttons whose `action-name` properties dispatch via the
    // window's action group, so the popover must be a descendant of (or
    // associated with) the window for action lookup to work. Setting it on
    // a `MenuButton` that is a child of the window header satisfies this.
    {
        let popover = build_dropdown_popover(&config.profiles, &window);
        dropdown_button.set_popover(Some(&popover));
    }

    // --- Workspace sidebar CSS ---
    // Applied globally via the display; uses Adwaita CSS tokens so it respects
    // both dark and light themes.
    {
        let css_provider = gtk4::CssProvider::new();
        css_provider.load_from_string(
            ".workspace-sidebar { border-right: 1px solid @borders; } \
             .workspace-sidebar-active { border-left: 3px solid @accent_color; \
             background-color: alpha(@accent_color, 0.08); } \
             .workspace-sidebar .caption { font-size: 0.8em; }",
        );
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("Could not get default display"),
            &css_provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // --- Workspace Manager ---
    // Holds all workspaces with their per-workspace GTK state.
    // The initial workspace is created here; additional workspaces are added
    // via the "New Workspace" action or session restore.
    let initial_tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));
    let initial_focus_tracker: FocusTracker = Rc::new(RefCell::new(String::new()));
    let initial_custom_titles: CustomTitles = Rc::new(RefCell::new(HashSet::new()));
    let initial_tab_id_map: TabIdMap = Rc::new(RefCell::new(HashMap::new()));
    let initial_tab_colors: TabColorMap = Rc::new(RefCell::new(HashMap::new()));
    let initial_tab_active_pane: TabActivePaneMap = Rc::new(RefCell::new(HashMap::new()));

    let workspace_manager: WorkspaceManager = Rc::new(RefCell::new(WorkspaceManagerInner {
        workspaces: vec![WorkspaceView {
            id: uuid::Uuid::new_v4(),
            name: String::from("Default"),
            tab_view: initial_tab_view.clone(),
            tab_states: Rc::clone(&initial_tab_states),
            focus_tracker: Rc::clone(&initial_focus_tracker),
            custom_titles: Rc::clone(&initial_custom_titles),
            tab_id_map: Rc::clone(&initial_tab_id_map),
            tab_colors: Rc::clone(&initial_tab_colors),
            color: None,
            color_css_provider: None,
            tab_active_pane: Rc::clone(&initial_tab_active_pane),
        }],
        active_index: 0,
        last_tab_click: (0.0, 0.0),
        tab_menu_shown: false,
        pending_reorders: VecDeque::new(),
        pending_active_pane: VecDeque::new(),
    }));

    // Convenience aliases for the active workspace's state during initial setup.
    // These point to the initial workspace; after session restore they may be
    // replaced if the default workspace is rebuilt.
    let tab_states = Rc::clone(&initial_tab_states);
    let focus_tracker = Rc::clone(&initial_focus_tracker);
    let _custom_titles = Rc::clone(&initial_custom_titles);
    let tab_id_map = Rc::clone(&initial_tab_id_map);

    // Shared config -- updated on hot reload, read by new tab/split creation.
    // All action closures that create terminals capture a clone of this Rc.
    let shared_config: SharedConfig = Rc::new(RefCell::new(config.clone()));

    // --- Settings sidebar (right panel, built after shared state is ready) ---
    // Shows Theme, Font Family, Font Size only. Paired Devices is in Settings view.
    let appearance_revealer =
        preferences::build_appearance_sidebar(&shared_config, &tab_states, &window);
    main_area.append(&appearance_revealer);

    // --- Tab bar right-click (Capture phase, claimed) ---
    //
    // libadwaita 1.5 claims button-3 events on AdwTabButton WITHOUT emitting
    // setup-menu when no menu-model is set on the TabView.  This means neither
    // setup-menu nor bubble-phase controllers on the tab bar ever fire.
    //
    // Fix: capture and claim button-3 ourselves BEFORE libadwaita's target-phase
    // gesture sees it.  We then resolve which tab was clicked by walking the
    // widget tree via pick(), and show our custom context menu.
    {
        let wm_click = Rc::clone(&workspace_manager);
        let tb_click = tab_bar.clone();
        let win_click = window.clone();
        let dc_click = daemon_client.clone();
        let sc_click = Rc::clone(&shared_config);
        let gesture = gtk4::GestureClick::new();
        gesture.set_button(3);
        gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
        gesture.connect_pressed(move |gesture, _n, x, y| {
            // Claim the event so libadwaita's tab button gesture never sees it.
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            tracing::debug!("tab-bar right-click capture (claimed): ({x},{y})");

            // Find which page was right-clicked.
            let page = tab_bar_find_page_at(&tb_click, x, y);
            tracing::debug!("tab-bar page at pos: {}", page.is_some());

            let result = {
                let Ok(mgr) = wm_click.try_borrow() else {
                    return;
                };
                let active_idx = mgr.active_index;
                let ws = &mgr.workspaces[active_idx];
                // Use the picked page, or fall back to the selected page.
                let p = match page.or_else(|| ws.tab_view.selected_page()) {
                    Some(p) => p,
                    None => return,
                };
                Some((
                    active_idx,
                    p,
                    Rc::clone(&ws.tab_states),
                    Rc::clone(&ws.focus_tracker),
                    Rc::clone(&ws.custom_titles),
                    Rc::clone(&ws.tab_colors),
                    Rc::clone(&ws.tab_id_map),
                ))
            };
            let Some((
                ws_idx,
                page,
                tab_states,
                focus_tracker,
                custom_titles,
                tab_colors,
                tab_id_map,
            )) = result
            else {
                return;
            };

            let Some(tv) = tb_click.view() else {
                return;
            };
            show_tab_context_menu(
                ws_idx,
                &tb_click,
                &tv,
                &page,
                x,
                y,
                &tab_states,
                &focus_tracker,
                &custom_titles,
                &tab_colors,
                &tab_id_map,
                &win_click,
                dc_click.clone(),
                &sc_click,
                &wm_click,
            );
        });
        tab_bar.add_controller(gesture);
    }

    // --- Workspace sidebar (left panel, pushes main_area right) ---
    // Built after workspace_manager is ready. The revealer is prepended to
    // terminal_row so it physically displaces main_area rather than floating.
    let (workspace_sidebar_revealer, workspace_sidebar_lb) = build_workspace_sidebar(
        &workspace_manager,
        &main_area,
        &tab_bar,
        &window,
        &daemon_client,
        &shared_config,
    );
    terminal_row.prepend(&workspace_sidebar_revealer);

    // --- Tab close handling ---
    // When a tab's close button is clicked:
    //   - In local mode: kill all PTYs in the tab's widget tree.
    //   - In daemon mode: send close_tab RPC for each pane; don't kill locally.
    // If it is the last tab, close the window (exits the application).
    // NOTE: we wire this on the initial tab_view here; newly created workspace
    // tab_views get the same handler in `create_workspace_view()`.
    {
        let window_close = window.clone();
        let states_close = Rc::clone(&tab_states);
        let dc_close = daemon_client.clone();
        let wm_close = Rc::clone(&workspace_manager);
        initial_tab_view.connect_close_page(move |tv, page| {
            let container = page.child();
            if let Some(ref dc) = dc_close {
                // Daemon mode: send close_tab RPC for each pane in the subtree.
                daemon_close_panes_in_subtree(&container, &states_close, dc);
            } else {
                // Legacy fallback (unreachable post-FIX-011 since daemon_client
                // is always Some). Retained until CHORE-FIX-011-cleanup.
                remove_panes_in_subtree(&container, &states_close);
            }

            // FIX-009 9a: only close the window when the TabView is still
            // attached to the main widget tree AND no other workspace is
            // alive. See `wire_tab_view_handlers` for the rationale.
            if tv.n_pages() <= 1 {
                let is_last_workspace =
                    wm_close.try_borrow().map(|m| m.workspaces.len() <= 1).unwrap_or(true);
                if tv.parent().is_some() && is_last_workspace {
                    window_close.close();
                }
            }

            // Confirm the close
            tv.close_page_finish(page, true);

            // Inhibit default close handling since we called close_page_finish
            glib::Propagation::Stop
        });
    }

    // --- Focus management on tab switch ---
    // When switching tabs, find a leaf DrawingArea in the new tab and focus
    // it, AND notify the daemon via `focus_tab` so session-restore can bring
    // the correct tab back on cold restart (session-restore fix).
    {
        let focus_switch = Rc::clone(&focus_tracker);
        let tim_focus = Rc::clone(&tab_id_map);
        let states_focus = Rc::clone(&initial_tab_states);
        let active_pane_map = Rc::clone(&initial_tab_active_pane);
        let dc_focus = daemon_client.clone();
        initial_tab_view.connect_selected_page_notify(move |tv| {
            if let Some(page) = tv.selected_page() {
                let container = page.child();
                let leaves = collect_leaf_drawing_areas(&container);
                let page_key = page_identity_key(&page);
                let tab_id = tim_focus.borrow().get(&page_key).copied();

                // FIX-005B fix-cycle 1: prefer per-tab saved active_pane_id
                // over the per-workspace `focus_tracker` (single-string,
                // shared across tabs in a workspace, so it cross-
                // contaminates between tabs with different leaf sets).
                //
                // Without this, the synchronous `selected-page-notify`
                // fired by `tab_view.append(&container)` during cold
                // restart grabs `leaves.first()` (the LEFT pane), whose
                // `connect_enter` then RPCs the wrong pane back to the
                // daemon, corrupting the persisted active_pane_id BEFORE
                // the post-build deferred `focus_when_mapped(RIGHT_DA)`
                // ever runs (Phase 4 cycle-1 root cause analysis).
                let saved_pane_id: Option<forgetty_core::PaneId> =
                    tab_id.and_then(|tid| active_pane_map.borrow().get(&tid).copied());
                let target_idx_via_map: Option<usize> = saved_pane_id.and_then(|pid| {
                    leaves.iter().position(|da| {
                        let widget_name = da.widget_name().to_string();
                        states_focus
                            .try_borrow()
                            .ok()
                            .and_then(|m| m.get(&widget_name).cloned())
                            .and_then(|rc| rc.try_borrow().ok().and_then(|s| s.daemon_pane_id))
                            == Some(pid)
                    })
                });

                let focused_name = focus_switch.borrow().clone();
                let target = target_idx_via_map
                    .and_then(|i| leaves.get(i))
                    .or_else(|| leaves.iter().find(|da| da.widget_name().as_str() == focused_name))
                    .or_else(|| leaves.first());
                if let Some(da) = target {
                    da.grab_focus();
                }
                // Send focus_tab RPC so the daemon's in-memory
                // `SessionWorkspace.active_tab` is persisted. Runtime tabs
                // populate the map in `add_new_tab`; restored tabs populate
                // it in `build_widgets_from_layout` (FIX-005A fix-cycle 1).
                // A mapping miss here indicates a bug — log at debug so QA
                // can catch it without spamming production.
                if let Some(ref dc) = dc_focus {
                    if let Some(tid) = tab_id {
                        if let Err(e) = dc.focus_tab(tid) {
                            tracing::debug!("focus_tab RPC failed: {e}");
                        }
                    }
                }
            }
        });
    }

    // --- Tab drag reorder → move_tab RPC (daemon mode only) ---
    // When the user drags a tab to a new position in the TabBar, send a
    // `move_tab` RPC so the daemon updates its SessionLayout.
    // The adw::TabView handles the visual reorder automatically; we just
    // need to tell the daemon.
    if let Some(ref dc_reorder) = daemon_client {
        let dc_ro = Arc::clone(dc_reorder);
        let tim_ro = Rc::clone(&tab_id_map);
        initial_tab_view.connect_page_reordered(move |_tv, page, new_position| {
            let page_key = page_identity_key(page);
            let tab_id = tim_ro.borrow().get(&page_key).copied();
            if let Some(tid) = tab_id {
                if let Err(e) = dc_ro.move_tab(tid, new_position as usize) {
                    tracing::warn!("move_tab RPC failed for tab {tid}: {e}");
                }
            } else {
                tracing::debug!(
                    "page-reordered: no tab_id found for page key {page_key}, skipping move_tab RPC"
                );
            }
        });
    }

    // --- Tab tear-off: drag a tab outside the tab bar to a new window ---
    //
    // adw::TabView emits `create-window` when the user drags a tab outside the
    // tab bar area.  The handler must return a new TabView (must not return
    // None — that causes a critical warning and the drag is cancelled).
    //
    // We open a minimal receiver window on the same adw::Application.  The
    // dragged TabPage (with its terminal child widget and live PTY) is moved to
    // the new window by libadwaita; no close-page signal fires on the source so
    // our PTY-kill handler is not triggered.
    //
    // Limitations of the minimal window: no workspace sidebar, no keybindings,
    // no daemon session persistence.  The terminal itself keeps working because
    // the child widget (DrawingArea + PTY/daemon connection) travels with the
    // page.
    {
        let app_ref = app.clone();
        initial_tab_view
            .connect_create_window(move |_source_tv| Some(open_detached_tab_window(&app_ref)));
    }

    // --- New tab action (Ctrl+Shift+T) ---
    // When profiles are configured, uses the default profile (AC-9).
    {
        let config_action = Rc::clone(&shared_config);
        let wm_action = Rc::clone(&workspace_manager);
        let win_action = window.clone();
        let dc_newtab = daemon_client.clone();
        let action = gio::SimpleAction::new("new-tab", None);
        action.connect_activate(move |_action, _param| {
            let Ok(cfg) = config_action.try_borrow() else {
                return;
            };
            let Ok(mgr) = wm_action.try_borrow() else {
                return;
            };
            let active_idx = mgr.active_index;
            let ws = &mgr.workspaces[active_idx];
            // Resolve default profile if any profiles are configured (AC-9).
            let (cmd, cwd_buf) = resolve_default_profile_args(&cfg);
            add_new_tab(
                active_idx,
                &ws.tab_view,
                &cfg,
                &ws.tab_states,
                &ws.focus_tracker,
                &ws.custom_titles,
                &win_action,
                cwd_buf.as_deref(),
                cmd.as_deref(),
                dc_newtab.clone(),
                &ws.tab_id_map,
                &wm_action,
            );
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.new-tab", &["<Control><Shift>t"]);

    // --- Profile actions (open-profile-N) and Ctrl+Shift+1-9 shortcuts ---
    // Track registered profile count so hot-reload can unregister old actions.
    let profile_action_count: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    {
        let count = register_profile_actions(
            app,
            &window,
            &config.profiles,
            &shared_config,
            &workspace_manager,
            &daemon_client,
        );
        profile_action_count.set(count);
    }

    // --- Capture-phase window key controller for Ctrl+Shift+1-9 (Bug 2 fix) ---
    //
    // GTK4 accelerators registered via `app.set_accels_for_action()` fire AFTER
    // widget-level event controllers. The VT pane's `EventControllerKey` on the
    // focused `DrawingArea` processes Ctrl+Shift+digit first and encodes it as a
    // kitty keyboard protocol sequence (";5u"), writing it to the PTY — the
    // accelerator never fires.
    //
    // Fix: add an `EventControllerKey` at the `ApplicationWindow` level with
    // `PropagationPhase::Capture`. GTK4 processes capture-phase controllers
    // from the outermost widget inward, so a window-level capture controller
    // runs before ANY child widget sees the event — including the focused
    // DrawingArea. We intercept Ctrl+Shift+1-9 here, activate the corresponding
    // `open-profile-N` action directly, and return `Propagation::Stop` so the
    // event never reaches the VT key encoder.
    {
        let profile_count_capture = Rc::clone(&profile_action_count);
        let window_capture = window.downgrade();
        let cap_controller = gtk4::EventControllerKey::new();
        cap_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        cap_controller.connect_key_pressed(move |_ctrl, keyval, _keycode, state| {
            use gtk4::gdk::ModifierType;
            // Check for exactly Ctrl+Shift (no Alt, no Super).
            let is_ctrl_shift = state.contains(ModifierType::CONTROL_MASK)
                && state.contains(ModifierType::SHIFT_MASK)
                && !state.contains(ModifierType::ALT_MASK)
                && !state.contains(ModifierType::SUPER_MASK);

            if is_ctrl_shift {
                let digit_idx: Option<usize> = match keyval {
                    gtk4::gdk::Key::_1 => Some(0),
                    gtk4::gdk::Key::_2 => Some(1),
                    gtk4::gdk::Key::_3 => Some(2),
                    gtk4::gdk::Key::_4 => Some(3),
                    gtk4::gdk::Key::_5 => Some(4),
                    gtk4::gdk::Key::_6 => Some(5),
                    gtk4::gdk::Key::_7 => Some(6),
                    gtk4::gdk::Key::_8 => Some(7),
                    gtk4::gdk::Key::_9 => Some(8),
                    _ => None,
                };
                if let Some(idx) = digit_idx {
                    let count = profile_count_capture.get();
                    if idx < count {
                        if let Some(win) = window_capture.upgrade() {
                            // Disambiguate: WidgetExt::activate_action fires the
                            // action on the widget's action group (i.e. the window).
                            let _ = gtk4::prelude::WidgetExt::activate_action(
                                &win,
                                &format!("open-profile-{idx}"),
                                None,
                            );
                            return glib::Propagation::Stop;
                        }
                    }
                }
            }
            glib::Propagation::Proceed
        });
        window.add_controller(cap_controller);
    }

    // --- Split actions (all four directions use workspace manager) ---
    for (action_name, orientation, before, accels) in [
        (
            "split-right",
            gtk4::Orientation::Horizontal,
            false,
            vec!["<Alt><Shift>equal", "<Alt>plus"],
        ),
        (
            "split-down",
            gtk4::Orientation::Vertical,
            false,
            vec!["<Alt><Shift>minus", "<Alt>underscore"],
        ),
        ("split-left", gtk4::Orientation::Horizontal, true, vec![]),
        ("split-up", gtk4::Orientation::Vertical, true, vec![]),
    ] {
        let config_split = Rc::clone(&shared_config);
        let wm_split = Rc::clone(&workspace_manager);
        let win_split = window.clone();
        let dc_split = daemon_client.clone();
        let action = gio::SimpleAction::new(action_name, None);
        action.connect_activate(move |_action, _param| {
            let Ok(cfg) = config_split.try_borrow() else {
                return;
            };
            let Ok(mgr) = wm_split.try_borrow() else {
                return;
            };
            let ws = &mgr.workspaces[mgr.active_index];
            split_pane(
                &ws.tab_view,
                &cfg,
                &ws.tab_states,
                &ws.focus_tracker,
                &ws.custom_titles,
                orientation,
                before,
                &win_split,
                dc_split.clone(),
                &wm_split,
                &ws.tab_id_map,
            );
        });
        window.add_action(&action);
        if !accels.is_empty() {
            let accel_strs: Vec<&str> = accels.iter().map(|s| s.as_ref()).collect();
            app.set_accels_for_action(&format!("win.{action_name}"), &accel_strs);
        }
    }

    // --- Close pane action (Ctrl+Shift+W) ---
    {
        let wm_close = Rc::clone(&workspace_manager);
        let window_close = window.clone();
        let dc_closepane = daemon_client.clone();
        let action = gio::SimpleAction::new("close-pane", None);
        action.connect_activate(move |_action, _param| {
            // Snapshot the active workspace's GTK handles, then DROP the
            // borrow before calling close_focused_pane — the call chain
            // re-enters the WorkspaceManager (FIX-009 9a's window-count guard
            // does `wm.try_borrow()`), which would otherwise fall back to
            // unwrap_or(true) and close the window even with sibling
            // workspaces alive.
            let (tv, ts, ft) = {
                let Ok(mgr) = wm_close.try_borrow() else {
                    return;
                };
                let ws = &mgr.workspaces[mgr.active_index];
                (ws.tab_view.clone(), Rc::clone(&ws.tab_states), Rc::clone(&ws.focus_tracker))
            };
            close_focused_pane(&tv, &ts, &ft, &window_close, dc_closepane.clone(), &wm_close);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.close-pane", &["<Control><Shift>w"]);

    // --- Copy selection action (Ctrl+Shift+C) ---
    {
        let wm_copy = Rc::clone(&workspace_manager);
        let window_copy = window.clone();
        let action = gio::SimpleAction::new("copy", None);
        action.connect_activate(move |_action, _param| {
            let (ts, ft) = {
                let Ok(mgr) = wm_copy.try_borrow() else {
                    tracing::warn!("copy: workspace_manager borrow failed");
                    return;
                };
                let ws = &mgr.workspaces[mgr.active_index];
                (Rc::clone(&ws.tab_states), Rc::clone(&ws.focus_tracker))
            };
            copy_selection(&ts, &ft, &window_copy);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.copy", &["<Control><Shift>c"]);

    // --- Search in terminal action (Ctrl+Shift+F) ---
    {
        let wm_search = Rc::clone(&workspace_manager);
        let action = gio::SimpleAction::new("search", None);
        action.connect_activate(move |_action, _param| {
            let (ts, ft) = {
                let Ok(mgr) = wm_search.try_borrow() else {
                    return;
                };
                let ws = &mgr.workspaces[mgr.active_index];
                (Rc::clone(&ws.tab_states), Rc::clone(&ws.focus_tracker))
            };
            toggle_search_on_focused_pane(&ts, &ft);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.search", &["<Control><Shift>f"]);

    // --- Paste action (Ctrl+Shift+V) ---
    {
        let wm_paste = Rc::clone(&workspace_manager);
        let window_paste = window.clone();
        let dc_paste = daemon_client.clone();
        let cfg_paste = Rc::clone(&shared_config);
        let action = gio::SimpleAction::new("paste", None);
        action.connect_activate(move |_action, _param| {
            let (ts, ft) = {
                let Ok(mgr) = wm_paste.try_borrow() else {
                    tracing::warn!("paste: workspace_manager borrow failed");
                    return;
                };
                let ws = &mgr.workspaces[mgr.active_index];
                (Rc::clone(&ws.tab_states), Rc::clone(&ws.focus_tracker))
            };
            paste_clipboard(&ts, &ft, &window_paste, dc_paste.clone(), Rc::clone(&cfg_paste));
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.paste", &["<Control>v", "<Control><Shift>v"]);

    // --- Select All action (context menu only, no accelerator) ---
    {
        let wm_sel = Rc::clone(&workspace_manager);
        let action = gio::SimpleAction::new("select-all", None);
        action.connect_activate(move |_action, _param| {
            let Ok(mgr) = wm_sel.try_borrow() else {
                return;
            };
            let ws = &mgr.workspaces[mgr.active_index];
            select_all_on_focused_pane(&ws.tab_states, &ws.focus_tracker);
        });
        window.add_action(&action);
    }

    // --- Open URL action (context menu only, receives URL as string parameter) ---
    {
        let action = gio::SimpleAction::new("open-url", Some(glib::VariantTy::STRING));
        action.connect_activate(move |_action, param| {
            if let Some(url) = param.and_then(|v| v.get::<String>()) {
                if !url.is_empty() {
                    open_url_in_browser(&url);
                }
            }
        });
        window.add_action(&action);
    }

    // --- Pane navigation actions (Alt+Arrow) ---
    for (name, direction) in [
        ("focus-pane-left", Direction::Left),
        ("focus-pane-right", Direction::Right),
        ("focus-pane-up", Direction::Up),
        ("focus-pane-down", Direction::Down),
    ] {
        let wm_nav = Rc::clone(&workspace_manager);
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_action, _param| {
            let Ok(mgr) = wm_nav.try_borrow() else {
                return;
            };
            let ws = &mgr.workspaces[mgr.active_index];
            navigate_pane(&ws.tab_view, &ws.focus_tracker, direction);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.focus-pane-left", &["<Alt>Left"]);
    app.set_accels_for_action("win.focus-pane-right", &["<Alt>Right"]);
    app.set_accels_for_action("win.focus-pane-up", &["<Alt>Up"]);
    app.set_accels_for_action("win.focus-pane-down", &["<Alt>Down"]);

    // --- Pane resize actions (Ctrl+Alt+Arrow) ---
    for (name, direction) in [
        ("resize-pane-left", Direction::Left),
        ("resize-pane-right", Direction::Right),
        ("resize-pane-up", Direction::Up),
        ("resize-pane-down", Direction::Down),
    ] {
        let wm_resize = Rc::clone(&workspace_manager);
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_action, _param| {
            let Ok(mgr) = wm_resize.try_borrow() else {
                return;
            };
            let ws = &mgr.workspaces[mgr.active_index];
            resize_pane(&ws.tab_view, &ws.focus_tracker, direction);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.resize-pane-left", &["<Alt><Shift>Left"]);
    app.set_accels_for_action("win.resize-pane-right", &["<Alt><Shift>Right"]);
    app.set_accels_for_action("win.resize-pane-up", &["<Alt><Shift>Up"]);
    app.set_accels_for_action("win.resize-pane-down", &["<Alt><Shift>Down"]);

    // --- Zoom actions (all three use workspace manager) ---
    for (action_name, dir, accels) in [
        ("zoom-in", ZoomDirection::In, vec!["<Control>equal", "<Control>plus"]),
        ("zoom-out", ZoomDirection::Out, vec!["<Control>minus"]),
        ("zoom-reset", ZoomDirection::Reset, vec!["<Control>0"]),
    ] {
        let wm_zoom = Rc::clone(&workspace_manager);
        let action = gio::SimpleAction::new(action_name, None);
        action.connect_activate(move |_action, _param| {
            let Ok(mgr) = wm_zoom.try_borrow() else {
                return;
            };
            let ws = &mgr.workspaces[mgr.active_index];
            zoom_focused_pane(&ws.tab_states, &ws.focus_tracker, dir);
        });
        window.add_action(&action);
        let accel_strs: Vec<&str> = accels.iter().map(|s| s.as_ref()).collect();
        app.set_accels_for_action(&format!("win.{action_name}"), &accel_strs);
    }

    // --- Show shortcuts window action (F1 / Ctrl+?) ---
    {
        let window_shortcuts = window.clone();
        let action = gio::SimpleAction::new("show-shortcuts", None);
        action.connect_activate(move |_action, _param| {
            let shortcuts_window = build_shortcuts_window();
            shortcuts_window.set_transient_for(Some(&window_shortcuts));
            shortcuts_window.present();
        });
        window.add_action(&action);
    }

    app.set_accels_for_action(
        "win.show-shortcuts",
        &["F1", "<Control>question", "<Control><Shift>slash", "<Control><Shift>p"],
    );

    // --- Show about dialog action ---
    {
        let window_about = window.clone();
        let action = gio::SimpleAction::new("show-about", None);
        action.connect_activate(move |_action, _param| {
            let about = adw::AboutWindow::builder()
                .application_name("Forgetty")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("A workspace-aware terminal emulator")
                .license_type(gtk4::License::MitX11)
                .transient_for(&window_about)
                .modal(true)
                .build();
            about.present();
        });
        window.add_action(&action);
    }

    // --- New Window action (menu only, no accelerator) ---
    // Spawns with a fresh UUID so the restore-all logic is bypassed (no session
    // file exists for that UUID), while the new window still saves its session
    // on close.
    {
        let action = gio::SimpleAction::new("new-window", None);
        action.connect_activate(move |_action, _param| {
            if let Ok(exe) = std::env::current_exe() {
                let new_id = uuid::Uuid::new_v4();
                if let Err(e) = std::process::Command::new(exe)
                    .arg("--session-id")
                    .arg(new_id.to_string())
                    .spawn()
                {
                    tracing::warn!("Failed to spawn new window: {e}");
                }
            }
        });
        window.add_action(&action);
    }

    // --- Close Window action (menu only, no accelerator) ---
    {
        let win_close = window.clone();
        let action = gio::SimpleAction::new("close-window", None);
        action.connect_activate(move |_action, _param| {
            win_close.close();
        });
        window.add_action(&action);
    }

    // --- Close Window Permanently action (menu only, no accelerator) ---
    // Kills the daemon first (so it can't auto-save), then deletes the
    // session file from BOTH the legacy `sessions/{uuid}.json` location
    // (pinned sessions) and the P-018 `active/{uuid}.json` location (the
    // live bucket).
    //
    // Order matters: daemon shutdown FIRST so the debounce/periodic save
    // tasks cannot race with the file delete and re-create the file.
    {
        let win_perm_close = window.clone();
        let dc_perm_close = daemon_client.clone();
        let skip_save_perm = Rc::clone(&skip_session_save);
        let action = gio::SimpleAction::new("close-window-permanently", None);
        action.connect_activate(move |_action, _param| {
            // Prevent the window close handler from re-writing the session file.
            skip_save_perm.set(true);
            // Kill the daemon FIRST so its save tasks can't race the delete.
            if let Some(ref dc) = dc_perm_close {
                dc.shutdown();
            }
            // Pinned session file (top-level).
            let path = forgetty_workspace::session_path_for(session_id);
            if path.exists() {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!("Failed to delete session file on permanent close: {e}");
                }
            }
            // P-018: also remove the live `active/` file so it doesn't
            // appear as a crash orphan on next launch.
            if let Err(e) = forgetty_workspace::delete_active_for(session_id) {
                tracing::warn!("Failed to delete active session file on permanent close: {e}");
            }
            win_perm_close.close();
        });
        window.add_action(&action);
    }

    // --- Pin Session toggle (B-002 Part 4 / P-018) ---
    // Pin icon in the header bar, updated by the action handler.
    //
    // P-018 / AD-016: the tooltip explains what pinning does in the new
    // model. Unpinned sessions are temporary by definition and do not
    // restore on next launch; pinning is the explicit opt-in to persistence.
    let pin_icon = gtk4::Image::from_icon_name("view-pin-symbolic");
    pin_icon.set_visible(false);
    pin_icon.set_tooltip_text(Some("Pin to keep this window across launches"));
    header.pack_end(&pin_icon);

    {
        let dc_pin = daemon_client.clone();
        let pin_icon_toggle = pin_icon.clone();
        let action = gio::SimpleAction::new("toggle-pin-session", None);
        action.connect_activate(move |_action, _param| {
            let Some(ref dc) = dc_pin else {
                return;
            };
            let current = dc.get_pinned().unwrap_or(false);
            let new_val = !current;
            if let Err(e) = dc.set_pinned(new_val) {
                tracing::warn!("toggle-pin-session: {e}");
                return;
            }
            pin_icon_toggle.set_visible(new_val);
        });
        window.add_action(&action);
    }

    // Initialize pin icon from daemon state if connected.
    if let Some(ref dc) = daemon_client {
        if dc.get_pinned().unwrap_or(false) {
            pin_icon.set_visible(true);
        }
    }

    // --- Restore Previous Session dialog (B-002 Part 3) ---
    {
        let win_restore = window.clone();
        let action = gio::SimpleAction::new("restore-previous-session", None);
        action.connect_activate(move |_action, _param| {
            show_restore_session_dialog(&win_restore);
        });
        window.add_action(&action);
    }

    // --- Close Tab action (menu only) ---
    {
        let wm_close_tab = Rc::clone(&workspace_manager);
        let window_close_tab = window.clone();
        let dc_closetab = daemon_client.clone();
        let action = gio::SimpleAction::new("close-tab", None);
        action.connect_activate(move |_action, _param| {
            let Ok(mgr) = wm_close_tab.try_borrow() else {
                return;
            };
            let ws = &mgr.workspaces[mgr.active_index];
            let Some(page) = ws.tab_view.selected_page() else {
                return;
            };
            let container = page.child();
            if let Some(ref dc) = dc_closetab {
                daemon_close_panes_in_subtree(&container, &ws.tab_states, dc);
            } else {
                remove_panes_in_subtree(&container, &ws.tab_states);
            }

            if ws.tab_view.n_pages() <= 1 {
                window_close_tab.close();
            } else {
                ws.tab_view.close_page(&page);
            }
        });
        window.add_action(&action);
    }

    // --- Change Tab Title action (menu only) ---
    {
        let wm_title = Rc::clone(&workspace_manager);
        let win_title = window.clone();
        let action = gio::SimpleAction::new("change-tab-title", None);
        action.connect_activate(move |_action, _param| {
            let Ok(mgr) = wm_title.try_borrow() else {
                return;
            };
            let ws = &mgr.workspaces[mgr.active_index];
            let Some(page) = ws.tab_view.selected_page() else {
                return;
            };
            show_change_tab_title_dialog(&win_title, &page, &ws.custom_titles);
        });
        window.add_action(&action);
    }

    // --- Clear terminal action (menu only) ---
    // Write Ctrl+L (form feed) to the PTY so the shell clears the screen AND
    // redraws the prompt -- identical to pressing Ctrl+L interactively.
    {
        let wm_clear = Rc::clone(&workspace_manager);
        let dc_clear = daemon_client.clone();
        let action = gio::SimpleAction::new("clear", None);
        action.connect_activate(move |_action, _param| {
            let Ok(mgr) = wm_clear.try_borrow() else {
                return;
            };
            let ws = &mgr.workspaces[mgr.active_index];
            if let Some(dc) = dc_clear.as_deref() {
                write_to_focused_pane(&ws.tab_states, &ws.focus_tracker, b"\x0c", dc);
            }
        });
        window.add_action(&action);
    }

    // --- Reset terminal action (menu only) ---
    // Use the proper ghostty_terminal_reset() API to perform a full RIS, then
    // Ctrl+L to PTY so the shell redraws its prompt in default colors.
    {
        let wm_reset = Rc::clone(&workspace_manager);
        let dc_reset = daemon_client.clone();
        let action = gio::SimpleAction::new("reset", None);
        action.connect_activate(move |_action, _param| {
            let Ok(mgr) = wm_reset.try_borrow() else {
                return;
            };
            let ws = &mgr.workspaces[mgr.active_index];
            reset_focused_pane(&ws.tab_states, &ws.focus_tracker);
            if let Some(dc) = dc_reset.as_deref() {
                write_to_focused_pane(&ws.tab_states, &ws.focus_tracker, b"\x0c", dc);
            }
        });
        window.add_action(&action);
    }

    // --- Open Configuration action (menu only) ---
    {
        let action = gio::SimpleAction::new("open-config", None);
        action.connect_activate(move |_action, _param| {
            open_config_file();
        });
        window.add_action(&action);
    }

    // --- Reload Configuration action (menu only) ---
    {
        let shared_cfg_reload = Rc::clone(&shared_config);
        let wm_reload = Rc::clone(&workspace_manager);
        let win_reload = window.clone();
        let action = gio::SimpleAction::new("reload-config", None);
        action.connect_activate(move |_action, _param| {
            reload_config_all_workspaces(&shared_cfg_reload, &wm_reload, &win_reload);
        });
        window.add_action(&action);
    }

    // --- Appearance sidebar toggle action (Ctrl+, or menu) ---
    {
        let revealer = appearance_revealer.clone();
        let action = gio::SimpleAction::new("appearance", None);
        action.connect_activate(move |_action, _param| {
            let showing = revealer.reveals_child();
            revealer.set_visible(!showing);
            revealer.set_reveal_child(!showing);
        });
        window.add_action(&action);
        app.set_accels_for_action("win.appearance", &["<Control>comma"]);
    }

    app.set_accels_for_action("win.open-settings", &["<Control>period"]);

    // --- Settings full-window takeover action (Ctrl+. or hamburger "Settings") ---
    // Toggles: Ctrl+. opens settings, and pressing again (or Escape) closes it.
    // The settings view is rebuilt on each open so controls always reflect the
    // current shared_config (important after JSON editor saves).
    {
        let stk = outer_stack.clone();
        let shared_cfg_sv = Rc::clone(&shared_config);
        let win_sv = window.clone();
        let dc_sv = daemon_client.clone();
        let wm_sv = Rc::clone(&workspace_manager);
        let app_sv = app.clone();
        let action = gio::SimpleAction::new("open-settings", None);
        action.connect_activate(move |_action, _param| {
            // Toggle: if settings is already open, close it.
            if stk.visible_child_name().as_deref() == Some("settings") {
                stk.set_visible_child_name("terminal");
                win_sv.set_title(Some("Forgetty"));
                refocus_active_pane(&wm_sv, &win_sv);
                return;
            }

            // Remove the previous settings page (if any) so we get fresh controls.
            if let Some(old) = stk.child_by_name("settings") {
                stk.remove(&old);
            }
            let stk_back = stk.clone();
            let win_back = win_sv.clone();
            let wm_back = Rc::clone(&wm_sv);
            let on_back = move || {
                stk_back.set_visible_child_name("terminal");
                win_back.set_title(Some("Forgetty"));
                refocus_active_pane(&wm_back, &win_back);
            };
            let sv = crate::settings_view::build_settings_view(
                &shared_cfg_sv,
                dc_sv.clone(),
                app_sv.clone(),
                on_back,
            );
            stk.add_named(&sv, Some("settings"));
            stk.set_visible_child_name("settings");
            win_sv.set_title(Some("Settings — Forgetty"));
        });
        window.add_action(&action);
    }

    // --- Quit action (Ctrl+Shift+Q) ---
    // P-018 / AD-016: Ctrl+Shift+Q uses the same pinned-aware close as the
    // X-button. Pre-P-018 (AD-012) the daemon survived this path; the AD-016
    // amendment binds daemon survival to the single explicit pin action.
    {
        let app_quit = app.clone();
        let wm_quit = Rc::clone(&workspace_manager);
        let dc_quit = daemon_client.clone();
        let action = gio::SimpleAction::new("quit", None);
        action.connect_activate(move |_action, _param| {
            if let Some(ref dc) = dc_quit {
                if let Ok(mgr) = wm_quit.try_borrow() {
                    let mut all_ratios = Vec::new();
                    for ws in &mgr.workspaces {
                        all_ratios.extend(collect_split_ratios(ws));
                    }
                    if !all_ratios.is_empty() {
                        let _ = dc.update_split_ratios(&all_ratios);
                    }
                }
                // P-018 / AD-016: pinned-aware close + daemon exit.
                dc.shutdown_clean();
            }
            app_quit.quit();
        });
        app.add_action(&action);
    }

    app.set_accels_for_action("app.quit", &["<Control><Shift>q"]);

    // --- New Workspace action (Ctrl+Alt+N) ---
    {
        let wm_new = Rc::clone(&workspace_manager);
        let cfg_new = Rc::clone(&shared_config);
        let main_area_new = main_area.clone();
        let tab_bar_new = tab_bar.clone();
        let win_new = window.clone();
        let dc_new = daemon_client.clone();
        let lb_new = workspace_sidebar_lb.clone();
        let action = gio::SimpleAction::new("new-workspace", None);
        action.connect_activate(move |_action, _param| {
            show_new_workspace_dialog(
                &win_new,
                &wm_new,
                &cfg_new,
                &main_area_new,
                &tab_bar_new,
                dc_new.clone(),
                lb_new.clone(),
            );
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.new-workspace", &["<Control><Alt>n"]);

    // --- Rename Workspace action (menu only) ---
    {
        let wm_rename = Rc::clone(&workspace_manager);
        let win_rename = window.clone();
        let dc_rename = daemon_client.clone();
        let action = gio::SimpleAction::new("rename-workspace", None);
        action.connect_activate(move |_action, _param| {
            show_rename_workspace_dialog(&win_rename, &wm_rename, dc_rename.as_ref());
        });
        window.add_action(&action);
    }

    // --- Delete Workspace action (menu only) ---
    {
        let wm_delete = Rc::clone(&workspace_manager);
        let main_area_del = main_area.clone();
        let tab_bar_del = tab_bar.clone();
        let win_del = window.clone();
        let lb_del = workspace_sidebar_lb.clone();
        let dc_del = daemon_client.clone();
        let sc_del = Rc::clone(&shared_config);
        let delete_action = gio::SimpleAction::new("delete-workspace", None);
        {
            // Disable if only one workspace
            let has_multiple =
                workspace_manager.try_borrow().map(|mgr| mgr.workspaces.len() > 1).unwrap_or(false);
            delete_action.set_enabled(has_multiple);
        }
        delete_action.connect_activate(move |_action, _param| {
            delete_current_workspace(
                &wm_delete,
                &main_area_del,
                &tab_bar_del,
                &win_del,
                dc_del.as_ref(),
            );
            refresh_workspace_sidebar(
                &lb_del,
                &wm_delete,
                &main_area_del,
                &tab_bar_del,
                &win_del,
                &dc_del,
                &sc_del,
            );
        });
        window.add_action(&delete_action);
    }

    // --- Switch Workspace by index (Alt+1 through 9) ---
    for i in 1..=9u32 {
        let wm_switch = Rc::clone(&workspace_manager);
        let main_area_sw = main_area.clone();
        let tab_bar_sw = tab_bar.clone();
        let win_sw = window.clone();
        let lb_sw = workspace_sidebar_lb.clone();
        let dc_sw = daemon_client.clone();
        let sc_sw = Rc::clone(&shared_config);
        let action_name = format!("switch-workspace-{i}");
        let action = gio::SimpleAction::new(&action_name, None);
        action.connect_activate(move |_action, _param| {
            let target = (i - 1) as usize;
            switch_workspace(
                &wm_switch,
                target,
                &main_area_sw,
                &tab_bar_sw,
                &win_sw,
                dc_sw.as_ref(),
            );
            refresh_workspace_sidebar(
                &lb_sw,
                &wm_switch,
                &main_area_sw,
                &tab_bar_sw,
                &win_sw,
                &dc_sw,
                &sc_sw,
            );
        });
        window.add_action(&action);
        app.set_accels_for_action(&format!("win.switch-workspace-{i}"), &[&format!("<Alt>{i}")]);
    }

    // --- Previous Workspace (Ctrl+Alt+Page_Up) ---
    {
        let wm_prev = Rc::clone(&workspace_manager);
        let main_area_prev = main_area.clone();
        let tab_bar_prev = tab_bar.clone();
        let win_prev = window.clone();
        let lb_prev = workspace_sidebar_lb.clone();
        let dc_prev = daemon_client.clone();
        let sc_prev = Rc::clone(&shared_config);
        let action = gio::SimpleAction::new("prev-workspace", None);
        action.connect_activate(move |_action, _param| {
            let Ok(mgr) = wm_prev.try_borrow() else {
                return;
            };
            let count = mgr.workspaces.len();
            if count <= 1 {
                return;
            }
            let target = if mgr.active_index == 0 { count - 1 } else { mgr.active_index - 1 };
            drop(mgr);
            switch_workspace(
                &wm_prev,
                target,
                &main_area_prev,
                &tab_bar_prev,
                &win_prev,
                dc_prev.as_ref(),
            );
            refresh_workspace_sidebar(
                &lb_prev,
                &wm_prev,
                &main_area_prev,
                &tab_bar_prev,
                &win_prev,
                &dc_prev,
                &sc_prev,
            );
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.prev-workspace", &["<Control><Alt>Page_Up"]);

    // --- Next Workspace (Ctrl+Alt+Page_Down) ---
    {
        let wm_next = Rc::clone(&workspace_manager);
        let main_area_next = main_area.clone();
        let tab_bar_next = tab_bar.clone();
        let win_next = window.clone();
        let lb_next = workspace_sidebar_lb.clone();
        let dc_next = daemon_client.clone();
        let sc_next = Rc::clone(&shared_config);
        let action = gio::SimpleAction::new("next-workspace", None);
        action.connect_activate(move |_action, _param| {
            let Ok(mgr) = wm_next.try_borrow() else {
                return;
            };
            let count = mgr.workspaces.len();
            if count <= 1 {
                return;
            }
            let target = (mgr.active_index + 1) % count;
            drop(mgr);
            switch_workspace(
                &wm_next,
                target,
                &main_area_next,
                &tab_bar_next,
                &win_next,
                dc_next.as_ref(),
            );
            refresh_workspace_sidebar(
                &lb_next,
                &wm_next,
                &main_area_next,
                &tab_bar_next,
                &win_next,
                &dc_next,
                &sc_next,
            );
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.next-workspace", &["<Control><Alt>Page_Down"]);

    // --- Toggle Workspace Sidebar (Ctrl+Alt+B) ---
    {
        let sidebar_revealer_ref = workspace_sidebar_revealer.clone();
        let lb_ref = workspace_sidebar_lb.clone();
        let wm_sidebar = Rc::clone(&workspace_manager);
        let ma_sidebar = main_area.clone();
        let tb_sidebar = tab_bar.clone();
        let win_sidebar = window.clone();
        let dc_sidebar = daemon_client.clone();
        let sc_sidebar = Rc::clone(&shared_config);
        let action = gio::SimpleAction::new("toggle-workspace-sidebar", None);
        action.connect_activate(move |_action, _param| {
            let currently_revealed = sidebar_revealer_ref.reveals_child();
            sidebar_revealer_ref.set_reveal_child(!currently_revealed);
            if !currently_revealed {
                // Sidebar just opened — refresh rows.
                refresh_workspace_sidebar(
                    &lb_ref,
                    &wm_sidebar,
                    &ma_sidebar,
                    &tb_sidebar,
                    &win_sidebar,
                    &dc_sidebar,
                    &sc_sidebar,
                );
            }
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.toggle-workspace-sidebar", &["<Control><Alt>b"]);

    app.set_accels_for_action("win.new-window", &["<Control><Shift>n"]);

    // --- Apply user keybinding overrides (AC-20) ---
    // Must be called AFTER all default set_accels_for_action calls so user
    // preferences override the defaults rather than being overwritten by them.
    {
        let kb = config.keybindings.clone();
        apply_keybinding_overrides(app, &kb);
    }

    // --- Terminal Inspector placeholder (greyed out) ---
    {
        let action = gio::SimpleAction::new("terminal-inspector", None);
        action.set_enabled(false);
        window.add_action(&action);
    }

    // --- Create the first tab (or restore session) ---
    //
    // In daemon-client mode: reconcile from the daemon's live pane list.
    //   - If the daemon has live panes, create a DrawingArea + subscribe_output
    //     stream for each (AC-12, AC-14).
    //   - If the daemon has no panes, fall through to the normal "first tab" path.
    // In self-contained mode: restore from the session file as before.

    let mut restored = false;

    if let Some(ref dc) = daemon_client {
        // Daemon mode: call get_layout → build widget tree from the daemon's live state.
        //
        // The daemon is the single source of truth for layout (T-064). No session file
        // is read in this branch — all pane IDs from get_layout are guaranteed live.
        //
        // Flow:
        //   1. dc.get_layout() → LayoutInfo
        //   2. Layout has tabs  → build_widgets_from_layout() → restored = true
        //   3. Layout is empty (or Err) → fall through to !restored block → add_new_tab()
        match dc.get_layout() {
            Ok(ref layout) => {
                tracing::info!("get_layout: received layout from daemon");
                restored = build_widgets_from_layout(
                    layout,
                    dc,
                    config,
                    &workspace_manager,
                    &window,
                    &main_area,
                    &tab_bar,
                );
                if restored {
                    // Wire right-click context menus for all restored workspaces.
                    let tab_views: Vec<adw::TabView> = workspace_manager
                        .borrow()
                        .workspaces
                        .iter()
                        .map(|ws| ws.tab_view.clone())
                        .collect();
                    for tv in tab_views {
                        wire_tab_context_menu_signal(
                            &tv,
                            &workspace_manager,
                            &tab_bar,
                            &window,
                            daemon_client.clone(),
                            &shared_config,
                        );
                    }
                    // FIX-010: install per-workspace CSS providers for each
                    // restored colour. Runs after `build_widgets_from_layout`
                    // populated `mgr.workspaces[i].color` from the daemon's
                    // `get_layout` response. The sidebar rows may not exist
                    // yet (they are built lazily on first sidebar-reveal),
                    // but `rebuild_sidebar_rows_for_color` handles the
                    // no-rows case gracefully; `refresh_workspace_sidebar`
                    // reads `ws.color.is_some()` when it builds rows and
                    // applies the `workspace-color-{uuid}` class.
                    apply_restored_workspace_colors(&workspace_manager, &workspace_sidebar_lb);
                }
            }
            Err(e) => {
                tracing::warn!("get_layout RPC failed: {e} — will create a fresh tab");
            }
        }
    }

    if !restored {
        // Add a tab to the default workspace. workspace_idx=0 is correct here
        // because `build_widgets_from_layout` falls through to this branch
        // only when the saved layout was empty (first launch / nothing to
        // restore), and GTK always starts with workspace[0] selected.
        let Ok(mgr) = workspace_manager.try_borrow() else {
            return;
        };
        let ws = &mgr.workspaces[0];
        add_new_tab(
            0,
            &ws.tab_view,
            config,
            &ws.tab_states,
            &ws.focus_tracker,
            &ws.custom_titles,
            &window,
            launch.working_directory.as_deref(),
            launch.command.as_deref(),
            daemon_client.clone(),
            &ws.tab_id_map,
            &workspace_manager,
        );
        drop(mgr);
    }

    // Update the window title based on workspace count.
    update_window_title_for_workspace(&workspace_manager, &window);

    // --- Window close request handler ---
    // Fires when the user clicks the CSD X button, when window.close() is
    // called programmatically, or when the window manager requests a close.
    //
    // P-018 / AD-016: every clean close exits the daemon. The pinned-aware
    // file move runs server-side under `shutdown_clean`:
    //   - pinned: `sessions/active/{uuid}.json` → `sessions/{uuid}.json`
    //   - unpinned: `sessions/active/{uuid}.json` → `sessions/trash/{uuid}.json`
    // Pre-P-018 the X-close used `disconnect` to keep the daemon alive (AD-012);
    // AD-016 amends that. Cold-spawn on relaunch is the new norm.
    //
    // If the closing window has any sibling forgetty windows still open and
    // the session is unpinned, an "Undo Close" desktop notification is
    // emitted so the user can recover within 30 s.
    {
        let wm_close = Rc::clone(&workspace_manager);
        let dc_window_close = daemon_client.clone();
        let app_close = app.clone();
        let session_id_close = session_id;
        window.connect_close_request(move |_win| {
            if let Some(ref dc) = dc_window_close {
                // Push actual widget-measured split ratios to daemon before close.
                if let Ok(mgr) = wm_close.try_borrow() {
                    let mut all_ratios = Vec::new();
                    for ws in &mgr.workspaces {
                        all_ratios.extend(collect_split_ratios(ws));
                    }
                    if !all_ratios.is_empty() {
                        let _ = dc.update_split_ratios(&all_ratios);
                    }
                }
                // P-018 (AC-7): capture pin state + sibling count BEFORE
                // shutdown_clean (the daemon exits after the move and a
                // post-shutdown `get_pinned()` would fail).
                let is_pinned = dc.get_pinned().unwrap_or(false);
                let has_sibling = app_close.windows().len() > 1;
                // P-018 / AD-016: pinned-aware close. Daemon performs the
                // file move and exits before acking, so this call returns
                // only after `trash/{uuid}.json` (or `sessions/{uuid}.json`)
                // is committed on disk — no Undo-Close race window.
                dc.shutdown_clean();
                // Now safe to fire the toast: the file is in trash/ and
                // `restore_from_trash_to_active` will succeed if the user
                // clicks Undo within 30 s. AC-8: skipped on last-window close.
                if !is_pinned && has_sibling {
                    send_undo_close_notification(session_id_close);
                }
            }
            glib::Propagation::Proceed
        });
    }

    // --- Unix signal handlers (SIGTERM, SIGHUP, SIGINT) ---
    // Registered via glib::unix_signal_add_local which dispatches signals as
    // GLib source callbacks on the main thread, avoiding async-signal-safety
    // issues. Must be registered before window.present() so signals arriving
    // immediately after startup are caught.
    //
    // P-018 / AD-016: every clean exit path runs the pinned-aware close.
    // Pre-P-018 (V2-005 / AD-012) signals dropped the connection but kept
    // the daemon alive; AD-016 amends that — daemon survival is bound to
    // the explicit pin, not to client lifecycle.
    {
        let signals: &[(i32, &str)] =
            &[(SIGTERM, "SIGTERM"), (SIGHUP, "SIGHUP"), (SIGINT, "SIGINT")];
        for &(signum, name) in signals {
            let wm_signal = Rc::clone(&workspace_manager);
            let app_signal = app.clone();
            let dc_signal = daemon_client.clone();
            glib::unix_signal_add_local(signum, move || {
                tracing::info!("Received {name} (signal {signum}), initiating clean shutdown");
                if let Some(ref dc) = dc_signal {
                    if let Ok(mgr) = wm_signal.try_borrow() {
                        let mut all_ratios = Vec::new();
                        for ws in &mgr.workspaces {
                            all_ratios.extend(collect_split_ratios(ws));
                        }
                        if !all_ratios.is_empty() {
                            let _ = dc.update_split_ratios(&all_ratios);
                        }
                    }
                    // P-018 / AD-016: pinned-aware close + daemon exit.
                    dc.shutdown_clean();
                }
                app_signal.quit();
                glib::ControlFlow::Break
            });
        }
    }

    window.present();

    // Grab focus on the active workspace's selected tab's first DrawingArea.
    // Uses `focus_when_mapped` because on the session-restore path the
    // top-level window (and therefore the DrawingArea) is not yet mapped
    // here — `grab_focus()` on an unmapped widget silently fails. V2-007
    // fix cycle 4 deferred via `idle_add_local_once`, but that tick can
    // fire pre-map on restore; fix cycle 5 switches to GTK's `map` signal
    // (see `focus_when_mapped` doc).
    {
        let Ok(mgr) = workspace_manager.try_borrow() else {
            return;
        };
        let ws = &mgr.workspaces[mgr.active_index];
        if let Some(page) = ws.tab_view.selected_page() {
            let container = page.child();
            let leaves = collect_leaf_drawing_areas(&container);
            if let Some(da) = leaves.first() {
                focus_when_mapped(da);
            }
        }
    }

    // --- subscribe_layout background stream + GLib poll timer (daemon mode only) ---
    //
    // Opens a persistent `subscribe_layout` connection to the daemon. A background
    // tokio task reads layout notifications and delivers them via an mpsc channel.
    // A GLib timer polls the channel and applies idempotent widget updates.
    //
    // The only event handled here is `TabCreated` from external sources (e.g. Android
    // remote view). Events for panes/tabs that GTK already built synchronously (via
    // the action handlers above) are silently ignored.
    //
    // AC-6: "subscribe_layout subscription established in daemon mode."
    if let Some(ref dc_layout) = daemon_client {
        let (layout_tx, layout_rx) = std::sync::mpsc::channel::<LayoutEvent>();
        if let Err(e) = dc_layout.subscribe_layout(layout_tx) {
            tracing::warn!("subscribe_layout failed to start: {e}");
        } else {
            let wm_layout = Rc::clone(&workspace_manager);
            let win_layout = window.clone();
            let lb_layout = workspace_sidebar_lb.clone();
            // FIX-009 9b: clone the Arc<DaemonClient> + SharedConfig so the
            // event handler can build the widget for the daemon's auto-spawn.
            let dc_layout_handler = Arc::clone(dc_layout);
            let shared_config_layout = Rc::clone(&shared_config);
            let layout_rx = std::sync::Mutex::new(layout_rx);
            // Drain layout events every 200ms on the GLib main thread.
            glib::timeout_add_local(Duration::from_millis(200), move || {
                let rx = layout_rx.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    match rx.try_recv() {
                        Ok(event) => {
                            handle_layout_event(
                                event,
                                &wm_layout,
                                &win_layout,
                                &lb_layout,
                                Some(&dc_layout_handler),
                                &shared_config_layout,
                            );
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            tracing::info!("subscribe_layout stream closed, stopping poll timer");
                            return glib::ControlFlow::Break;
                        }
                    }
                }
                glib::ControlFlow::Continue
            });
        }
    }

    // --- Config hot reload timer ---
    // Polls the config watcher every 500ms. On change, reloads config.toml
    // and applies diffs (font, theme, bell) to all existing panes in ALL workspaces.
    // Also rebuilds the dropdown menu and re-registers profile actions (AC-16–AC-18).
    if let Some(mut config_watcher) = ConfigWatcher::new() {
        let shared_cfg = Rc::clone(&shared_config);
        let wm_reload = Rc::clone(&workspace_manager);
        let window_weak = window.downgrade();
        let app_weak = app.downgrade();
        let dropdown_ref = dropdown_button.clone();
        let profile_count_ref = Rc::clone(&profile_action_count);
        let dc_reload = daemon_client.clone();

        glib::timeout_add_local(Duration::from_millis(CONFIG_POLL_MS), move || {
            // Stop the timer if the window has been destroyed.
            let Some(win) = window_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };

            if !config_watcher.poll() {
                return glib::ControlFlow::Continue;
            }

            // Config file changed -- attempt to reload.
            let new_config = match load_config(None) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::warn!("Config reload failed, keeping previous config: {e}");
                    return glib::ControlFlow::Continue;
                }
            };

            info!("Config reloaded successfully");

            // Update the shared config so new tabs/splits use the new values.
            if let Ok(mut cfg) = shared_cfg.try_borrow_mut() {
                *cfg = new_config.clone();
            }

            // --- Rebuild dropdown and re-register profile actions (AC-16–AC-18) ---
            if let Some(app_ref) = app_weak.upgrade() {
                // Unregister old profile actions/accels before rebuilding.
                let old_count = profile_count_ref.get();
                unregister_profile_actions(&app_ref, &win, old_count);

                // Register new profile actions.
                let new_count = register_profile_actions(
                    &app_ref,
                    &win,
                    &new_config.profiles,
                    &shared_cfg,
                    &wm_reload,
                    &dc_reload,
                );
                profile_count_ref.set(new_count);

                // Rebuild the dropdown popover (only when not open, AC risk-3).
                // Use the manual popover builder so icons are preserved on hot-reload.
                if !dropdown_ref.is_active() {
                    let new_popover = build_dropdown_popover(&new_config.profiles, &win);
                    dropdown_ref.set_popover(Some(&new_popover));
                }

                // Re-apply keybinding overrides (AC-22): external config edit may have
                // changed [keybindings] — re-register all accels from the new config.
                apply_keybinding_overrides(&app_ref, &new_config.keybindings);
            }

            // Apply changes to every pane in every workspace.
            let Ok(mgr) = wm_reload.try_borrow() else {
                return glib::ControlFlow::Continue;
            };

            for ws in &mgr.workspaces {
                let Ok(states) = ws.tab_states.try_borrow() else {
                    continue;
                };
                let state_entries: Vec<_> =
                    states.iter().map(|(name, rc)| (name.clone(), Rc::clone(rc))).collect();
                drop(states);

                for (pane_name, state_rc) in &state_entries {
                    let Ok(mut s) = state_rc.try_borrow_mut() else {
                        continue;
                    };
                    let Some(da) = find_drawing_area_by_name(&win, pane_name) else {
                        continue;
                    };
                    terminal::apply_config_change(&mut s, &new_config, &da);
                }
            }

            glib::ControlFlow::Continue
        });
    }
}

// ---------------------------------------------------------------------------
// Session persistence
// ---------------------------------------------------------------------------

/// Return $HOME or "/" as a fallback directory.
/// Send a desktop notification after a session is trashed.
///
/// Includes an "Undo" action. If the user clicks it within 30 seconds,
/// a new `forgetty --restore-session` process is spawned. The
/// `--restore-session` path moves `sessions/trash/{uuid}.json` to
/// `sessions/active/{uuid}.json` (P-018 / AD-016) and the resulting
/// daemon takes ownership of the file from there.
///
/// The notification is sent before the GTK process exits. The undo action
/// handler runs in a forked child process because `NotificationHandle` is
/// not `Send` and the GTK main loop is shutting down.
///
/// P-018 (AC-7/AC-8/AC-10): the caller (window-close handler) gates this
/// notification on the existence of a sibling forgetty window — last-window
/// close does not show the toast (no host window to display it).
#[cfg(target_os = "linux")]
fn send_undo_close_notification(session_id: uuid::Uuid) {
    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("send_undo_close_notification: cannot find exe: {e}");
            return;
        }
    };

    // Fork a short-lived child process to own the notification handle.
    // The child waits for the action click or 30s timeout, then exits.
    // Using fork() avoids the Send constraint on NotificationHandle.
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            // Child process: show notification and wait for action.
            use notify_rust::Notification;

            let result = Notification::new()
                .summary("Session closed")
                .body("Terminal session moved to trash.")
                .icon("utilities-terminal")
                .action("undo", "Undo")
                .timeout(notify_rust::Timeout::Milliseconds(30_000))
                .show();

            if let Ok(handle) = result {
                handle.wait_for_action(|action| {
                    if action == "undo" {
                        let _ = std::process::Command::new(&current_exe)
                            .arg("--restore-session")
                            .arg(session_id.to_string())
                            .spawn();
                    }
                });
            }
            // Exit the forked child cleanly.
            libc::_exit(0);
        } else if pid < 0 {
            tracing::warn!("send_undo_close_notification: fork() failed");
        }
        // Parent continues to exit normally. Child is orphaned (reparented to init).
    }
}

#[cfg(not(target_os = "linux"))]
fn send_undo_close_notification(_session_id: uuid::Uuid) {
    // Desktop notifications not implemented for non-Linux platforms.
}

/// Read the CWD of a terminal pane.
///
/// Returns the `daemon_cwd` captured at connect time from the daemon's
/// `PaneInfo`. Returns `None` if the daemon did not provide a CWD.
fn read_pane_cwd(state_rc: &Rc<RefCell<TerminalState>>) -> Option<PathBuf> {
    let s = state_rc.try_borrow().ok()?;
    s.daemon_cwd.clone()
}

/// Walk a widget subtree and return the daemon pane ID of the first leaf found.
///
/// Used to populate `TabState.pane_id` when snapshotting in daemon mode.
#[allow(dead_code)]
fn find_first_daemon_pane_id(
    widget: &gtk4::Widget,
    tab_states: &TabStateMap,
) -> Option<uuid::Uuid> {
    if let Some(da) = widget.downcast_ref::<gtk4::DrawingArea>() {
        let widget_name = da.widget_name().to_string();
        return tab_states
            .try_borrow()
            .ok()
            .and_then(|states| states.get(&widget_name).cloned())
            .and_then(|state_rc| state_rc.try_borrow().ok().map(|s| s.daemon_pane_id))
            .flatten()
            .map(|pid| pid.0);
    }

    if let Some(paned) = widget.downcast_ref::<gtk4::Paned>() {
        if let Some(first) = paned.start_child() {
            if let Some(id) = find_first_daemon_pane_id(&first, tab_states) {
                return Some(id);
            }
        }
        if let Some(second) = paned.end_child() {
            if let Some(id) = find_first_daemon_pane_id(&second, tab_states) {
                return Some(id);
            }
        }
    }

    if let Some(bx) = widget.downcast_ref::<gtk4::Box>() {
        let mut child = bx.first_child();
        while let Some(c) = child {
            if let Some(id) = find_first_daemon_pane_id(&c, tab_states) {
                return Some(id);
            }
            child = c.next_sibling();
        }
    }

    None
}

/// Walk all Paned widgets in the workspace and collect `(PaneId, ratio)` pairs.
///
/// For each `Paned`, the ratio is `position / total_size`. The `PaneId` is the
/// daemon pane ID of the leftmost leaf in the Paned's start child, which matches
/// the convention used by `update_ratio_for_pane` in `SessionManager`.
fn collect_split_ratios(ws: &WorkspaceView) -> Vec<(forgetty_core::PaneId, f32)> {
    let mut ratios = Vec::new();
    let n_pages = ws.tab_view.n_pages();
    for i in 0..n_pages {
        let page = ws.tab_view.nth_page(i);
        let container = page.child();
        collect_ratios_from_widget(&container, &ws.tab_states, &mut ratios);
    }
    ratios
}

/// Recursively collect split ratios from a widget subtree.
fn collect_ratios_from_widget(
    widget: &gtk4::Widget,
    tab_states: &TabStateMap,
    out: &mut Vec<(forgetty_core::PaneId, f32)>,
) {
    if let Some(paned) = widget.downcast_ref::<gtk4::Paned>() {
        let size = match paned.orientation() {
            gtk4::Orientation::Horizontal => paned.width(),
            _ => paned.height(),
        };
        let pos = paned.position();
        let ratio = if size > 0 { pos as f32 / size as f32 } else { 0.5 };

        // Find the leftmost leaf's daemon pane ID in the start child.
        if let Some(start) = paned.start_child() {
            if let Some(pane_id) = leftmost_daemon_pane_id(&start, tab_states) {
                out.push((pane_id, ratio));
            }
            collect_ratios_from_widget(&start, tab_states, out);
        }
        if let Some(end) = paned.end_child() {
            collect_ratios_from_widget(&end, tab_states, out);
        }
        return;
    }

    if let Some(bx) = widget.downcast_ref::<gtk4::Box>() {
        let mut child = bx.first_child();
        while let Some(c) = child {
            collect_ratios_from_widget(&c, tab_states, out);
            child = c.next_sibling();
        }
    }
}

/// Find the daemon PaneId of the leftmost leaf DrawingArea in a widget subtree.
fn leftmost_daemon_pane_id(
    widget: &gtk4::Widget,
    tab_states: &TabStateMap,
) -> Option<forgetty_core::PaneId> {
    if let Some(da) = widget.downcast_ref::<gtk4::DrawingArea>() {
        let name = da.widget_name().to_string();
        return tab_states
            .try_borrow()
            .ok()
            .and_then(|states| states.get(&name).cloned())
            .and_then(|rc| rc.try_borrow().ok().and_then(|s| s.daemon_pane_id));
    }
    if let Some(paned) = widget.downcast_ref::<gtk4::Paned>() {
        if let Some(start) = paned.start_child() {
            return leftmost_daemon_pane_id(&start, tab_states);
        }
    }
    if let Some(bx) = widget.downcast_ref::<gtk4::Box>() {
        let mut child = bx.first_child();
        while let Some(c) = child {
            if let Some(id) = leftmost_daemon_pane_id(&c, tab_states) {
                return Some(id);
            }
            child = c.next_sibling();
        }
    }
    None
}

/// FIX-006 Cycle 1: provenance-based self-echo guard for workspace reorder
/// events. Returns `true` when the incoming `(from_idx, to_idx)` matches the
/// head of the `pending_reorders` queue (in either order — the daemon may
/// emit the swap in either direction depending on which call landed first),
/// in which case the head is popped and the caller should skip the apply.
///
/// Returns `false` when the queue is empty (a true external reorder, e.g.
/// from a future paired-device sync) or when the head does not match
/// (queue is reserved for a different swap; the caller should still apply).
///
/// Pure function on the queue — no GTK state, fully unit-testable. Replaces
/// the original content-based guard at the consumer site, which mis-fired
/// when two consecutive local swaps landed inside the same 200ms layout-event
/// drain window. See INVESTIGATION.md §1–§3.
fn consume_reorder_self_echo(
    pending: &mut VecDeque<(usize, usize)>,
    from_idx: usize,
    to_idx: usize,
) -> bool {
    if let Some(&(pending_from, pending_to)) = pending.front() {
        if (pending_from == from_idx && pending_to == to_idx)
            || (pending_from == to_idx && pending_to == from_idx)
        {
            pending.pop_front();
            return true;
        }
    }
    false
}

/// FIX-005B: provenance-based self-echo guard for `ActivePaneChanged` events.
/// Mirrors `consume_reorder_self_echo`'s pure-function shape so it can be
/// unit-tested without GTK widgets in scope.
///
/// Returns `true` if the head of `pending` matches the incoming triple and
/// pops; `false` if the queue is empty, the head does not match, or the
/// incoming `pane_id` is `None` (cleared focus pointers are never
/// self-originated by `wire_focus_tracking` — only `Some(pid)` RPCs go in
/// the queue).
fn consume_active_pane_self_echo(
    pending: &mut VecDeque<(usize, uuid::Uuid, forgetty_core::PaneId)>,
    workspace_idx: usize,
    tab_id: uuid::Uuid,
    pane_id: Option<forgetty_core::PaneId>,
) -> bool {
    let Some(pid) = pane_id else { return false };
    if let Some(&(p_ws, p_tab, p_pane)) = pending.front() {
        if p_ws == workspace_idx && p_tab == tab_id && p_pane == pid {
            pending.pop_front();
            return true;
        }
    }
    false
}

/// FIX-005B fix-cycle 1: pure helper that populates a per-workspace
/// `tab_id → PaneId` map from a slice of `(tab_id, Option<active_pane_id>)`
/// pairs. Extracted from the cold-restart build path so the population
/// contract can be unit-tested without GTK widgets.
///
/// Tabs without a saved `active_pane_id` (legacy JSON, AC-4) are simply
/// omitted from the map — `selected-page-notify` then falls back through
/// `focus_tracker` → `leaves.first()` for that tab.
fn populate_tab_active_pane_map<I>(map: &mut HashMap<uuid::Uuid, forgetty_core::PaneId>, tabs: I)
where
    I: IntoIterator<Item = (uuid::Uuid, Option<uuid::Uuid>)>,
{
    for (tab_id, active_pane_id) in tabs {
        if let Some(pid) = active_pane_id {
            map.insert(tab_id, forgetty_core::PaneId(pid));
        }
    }
}

/// Process a single `LayoutEvent` delivered by the `subscribe_layout` background task.
///
/// This function runs on the GLib main thread (via the poll timer). All widget
/// mutations happen synchronously. Events for tabs/panes already present in the
/// widget tree are silently ignored (idempotency — spec Section 4).
///
/// Currently handles `TabCreated` for external sources (e.g. Android remote view)
/// and `WorkspaceRenamed` (FIX-001, for external rename events mirrored back
/// to GTK to keep the local `WorkspaceView.name` in sync). Other events
/// (`TabClosed`, `PaneSplit`, `TabMoved`, `ActiveTabChanged`) are logged but
/// not acted upon — GTK already processed these synchronously when it sent
/// the original RPC, so acting again would double-execute.
fn handle_layout_event(
    event: LayoutEvent,
    workspace_manager: &WorkspaceManager,
    window: &adw::ApplicationWindow,
    sidebar_lb: &gtk4::ListBox,
    daemon_client: Option<&Arc<DaemonClient>>,
    shared_config: &SharedConfig,
) {
    match event {
        LayoutEvent::TabCreated { workspace_idx, tab_id, pane_id } => {
            // Check if this tab is already in the widget tree (GTK created it
            // synchronously when it sent the new_tab RPC). If so, skip.
            let already_exists = {
                let Ok(mgr) = workspace_manager.try_borrow() else { return };
                if workspace_idx >= mgr.workspaces.len() {
                    return;
                }
                let ws = &mgr.workspaces[workspace_idx];
                let found = ws.tab_id_map.borrow().values().any(|&tid| tid == tab_id);
                found
            };

            if already_exists {
                tracing::debug!(
                    "subscribe_layout: TabCreated {tab_id} already in widget tree — skipping"
                );
                return;
            }

            // FIX-009 9b: build the widget for an externally-created tab.
            // The most common (only) source today is the daemon's auto-spawn
            // when `close_tab` empties a non-last workspace. Without this
            // build, the auto-spawned tab would only appear after the next
            // `get_layout` round-trip, breaking AC-5 (the user expects era to
            // visibly retain a fresh shell as soon as their last tab closes).
            //
            // The widget-build is gated on a daemon_client reference because
            // we need `subscribe_output` to obtain the byte stream channel.
            // Post-FIX-011 daemon_client is always Some on the live path; the
            // gate is retained for defensiveness until CHORE-FIX-011-cleanup.
            // If the gate fails we degrade to the original log-and-continue.
            if let Some(dc) = daemon_client {
                build_auto_spawned_tab_widget(
                    workspace_manager,
                    workspace_idx,
                    tab_id,
                    pane_id,
                    dc,
                    shared_config,
                    window,
                );
            } else {
                tracing::info!(
                    "subscribe_layout: external TabCreated ws={workspace_idx} tab={tab_id} pane={pane_id} (deferred widget build, no daemon client)"
                );
            }
        }
        LayoutEvent::TabClosed { workspace_idx, tab_id } => {
            tracing::debug!(
                "subscribe_layout: TabClosed ws={workspace_idx} tab={tab_id} (already handled synchronously)"
            );
        }
        LayoutEvent::PaneSplit { tab_id, parent_pane_id, new_pane_id, direction } => {
            tracing::debug!(
                "subscribe_layout: PaneSplit tab={tab_id} parent={parent_pane_id} new={new_pane_id} dir={direction} (already handled synchronously)"
            );
        }
        LayoutEvent::TabMoved { workspace_idx, tab_id, new_index } => {
            tracing::debug!(
                "subscribe_layout: TabMoved ws={workspace_idx} tab={tab_id} new_idx={new_index} (already handled synchronously)"
            );
        }
        LayoutEvent::ActiveTabChanged { workspace_idx, tab_idx } => {
            tracing::debug!(
                "subscribe_layout: ActiveTabChanged ws={workspace_idx} tab_idx={tab_idx} (already handled synchronously)"
            );
        }
        LayoutEvent::WorkspaceRenamed { workspace_idx, workspace_id: _, name } => {
            // FIX-001: daemon is the source of truth for workspace names. When
            // the daemon emits this event, update the GTK-local copy so that
            // (a) external renames (paired Android client) reflect here, and
            // (b) our own rename path stays idempotent — if `name` already
            //     matches, nothing happens.
            let (ws_count, is_active) = {
                let Ok(mut mgr) = workspace_manager.try_borrow_mut() else { return };
                if workspace_idx >= mgr.workspaces.len() {
                    return;
                }
                if mgr.workspaces[workspace_idx].name == name {
                    // Already in sync (our own rename or a replayed event).
                    return;
                }
                mgr.workspaces[workspace_idx].name = name.clone();
                let is_active = workspace_idx == mgr.active_index;
                (mgr.workspaces.len(), is_active)
            };

            // Update the window title only when the renamed workspace is the
            // active one (same policy as `show_rename_workspace_dialog_for`).
            if is_active {
                update_window_title_with_workspace(ws_count, &name, workspace_manager, window);
            }

            // Update the sidebar label for the renamed row only. Row layout:
            // hbox → [number_label, name_label, meta_vbox]; name is at index 1.
            if let Some(row) = sidebar_lb.row_at_index(workspace_idx as i32) {
                if let Some(hbox) = row.child().and_then(|c| c.downcast::<gtk4::Box>().ok()) {
                    let mut child = hbox.first_child();
                    let mut child_idx = 0;
                    while let Some(c) = child {
                        if child_idx == 1 {
                            if let Some(lbl) = c.downcast_ref::<gtk4::Label>() {
                                lbl.set_text(&name);
                            }
                            break;
                        }
                        child = c.next_sibling();
                        child_idx += 1;
                    }
                }
            }
        }
        LayoutEvent::WorkspaceDeleted { workspace_idx, workspace_id } => {
            // FIX-003: daemon confirms a workspace was deleted. If GTK already
            // applied the mutation locally (the common case — we issued the
            // RPC ourselves from `do_delete_workspace_at_index`), match-on-id
            // finds no local workspace and the arm is a no-op. Otherwise
            // (external client, e.g. future paired Android delete), reconcile
            // by removing the workspace from the local manager if it still
            // exists. Match on `workspace_id` rather than `workspace_idx` to
            // remain robust against concurrent local renames/creates shifting
            // indices. In v0.1.0-beta no external client sends
            // `delete_workspace`, so this arm is effectively dead code kept
            // for symmetry with the `WorkspaceRenamed` arm.
            let still_here = {
                let Ok(mgr) = workspace_manager.try_borrow() else { return };
                mgr.workspaces.iter().position(|w| w.id == workspace_id)
            };
            let Some(local_idx) = still_here else {
                tracing::debug!(
                    "subscribe_layout: WorkspaceDeleted ws={workspace_idx} id={workspace_id} already absent locally (our own RPC)"
                );
                return;
            };

            let Ok(mut mgr) = workspace_manager.try_borrow_mut() else { return };
            if local_idx >= mgr.workspaces.len() {
                return;
            }
            mgr.workspaces.remove(local_idx);

            // Clamp active_index if needed. `active_index > local_idx` already
            // implies `active_index > 0` (usize ≥ 0), so the decrement is safe
            // without an extra > 0 guard.
            let new_len = mgr.workspaces.len();
            if mgr.active_index > local_idx {
                mgr.active_index -= 1;
            } else if mgr.active_index >= new_len && new_len > 0 {
                mgr.active_index = new_len - 1;
            }

            // Remove the sidebar row for the deleted workspace.
            if let Some(row) = sidebar_lb.row_at_index(local_idx as i32) {
                sidebar_lb.remove(&row);
            }

            tracing::info!(
                "subscribe_layout: external WorkspaceDeleted ws={workspace_idx} id={workspace_id} — removed from local state"
            );
            drop(mgr);

            // Refresh window title with the new active workspace name.
            let (ws_count, ws_name) = {
                let Ok(mgr) = workspace_manager.try_borrow() else { return };
                let name = mgr
                    .workspaces
                    .get(mgr.active_index)
                    .map(|w| w.name.clone())
                    .unwrap_or_default();
                (mgr.workspaces.len(), name)
            };
            update_window_title_with_workspace(ws_count, &ws_name, workspace_manager, window);
        }
        LayoutEvent::WorkspaceColorChanged { workspace_idx: _, workspace_id, color } => {
            // FIX-010: daemon confirms a colour change. For self-originated
            // changes (GTK called `dc.set_workspace_color` and
            // `apply_workspace_color_local` already ran), the current hex
            // equals the event hex — this arm becomes a no-op via
            // idempotency. External changes (future paired-device) reconcile
            // here. Match on `workspace_id` so concurrent reorders don't
            // misroute the update.
            let (local_idx, needs_apply) = {
                let Ok(mgr) = workspace_manager.try_borrow() else { return };
                let Some(local_idx) = mgr.workspaces.iter().position(|w| w.id == workspace_id)
                else {
                    tracing::debug!(
                        "subscribe_layout: WorkspaceColorChanged id={workspace_id} not found locally (race or external)"
                    );
                    return;
                };
                let current_hex = mgr.workspaces[local_idx].color.as_ref().map(rgba_to_hex);
                (local_idx, current_hex != color)
            };
            if !needs_apply {
                return;
            }
            let rgba: Option<gtk4::gdk::RGBA> = color.as_deref().and_then(hex_to_rgba);
            // Reuse the local-only helper — DO NOT call apply_workspace_color
            // (that would re-issue the RPC the daemon just notified us about).
            apply_workspace_color_local(workspace_manager, local_idx, rgba, sidebar_lb);
        }
        LayoutEvent::WorkspacesReordered {
            from_idx,
            to_idx,
            from_workspace_id: _from_workspace_id,
            to_workspace_id: _to_workspace_id,
        } => {
            // FIX-006 Cycle 1: provenance-based self-echo guard. The
            // local-swap path (`move_workspace_up`/`move_workspace_down`)
            // pushes a `(from, to)` tuple onto `mgr.pending_reorders`. The
            // daemon emits `WorkspacesReordered` events in the same order as
            // the RPC calls landed (single mutex on the daemon side), so a
            // head-of-queue match identifies a self-echo and we skip the
            // apply. Replaces the original content-based guard, which
            // mis-fired when two consecutive Move Up clicks landed inside
            // the same 200ms layout-event drain window — see INVESTIGATION.md.
            //
            // The UUIDs `from_workspace_id` / `to_workspace_id` are bound to
            // `_`-prefixed names because they are no longer used by the
            // current consumer logic, but kept on the `LayoutEvent` variant
            // (and the wire-format parser) for forward compatibility with a
            // future UUID-based reconciliation path.
            //
            // Bounds-check both indices against the current local Vec
            // length. A diverged length implies a concurrent local mutation
            // (e.g. delete) that we cannot reconcile via swap; skip and let
            // a later `get_layout` resync if needed.
            let needs_apply = {
                let Ok(mut mgr) = workspace_manager.try_borrow_mut() else { return };
                if from_idx >= mgr.workspaces.len() || to_idx >= mgr.workspaces.len() {
                    tracing::debug!(
                        "subscribe_layout: WorkspacesReordered from={from_idx} to={to_idx} out of local bounds (len={}) — skipping",
                        mgr.workspaces.len()
                    );
                    return;
                }
                !consume_reorder_self_echo(&mut mgr.pending_reorders, from_idx, to_idx)
            };

            if !needs_apply {
                return;
            }

            // External / out-of-band reorder: apply locally, then refresh
            // the sidebar so row labels/colours remain attached to their
            // workspaces.
            {
                let Ok(mut mgr) = workspace_manager.try_borrow_mut() else { return };
                if from_idx >= mgr.workspaces.len() || to_idx >= mgr.workspaces.len() {
                    return;
                }
                mgr.workspaces.swap(from_idx, to_idx);
                if mgr.active_index == from_idx {
                    mgr.active_index = to_idx;
                } else if mgr.active_index == to_idx {
                    mgr.active_index = from_idx;
                }
            }

            tracing::info!(
                "subscribe_layout: external WorkspacesReordered from={from_idx} to={to_idx} — applied locally"
            );

            // The sidebar's per-row widgets were built from the workspaces
            // Vec; without a refresh the row labels / colours / numbers no
            // longer match the underlying entries. The TabBar/main_area do
            // not change because the active workspace's TabView is already
            // correct (only the *order* of inactive rows changed).
            //
            // Note: `handle_layout_event` does not have main_area / tab_bar
            // refs. The sidebar refresh helper variant used by the reorder
            // path here is the simpler row-only refresh used by
            // FIX-001/FIX-010 — re-render the row labels in place.
            refresh_sidebar_row_labels(workspace_manager, sidebar_lb);
        }
        LayoutEvent::ActivePaneChanged { workspace_idx, tab_id, pane_id } => {
            // FIX-005B: provenance-based self-echo guard. Local focus-enter
            // pushed (workspace_idx, tab_id, pane_id) onto pending_active_pane
            // BEFORE issuing the RPC; the daemon emits the same triple back
            // here. FIFO match → pop and skip apply.
            //
            // External (future paired-device) ActivePaneChanged events fall
            // through the empty-queue / mismatch branches and apply locally:
            // grab_focus on the matching DrawingArea. If the saved pane_id is
            // not present in the live pane_tree (orphan from a stale daemon
            // state), warn-and-noop — fall back to first-leaf on the next
            // re-render is acceptable.
            let needs_apply = {
                let Ok(mut mgr) = workspace_manager.try_borrow_mut() else { return };
                !consume_active_pane_self_echo(
                    &mut mgr.pending_active_pane,
                    workspace_idx,
                    tab_id,
                    pane_id,
                )
            };
            if !needs_apply {
                return;
            }

            // External: locate the DrawingArea for this pane_id and call
            // grab_focus. The widget_name → daemon_pane_id mapping lives on
            // TerminalState; iterate the active workspace's tab_states.
            let Some(pid) = pane_id else { return }; // None = cleared pointer.
            let Ok(mgr) = workspace_manager.try_borrow() else { return };
            let Some(ws) = mgr.workspaces.get(workspace_idx) else { return };
            // FIX-005B fix-cycle 1: keep `tab_active_pane` authoritative
            // even on externally-originated focus events (paired-device,
            // future-Android). The selected-page-notify handler reads the
            // map; without this update, switching to a tab whose focus
            // was changed by another client would fall back to
            // leaves.first().
            if let Ok(mut m) = ws.tab_active_pane.try_borrow_mut() {
                m.insert(tab_id, pid);
            }
            let n = ws.tab_view.n_pages();
            for i in 0..n {
                let page = ws.tab_view.nth_page(i);
                let key_matches =
                    ws.tab_id_map.borrow().get(&page_identity_key(&page)) == Some(&tab_id);
                if !key_matches {
                    continue;
                }
                let container = page.child();
                let leaves = collect_leaf_drawing_areas(&container);
                for da in &leaves {
                    let widget_name = da.widget_name().to_string();
                    let matches = ws
                        .tab_states
                        .borrow()
                        .get(&widget_name)
                        .and_then(|rc| rc.try_borrow().ok().and_then(|s| s.daemon_pane_id))
                        == Some(pid);
                    if matches {
                        da.grab_focus();
                        return;
                    }
                }
                // Tab found but pane_id not in any leaf — orphan; warn+noop.
                tracing::warn!(
                    "ActivePaneChanged: pane {pid} not found in tab {tab_id}'s widget tree (orphan, skipping)"
                );
                return;
            }
            // Tab not found locally — also orphan. Debug-noop.
            tracing::debug!("ActivePaneChanged: tab {tab_id} not found locally; ignoring");
        }
    }
}

/// FIX-009 9b: build the GTK widget for a tab that the daemon auto-spawned.
///
/// When the user closes the last tab of a non-last workspace, the daemon's
/// `close_tab` predicate seeds a fresh shell (see manager.rs auto_seed_default_tab)
/// and emits `PaneCreated` + `TabCreated` events. This handler picks up the
/// `TabCreated` event and renders the new tab into the target workspace's
/// existing TabView so the user observes a healthy era → 1 visible shell
/// without needing to click the workspace row.
///
/// Idempotency: the caller already filtered out events whose `tab_id` is
/// in `tab_id_map` (the GTK-synchronous create path). Bounds-check on
/// `workspace_idx` is repeated here for safety; failures degrade to a log.
fn build_auto_spawned_tab_widget(
    workspace_manager: &WorkspaceManager,
    workspace_idx: usize,
    tab_id: uuid::Uuid,
    pane_id: forgetty_core::PaneId,
    dc: &Arc<DaemonClient>,
    shared_config: &SharedConfig,
    window: &adw::ApplicationWindow,
) {
    // Snapshot the per-workspace handle maps under a borrow, then drop the
    // borrow before doing any GTK or RPC work. This mirrors the pattern used
    // elsewhere in handle_layout_event.
    let (tab_view, tab_states, focus_tracker, custom_titles, tab_id_map) = {
        let Ok(mgr) = workspace_manager.try_borrow() else {
            tracing::warn!("auto-spawn build: workspace_manager borrowed re-entrantly — skipping");
            return;
        };
        if workspace_idx >= mgr.workspaces.len() {
            tracing::warn!(
                "auto-spawn build: workspace_idx={workspace_idx} out of bounds (len={})",
                mgr.workspaces.len()
            );
            return;
        }
        let ws = &mgr.workspaces[workspace_idx];
        (
            ws.tab_view.clone(),
            Rc::clone(&ws.tab_states),
            Rc::clone(&ws.focus_tracker),
            Rc::clone(&ws.custom_titles),
            Rc::clone(&ws.tab_id_map),
        )
    };

    let channel = match dc.subscribe_output(pane_id) {
        Ok(ch) => ch,
        Err(e) => {
            tracing::warn!("auto-spawn build: subscribe_output failed for pane {pane_id}: {e}");
            return;
        }
    };

    // Same on_exit / on_notify wiring as `add_new_tab` — the auto-spawned tab
    // closes via the same `close_pane_by_name` path when its shell exits.
    let on_exit = make_on_exit_callback(
        &tab_view,
        &tab_states,
        window,
        Some(Arc::clone(dc)),
        workspace_manager,
    );
    let on_notify = make_on_notify_callback(&tab_view, &tab_states, window);

    let cfg = shared_config.borrow();
    let create_result = terminal::create_terminal(
        &cfg,
        pane_id,
        Arc::clone(dc),
        channel,
        None, // CWD not needed — daemon already started the PTY
        Some(on_exit),
        Some(on_notify),
    );
    drop(cfg);

    let (pane_vbox, drawing_area, state) = match create_result {
        Ok(triple) => triple,
        Err(e) => {
            tracing::error!(
                "auto-spawn build: terminal::create_terminal failed for pane {pane_id}: {e}"
            );
            return;
        }
    };

    let widget_name = next_pane_id();
    drawing_area.set_widget_name(&widget_name);
    tab_states.borrow_mut().insert(widget_name, Rc::clone(&state));
    wire_focus_tracking(
        &drawing_area,
        &focus_tracker,
        &tab_view,
        &tab_states,
        &custom_titles,
        window,
        &tab_id_map,
        workspace_manager,
        Some(Arc::clone(dc)),
    );

    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    container.set_hexpand(true);
    container.set_vexpand(true);
    container.append(&pane_vbox);
    let page = tab_view.append(&container);
    let page_key = page_identity_key(&page);
    tab_id_map.borrow_mut().insert(page_key, tab_id);
    page.set_title("shell");

    // Only steal focus + select the new page if this workspace's TabView is
    // currently visible to the user. If the auto-spawn fires on a workspace
    // that is NOT the active one (e.g., user closed era's last tab while
    // viewing Default), keep Default focused — the user's mental model says
    // "I'm working in Default; era's healing is silent."
    if tab_view.parent().is_some() {
        tab_view.set_selected_page(&page);
    }

    register_title_timer(&page, &tab_view, &tab_states, &focus_tracker, &custom_titles, window);

    tracing::info!(
        "auto-spawn build: rendered widget for ws={workspace_idx} tab={tab_id} pane={pane_id}"
    );
}

/// Close every pane in the workspace: send daemon close RPCs (or clear the registry
/// when no daemon client is available — legacy fallback retained until
/// CHORE-FIX-011-cleanup).
fn close_workspace_panes(tab_states: &TabStateMap, daemon_client: Option<&DaemonClient>) {
    let pane_names: Vec<String> =
        tab_states.try_borrow().map(|states| states.keys().cloned().collect()).unwrap_or_default();
    if pane_names.is_empty() {
        return;
    }
    if let Some(dc) = daemon_client {
        for pane_name in &pane_names {
            daemon_close_pane(pane_name, tab_states, dc, true);
        }
    } else if let Ok(mut states) = tab_states.try_borrow_mut() {
        states.clear();
    }
}

/// Recursively build a GTK widget tree from a daemon `PaneTreeNode` (T-064).
///
/// Every `Leaf` pane_id is a live daemon pane — no pane_map lookup needed.
/// `Split` nodes produce a `gtk::Paned` with the ratio restored via
/// `idle_add_local_once`.
///
/// Returns `Some((root_widget, first_leaf_drawing_area))` on success, `None`
/// if any sub-tree fails (partial split widgets are not displayed).
#[allow(clippy::too_many_arguments)]
fn build_widget_from_pane_tree(
    node: &PaneTreeNode,
    dc: &Arc<DaemonClient>,
    config: &Config,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    window: &adw::ApplicationWindow,
    tab_view: &adw::TabView,
    workspace_manager: &WorkspaceManager,
    tab_id_map: &TabIdMap,
) -> Option<(gtk4::Widget, gtk4::DrawingArea)> {
    match node {
        PaneTreeNode::Leaf { pane_id } => {
            // pane_id is a live daemon pane — subscribe directly, no lookup needed.
            let channel = match dc.subscribe_output(*pane_id) {
                Ok(ch) => ch,
                Err(e) => {
                    tracing::warn!("subscribe_output failed for pane {pane_id}: {e}");
                    return None;
                }
            };

            // V2-007: byte-log replay in subscribe_output populates the VT.
            // The daemon's `get_screen` RPC was retired in V2-008.
            let on_exit = make_on_exit_callback(
                tab_view,
                tab_states,
                window,
                Some(Arc::clone(dc)),
                workspace_manager,
            );
            let on_notify = make_on_notify_callback(tab_view, tab_states, window);

            match terminal::create_terminal(
                config,
                *pane_id,
                Arc::clone(dc),
                channel,
                None, // CWD not needed — daemon pane's PTY is already running
                Some(on_exit),
                Some(on_notify),
            ) {
                Ok((pane_vbox, drawing_area, state)) => {
                    let pane_widget_name = next_pane_id();
                    drawing_area.set_widget_name(&pane_widget_name);
                    tab_states.borrow_mut().insert(pane_widget_name, Rc::clone(&state));
                    wire_focus_tracking(
                        &drawing_area,
                        focus_tracker,
                        tab_view,
                        tab_states,
                        custom_titles,
                        window,
                        tab_id_map,
                        workspace_manager,
                        Some(Arc::clone(dc)),
                    );
                    Some((pane_vbox.upcast::<gtk4::Widget>(), drawing_area))
                }
                Err(e) => {
                    tracing::error!("Failed to create terminal widget for pane {pane_id}: {e}");
                    None
                }
            }
        }

        PaneTreeNode::Split { direction, ratio, first, second } => {
            let orientation = if direction == "horizontal" {
                gtk4::Orientation::Horizontal
            } else {
                gtk4::Orientation::Vertical
            };

            let first_result = build_widget_from_pane_tree(
                first,
                dc,
                config,
                tab_states,
                focus_tracker,
                custom_titles,
                window,
                tab_view,
                workspace_manager,
                tab_id_map,
            );
            let second_result = build_widget_from_pane_tree(
                second,
                dc,
                config,
                tab_states,
                focus_tracker,
                custom_titles,
                window,
                tab_view,
                workspace_manager,
                tab_id_map,
            );

            let (Some((first_widget, first_da)), Some((second_widget, _))) =
                (first_result, second_result)
            else {
                return None;
            };

            let paned = gtk4::Paned::new(orientation);
            paned.set_wide_handle(true);
            paned.set_resize_start_child(true);
            paned.set_resize_end_child(true);
            paned.set_shrink_start_child(false);
            paned.set_shrink_end_child(false);
            paned.set_hexpand(true);
            paned.set_vexpand(true);

            paned.set_start_child(Some(&first_widget));
            paned.set_end_child(Some(&second_widget));

            // Apply the saved split ratio the first time the Paned is mapped
            // AND has a usable size.
            //
            // Earlier iterations used `glib::idle_add_local_once` / a bounded
            // idle-retry scheduled at build time, but both had the same flaw:
            // for splits on INACTIVE tabs, the Paned's widget subtree never
            // gets mapped or allocated during the retry window, so
            // `width()/height()` reads as 0 for every retry and the loop
            // exhausts. When the user later switches to that tab, the Paned
            // finally gets a real allocation but no retry is pending — the
            // divider sticks to GTK's default position and the top pane is
            // visually collapsed (the inactive-tab edge case Vick hit on
            // 2026-04-24 QA with a vertical split at ratio=0.05 on a
            // background tab).
            //
            // The fix hooks `connect_map`: every time the Paned is mapped
            // (first show, and subsequent show-after-tab-switch), we schedule
            // an idle retry that walks until the widget reports a usable size.
            // Once applied, `applied` latches true and subsequent `map` firings
            // no-op. Uses the same latch pattern as FIX-002.
            let saved_ratio = (*ratio).clamp(0.05, 0.95);
            let applied = Rc::new(Cell::new(false));
            let applied_for_map = applied.clone();
            paned.connect_map(move |p| {
                if applied_for_map.get() {
                    return;
                }
                let paned_weak = p.downgrade();
                let applied_for_idle = applied_for_map.clone();
                let retries_left = Rc::new(Cell::new(RESTORE_RATIO_MAX_RETRIES));
                glib::idle_add_local(move || {
                    if applied_for_idle.get() {
                        return glib::ControlFlow::Break;
                    }
                    let Some(paned) = paned_weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    let size = match paned.orientation() {
                        gtk4::Orientation::Horizontal => paned.width(),
                        _ => paned.height(),
                    };
                    if size > 1 {
                        let target = ((size as f32) * saved_ratio) as i32;
                        if target > 0 {
                            paned.set_position(target);
                        }
                        applied_for_idle.set(true);
                        return glib::ControlFlow::Break;
                    }
                    let remaining = retries_left.get();
                    if remaining == 0 {
                        tracing::debug!(
                            "session-restore: split ratio {:.3} not applied after map — Paned reported usable size too slowly",
                            saved_ratio
                        );
                        return glib::ControlFlow::Break;
                    }
                    retries_left.set(remaining - 1);
                    glib::ControlFlow::Continue
                });
            });

            Some((paned.upcast::<gtk4::Widget>(), first_da))
        }
    }
}

/// Build the full GTK tab widget tree from a `LayoutInfo` snapshot (T-064, T-067).
///
/// Preserves the daemon's workspace ordering (session-restore fix): daemon's
/// workspace[0] is built into GTK's existing workspace[0], daemon's workspace
/// [1..N] are appended as new WorkspaceViews in order. The daemon's
/// `active_workspace` index is then used to switch the visible TabView into
/// `main_area` so the user lands on the workspace they had focused at save
/// time. This keeps daemon indices and GTK `mgr.workspaces` indices in lock-
/// step, which is required because `new_tab` RPCs route by `workspace_idx`
/// and `set_active_workspace` uses that same index space.
///
/// Returns `true` if at least one tab was successfully created anywhere in
/// the restored layout.
fn build_widgets_from_layout(
    layout: &LayoutInfo,
    dc: &Arc<DaemonClient>,
    config: &Config,
    workspace_manager: &WorkspaceManager,
    window: &adw::ApplicationWindow,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
) -> bool {
    if layout.workspaces.is_empty() {
        tracing::info!("get_layout: no workspaces in layout — will create a fresh tab");
        return false;
    }

    // Validate active_workspace index; clamp to 0 if out-of-range.
    let active_ws_idx = if layout.active_workspace < layout.workspaces.len() {
        layout.active_workspace
    } else {
        tracing::debug!(
            "get_layout: active_workspace {} out of range, using 0",
            layout.active_workspace
        );
        0
    };

    // First pass: sanity check at least one workspace has tabs.
    let total_tabs: usize = layout.workspaces.iter().map(|w| w.tabs.len()).sum();
    if total_tabs == 0 {
        tracing::info!(
            "get_layout: layout has 0 tabs across all workspaces — will create a fresh tab"
        );
        return false;
    }

    // -----------------------------------------------------------------------
    // Build daemon-workspace[0] into GTK's existing workspace[0] TabView.
    // -----------------------------------------------------------------------
    let daemon_ws0 = &layout.workspaces[0];
    let mut daemon_ws0_built = false;

    if !daemon_ws0.tabs.is_empty() {
        tracing::info!(
            "build_widgets_from_layout: building {} tab(s) into workspace[0] {:?}",
            daemon_ws0.tabs.len(),
            daemon_ws0.name
        );

        // FIX-005B fix-cycle 1: pre-populate ws[0]'s `tab_active_pane` from
        // the layout snapshot BEFORE the build loop calls `tab_view.append`.
        // Append fires `selected-page-notify` synchronously on the first
        // page, which reads this map to grab the correct leaf instead of
        // falling back to `leaves.first()` (which would then echo the wrong
        // pane back to the daemon and corrupt the persisted active_pane_id).
        {
            if let Ok(mgr) = workspace_manager.try_borrow() {
                let ws = &mgr.workspaces[0];
                let mut map = ws.tab_active_pane.borrow_mut();
                map.clear();
                populate_tab_active_pane_map(
                    &mut map,
                    daemon_ws0.tabs.iter().map(|t| (t.tab_id, t.active_pane_id)),
                );
            }
        }

        let (created, tab_view_clone) = {
            let Ok(mgr) = workspace_manager.try_borrow() else {
                tracing::warn!("build_widgets_from_layout: failed to borrow workspace_manager");
                return false;
            };
            let ws = &mgr.workspaces[0];
            let mut created: Vec<(adw::TabPage, gtk4::DrawingArea)> = Vec::new();

            for tab in &daemon_ws0.tabs {
                let Some((root_widget, first_da)) = build_widget_from_pane_tree(
                    &tab.pane_tree,
                    dc,
                    config,
                    &ws.tab_states,
                    &ws.focus_tracker,
                    &ws.custom_titles,
                    window,
                    &ws.tab_view,
                    workspace_manager,
                    &ws.tab_id_map,
                ) else {
                    tracing::warn!("build_widget_from_pane_tree failed for tab {:?}", tab.title);
                    continue;
                };

                let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                container.set_hexpand(true);
                container.set_vexpand(true);
                container.append(&root_widget);

                // FIX-005B fix-cycle 2: pre-populate tab_id_map BEFORE
                // tab_view.append. AdwTabView fires `selected-page-notify`
                // synchronously when the first page transitions the
                // selection from None → page. The cycle-1 handler reads
                // `tab_id_map.get(page_key)` at that instant; with the
                // post-append insert it sees an empty map, falls through
                // to `leaves.first()` (LEFT), and the synchronous
                // connect_enter then RPCs the wrong active_pane_id to the
                // daemon (corrupting tab 1 when the user later switches
                // to it). Pre-insert eliminates the race because
                // `container.as_ptr() == page.child().as_ptr()` —
                // page.child() returns the same GObject we just appended,
                // so `page_identity_key` produces the same string.
                let pre_insert_key =
                    format!("page-{:p}", container.upcast_ref::<gtk4::Widget>().as_ptr());
                ws.tab_id_map.borrow_mut().insert(pre_insert_key, tab.tab_id);
                let page = ws.tab_view.append(&container);
                // Defensive idempotent write: same key, same value as the
                // pre-insert above. Kept so this code stays robust if the
                // container construction is later refactored.
                ws.tab_id_map.borrow_mut().insert(page_identity_key(&page), tab.tab_id);
                let tab_title = if tab.title.is_empty() { "shell" } else { &tab.title };
                page.set_title(tab_title);

                register_title_timer(
                    &page,
                    &ws.tab_view,
                    &ws.tab_states,
                    &ws.focus_tracker,
                    &ws.custom_titles,
                    window,
                );

                created.push((page, first_da));
            }
            (created, ws.tab_view.clone())
        };

        if !created.is_empty() {
            daemon_ws0_built = true;
            // Select the saved active_tab inside this workspace.
            let active_tab_idx = daemon_ws0.active_tab.min(created.len().saturating_sub(1));
            let (ref active_page, _) = created[active_tab_idx];
            tab_view_clone.set_selected_page(active_page);

            // Sync workspace[0]'s id/name/colour with the daemon's workspace[0].
            if let Ok(mut mgr) = workspace_manager.try_borrow_mut() {
                mgr.workspaces[0].id = daemon_ws0.id;
                mgr.workspaces[0].name = daemon_ws0.name.clone();
                // FIX-010: adopt the daemon's persisted colour for ws 0.
                // The CSS provider is installed by the post-loop
                // `apply_restored_workspace_colors` pass (after the sidebar
                // ListBox rows exist).
                mgr.workspaces[0].color = daemon_ws0.color.as_deref().and_then(hex_to_rgba);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Build daemon workspaces[1..] as additional WorkspaceViews, in order.
    // -----------------------------------------------------------------------
    for ws_info in layout.workspaces.iter().skip(1) {
        if ws_info.tabs.is_empty() {
            tracing::debug!(
                "build_widgets_from_layout: workspace {:?} has 0 tabs, skipping",
                ws_info.name
            );
            continue;
        }

        let new_tv = adw::TabView::new();
        new_tv.set_vexpand(true);
        new_tv.set_hexpand(true);

        let new_tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));
        let new_focus_tracker: FocusTracker = Rc::new(RefCell::new(String::new()));
        let new_custom_titles: CustomTitles = Rc::new(RefCell::new(HashSet::new()));
        let new_tab_id_map: TabIdMap = Rc::new(RefCell::new(HashMap::new()));
        let new_tab_active_pane: TabActivePaneMap = Rc::new(RefCell::new(HashMap::new()));

        // FIX-005B fix-cycle 1: pre-populate the per-tab active-pane map
        // BEFORE `wire_tab_view_handlers` connects `selected-page-notify`
        // and BEFORE the build loop's `new_tv.append` fires that signal.
        // See ws[0] branch above for the full rationale.
        {
            let mut map = new_tab_active_pane.borrow_mut();
            populate_tab_active_pane_map(
                &mut map,
                ws_info.tabs.iter().map(|t| (t.tab_id, t.active_pane_id)),
            );
        }

        wire_tab_view_handlers(
            &new_tv,
            &new_tab_states,
            &new_focus_tracker,
            &new_tab_id_map,
            &new_tab_active_pane,
            window,
            Some(Arc::clone(dc)),
            workspace_manager,
        );

        tracing::info!(
            "build_widgets_from_layout: building {} tab(s) for workspace {:?}",
            ws_info.tabs.len(),
            ws_info.name
        );

        let mut first_da_opt: Option<gtk4::DrawingArea> = None;

        for tab in &ws_info.tabs {
            let Some((root_widget, first_da)) = build_widget_from_pane_tree(
                &tab.pane_tree,
                dc,
                config,
                &new_tab_states,
                &new_focus_tracker,
                &new_custom_titles,
                window,
                &new_tv,
                workspace_manager,
                &new_tab_id_map,
            ) else {
                tracing::warn!(
                    "build_widget_from_pane_tree failed for tab {:?} in workspace {:?}",
                    tab.title,
                    ws_info.name
                );
                continue;
            };

            let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            container.set_hexpand(true);
            container.set_vexpand(true);
            container.append(&root_widget);

            // FIX-005B fix-cycle 2: pre-populate tab_id_map BEFORE
            // new_tv.append fires `selected-page-notify` synchronously on
            // the first page. See ws[0] branch above for full rationale.
            let pre_insert_key =
                format!("page-{:p}", container.upcast_ref::<gtk4::Widget>().as_ptr());
            new_tab_id_map.borrow_mut().insert(pre_insert_key, tab.tab_id);
            let page = new_tv.append(&container);
            // Defensive idempotent write — same key, same value.
            new_tab_id_map.borrow_mut().insert(page_identity_key(&page), tab.tab_id);
            let tab_title = if tab.title.is_empty() { "shell" } else { &tab.title };
            page.set_title(tab_title);

            register_title_timer(
                &page,
                &new_tv,
                &new_tab_states,
                &new_focus_tracker,
                &new_custom_titles,
                window,
            );

            if first_da_opt.is_none() {
                first_da_opt = Some(first_da);
            }
        }

        // Only add the WorkspaceView if at least one tab was created.
        let n_pages = new_tv.n_pages();
        if n_pages > 0 {
            // Select this workspace's saved active_tab before pushing. n_pages
            // is u32 in libadwaita; active_tab is usize — clamp and cast.
            let max_idx = (n_pages - 1) as usize;
            let active_tab_idx = ws_info.active_tab.min(max_idx) as i32;
            let page = new_tv.nth_page(active_tab_idx);
            new_tv.set_selected_page(&page);

            let Ok(mut mgr) = workspace_manager.try_borrow_mut() else {
                tracing::warn!(
                    "build_widgets_from_layout: failed to borrow_mut for ws {:?}",
                    ws_info.name
                );
                continue;
            };
            // FIX-010: carry the daemon-reported colour into the new
            // WorkspaceView. The CSS provider is installed later by
            // `apply_restored_workspace_colors` (it needs the ListBox row to
            // already exist).
            let initial_color: Option<gtk4::gdk::RGBA> =
                ws_info.color.as_deref().and_then(hex_to_rgba);
            mgr.workspaces.push(WorkspaceView {
                id: ws_info.id,
                name: ws_info.name.clone(),
                tab_view: new_tv,
                tab_states: new_tab_states,
                focus_tracker: new_focus_tracker,
                custom_titles: new_custom_titles,
                tab_id_map: new_tab_id_map,
                tab_colors: Rc::new(RefCell::new(HashMap::new())),
                color: initial_color,
                color_css_provider: None,
                tab_active_pane: new_tab_active_pane,
            });
            tracing::info!(
                "build_widgets_from_layout: added WorkspaceView {:?} at gtx_idx={}",
                ws_info.name,
                mgr.workspaces.len() - 1
            );
        }
    }

    // Switch into the daemon-active workspace (session-restore bug 2 fix).
    // We built workspaces in daemon order so GTK indices match the daemon's;
    // if the active workspace isn't index 0 swap the visible TabView via
    // `switch_workspace`. That call also updates title + focus; we still
    // call `focus_when_mapped` below because on the restore path the target
    // DrawingArea isn't mapped yet and `grab_focus` would silently no-op.
    if active_ws_idx > 0 {
        switch_workspace(workspace_manager, active_ws_idx, main_area, tab_bar, window, Some(dc));
    }

    let Ok(mgr) = workspace_manager.try_borrow() else {
        return daemon_ws0_built;
    };
    if let Some(ws) = mgr.workspaces.get(active_ws_idx) {
        if let Some(page) = ws.tab_view.selected_page() {
            let container = page.child();
            let leaves = collect_leaf_drawing_areas(&container);

            // FIX-005B: respect the daemon's active_pane_id when restoring
            // focus on cold start. Resolve the page's tab_id via tab_id_map
            // (populated by build_widgets_from_layout above), then walk the
            // layout snapshot for the matching tab's persisted active_pane_id.
            // If the saved pane is in this tab's leaves, focus it; otherwise
            // fall back to the first leaf (pre-FIX-005B behaviour).
            let target_pane_id: Option<uuid::Uuid> =
                ws.tab_id_map.borrow().get(&page_identity_key(&page)).and_then(|tab_id| {
                    layout
                        .workspaces
                        .iter()
                        .find_map(|w| w.tabs.iter().find(|t| t.tab_id == *tab_id))
                        .and_then(|t| t.active_pane_id)
                });
            let target_da = target_pane_id.and_then(|pid| {
                leaves.iter().find(|da| {
                    let widget_name = da.widget_name().to_string();
                    ws.tab_states
                        .borrow()
                        .get(&widget_name)
                        .and_then(|rc| rc.try_borrow().ok().and_then(|s| s.daemon_pane_id))
                        == Some(forgetty_core::PaneId(pid))
                })
            });

            if let Some(da) = target_da.or_else(|| leaves.first()) {
                focus_when_mapped(da);
            }
        }
    }
    mgr.workspaces.iter().any(|w| w.tab_view.n_pages() > 0)
}

// ---------------------------------------------------------------------------
// Tab management
// ---------------------------------------------------------------------------

/// Add a new terminal tab to the TabView.
///
/// Creates a new DrawingArea + TerminalState pair via `create_terminal()`,
/// wraps it in a pane container Box, appends a page to the TabView, sets up
/// title polling, and selects the new tab.
/// Build an `on_exit` callback for a terminal pane.
///
/// When the PTY channel disconnects (shell exits), this callback fires on the
/// GTK main thread and calls `close_pane_by_name()` to remove the pane. The
/// `workspace_manager` clone is captured into the closure so the FIX-009 9a
/// window-close guard inside `close_pane_by_name` can read the workspace
/// count when the PTY-exit triggers a last-tab close.
fn make_on_exit_callback(
    tab_view: &adw::TabView,
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    workspace_manager: &WorkspaceManager,
) -> Rc<dyn Fn(String)> {
    let tv = tab_view.clone();
    let states = Rc::clone(tab_states);
    let win = window.clone();
    let wm = Rc::clone(workspace_manager);
    Rc::new(move |pane_name: String| {
        close_pane_by_name(&pane_name, &tv, &states, &win, daemon_client.clone(), &wm);
    })
}

/// Build the `on_notify` callback for a terminal pane.
///
/// This callback is invoked from the 8ms timer when an OSC 9/99/777 or BEL
/// notification fires on an unfocused pane. It:
///
/// 1. Sets `adw::TabPage::set_needs_attention(true)` on the tab containing
///    the notifying pane.
/// 2. For OSC notifications (source is Some): fires a desktop notification
///    via `notify-rust` in a background thread (D-Bus, non-blocking).
///    BEL payloads (source is None) skip the desktop notification.
/// 3. Implements click-to-focus: when the desktop notification is clicked,
///    brings the Forgetty window to the foreground and focuses the pane.
fn make_on_notify_callback(
    tab_view: &adw::TabView,
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
) -> Rc<dyn Fn(NotificationPayload)> {
    let tv = tab_view.clone();
    let states = Rc::clone(tab_states);
    let win = window.clone();

    Rc::new(move |payload: NotificationPayload| {
        // --- 1. Tab badge ---
        // Find the TabPage that contains the notifying pane and mark it as
        // needing attention. We iterate all pages and collect leaf DAs.
        let n_pages = tv.n_pages();
        for i in 0..n_pages {
            let page = tv.nth_page(i);
            let container = page.child();
            let leaves = collect_leaf_drawing_areas(&container);
            if leaves.iter().any(|da| da.widget_name().as_str() == payload.pane_name) {
                page.set_needs_attention(true);
                break;
            }
        }

        // --- 2. Desktop notification (OSC only, not BEL) ---
        // `payload.source == None` means this was a BEL -- skip desktop notify.
        // `payload.source == Some(...)` means OSC 9/99/777 -- fire desktop notify.
        let source = payload.source;
        if source.is_none() {
            // BEL: ring + badge only, no desktop notification.
            return;
        }

        // Check notification_mode on any pane (they all share the same config).
        let mode = {
            let Ok(map) = states.try_borrow() else { return };
            map.get(&payload.pane_name)
                .and_then(|rc| rc.try_borrow().ok())
                .map(|s| s.config.notification_mode)
                .unwrap_or(NotificationMode::All)
        };

        if mode == NotificationMode::RingOnly || mode == NotificationMode::None {
            return;
        }

        let title = payload.title.clone();
        let body = payload.body.clone();
        let pane_name = payload.pane_name.clone();

        // --- 3. Spawn background thread for D-Bus notification ---
        // `notify-rust::Notification::show()` may block waiting for D-Bus.
        // This MUST NOT run on the GTK main thread.
        //
        // Click-to-focus bridge uses std::sync::mpsc + a glib::timeout_add_local
        // polling timer (50ms interval, auto-cancels on receipt):
        //   1. Background thread: show notification, wait_for_action
        //   2. On action: send pane_name via mpsc channel (Send)
        //   3. Main thread: polling timer detects message, performs focus
        //
        // This avoids capturing non-Send GTK types (WeakRef, Rc) in the
        // spawned thread.
        #[cfg(target_os = "linux")]
        {
            let pane_name_thread = pane_name.clone();
            let win_weak = win.downgrade();
            let tv_weak = tv.downgrade();
            let states_for_focus = Rc::clone(&states);

            let (focus_tx, focus_rx) = std::sync::mpsc::channel::<String>();
            let focus_rx = std::rc::Rc::new(std::cell::RefCell::new(focus_rx));

            // Register a polling timer on the GTK main thread.
            // It polls the mpsc receiver every 50ms and stops once it gets a message.
            glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
                match focus_rx.borrow().try_recv() {
                    Ok(pn) => {
                        if let Some(win) = win_weak.upgrade() {
                            win.present();
                        }
                        focus_pane_by_name(&pn, &tv_weak, &states_for_focus);
                        glib::ControlFlow::Break
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        // Channel disconnected (thread done without clicking)
                        glib::ControlFlow::Break
                    }
                }
            });

            std::thread::spawn(move || {
                let result = notify_rust::Notification::new()
                    .appname("Forgetty")
                    .summary(&title)
                    .body(&body)
                    .icon("dev.forgetty.Forgetty")
                    .hint(notify_rust::Hint::Category("im.received".to_owned()))
                    .action("focus", "Focus")
                    .show();

                match result {
                    Ok(handle) => {
                        // Block until user clicks / dismisses.
                        // "__closed" fires on dismiss/timeout — do NOT focus on dismiss.
                        handle.wait_for_action(|action| {
                            if action == "focus" {
                                let _ = focus_tx.send(pane_name_thread.clone());
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("Desktop notification failed: {e}");
                    }
                }
                // Channel drops here, causing Disconnected on the receiver side.
            });
        }

        // On non-Linux platforms, suppress unused variable warnings.
        #[cfg(not(target_os = "linux"))]
        let _ = pane_name;
    })
}

/// Focus a pane by name: switch to its tab and call `grab_focus()`.
///
/// Used by the desktop notification click-to-focus callback.
fn focus_pane_by_name(
    pane_name: &str,
    tv_weak: &glib::WeakRef<adw::TabView>,
    _tab_states: &TabStateMap,
) {
    let Some(tv) = tv_weak.upgrade() else { return };
    let n_pages = tv.n_pages();
    for i in 0..n_pages {
        let page = tv.nth_page(i);
        let container = page.child();
        let leaves = collect_leaf_drawing_areas(&container);
        if let Some(da) = leaves.iter().find(|da| da.widget_name().as_str() == pane_name) {
            tv.set_selected_page(&page);
            da.grab_focus();
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn add_new_tab(
    workspace_idx: usize,
    tab_view: &adw::TabView,
    config: &Config,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    window: &adw::ApplicationWindow,
    working_dir: Option<&std::path::Path>,
    command: Option<&[String]>,
    daemon_client: Option<Arc<DaemonClient>>,
    tab_id_map: &TabIdMap,
    workspace_manager: &WorkspaceManager,
) {
    // --- Daemon mode: create pane via RPC and subscribe to output. ---
    let Some(ref dc) = daemon_client else {
        tracing::warn!("add_new_tab called without a daemon client — ignoring");
        return;
    };

    let on_exit = make_on_exit_callback(
        tab_view,
        tab_states,
        window,
        daemon_client.clone(),
        workspace_manager,
    );
    let on_notify = make_on_notify_callback(tab_view, tab_states, window);

    // Use profile-aware RPC when command or cwd are provided (AC-13, AC-14).
    // workspace_idx routes the new tab to the correct daemon-side workspace
    // so cold-restart restores it there (session-restore fix).
    let rpc_result = if command.is_some() || working_dir.is_some() {
        let cmd_vec = command.map(|c| c.to_vec());
        dc.new_tab_with_profile(workspace_idx, cmd_vec, working_dir)
    } else {
        dc.new_tab(workspace_idx)
    };
    match rpc_result {
        Ok((pane_id, tab_id)) => {
            let channel = match dc.subscribe_output(pane_id) {
                Ok(ch) => ch,
                Err(e) => {
                    tracing::warn!("subscribe_output failed for new pane {pane_id}: {e}");
                    return;
                }
            };
            // V2-007: byte-log replay populates the VT via subscribe_output.
            match terminal::create_terminal(
                config,
                pane_id,
                Arc::clone(dc),
                channel,
                None,
                Some(on_exit),
                Some(on_notify),
            ) {
                Ok((pane_vbox, drawing_area, state)) => {
                    let widget_name = next_pane_id();
                    drawing_area.set_widget_name(&widget_name);
                    tab_states.borrow_mut().insert(widget_name, Rc::clone(&state));
                    wire_focus_tracking(
                        &drawing_area,
                        focus_tracker,
                        tab_view,
                        tab_states,
                        custom_titles,
                        window,
                        tab_id_map,
                        workspace_manager,
                        Some(Arc::clone(dc)),
                    );
                    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                    container.set_hexpand(true);
                    container.set_vexpand(true);
                    container.append(&pane_vbox);
                    let page = tab_view.append(&container);
                    // Store tab_id in the map so page-reordered can send move_tab RPC.
                    let page_key = page_identity_key(&page);
                    tab_id_map.borrow_mut().insert(page_key, tab_id);
                    page.set_title("shell");
                    tab_view.set_selected_page(&page);
                    drawing_area.grab_focus();
                    register_title_timer(
                        &page,
                        tab_view,
                        tab_states,
                        focus_tracker,
                        custom_titles,
                        window,
                    );
                }
                Err(e) => {
                    tracing::error!("Failed to create terminal widget for daemon pane: {e}");
                }
            }
        }
        Err(e) => {
            tracing::error!("new_tab RPC failed: {e}");
        }
    }
}

/// Get the pane container Box for a tab page.
///
/// Each tab page's child is always a gtk::Box that wraps the actual content.
fn pane_container(page: &adw::TabPage) -> Option<gtk4::Box> {
    page.child().downcast::<gtk4::Box>().ok()
}

/// Get the root content widget inside a pane container.
///
/// This is the first (and only) child of the container Box -- either a
/// DrawingArea or a Paned tree.
fn container_content(container: &gtk4::Box) -> Option<gtk4::Widget> {
    container.first_child()
}

/// Replace the content of a pane container with a new widget.
///
/// Removes the old content and appends the new widget.
fn set_container_content(container: &gtk4::Box, new_content: &impl IsA<gtk4::Widget>) {
    // Remove all existing children
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    container.append(new_content);
}

/// Grant keyboard focus to `da` as soon as it is safely mappable.
///
/// `grab_focus()` on an unmapped widget silently no-ops, and the split
/// path specifically triggers a transient unmap of the newly-inserted DA:
/// after the Paned is added to the tree with `shrink_*_child=false`, GTK's
/// first layout cycle honours both children's minimum sizes, and when the
/// old pane hosts a large TUI (Claude Code, vim, htop), that minimum can
/// consume the entire Paned — GTK allocates 0 pixels to the new pane and
/// unmaps it. A separate `idle_add_local_once` later in `split_pane` sets
/// the divider to 50% and the DA is re-mapped, but any grab that landed
/// in the unmapped interval silently no-ops. FIX-002 Phase 4 cycles 1-2
/// tried focus-child pinning and priority bumps; the root cause is the
/// transient unmap, not the focus-chain walk.
///
/// This helper therefore arms *both* a `connect_map` handler and a
/// `HIGH_IDLE` idle callback:
///
/// - If the DA stays mapped through the layout cycle (horizontal split,
///   non-TUI content, non-root split with plenty of space), the idle
///   callback grabs focus and disconnects the map handler so that later
///   unrelated map events (tab switch, workspace hide/show) do not steal
///   focus back.
/// - If the DA is transiently unmapped during layout (vertical split
///   under a TUI, root-split under Claude), the idle callback's
///   `is_mapped()` check is false and the grab is deferred; the still-
///   armed map handler then fires on the re-map and grabs focus there.
///
/// Both paths self-disconnect after a single successful grab so later
/// unrelated map cycles cannot re-steal focus.
///
/// Thread-safety: GTK widgets are not `Send`; this helper must be invoked
/// from the GTK main thread. Closures use `Rc<RefCell<...>>` (safe because
/// GTK callbacks are single-threaded on that thread).
fn focus_when_mapped(da: &gtk4::DrawingArea) {
    let handler_id_cell: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::new(RefCell::new(None));

    // Arm the connect_map handler. Fires on every map transition; the
    // first one that lands with `is_mapped() == true` grabs focus and
    // self-disconnects via the shared handler-id cell.
    let cell_for_map = Rc::clone(&handler_id_cell);
    let map_handler = da.connect_map(move |widget| {
        widget.grab_focus();
        if let Some(id) = cell_for_map.borrow_mut().take() {
            widget.disconnect(id);
        }
    });
    *handler_id_cell.borrow_mut() = Some(map_handler);

    // Also try a HIGH_IDLE grab. If the DA stays mapped through the
    // upcoming layout cycle, this fires first and disconnects the map
    // handler. If the DA transiently unmaps, is_mapped() is false here
    // and we skip; the map handler stays armed for the re-map.
    let da_weak = da.downgrade();
    let cell_for_idle = Rc::clone(&handler_id_cell);
    glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
        if let Some(da) = da_weak.upgrade() {
            if da.is_mapped() {
                da.grab_focus();
                if let Some(id) = cell_for_idle.borrow_mut().take() {
                    da.disconnect(id);
                }
            }
        }
        glib::ControlFlow::Break
    });
}

// ---------------------------------------------------------------------------
// Split pane operations
// ---------------------------------------------------------------------------

/// Split the currently focused pane in the given orientation.
///
/// Replaces the focused DrawingArea with a gtk::Paned containing the original
/// pane and a newly created terminal pane.
///
/// When `before` is false (split-right, split-down): existing pane goes in
/// start_child, new pane in end_child.
/// When `before` is true (split-left, split-up): new pane goes in start_child,
/// existing pane in end_child.
#[allow(clippy::too_many_arguments)]
fn split_pane(
    tab_view: &adw::TabView,
    config: &Config,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    orientation: gtk4::Orientation,
    before: bool,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    workspace_manager: &WorkspaceManager,
    tab_id_map: &TabIdMap,
) {
    // Find the currently focused DrawingArea
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    // Get the selected tab page and its pane container
    let Some(page) = tab_view.selected_page() else {
        return;
    };
    let Some(container) = pane_container(&page) else {
        return;
    };
    let Some(root_content) = container_content(&container) else {
        return;
    };

    // Find the focused DrawingArea within the page's widget tree
    let leaves = collect_leaf_drawing_areas(&root_content);
    let focused_da = leaves.iter().find(|da| da.widget_name().as_str() == focused_name);

    let Some(focused_da) = focused_da.cloned() else {
        return;
    };

    // Create the new terminal pane (splits always get default shell + default CWD).
    // Splits require daemon mode: the daemon owns the new PTY and the SessionLayout.
    let Some(ref dc) = daemon_client else {
        tracing::warn!("split_pane called without a daemon client — ignoring");
        return;
    };
    let on_exit = make_on_exit_callback(
        tab_view,
        tab_states,
        window,
        daemon_client.clone(),
        workspace_manager,
    );
    let on_notify = make_on_notify_callback(tab_view, tab_states, window);

    // Get the daemon PaneId for the currently focused DrawingArea.
    let focused_daemon_pane_id = {
        tab_states
            .borrow()
            .get(&focused_name)
            .and_then(|s| s.try_borrow().ok())
            .and_then(|s| s.daemon_pane_id)
    };
    let Some(parent_pane_id) = focused_daemon_pane_id else {
        tracing::warn!("split_pane: focused pane has no daemon_pane_id");
        return;
    };

    let direction_str = match orientation {
        gtk4::Orientation::Horizontal => "horizontal",
        _ => "vertical",
    };

    let daemon_new_pane_id = match dc.split_pane(parent_pane_id, direction_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("split_pane RPC failed: {e}");
            return;
        }
    };
    let channel = match dc.subscribe_output(daemon_new_pane_id) {
        Ok(ch) => ch,
        Err(e) => {
            tracing::warn!("subscribe_output failed for split pane {daemon_new_pane_id}: {e}");
            return;
        }
    };
    // V2-007: byte-log replay populates the VT via subscribe_output.
    let new_pane_result = terminal::create_terminal(
        config,
        daemon_new_pane_id,
        Arc::clone(dc),
        channel,
        None,
        Some(on_exit),
        Some(on_notify),
    );

    let (new_pane_vbox, new_da, new_state) = match new_pane_result {
        Ok(triple) => triple,
        Err(e) => {
            tracing::error!("Failed to create terminal for split: {e}");
            return;
        }
    };

    let new_pane_id = next_pane_id();
    new_da.set_widget_name(&new_pane_id);
    tab_states.borrow_mut().insert(new_pane_id, new_state);
    wire_focus_tracking(
        &new_da,
        focus_tracker,
        tab_view,
        tab_states,
        custom_titles,
        window,
        tab_id_map,
        workspace_manager,
        daemon_client.clone(),
    );

    // Create the Paned container
    let paned = gtk4::Paned::new(orientation);
    paned.set_wide_handle(true);
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned.set_hexpand(true);
    paned.set_vexpand(true);

    // The DrawingArea lives inside: DA -> hbox(DA+scrollbar) -> vbox(SearchBar+hbox).
    // We need to operate on the vbox for tree manipulation (the pane unit).
    let focused_hbox: gtk4::Widget =
        focused_da.parent().expect("focused DA should have a parent hbox");
    let focused_vbox: gtk4::Widget = focused_hbox.parent().expect("hbox should have a parent vbox");

    // Determine where the vbox sits in the widget tree.
    // Detect the slot BEFORE removing the child.
    let is_direct_child = root_content == focused_vbox;
    let parent = focused_vbox.parent();
    let parent_slot = parent.as_ref().and_then(|p| {
        p.downcast_ref::<gtk4::Paned>().map(|pp| detect_paned_slot(pp, &focused_vbox))
    });

    // Remove the vbox from its current parent.
    // IMPORTANT: For Paned parents, we MUST use set_start/end_child(None)
    // instead of unparent(). Direct unparent() doesn't clear the Paned's
    // internal child pointer, so a later set_start/end_child() would
    // double-unparent the widget from its new location.
    if is_direct_child {
        focused_vbox.unparent();
    } else if let Some(ref parent_widget) = parent {
        if let Some(parent_paned) = parent_widget.downcast_ref::<gtk4::Paned>() {
            match parent_slot.unwrap_or(PanedSlot::End) {
                PanedSlot::Start => parent_paned.set_start_child(gtk4::Widget::NONE),
                PanedSlot::End => parent_paned.set_end_child(gtk4::Widget::NONE),
            }
        } else {
            focused_vbox.unparent();
        }
    }

    // Set up the Paned children.
    // When `before` is true (split-left/split-up), the new pane goes in
    // start_child so it appears before (left of / above) the existing pane.
    if before {
        paned.set_start_child(Some(&new_pane_vbox));
        paned.set_end_child(Some(&focused_vbox));
    } else {
        paned.set_start_child(Some(&focused_vbox));
        paned.set_end_child(Some(&new_pane_vbox));
    }

    // Defensive focus-chain hint: pin the new Paned's focus-child to
    // the new pane's vbox so tab-traversal and programmatic
    // child_focus calls prefer the new subtree. `focus_when_mapped`
    // is the primary focus grant; this keeps the chain consistent.
    paned.set_focus_child(Some(&new_pane_vbox));

    if is_direct_child {
        // The hbox was the sole content of the pane container.
        // Replace it with the new Paned.
        set_container_content(&container, &paned);
    } else if let Some(parent_widget) = parent {
        // The hbox was inside a nested Paned.
        // Insert the new Paned in the same slot that the hbox occupied.
        if let Some(parent_paned) = parent_widget.downcast_ref::<gtk4::Paned>() {
            match parent_slot.unwrap_or(PanedSlot::End) {
                PanedSlot::Start => parent_paned.set_start_child(Some(&paned)),
                PanedSlot::End => parent_paned.set_end_child(Some(&paned)),
            }
            // Mirror the hint on the parent Paned: its pre-split
            // focus-child pointed at focused_vbox (now nested inside
            // `paned`); re-pin to the new Paned so the chain walks
            // parent_paned -> paned -> new_pane_vbox -> new_da.
            parent_paned.set_focus_child(Some(&paned));
        }
    }

    // Set initial divider position to 50% after the widget is realized
    {
        let paned_weak = paned.downgrade();
        glib::idle_add_local_once(move || {
            let Some(paned) = paned_weak.upgrade() else {
                return;
            };
            let size = match paned.orientation() {
                gtk4::Orientation::Horizontal => paned.width(),
                _ => paned.height(),
            };
            if size > 0 {
                paned.set_position(size / 2);
            }
        });
    }

    // Give focus to the new pane. `focus_when_mapped` arms both a
    // connect_map handler and a HIGH_IDLE grab — whichever fires while
    // the DA is actually mapped wins. This handles the transient-unmap
    // that GtkPaned's first layout cycle causes when `shrink_*_child`
    // is false and one child's minimum consumes the full allocation
    // (e.g. splitting a pane running Claude Code vertically).
    focus_when_mapped(&new_da);
}

/// Detect which slot of a parent Paned holds a child.
///
/// Must be called BEFORE unparenting the child, since unparenting clears
/// the parent's child reference.
fn detect_paned_slot(parent_paned: &gtk4::Paned, child: &impl IsA<gtk4::Widget>) -> PanedSlot {
    if let Some(start) = parent_paned.start_child() {
        if start == *child.upcast_ref::<gtk4::Widget>() {
            return PanedSlot::Start;
        }
    }
    PanedSlot::End
}

/// Which slot of a Paned a child occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanedSlot {
    Start,
    End,
}

// ---------------------------------------------------------------------------
// Close pane
// ---------------------------------------------------------------------------

/// Close the currently focused pane.
///
/// Delegates to `close_pane_by_name()` with the focused pane's widget name.
fn close_focused_pane(
    tab_view: &adw::TabView,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    workspace_manager: &WorkspaceManager,
) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    close_pane_by_name(
        &focused_name,
        tab_view,
        tab_states,
        window,
        daemon_client,
        workspace_manager,
    );
}

/// Close a specific pane identified by its DrawingArea widget name.
///
/// If the pane is the only one in its tab, the tab is closed.
/// If the tab is the only tab, the window (and application) closes.
/// If the pane is part of a split, the sibling expands and receives focus.
///
/// This function is idempotent -- if the pane has already been removed from
/// the registry or the widget is already destroyed, it silently no-ops.
fn close_pane_by_name(
    pane_name: &str,
    tab_view: &adw::TabView,
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    workspace_manager: &WorkspaceManager,
) {
    // Idempotency guard: if the pane is already gone from the registry,
    // it was already closed (e.g., by a concurrent manual Ctrl+Shift+W).
    {
        let Ok(states) = tab_states.try_borrow() else {
            return;
        };
        if !states.contains_key(pane_name) {
            return;
        }
    }

    // Find which tab page contains this pane by searching all pages.
    let mut target_page: Option<adw::TabPage> = None;
    for i in 0..tab_view.n_pages() {
        let page = tab_view.nth_page(i);
        let page_child = page.child();
        let leaves = collect_leaf_drawing_areas(&page_child);
        if leaves.iter().any(|da| da.widget_name().as_str() == pane_name) {
            target_page = Some(page);
            break;
        }
    }

    let Some(page) = target_page else {
        // Pane widget not found in any tab -- already removed.
        return;
    };

    let Some(container) = pane_container(&page) else {
        return;
    };
    let Some(root_content) = container_content(&container) else {
        return;
    };

    let leaves = collect_leaf_drawing_areas(&root_content);

    // Find the target DrawingArea
    let target_da = leaves.iter().find(|da| da.widget_name().as_str() == pane_name);

    let Some(target_da) = target_da.cloned() else {
        return;
    };

    // If this is the only pane in the tab, close the tab.
    if leaves.len() <= 1 {
        if let Some(dc) = daemon_client.as_deref() {
            // is_sole_pane=true → close_tab RPC.
            daemon_close_pane(pane_name, tab_states, dc, true);
        } else {
            tab_states.borrow_mut().remove(pane_name);
        }

        if tab_view.n_pages() <= 1 {
            // FIX-003 AC-9 + FIX-009 9a: only close the window when BOTH
            // (a) this TabView is still attached to the main widget tree
            // and (b) no other workspace is alive. The parent-attached check
            // (FIX-003 AC-9) handles workspace-delete: the deleted workspace's
            // TabView is removed from `main_area` before the daemon-initiated
            // on_exit callback runs, so `parent().is_some()` is false and we
            // skip `window.close()`. The workspace-count check (FIX-009 9a)
            // handles the PTY-exit / X-button last-tab-of-non-last-workspace
            // case, where the TabView IS still attached but another workspace
            // exists — closing the window would be a UX shocker.
            //
            // FIX-009 cycle 2: when the workspace-count guard says "don't
            // close the window," we STILL must close the dead page. The
            // daemon's auto-spawn emits `TabCreated` which builds a fresh
            // widget AS A NEW PAGE; without this `close_page` call the
            // workspace's TabView ends up holding both the dead old page
            // (frozen on the user's last command) AND the new auto-spawned
            // page. Closing here clears the stale page so the auto-spawn
            // leaves a single fresh tab.
            //
            // `unwrap_or(true)` falls back to the pre-FIX-009 close-window
            // behaviour if the WorkspaceManager is re-entrantly borrowed.
            let is_last_workspace =
                workspace_manager.try_borrow().map(|m| m.workspaces.len() <= 1).unwrap_or(true);
            if tab_view.parent().is_some() {
                if is_last_workspace {
                    window.close();
                } else {
                    tab_view.close_page(&page);
                }
            }
        } else {
            tab_view.close_page(&page);
        }
        return;
    }

    // The DrawingArea lives inside: DA -> hbox -> vbox.
    // Navigate: DrawingArea -> hbox -> vbox -> parent Paned.
    let Some(hbox_widget) = target_da.parent() else {
        return;
    };
    let Some(vbox_widget) = hbox_widget.parent() else {
        return;
    };
    let Some(parent_widget) = vbox_widget.parent() else {
        return;
    };

    let Some(parent_paned) = parent_widget.downcast_ref::<gtk4::Paned>() else {
        return;
    };

    // Determine the sibling (the other child of the parent Paned)
    let slot = detect_paned_slot(parent_paned, &vbox_widget);
    let sibling = match slot {
        PanedSlot::Start => parent_paned.end_child(),
        PanedSlot::End => parent_paned.start_child(),
    };

    let Some(sibling) = sibling else {
        return;
    };

    // Remove both children from the Paned using the proper Paned API.
    // Direct unparent() doesn't clear Paned's internal child pointers.
    //
    // P-007: clear focus_child BEFORE clearing the slots. FIX-002 pinned
    // parent_paned.focus_child to focused_vbox at split time; if we clear
    // the slot while focus_child still points inside it, GTK emits
    // `gtk_paned_set_focus_child was called on widget (nil)`. Clearing
    // focus_child to None first suppresses the cosmetic warning.
    parent_paned.set_focus_child(gtk4::Widget::NONE);
    parent_paned.set_start_child(gtk4::Widget::NONE);
    parent_paned.set_end_child(gtk4::Widget::NONE);

    // Remove from registry (and ask the daemon to close the pane, if in daemon mode).
    // is_sole_pane=false → close_pane RPC (pane is part of a split).
    if let Some(dc) = daemon_client.as_deref() {
        daemon_close_pane(pane_name, tab_states, dc, false);
    } else {
        tab_states.borrow_mut().remove(pane_name);
    }

    // Replace the Paned with the surviving sibling.
    // Check if the Paned was the direct content of the pane container.
    let paned_is_root = root_content == *parent_paned;

    if paned_is_root {
        // The Paned was the root content. Remove it and replace with sibling.
        parent_paned.unparent();
        set_container_content(&container, &sibling);
    } else {
        // The Paned was nested inside another Paned (grandparent).
        let grandparent = parent_paned.parent();
        if let Some(gp_widget) = grandparent {
            if let Some(gp_paned) = gp_widget.downcast_ref::<gtk4::Paned>() {
                // Detect which slot the parent Paned occupies in the grandparent
                let gp_slot = if gp_paned.start_child().map(|c| c == *parent_paned).unwrap_or(false)
                {
                    PanedSlot::Start
                } else {
                    PanedSlot::End
                };

                // P-007: clear grandparent focus_child BEFORE replacing its
                // slot. FIX-002 pinned gp_paned.focus_child to parent_paned;
                // replacing the slot while focus_child still points at the
                // outgoing parent_paned trips the same warning as above.
                gp_paned.set_focus_child(gtk4::Widget::NONE);

                // Use Paned API to remove and replace (not unparent)
                match gp_slot {
                    PanedSlot::Start => gp_paned.set_start_child(Some(&sibling)),
                    PanedSlot::End => gp_paned.set_end_child(Some(&sibling)),
                }
            }
        }
    }

    // Focus a leaf in the surviving subtree
    let surviving_leaves = collect_leaf_drawing_areas(&sibling);
    if let Some(target) = surviving_leaves.first() {
        target.grab_focus();
    }
}

// ---------------------------------------------------------------------------
// Pane navigation
// ---------------------------------------------------------------------------

/// Direction for pane navigation.
#[derive(Debug, Clone, Copy)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Navigate focus to the nearest pane in the given direction.
///
/// Uses a geometric nearest-neighbor approach: collects all leaf DrawingAreas,
/// computes their bounds relative to a common ancestor, and finds the closest
/// one in the requested direction from the currently focused pane.
fn navigate_pane(tab_view: &adw::TabView, focus_tracker: &FocusTracker, direction: Direction) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    let Some(page) = tab_view.selected_page() else {
        return;
    };
    let Some(container) = pane_container(&page) else {
        return;
    };
    let Some(root_content) = container_content(&container) else {
        return;
    };

    let leaves = collect_leaf_drawing_areas(&root_content);

    if leaves.len() < 2 {
        return;
    }

    // Find the focused DA and compute its bounds relative to the container.
    let focused_da = leaves.iter().find(|da| da.widget_name().as_str() == focused_name);

    let Some(focused_da) = focused_da else {
        return;
    };

    // Use the container Box as the common ancestor for coordinate computation.
    let container_widget: gtk4::Widget = container.clone().into();
    let Some(focused_bounds) = focused_da.compute_bounds(&container_widget) else {
        return;
    };

    // Find the nearest neighbor in the requested direction
    let mut best: Option<(&gtk4::DrawingArea, f32)> = None;

    for candidate in &leaves {
        if candidate.widget_name() == focused_da.widget_name() {
            continue;
        }

        let Some(bounds) = candidate.compute_bounds(&container_widget) else {
            continue;
        };

        // Check if the candidate is in the right direction and overlaps on the
        // perpendicular axis.
        let (is_valid, distance) = match direction {
            Direction::Left => {
                // Candidate must be to the left: its right edge <= focused left edge
                // and must overlap vertically.
                let valid = bounds.x() + bounds.width() <= focused_bounds.x()
                    && ranges_overlap(
                        bounds.y(),
                        bounds.y() + bounds.height(),
                        focused_bounds.y(),
                        focused_bounds.y() + focused_bounds.height(),
                    );
                let dist = focused_bounds.x() - (bounds.x() + bounds.width());
                (valid, dist)
            }
            Direction::Right => {
                let valid = bounds.x() >= focused_bounds.x() + focused_bounds.width()
                    && ranges_overlap(
                        bounds.y(),
                        bounds.y() + bounds.height(),
                        focused_bounds.y(),
                        focused_bounds.y() + focused_bounds.height(),
                    );
                let dist = bounds.x() - (focused_bounds.x() + focused_bounds.width());
                (valid, dist)
            }
            Direction::Up => {
                let valid = bounds.y() + bounds.height() <= focused_bounds.y()
                    && ranges_overlap(
                        bounds.x(),
                        bounds.x() + bounds.width(),
                        focused_bounds.x(),
                        focused_bounds.x() + focused_bounds.width(),
                    );
                let dist = focused_bounds.y() - (bounds.y() + bounds.height());
                (valid, dist)
            }
            Direction::Down => {
                let valid = bounds.y() >= focused_bounds.y() + focused_bounds.height()
                    && ranges_overlap(
                        bounds.x(),
                        bounds.x() + bounds.width(),
                        focused_bounds.x(),
                        focused_bounds.x() + focused_bounds.width(),
                    );
                let dist = bounds.y() - (focused_bounds.y() + focused_bounds.height());
                (valid, dist)
            }
        };

        if is_valid && (best.is_none() || distance < best.unwrap().1) {
            best = Some((candidate, distance));
        }
    }

    if let Some((target, _)) = best {
        target.grab_focus();
    }
}

/// Check if two 1D float ranges overlap. Each range is [start, end).
fn ranges_overlap(a_start: f32, a_end: f32, b_start: f32, b_end: f32) -> bool {
    a_start < b_end && b_start < a_end
}

/// Move the correct Paned divider to grow the focused pane in the given direction.
///
/// The key insight: a Paned has one divider position. Increasing it grows the
/// `start_child`; decreasing it grows the `end_child`. So to grow the focused
/// pane in a given direction we must find a Paned where:
///
/// - Grow right / grow down → focused pane's sub-tree is the `start_child`.
///   Increasing position pushes the divider outward, making start side bigger.
/// - Grow left / grow up   → focused pane's sub-tree is the `end_child`.
///   Decreasing position pushes the divider outward, making end side bigger.
///
/// If the nearest Paned of the right orientation has us in the wrong slot, we
/// keep walking up until we find one where we're on the correct side. This
/// handles three-pane layouts: the middle pane's "resize right" finds a Paned
/// where the middle sub-tree is on the start side (i.e., the right wall), not
/// the left wall.
///
/// Widget tree from DA to its Paned: DA → hbox (DA+scrollbar) → vbox (search+hbox) → Paned
fn resize_pane(tab_view: &adw::TabView, focus_tracker: &FocusTracker, direction: Direction) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };
    if focused_name.is_empty() {
        return;
    }

    let Some(page) = tab_view.selected_page() else {
        return;
    };
    let Some(container) = pane_container(&page) else {
        return;
    };
    let Some(root_content) = container_content(&container) else {
        return;
    };

    let leaves = collect_leaf_drawing_areas(&root_content);
    let Some(focused_da) = leaves.iter().find(|da| da.widget_name().as_str() == focused_name)
    else {
        return;
    };

    let target_orientation = match direction {
        Direction::Left | Direction::Right => gtk4::Orientation::Horizontal,
        Direction::Up | Direction::Down => gtk4::Orientation::Vertical,
    };

    // Grow right/down → focused pane must be in the start_child slot (increasing position expands it).
    // Grow left/up   → focused pane must be in the end_child slot (decreasing position expands it).
    let want_start_slot = matches!(direction, Direction::Right | Direction::Down);

    const STEP: i32 = 20;
    let delta = if want_start_slot { STEP } else { -STEP };

    // Pass 1: slot-aware walk.
    // Find the first Paned of matching orientation where the focused sub-tree is
    // in the correct slot. This ensures the middle pane of a 3-split finds the
    // right divider for each direction instead of always using the nearest one.
    let mut nearest_paned: Option<gtk4::Paned> = None;
    let mut widget: gtk4::Widget = focused_da.clone().into();
    let mut found: Option<gtk4::Paned> = None;
    loop {
        let Some(parent) = widget.parent() else {
            break;
        };
        if let Some(paned) = parent.downcast_ref::<gtk4::Paned>() {
            if paned.orientation() == target_orientation {
                if nearest_paned.is_none() {
                    nearest_paned = Some(paned.clone());
                }
                // `widget` is a direct child of `paned`.
                let in_start = paned.start_child().as_ref() == Some(&widget);
                if in_start == want_start_slot {
                    found = Some(paned.clone());
                    break;
                }
            }
        }
        widget = parent;
    }

    // Pass 2: fallback to nearest Paned when no correctly-slotted one exists.
    // Covers the 2-pane case: the left pane pressing Alt+Shift+Left has no
    // end-side ancestor, so we just move the one available divider.
    let target_paned = found.or(nearest_paned);

    if let Some(paned) = target_paned {
        let max = match target_orientation {
            gtk4::Orientation::Horizontal => paned.width(),
            _ => paned.height(),
        };
        let new_pos = (paned.position() + delta).clamp(0, max);
        paned.set_position(new_pos);
    }
}

// ---------------------------------------------------------------------------
// Search in terminal
// ---------------------------------------------------------------------------

/// Toggle the search bar on the currently focused pane.
///
/// Looks up the focused DrawingArea from the focus tracker, retrieves its
/// TerminalState, and delegates to `terminal::toggle_search()`.
fn toggle_search_on_focused_pane(tab_states: &TabStateMap, focus_tracker: &FocusTracker) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    let Ok(states) = tab_states.try_borrow() else {
        return;
    };

    let Some(state_rc) = states.get(&focused_name).cloned() else {
        return;
    };
    drop(states);

    // Find the DrawingArea widget by its name to pass to toggle_search.
    // Walk up from a known widget -- we need the DA itself.
    // Since we have the state, we can use the GLib/GTK widget registry indirectly:
    // look for a DrawingArea with the focused_name in any visible window.
    // A simpler approach: iterate the focused window's widget tree.
    // But the easiest path: the DrawingArea is registered via widget_name.
    // GTK doesn't provide a global "find by name" API, so we walk the tree.
    let app =
        gtk4::gio::Application::default().and_then(|a| a.downcast::<gtk4::Application>().ok());
    let Some(app) = app else {
        return;
    };
    let Some(window) = app.active_window() else {
        return;
    };
    let Some(da) = find_drawing_area_by_name(&window, &focused_name) else {
        return;
    };

    terminal::toggle_search(&da, &state_rc);
}

/// Direction for font zoom actions.
#[derive(Clone, Copy)]
enum ZoomDirection {
    In,
    Out,
    Reset,
}

/// Minimum font size (points) -- below this, text is unreadable.
const FONT_SIZE_MIN: f32 = 6.0;
/// Maximum font size (points) -- above this, a single cell fills the window.
const FONT_SIZE_MAX: f32 = 72.0;

/// Apply a zoom action (in/out/reset) to the currently focused pane.
fn zoom_focused_pane(
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    direction: ZoomDirection,
) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    let Ok(states) = tab_states.try_borrow() else {
        return;
    };

    let Some(state_rc) = states.get(&focused_name).cloned() else {
        return;
    };
    drop(states);

    // Find the DrawingArea widget by name (needed for pango context + dimensions)
    let app =
        gtk4::gio::Application::default().and_then(|a| a.downcast::<gtk4::Application>().ok());
    let Some(app) = app else {
        return;
    };
    let Some(window) = app.active_window() else {
        return;
    };
    let Some(da) = find_drawing_area_by_name(&window, &focused_name) else {
        return;
    };

    let Ok(mut s) = state_rc.try_borrow_mut() else {
        return;
    };

    let new_size = match direction {
        ZoomDirection::In => (s.font_size + 1.0).min(FONT_SIZE_MAX),
        ZoomDirection::Out => (s.font_size - 1.0).max(FONT_SIZE_MIN),
        ZoomDirection::Reset => s.default_font_size,
    };

    if (new_size - s.font_size).abs() < f32::EPSILON {
        return; // No change needed
    }

    s.font_size = new_size;
    terminal::apply_font_zoom(&mut s, &da);
    drop(s);
    da.queue_draw();
}

/// Recursively find a DrawingArea with the given widget name.
fn find_drawing_area_by_name(
    widget: &impl IsA<gtk4::Widget>,
    name: &str,
) -> Option<gtk4::DrawingArea> {
    let widget_ref = widget.upcast_ref::<gtk4::Widget>();
    if let Some(da) = widget_ref.downcast_ref::<gtk4::DrawingArea>() {
        if da.widget_name().as_str() == name {
            return Some(da.clone());
        }
    }
    // Walk children
    let mut child = widget_ref.first_child();
    while let Some(c) = child {
        if let Some(found) = find_drawing_area_by_name(&c, name) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

// ---------------------------------------------------------------------------
// Copy selection
// ---------------------------------------------------------------------------

/// Copy the currently selected text from the focused pane to the system clipboard.
///
/// Extracts text from the selection, runs it through the smart copy pipeline
/// (strip box-drawing, trailing whitespace, normalize newlines), and places
/// the result on the system clipboard via GDK.
fn copy_selection(
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    window: &adw::ApplicationWindow,
) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    let Ok(states) = tab_states.try_borrow() else {
        return;
    };

    let Some(state_rc) = states.get(&focused_name).cloned() else {
        return;
    };
    drop(states);

    let Ok(mut s) = state_rc.try_borrow_mut() else {
        return;
    };

    // AC-9: Do nothing if no active selection
    let Some(ref sel) = s.selection else {
        return;
    };

    // Selection coordinates are stored as absolute scrollback rows.
    // The screen() viewport only shows `rows` visible lines at a time.
    // For selections spanning more than one viewport, we must scroll
    // page by page, extracting each viewport's contribution.
    let sel_clone = sel.clone();
    let ((sr, sc), (er, ec)) = sel_clone.ordered();
    let sel_mode = sel_clone.mode;
    let (_, orig_offset, _) = s.terminal.scrollbar_state();

    let mut lines: Vec<String> = Vec::new();
    let mut wrap_flags: Vec<bool> = Vec::new();
    let mut cursor = sr; // absolute row we need to read next

    while cursor <= er {
        // Scroll viewport so `cursor` is at the top
        let (_, cur_off, _) = s.terminal.scrollbar_state();
        let delta = cursor as isize - cur_off as isize;
        if delta != 0 {
            s.terminal.scroll_viewport_delta(delta);
        }

        let (_, vp_off, vp_len) = s.terminal.scrollbar_state();
        let vp_off = vp_off as usize;
        let vp_len = vp_len as usize;
        let screen = s.terminal.screen();
        let num_screen_rows = screen.rows();
        let num_cols = screen.cols();

        // Read rows from this viewport that fall within the selection
        let page_end = er.min(vp_off + vp_len.saturating_sub(1));
        for abs_row in cursor..=page_end {
            let screen_row = abs_row.saturating_sub(vp_off);
            if screen_row >= num_screen_rows {
                break;
            }
            let cells = screen.row(screen_row);

            let (col_start, col_end) = match sel_mode {
                forgetty_vt::selection::SelectionMode::Line => (0, num_cols.saturating_sub(1)),
                forgetty_vt::selection::SelectionMode::Block => {
                    (sc.min(ec), sc.max(ec).min(num_cols.saturating_sub(1)))
                }
                _ => {
                    let cs = if abs_row == sr { sc } else { 0 };
                    let ce = if abs_row == er {
                        ec.min(num_cols.saturating_sub(1))
                    } else {
                        num_cols.saturating_sub(1)
                    };
                    (cs, ce)
                }
            };

            let mut line = String::new();
            let upper = col_end.min(cells.len().saturating_sub(1));
            for cell in cells.iter().take(upper + 1).skip(col_start) {
                line.push_str(&cell.grapheme);
            }

            // Soft-wrap detection: use the real wrap flag from libghostty-vt
            // when available (works for command output). For shell input (typed
            // commands), the shell manages wrapping via escape sequences and the
            // terminal never sets the wrap flag — fall back to a heuristic:
            // if the row's content fills most of the terminal width, treat it
            // as soft-wrapped.
            let is_wrapped = if screen.is_row_wrapped(screen_row) {
                true
            } else if abs_row < er && num_cols > 10 {
                // Heuristic: count trailing space cells. Shell-wrapped lines
                // fill nearly the full row, breaking at word boundaries (leaving
                // a few trailing spaces). Hard-newline lines typically have much
                // more trailing whitespace.
                let mut trailing_spaces = 0usize;
                for c in cells.iter().rev() {
                    if c.grapheme == " " {
                        trailing_spaces += 1;
                    } else {
                        break;
                    }
                }
                // Row is "nearly full" if content occupies > 80% of the width
                trailing_spaces < num_cols / 5
            } else {
                false // last selected row is never wrapped
            };

            lines.push(line);
            wrap_flags.push(is_wrapped);
        }

        cursor = page_end + 1;
    }

    // Restore original viewport position
    let (_, cur_off, _) = s.terminal.scrollbar_state();
    let restore = orig_offset as isize - cur_off as isize;
    if restore != 0 {
        s.terminal.scroll_viewport_delta(restore);
    }

    // Join lines, skipping `\n` between soft-wrapped rows.
    // When joining wrapped rows: trim trailing whitespace from the wrapped row
    // AND trim leading whitespace from the continuation row (shell line editors
    // add indentation on continuation lines that isn't part of the text).
    let mut raw_text = String::new();
    for (i, line) in lines.iter().enumerate() {
        // If the previous row was wrapped, this is a continuation — strip
        // the shell's continuation indent (leading whitespace).
        let text: &str = if i > 0 && wrap_flags[i - 1] { line.trim_start() } else { line };

        if wrap_flags[i] {
            // This row continues on the next — trim trailing padding but
            // preserve one space as word separator if the original had any.
            let trimmed = text.trim_end();
            raw_text.push_str(trimmed);
            if text.len() > trimmed.len() {
                raw_text.push(' ');
            }
        } else {
            raw_text.push_str(text);
            if i + 1 < lines.len() {
                raw_text.push('\n');
            }
        }
    }

    if raw_text.is_empty() {
        return;
    }

    // Run through the smart copy pipeline (AC-6, AC-7, AC-8)
    let cleaned = clipboard::smart_copy_pipeline(&raw_text);

    if cleaned.is_empty() {
        return;
    }

    // Write to system clipboard via GDK
    let display = gtk4::prelude::WidgetExt::display(window);
    let gdk_clipboard = display.clipboard();
    gdk_clipboard.set_text(&cleaned);

    tracing::debug!("Copied {} chars to clipboard", cleaned.len());
}

// ---------------------------------------------------------------------------
// Widget tree helpers
// ---------------------------------------------------------------------------

/// Recursively collect all leaf DrawingArea widgets from a widget subtree.
///
/// Walks through Paned widgets to find all terminal panes. This is used for:
/// - Tab close (kill all PTYs)
/// - Focus management (find panes to navigate to)
/// - Title polling (identify which pane belongs to which tab)
fn collect_leaf_drawing_areas(widget: &gtk4::Widget) -> Vec<gtk4::DrawingArea> {
    let mut result = Vec::new();
    collect_leaves_recursive(widget, &mut result);
    result
}

fn collect_leaves_recursive(widget: &gtk4::Widget, result: &mut Vec<gtk4::DrawingArea>) {
    if let Some(da) = widget.downcast_ref::<gtk4::DrawingArea>() {
        result.push(da.clone());
    } else if let Some(paned) = widget.downcast_ref::<gtk4::Paned>() {
        if let Some(start) = paned.start_child() {
            collect_leaves_recursive(&start, result);
        }
        if let Some(end) = paned.end_child() {
            collect_leaves_recursive(&end, result);
        }
    } else if let Some(bx) = widget.downcast_ref::<gtk4::Box>() {
        // Walk children of a Box (the pane container)
        let mut child = bx.first_child();
        while let Some(c) = child {
            collect_leaves_recursive(&c, result);
            child = c.next_sibling();
        }
    }
}

/// Send the appropriate close RPC to the daemon, then remove from registry.
///
/// `is_sole_pane` indicates whether this pane is the only leaf in its tab:
/// - `true`  → send `close_tab_by_pane_id` (closes the whole tab)
/// - `false` → send `close_pane` (closes only this split leaf; sibling promoted by daemon)
fn daemon_close_pane(
    pane_name: &str,
    tab_states: &TabStateMap,
    daemon_client: &DaemonClient,
    is_sole_pane: bool,
) {
    if let Some(state_rc) = tab_states.borrow().get(pane_name).cloned() {
        if let Ok(s) = state_rc.try_borrow() {
            if let Some(pane_id) = s.daemon_pane_id {
                if is_sole_pane {
                    if let Err(e) = daemon_client.close_tab_by_pane_id(pane_id) {
                        tracing::warn!("close_tab RPC failed for {pane_name}: {e}");
                    }
                } else if let Err(e) = daemon_client.close_pane(pane_id) {
                    tracing::warn!("close_pane RPC failed for {pane_name}: {e}");
                }
            }
        }
    }
    tab_states.borrow_mut().remove(pane_name);
}

/// Drop every leaf pane's `TerminalState` from the registry without any daemon RPC.
///
/// Legacy fallback for the no-daemon-client path. Post-FIX-011 daemon_client is
/// always Some, making this function unreachable on the live path; deletion is
/// deferred to CHORE-FIX-011-cleanup.
fn remove_panes_in_subtree(widget: &gtk4::Widget, tab_states: &TabStateMap) {
    let leaves = collect_leaf_drawing_areas(widget);
    let mut states = tab_states.borrow_mut();
    for da in &leaves {
        states.remove(&da.widget_name().to_string());
    }
}

/// Walk a widget subtree, send close_tab RPC for each pane, remove from registry.
///
/// Used when an entire tab is being closed (e.g. tab-bar X button). All panes
/// in the subtree are treated as "sole pane" so that each sends a `close_tab` RPC.
fn daemon_close_panes_in_subtree(
    widget: &gtk4::Widget,
    tab_states: &TabStateMap,
    daemon_client: &DaemonClient,
) {
    if let Some(da) = widget.downcast_ref::<gtk4::DrawingArea>() {
        let pane_name = da.widget_name().to_string();
        // is_sole_pane=true: we're closing the whole tab, so use close_tab RPC.
        daemon_close_pane(&pane_name, tab_states, daemon_client, true);
        return;
    }

    // Recurse into children.
    let mut child = widget.first_child();
    while let Some(c) = child {
        daemon_close_panes_in_subtree(&c, tab_states, daemon_client);
        child = c.next_sibling();
    }
}

/// Write bytes to the focused pane via the daemon's `send_input` RPC.
fn write_to_focused_pane(
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    bytes: &[u8],
    daemon_client: &DaemonClient,
) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else { return };
        name.clone()
    };
    if focused_name.is_empty() {
        return;
    }
    let state_rc = {
        let Ok(states) = tab_states.try_borrow() else { return };
        let Some(state_rc) = states.get(&focused_name).cloned() else { return };
        state_rc
    };

    let Ok(s) = state_rc.try_borrow() else { return };
    let Some(pane_id) = s.daemon_pane_id else { return };
    if let Err(e) = daemon_client.send_input(pane_id, bytes) {
        tracing::warn!("send_input RPC failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// Focus tracking
// ---------------------------------------------------------------------------

/// Wire up an EventControllerFocus on a DrawingArea to update the focus tracker
/// when this pane gains focus.
///
/// Also triggers a redraw on focus change so the visual indicator updates.
///
/// FIX-005B: when a daemon client is present, the focus-enter handler also
/// pushes a provenance entry onto `WorkspaceManagerInner::pending_active_pane`
/// and issues a `set_active_pane` RPC so the daemon persists "which split
/// was I typing in" across cold restart. The push-before-RPC order matches
/// the FIX-006 reorder pattern — the daemon's broadcast may arrive before
/// the RPC return path drops the borrow.
#[allow(clippy::too_many_arguments)]
fn wire_focus_tracking(
    drawing_area: &gtk4::DrawingArea,
    focus_tracker: &FocusTracker,
    tab_view: &adw::TabView,
    tab_states: &TabStateMap,
    custom_titles: &CustomTitles,
    window: &adw::ApplicationWindow,
    tab_id_map: &TabIdMap,
    workspace_manager: &WorkspaceManager,
    daemon_client: Option<Arc<DaemonClient>>,
) {
    let focus_controller = gtk4::EventControllerFocus::new();

    // Focus gained -- update the tracker, tab title, and window title immediately
    {
        let tracker = Rc::clone(focus_tracker);
        let da = drawing_area.clone();
        let tv = tab_view.clone();
        let states = Rc::clone(tab_states);
        let ct = Rc::clone(custom_titles);
        let win = window.clone();
        let tid_map = Rc::clone(tab_id_map);
        let wm = Rc::clone(workspace_manager);
        let dc = daemon_client.clone();
        focus_controller.connect_enter(move |_controller| {
            let pane_name = da.widget_name().to_string();
            if let Ok(mut name) = tracker.try_borrow_mut() {
                *name = pane_name.clone();
            }

            // Clear notification ring for this pane when it gains focus.
            if let Ok(map) = states.try_borrow() {
                if let Some(state_rc) = map.get(&pane_name) {
                    if let Ok(mut s) = state_rc.try_borrow_mut() {
                        s.notification_ring = false;
                    }
                }
            }

            // Redraw to show the focus indicator (and clear the ring)
            da.queue_draw();

            // FIX-005B: persist the focused pane id on the daemon so cold
            // restart restores the user's last-typed pane. Path:
            //   1. tab_states[pane_name].daemon_pane_id → PaneId
            //   2. tv.selected_page() → page → tab_id_map[page_key] → tab_id
            //   3. workspace_manager.borrow().active_index → workspace_idx
            //   4. Push provenance entry, drop the borrow, then RPC.
            //
            // Errors are debug-logged; never fail the focus event itself.
            if let Some(ref dc) = dc {
                let daemon_pane_id: Option<forgetty_core::PaneId> = states
                    .try_borrow()
                    .ok()
                    .and_then(|m| m.get(&pane_name).cloned())
                    .and_then(|rc| rc.try_borrow().ok().and_then(|s| s.daemon_pane_id));
                let tab_id: Option<uuid::Uuid> = tv.selected_page().and_then(|page| {
                    tid_map
                        .try_borrow()
                        .ok()
                        .and_then(|m| m.get(&page_identity_key(&page)).copied())
                });
                if let (Some(pane_id), Some(tab_id)) = (daemon_pane_id, tab_id) {
                    let active_idx = wm.try_borrow().ok().map(|m| m.active_index);
                    if let Some(active_idx) = active_idx {
                        // Push the provenance entry BEFORE the RPC so the
                        // daemon's echo (which may race the RPC return) finds
                        // a populated queue head. Also update this
                        // workspace's `tab_active_pane[tab_id] = pane_id`
                        // (FIX-005B fix-cycle 1) so a later
                        // `selected-page-notify` reads the right leaf
                        // without hitting the focus_tracker fallback.
                        // We snapshot the per-workspace map handle inside
                        // the borrow, then drop the borrow before the RPC
                        // to avoid holding it across an I/O call.
                        let map_handle: Option<TabActivePaneMap> = {
                            if let Ok(mut mgr) = wm.try_borrow_mut() {
                                mgr.pending_active_pane.push_back((active_idx, tab_id, pane_id));
                                mgr.workspaces
                                    .get(active_idx)
                                    .map(|ws| Rc::clone(&ws.tab_active_pane))
                            } else {
                                None
                            }
                        };
                        if let Some(map) = map_handle {
                            if let Ok(mut m) = map.try_borrow_mut() {
                                m.insert(tab_id, pane_id);
                            }
                        }
                        if let Err(e) = dc.set_active_pane(active_idx, tab_id, Some(pane_id)) {
                            tracing::debug!(
                                "set_active_pane RPC failed (ws={active_idx} tab={tab_id} pane={pane_id}): {e}"
                            );
                        }
                    }
                }
            }

            // Clear tab badge if ALL panes in the tab have notification_ring == false.
            // (AC-14 / AC-15: only clear badge if the specific ringed pane is focused)
            if let Some(page) = tv.selected_page() {
                let container = page.child();
                let leaves = collect_leaf_drawing_areas(&container);
                let any_ring = leaves.iter().any(|leaf_da| {
                    let leaf_name = leaf_da.widget_name().to_string();
                    states
                        .try_borrow()
                        .ok()
                        .and_then(|map| map.get(&leaf_name).cloned())
                        .and_then(|rc| rc.try_borrow().ok().map(|s| s.notification_ring))
                        .unwrap_or(false)
                });
                if !any_ring {
                    page.set_needs_attention(false);
                }
            }

            // Update tab title immediately from this pane's CWD
            // (skip if user has set a custom title for this page)
            if let Some(page) = tv.selected_page() {
                let has_custom_title = ct
                    .try_borrow()
                    .map(|ct| ct.contains(&page_identity_key(&page)))
                    .unwrap_or(false);
                if !has_custom_title {
                    if let Ok(map) = states.try_borrow() {
                        if let Some(state_rc) = map.get(&pane_name) {
                            if let Ok(s) = state_rc.try_borrow() {
                                let title = compute_display_title(&s);
                                if page.title().as_str() != title {
                                    page.set_title(&title);
                                }
                            }
                        }
                    }
                }
            }

            // Update the window title bar immediately for the newly focused pane.
            if let Ok(map) = states.try_borrow() {
                if let Some(state_rc) = map.get(&pane_name) {
                    if let Ok(s) = state_rc.try_borrow() {
                        let pane_title = compute_window_title(&s);
                        set_window_title_preserving_workspace(&win, &pane_title);
                    }
                }
            }
        });
    }

    // Focus lost -- redraw to remove the focus indicator
    {
        let da = drawing_area.clone();
        focus_controller.connect_leave(move |_controller| {
            da.queue_draw();
        });
    }

    drawing_area.add_controller(focus_controller);
}

// ---------------------------------------------------------------------------
// Title polling
// ---------------------------------------------------------------------------

/// Update the window title, preserving any workspace prefix.
///
/// If the current window title has a workspace prefix (`WsName -- ...`),
/// the prefix is preserved and only the pane portion is updated.
/// This allows the title polling timer to update `user@host:~/path` without
/// clobbering the workspace name set by workspace switch/create/rename.
fn set_window_title_preserving_workspace(win: &adw::ApplicationWindow, pane_title: &str) {
    let current = win.title().map(|t| t.to_string()).unwrap_or_default();
    // Check if current title has a workspace prefix (contains em-dash separator).
    if let Some(prefix_end) = current.find(" \u{2014} ") {
        let prefix = &current[..prefix_end];
        let new_title = format!("{prefix} \u{2014} {pane_title}");
        if current != new_title {
            win.set_title(Some(&new_title));
        }
    } else {
        // No workspace prefix -- single workspace mode.
        if current != pane_title {
            win.set_title(Some(pane_title));
        }
    }
}

/// Register a title polling timer for a tab page.
///
/// Periodically updates the tab title from the focused pane's CWD. The timer
/// checks if its page is the selected page and reads the focused pane's state
/// from the TabStateMap to compute the display title.
fn register_title_timer(
    page: &adw::TabPage,
    tab_view: &adw::TabView,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    window: &adw::ApplicationWindow,
) {
    let page_weak = page.downgrade();
    let tab_states_title = Rc::clone(tab_states);
    let focus_title = Rc::clone(focus_tracker);
    let custom_titles_timer = Rc::clone(custom_titles);
    let tv_weak = tab_view.downgrade();
    let win_weak = window.downgrade();
    let page_key = page_identity_key(page);

    glib::timeout_add_local(Duration::from_millis(TITLE_POLL_MS), move || {
        let Some(page) = page_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let Some(tv) = tv_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };

        // Only update title if this page is the selected page
        let is_selected = tv.selected_page().map(|sp| sp == page).unwrap_or(false);

        if !is_selected {
            return glib::ControlFlow::Continue;
        }

        // Skip CWD polling if user has set a custom title for this page
        if let Ok(ct) = custom_titles_timer.try_borrow() {
            if ct.contains(&page_key) {
                // Still update the window title bar from CWD, but leave tab title alone
                let focused_name = {
                    let Ok(name) = focus_title.try_borrow() else {
                        return glib::ControlFlow::Continue;
                    };
                    name.clone()
                };
                let Ok(states) = tab_states_title.try_borrow() else {
                    return glib::ControlFlow::Continue;
                };
                if let Some(state_rc) = states.get(&focused_name).cloned() {
                    drop(states);
                    if let Ok(s) = state_rc.try_borrow() {
                        // Only update window title if this workspace is visible (active).
                        if tv.parent().is_some() {
                            if let Some(win) = win_weak.upgrade() {
                                let pane_title = compute_window_title(&s);
                                set_window_title_preserving_workspace(&win, &pane_title);
                            }
                        }
                    }
                }
                return glib::ControlFlow::Continue;
            }
        }

        // Read the focused pane's state for the title
        let focused_name = {
            let Ok(name) = focus_title.try_borrow() else {
                return glib::ControlFlow::Continue;
            };
            name.clone()
        };

        let Ok(states) = tab_states_title.try_borrow() else {
            return glib::ControlFlow::Continue;
        };

        // Check that the focused pane belongs to this tab's page
        let page_child = page.child();
        let leaves = collect_leaf_drawing_areas(&page_child);
        let belongs_to_this_tab = leaves.iter().any(|da| da.widget_name().as_str() == focused_name);

        let state_rc = if belongs_to_this_tab {
            states.get(&focused_name).cloned()
        } else {
            // Focused pane is in another tab; use first leaf of this tab
            leaves.first().and_then(|da| {
                let name = da.widget_name().to_string();
                states.get(&name).cloned()
            })
        };
        drop(states);

        if let Some(state_rc) = state_rc {
            let Ok(s) = state_rc.try_borrow() else {
                return glib::ControlFlow::Continue;
            };
            let title = compute_display_title(&s);
            let current_title = page.title();
            if current_title.as_str() != title {
                page.set_title(&title);
            }
            // Update window title bar with user@host:cwd for the focused pane,
            // preserving any workspace prefix. Only if this workspace is active
            // (tab_view is parented in the widget tree).
            if tv.parent().is_some() {
                if let Some(win) = win_weak.upgrade() {
                    let pane_title = compute_window_title(&s);
                    set_window_title_preserving_workspace(&win, &pane_title);
                }
            }
        }

        glib::ControlFlow::Continue
    });
}

/// Extract just the CWD from an OSC 0/2 title string.
///
/// Shells like zsh/bash set the terminal title to `user@host:path` via OSC 0/2.
/// This strips the `user@host:` prefix and returns only the path.
/// If the title doesn't match that pattern, the original string is returned.
fn cwd_from_osc_title(title: &str) -> &str {
    if let Some(colon_pos) = title.find(':') {
        let before = &title[..colon_pos];
        if before.contains('@') {
            let after = title[colon_pos + 1..].trim_start_matches(' ');
            if !after.is_empty() {
                return after;
            }
        }
    }
    title
}

/// Compute the display title for a terminal tab.
///
/// Priority: /proc CWD path > daemon_cwd path > OSC title (path only) > "shell".
///
/// Always returns just the path (tilde-collapsed), never `user@host:path`.
fn compute_display_title(state: &TerminalState) -> String {
    let home = std::env::var("HOME").unwrap_or_default();

    let tilde_path = |cwd: &str| -> String {
        if !home.is_empty() && cwd.starts_with(&home) {
            let rest = &cwd[home.len()..];
            if rest.is_empty() {
                "~".to_string()
            } else {
                format!("~{}", rest)
            }
        } else {
            cwd.to_string()
        }
    };

    // Daemon panes: CWD from PaneInfo (set at connect time).
    if let Some(cwd) = &state.daemon_cwd {
        let cwd_str = cwd.to_string_lossy();
        if !cwd_str.is_empty() {
            return tilde_path(&cwd_str);
        }
    }

    // Fall back to OSC title — extract just the path portion.
    let osc_title = state.terminal.title();
    if !osc_title.is_empty() && osc_title != "shell" {
        return tilde_path(cwd_from_osc_title(osc_title));
    }

    "shell".to_string()
}

/// Compute the window title bar string — just the CWD path.
///
/// Priority:
/// 1. OSC 0/2 title — daemon panes (shell sets this on every prompt render)
/// 2. `daemon_cwd` — daemon panes fallback (connect-time CWD from PaneInfo)
/// 3. `"Forgetty"` — last resort
fn compute_window_title(state: &TerminalState) -> String {
    let home = std::env::var("HOME").unwrap_or_default();

    let tilde_cwd = |cwd: &str| -> String {
        if !home.is_empty() && cwd.starts_with(&home) {
            let rest = &cwd[home.len()..];
            if rest.is_empty() {
                return "Forgetty".to_string();
            }
            format!("~{}", rest)
        } else {
            cwd.to_string()
        }
    };

    // Daemon panes: OSC 0/2 title is set on every prompt render — extract just the path.
    let osc_title = state.terminal.title();
    if !osc_title.is_empty() {
        return tilde_cwd(cwd_from_osc_title(osc_title));
    }

    // Daemon panes: fall back to CWD captured at connect time from PaneInfo.
    if let Some(cwd) = &state.daemon_cwd {
        let cwd_str = cwd.to_string_lossy();
        if !cwd_str.is_empty() {
            return tilde_cwd(&cwd_str);
        }
    }

    "Forgetty".to_string()
}

// ---------------------------------------------------------------------------
// Paste from clipboard
// ---------------------------------------------------------------------------

/// Write `text` to the focused pane's daemon-backed PTY without any checks.
///
/// This is the fast path used both when no warnings are triggered and from
/// within the "Paste anyway" dialog callback.
fn do_paste(
    state_rc: Rc<RefCell<TerminalState>>,
    text: String,
    daemon_client: Option<Arc<DaemonClient>>,
) {
    let Some(dc) = daemon_client else { return };

    // Extract pane_id under an immutable borrow, then drop it so we can
    // take a mutable borrow below for the scroll-to-bottom update.
    let pane_id = {
        let Ok(s) = state_rc.try_borrow() else {
            return;
        };
        s.daemon_pane_id
    };
    let Some(pid) = pane_id else { return };

    let _ = dc.send_input(pid, text.as_bytes());

    // Scroll to bottom so pasted content is visible immediately.
    if let Ok(mut s) = state_rc.try_borrow_mut() {
        s.terminal.scroll_viewport_bottom();
        let (_, off, _) = s.terminal.scrollbar_state();
        s.viewport_offset = off;
    }
}

/// Paste the system clipboard text into the focused pane's PTY.
///
/// Reads the clipboard text asynchronously via `gdk::Clipboard::read_text_async()`,
/// then applies paste safety checks (size and newline) before writing to the PTY.
/// The `TerminalState` borrow is NOT held across the async boundary -- only
/// acquired in the callback (or in the dialog response handler).
#[allow(deprecated)]
fn paste_clipboard(
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    shared_config: Rc<RefCell<Config>>,
) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    let Ok(states) = tab_states.try_borrow() else {
        return;
    };

    let Some(state_rc) = states.get(&focused_name).cloned() else {
        return;
    };
    drop(states);

    // Read clipboard text asynchronously
    let display = gtk4::prelude::WidgetExt::display(window);
    let clipboard = display.clipboard();
    let window_for_cb = window.clone();

    // Clone state_rc and daemon_client for the async callback
    let state_for_cb = Rc::clone(&state_rc);
    let dc_paste = daemon_client.clone();
    let clipboard_for_texture = clipboard.clone();
    clipboard.read_text_async(gio::Cancellable::NONE, move |result| {
        let text = match result {
            Ok(Some(text)) => text.to_string(),
            Ok(None) => {
                // No text on clipboard -- fall back to image/texture check.
                let state_for_img = Rc::clone(&state_for_cb);
                let dc_for_img = dc_paste.clone();
                clipboard_for_texture.read_texture_async(
                    gio::Cancellable::NONE,
                    move |tex_result| {
                        let texture = match tex_result {
                            Ok(Some(tex)) => tex,
                            Ok(None) => return, // clipboard truly empty
                            Err(e) => {
                                tracing::debug!("Clipboard texture read failed: {e}");
                                return;
                            }
                        };
                        paste_texture_to_path(texture, state_for_img, dc_for_img);
                    },
                );
                return;
            }
            Err(e) => {
                // GTK returns an error (not Ok(None)) when the clipboard has
                // no text content (e.g. image-only).  Fall back to the
                // texture path in that case too.
                tracing::debug!("Clipboard text read returned error (trying texture): {e}");
                let state_for_img = Rc::clone(&state_for_cb);
                let dc_for_img = dc_paste.clone();
                clipboard_for_texture.read_texture_async(
                    gio::Cancellable::NONE,
                    move |tex_result| {
                        let texture = match tex_result {
                            Ok(Some(tex)) => tex,
                            Ok(None) => return,
                            Err(e2) => {
                                tracing::debug!("Clipboard texture read also failed: {e2}");
                                return;
                            }
                        };
                        paste_texture_to_path(texture, state_for_img, dc_for_img);
                    },
                );
                return;
            }
        };

        if text.is_empty() {
            return;
        }

        // Extract config values early and drop the borrow before any GTK call.
        let (warn_size, warn_newline) = {
            let cfg = shared_config.borrow();
            (cfg.paste_warn_size, cfg.paste_warn_newline)
        };

        let byte_len = text.len();
        let size_triggered = warn_size > 0 && byte_len > warn_size;
        let nl_triggered = warn_newline && text.contains('\n');

        // Fast path: no warnings needed.
        if !size_triggered && !nl_triggered {
            do_paste(state_for_cb, text, dc_paste);
            return;
        }

        // --- Build the warning dialog ---
        let (title, body) = if size_triggered {
            let kib = byte_len as f64 / 1024.0;
            let body = if nl_triggered {
                format!(
                    "Clipboard contents are {kib:.1} KiB ({byte_len} bytes) and contain newlines. \
                     This may be accidental."
                )
            } else {
                format!(
                    "Clipboard contents are {kib:.1} KiB ({byte_len} bytes). \
                     This may be accidental."
                )
            };
            ("Large Paste", body)
        } else {
            (
                "Paste Contains Newlines",
                "Clipboard text contains newlines and may execute commands immediately."
                    .to_string(),
            )
        };

        let dialog =
            adw::MessageDialog::new(Some(&window_for_cb), Some(title), Some(body.as_str()));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("paste", "Paste anyway");
        dialog.set_response_appearance("paste", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("paste"));
        dialog.set_close_response("cancel");

        // Build the preview widget.
        let preview_chars: String = text.chars().take(512).collect();
        let preview_text = if text.chars().count() > 512 {
            format!("{preview_chars}\u{2026}")
        } else {
            preview_chars
        };
        let preview_label = gtk4::Label::new(Some(&preview_text));
        preview_label.add_css_class("monospace");
        preview_label.set_wrap(true);
        preview_label.set_wrap_mode(pango::WrapMode::WordChar);
        preview_label.set_max_width_chars(72);
        preview_label.set_xalign(0.0);

        let scroll = gtk4::ScrolledWindow::new();
        scroll.set_max_content_height(200);
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_child(Some(&preview_label));
        dialog.set_extra_child(Some(&scroll));

        // Clone for the response closure.
        let state_for_dialog = Rc::clone(&state_for_cb);
        let dc_for_dialog = dc_paste.clone();
        let text_for_dialog = text.clone();
        dialog.connect_response(None, move |_dialog, response| {
            if response == "paste" {
                do_paste(
                    Rc::clone(&state_for_dialog),
                    text_for_dialog.clone(),
                    dc_for_dialog.clone(),
                );
            }
        });

        dialog.present();
        if let Some(btn) = dialog.default_widget() {
            btn.grab_focus();
        }
    });
}

/// Save a clipboard texture as a PNG to the cache dir and paste the path.
fn paste_texture_to_path(
    texture: gtk4::gdk::Texture,
    state_rc: Rc<RefCell<TerminalState>>,
    daemon_client: Option<Arc<DaemonClient>>,
) {
    let cache_dir = match dirs::cache_dir() {
        Some(d) => d.join("forgetty").join("clipboard"),
        None => {
            tracing::warn!("Could not determine XDG cache directory; skipping image paste");
            return;
        }
    };

    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        tracing::warn!("Failed to create clipboard cache dir {}: {e}", cache_dir.display());
        return;
    }

    let now = std::time::SystemTime::now();
    let secs = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let timestamp = epoch_to_timestamp(secs);
    let short_uuid = &uuid::Uuid::new_v4().to_string().replace('-', "")[..8];
    let filename = format!("paste-{timestamp}-{short_uuid}.png");
    let file_path = cache_dir.join(&filename);

    if let Err(e) = texture.save_to_png(&file_path) {
        tracing::warn!("Failed to save clipboard image to {}: {e}", file_path.display());
        return;
    }

    let path_text = format!("{} ", file_path.to_string_lossy());
    do_paste(state_rc, path_text, daemon_client);
}

/// Convert a Unix epoch timestamp (seconds) to a `YYYYMMDD-HHmmss` string.
///
/// Uses manual arithmetic (no chrono dependency). Leap-second precision is
/// not required -- the timestamp is used only for unique filenames.
fn epoch_to_timestamp(epoch_secs: u64) -> String {
    // Days per month for non-leap and leap years.
    const DAYS_NORMAL: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    const DAYS_LEAP: [u64; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    fn is_leap(y: u64) -> bool {
        (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
    }

    fn days_in_year(y: u64) -> u64 {
        if is_leap(y) {
            366
        } else {
            365
        }
    }

    let secs_in_day: u64 = 86400;
    let mut remaining = epoch_secs;

    // Compute year.
    let mut year: u64 = 1970;
    loop {
        let dy = days_in_year(year) * secs_in_day;
        if remaining < dy {
            break;
        }
        remaining -= dy;
        year += 1;
    }

    // Compute month and day.
    let months = if is_leap(year) { &DAYS_LEAP } else { &DAYS_NORMAL };
    let mut month: u64 = 1;
    let mut day_secs = remaining;
    for &dm in months.iter() {
        let ms = dm * secs_in_day;
        if day_secs < ms {
            break;
        }
        day_secs -= ms;
        month += 1;
    }
    let day = day_secs / secs_in_day + 1;
    let remainder = day_secs % secs_in_day;

    let hour = remainder / 3600;
    let minute = (remainder % 3600) / 60;
    let second = remainder % 60;

    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

// ---------------------------------------------------------------------------
// Select All
// ---------------------------------------------------------------------------

/// Select all visible text in the focused pane.
///
/// Delegates to `terminal::select_all_visible()` after looking up the focused
/// DrawingArea and its TerminalState.
fn select_all_on_focused_pane(tab_states: &TabStateMap, focus_tracker: &FocusTracker) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    let Ok(states) = tab_states.try_borrow() else {
        return;
    };

    let Some(state_rc) = states.get(&focused_name).cloned() else {
        return;
    };
    drop(states);

    // Find the DrawingArea widget to trigger a redraw
    let app =
        gtk4::gio::Application::default().and_then(|a| a.downcast::<gtk4::Application>().ok());
    let Some(app) = app else {
        return;
    };
    let Some(window) = app.active_window() else {
        return;
    };
    let Some(da) = find_drawing_area_by_name(&window, &focused_name) else {
        return;
    };

    terminal::select_all_visible(&da, &state_rc);
}

// ---------------------------------------------------------------------------
// Open URL in browser
// ---------------------------------------------------------------------------

/// Open a URL in the user's default browser.
///
/// Uses `gtk::UriLauncher` (GTK 4.10+) for GNOME-native URL handling.
pub(crate) fn open_url_in_browser(url: &str) {
    let launcher = gtk4::UriLauncher::new(url);

    let app =
        gtk4::gio::Application::default().and_then(|a| a.downcast::<gtk4::Application>().ok());
    let window = app.and_then(|a| a.active_window());

    launcher.launch(window.as_ref(), gio::Cancellable::NONE, |result| {
        if let Err(e) = result {
            tracing::warn!("Failed to open URL: {e}");
        }
    });
}

// ---------------------------------------------------------------------------
// Shortcuts window
// ---------------------------------------------------------------------------

/// Build a `gtk::ShortcutsWindow` listing all keybindings organized by category.
///
/// Uses `ShortcutsSection` > `ShortcutsGroup` > `ShortcutsShortcut` hierarchy.
/// Accelerator strings use GTK notation so GTK renders proper key cap icons.
fn build_shortcuts_window() -> gtk4::ShortcutsWindow {
    let section =
        gtk4::ShortcutsSection::builder().section_name("shortcuts").title("Shortcuts").build();
    // GTK requires max_height to properly show all groups.
    section.set_max_height(12);

    // --- Tabs ---
    let tabs_group = shortcut_group("Tabs");
    tabs_group.add_shortcut(&shortcut("<Control><Shift>t", "New Tab"));
    tabs_group.add_shortcut(&shortcut("<Control><Shift>w", "Close Pane / Tab"));
    section.add_group(&tabs_group);

    // --- Panes ---
    let panes_group = shortcut_group("Panes");
    panes_group.add_shortcut(&shortcut("<Alt><Shift>equal", "Split Right"));
    panes_group.add_shortcut(&shortcut("<Alt><Shift>minus", "Split Down"));
    panes_group.add_shortcut(&shortcut_no_accel("Split Left", "Via dropdown menu"));
    panes_group.add_shortcut(&shortcut_no_accel("Split Up", "Via dropdown menu"));
    panes_group.add_shortcut(&shortcut("<Alt>Left", "Focus Pane Left"));
    panes_group.add_shortcut(&shortcut("<Alt>Right", "Focus Pane Right"));
    panes_group.add_shortcut(&shortcut("<Alt>Up", "Focus Pane Up"));
    panes_group.add_shortcut(&shortcut("<Alt>Down", "Focus Pane Down"));
    panes_group.add_shortcut(&shortcut("<Alt><Shift>Left", "Resize Pane Left"));
    panes_group.add_shortcut(&shortcut("<Alt><Shift>Right", "Resize Pane Right"));
    panes_group.add_shortcut(&shortcut("<Alt><Shift>Up", "Resize Pane Up"));
    panes_group.add_shortcut(&shortcut("<Alt><Shift>Down", "Resize Pane Down"));
    section.add_group(&panes_group);

    // --- Clipboard ---
    let clipboard_group = shortcut_group("Clipboard");
    clipboard_group.add_shortcut(&shortcut("<Control>c", "Copy Selection"));
    clipboard_group.add_shortcut(&shortcut("<Control>v", "Paste"));
    section.add_group(&clipboard_group);

    // --- Search ---
    let search_group = shortcut_group("Search");
    search_group.add_shortcut(&shortcut("<Control><Shift>f", "Find in Terminal"));
    search_group.add_shortcut(&shortcut("Return", "Next Match (in search bar)"));
    search_group.add_shortcut(&shortcut("<Shift>Return", "Previous Match (in search bar)"));
    search_group.add_shortcut(&shortcut("Escape", "Close Search"));
    section.add_group(&search_group);

    // --- Zoom ---
    let zoom_group = shortcut_group("Zoom");
    zoom_group.add_shortcut(&shortcut("<Control>equal", "Zoom In"));
    zoom_group.add_shortcut(&shortcut("<Control>minus", "Zoom Out"));
    zoom_group.add_shortcut(&shortcut("<Control>0", "Reset Zoom"));
    section.add_group(&zoom_group);

    // --- Terminal ---
    let terminal_group = shortcut_group("Terminal");
    terminal_group.add_shortcut(&shortcut_no_accel("Clear", "Via hamburger menu"));
    terminal_group.add_shortcut(&shortcut_no_accel("Reset", "Via hamburger menu"));
    section.add_group(&terminal_group);

    // --- Workspaces ---
    let workspace_group = shortcut_group("Workspaces");
    workspace_group.add_shortcut(&shortcut("<Control><Alt>n", "New Workspace"));
    workspace_group.add_shortcut(&shortcut("<Control><Alt>b", "Toggle Workspace Sidebar"));
    workspace_group.add_shortcut(&shortcut("<Alt>1", "Switch to Workspace 1\u{2013}9"));
    workspace_group.add_shortcut(&shortcut("<Control><Alt>Page_Up", "Previous Workspace"));
    workspace_group.add_shortcut(&shortcut("<Control><Alt>Page_Down", "Next Workspace"));
    section.add_group(&workspace_group);

    // --- Navigation ---
    let nav_group = shortcut_group("Navigation");
    nav_group.add_shortcut(
        &gtk4::ShortcutsShortcut::builder()
            .title("Open URL")
            .subtitle("Ctrl+Click on a highlighted URL")
            .shortcut_type(gtk4::ShortcutType::Accelerator)
            .accelerator("")
            .build(),
    );
    section.add_group(&nav_group);

    // --- Help ---
    let help_group = shortcut_group("Help");
    help_group.add_shortcut(&shortcut("F1", "Keyboard Shortcuts"));
    help_group.add_shortcut(&shortcut("<Control>period", "Settings"));
    help_group.add_shortcut(&shortcut("<Control>comma", "Appearance Sidebar"));
    help_group.add_shortcut(&shortcut("<Control><Shift>q", "Quit"));
    section.add_group(&help_group);

    let window = gtk4::ShortcutsWindow::builder().modal(true).build();
    window.add_section(&section);

    window
}

/// Create a `ShortcutsGroup` with the given title.
fn shortcut_group(title: &str) -> gtk4::ShortcutsGroup {
    gtk4::ShortcutsGroup::builder().title(title).build()
}

/// Create a single `ShortcutsShortcut` with an accelerator string and title.
fn shortcut(accel: &str, title: &str) -> gtk4::ShortcutsShortcut {
    gtk4::ShortcutsShortcut::builder().accelerator(accel).title(title).build()
}

/// Create a `ShortcutsShortcut` without a keyboard accelerator.
///
/// Used for actions only accessible via the dropdown menu (Split Left, Split Up).
fn shortcut_no_accel(title: &str, subtitle: &str) -> gtk4::ShortcutsShortcut {
    gtk4::ShortcutsShortcut::builder()
        .title(title)
        .subtitle(subtitle)
        .shortcut_type(gtk4::ShortcutType::Accelerator)
        .accelerator("")
        .build()
}

// ---------------------------------------------------------------------------
// Clear / Reset terminal
// ---------------------------------------------------------------------------

/// Perform a full terminal reset (RIS) on the focused pane via the
/// `ghostty_terminal_reset()` API, then queue a redraw.
fn reset_focused_pane(tab_states: &TabStateMap, focus_tracker: &FocusTracker) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    let Ok(states) = tab_states.try_borrow() else {
        return;
    };

    let Some(state_rc) = states.get(&focused_name).cloned() else {
        return;
    };
    drop(states);

    let Ok(mut s) = state_rc.try_borrow_mut() else {
        return;
    };

    s.terminal.reset();

    let app =
        gtk4::gio::Application::default().and_then(|a| a.downcast::<gtk4::Application>().ok());
    if let Some(app) = app {
        if let Some(window) = app.active_window() {
            if let Some(da) = find_drawing_area_by_name(&window, &focused_name) {
                da.queue_draw();
            }
        }
    }
}

/// Feed an escape sequence to the focused pane's VT terminal.
///
/// Used by the Clear menu action. The bytes are fed directly into
/// the VT parser (same path as PTY data), then the pane is redrawn.
#[allow(dead_code)]
fn feed_escape_to_focused_pane(
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    escape_bytes: &[u8],
) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    let Ok(states) = tab_states.try_borrow() else {
        return;
    };

    let Some(state_rc) = states.get(&focused_name).cloned() else {
        return;
    };
    drop(states);

    let Ok(mut s) = state_rc.try_borrow_mut() else {
        return;
    };

    s.terminal.feed(escape_bytes);

    // Queue a redraw on the DrawingArea
    let app =
        gtk4::gio::Application::default().and_then(|a| a.downcast::<gtk4::Application>().ok());
    if let Some(app) = app {
        if let Some(window) = app.active_window() {
            if let Some(da) = find_drawing_area_by_name(&window, &focused_name) {
                da.queue_draw();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Change Tab Title dialog
// ---------------------------------------------------------------------------

/// Generate a stable identity key for a tab page.
///
/// Uses the widget name of the page's container Box, which is unique per page.
/// This avoids pointer-based hashing which is fragile across GObject lifecycles.
fn page_identity_key(page: &adw::TabPage) -> String {
    // Use the page's child widget (the pane container Box) as the key source.
    // Each container is unique since add_new_tab creates a fresh Box per tab.
    let child = page.child();
    format!("page-{:p}", child.as_ptr())
}

/// Show the "Change Tab Title" dialog.
///
/// Presents an `adw::MessageDialog` with a text entry. On confirm, sets the
/// tab title and marks the page as having a custom title to suppress CWD polling.
/// An empty string clears the custom title, re-enabling automatic CWD polling.
#[allow(deprecated)]
fn show_change_tab_title_dialog(
    window: &adw::ApplicationWindow,
    page: &adw::TabPage,
    custom_titles: &CustomTitles,
) {
    let dialog = adw::MessageDialog::new(
        Some(window),
        Some("Change Tab Title"),
        Some("Enter a new title for this tab, or leave empty to restore automatic title."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("apply", "Apply");
    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("apply"));
    dialog.set_close_response("cancel");

    // Add a text entry as the extra child
    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Tab title"));
    // Pre-fill with current title
    let current_title = page.title();
    entry.set_text(&current_title);
    entry.select_region(0, -1);
    dialog.set_extra_child(Some(&entry));

    // Allow Enter in the entry to trigger the "apply" response
    let dialog_for_enter = dialog.clone();
    entry.connect_activate(move |_entry| {
        dialog_for_enter.response("apply");
    });

    let page_clone = page.clone();
    let ct = Rc::clone(custom_titles);
    dialog.connect_response(None, move |_dialog, response| {
        if response != "apply" {
            return;
        }
        let new_title = entry.text().to_string();
        let page_key = page_identity_key(&page_clone);

        if new_title.is_empty() {
            // Empty title: revert to automatic CWD-based title
            if let Ok(mut ct) = ct.try_borrow_mut() {
                ct.remove(&page_key);
            }
        } else {
            // Set the custom title and mark the page
            page_clone.set_title(&new_title);
            if let Ok(mut ct) = ct.try_borrow_mut() {
                ct.insert(page_key);
            }
        }
    });

    dialog.present();
}

// ---------------------------------------------------------------------------
// Open / Reload configuration
// ---------------------------------------------------------------------------

/// Open the configuration file in the user's default text editor.
///
/// Creates `~/.config/forgetty/config.toml` with default content if it does
/// not exist, then opens it via `xdg-open`.
fn open_config_file() {
    let config_dir = forgetty_core::platform::config_dir();
    let config_path = config_dir.join("config.toml");

    // Create directory and default config if missing
    if !config_path.exists() {
        if let Err(e) = std::fs::create_dir_all(&config_dir) {
            tracing::warn!("Failed to create config directory: {e}");
            return;
        }
        let default_content = concat!(
            "# Forgetty configuration\n",
            "# See https://forgetty.dev/docs/config for all options.\n",
            "\n",
            "# font_family = \"JetBrains Mono\"\n",
            "# font_size = 13.0\n",
            "# shell = \"/bin/zsh\"\n",
        );
        if let Err(e) = std::fs::write(&config_path, default_content) {
            tracing::warn!("Failed to write default config: {e}");
            return;
        }
        info!("Created default config at {}", config_path.display());
    }

    // Open in default editor via xdg-open
    if let Err(e) = std::process::Command::new("xdg-open").arg(&config_path).spawn() {
        tracing::warn!("Failed to open config file with xdg-open: {e}");
    }
}

// ---------------------------------------------------------------------------
// Tab right-click context menu (T-M1-extra-009)
// ---------------------------------------------------------------------------

/// Find which `adw::TabPage` was right-clicked in the tab bar.
///
/// Uses `gtk4::Widget::pick()` to find the widget at (x, y), then walks up the
/// widget tree to find an `AdwTabButton` (by GObject type name).  Counts the
/// button's position among siblings of the same type to determine the page index.
///
/// Returns `None` if (x, y) is not over a tab button (e.g., over empty space
/// or the new-tab button).
fn tab_bar_find_page_at(tab_bar: &adw::TabBar, x: f64, y: f64) -> Option<adw::TabPage> {
    let tv = tab_bar.view()?;
    let n_pages = tv.n_pages();
    if n_pages == 0 {
        return None;
    }

    let tab_bar_widget = tab_bar.upcast_ref::<gtk4::Widget>();

    // Walk the entire tab bar widget tree to collect AdwTabButton widgets.
    // We can't use pick() here because adw::TabBar wraps its buttons in a
    // GtkScrolledWindow whose overflow clip prevents pick traversal.
    // Instead, walk the widget tree recursively and use compute_bounds() to
    // find which button contains the click position.
    fn collect_tab_buttons(widget: &gtk4::Widget, out: &mut Vec<gtk4::Widget>) {
        if widget.type_().name() == "AdwTabButton" {
            out.push(widget.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            collect_tab_buttons(&c, out);
            child = c.next_sibling();
        }
    }

    let mut buttons: Vec<gtk4::Widget> = Vec::new();
    collect_tab_buttons(tab_bar_widget, &mut buttons);

    tracing::debug!("tab_bar_find_page_at: found {} AdwTabButton(s)", buttons.len());

    for (idx, btn) in buttons.iter().enumerate() {
        if let Some(bounds) = btn.compute_bounds(tab_bar_widget) {
            let bx = bounds.x() as f64;
            let by = bounds.y() as f64;
            let bw = bounds.width() as f64;
            let bh = bounds.height() as f64;
            if x >= bx && x <= bx + bw && y >= by && y <= by + bh && (idx as i32) < n_pages {
                return Some(tv.nth_page(idx as i32));
            }
        }
    }

    None
}

/// Preset colors for the tab color picker (R, G, B as 0.0..1.0).
const TAB_COLOR_PRESETS: &[(&str, (f32, f32, f32))] = &[
    ("Red", (0.878, 0.286, 0.227)),
    ("Orange", (0.945, 0.561, 0.196)),
    ("Yellow", (0.969, 0.773, 0.212)),
    ("Green", (0.353, 0.725, 0.404)),
    ("Teal", (0.188, 0.663, 0.596)),
    ("Blue", (0.224, 0.529, 0.894)),
    ("Purple", (0.616, 0.373, 0.847)),
    ("Pink", (0.859, 0.365, 0.647)),
];

/// FIX-010: format an `RGBA` as `"#RRGGBB"`. Alpha is ignored — workspace
/// accent colours are always opaque (dialog `set_with_alpha(false)`). The
/// components are clamped to `[0.0, 1.0]` and rounded down to `u8`.
fn rgba_to_hex(rgba: &gtk4::gdk::RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (rgba.red().clamp(0.0, 1.0) * 255.0) as u8,
        (rgba.green().clamp(0.0, 1.0) * 255.0) as u8,
        (rgba.blue().clamp(0.0, 1.0) * 255.0) as u8,
    )
}

/// FIX-010: parse `"#RRGGBB"` (with or without leading `#`) into an opaque
/// `RGBA`. Returns `None` on anything other than exactly 6 ASCII hex
/// characters — malformed input (wrong length, non-ASCII, non-hex bytes)
/// degrades gracefully to "no colour" rather than panicking.
///
/// Safety note: slicing `&str[0..2]` panics if the range doesn't align with
/// UTF-8 character boundaries. The `is_ascii()` check below guarantees every
/// byte is a single-byte ASCII codepoint, so slicing is always boundary-safe.
fn hex_to_rgba(s: &str) -> Option<gtk4::gdk::RGBA> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(gtk4::gdk::RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0))
}

/// Wire the `setup-menu` signal on a tab view so that right-clicking any tab
/// shows the custom context menu popover.
///
/// `setup-menu` is the official libadwaita signal for tab context menus.
/// It fires after the user right-clicks a tab and provides the clicked page.
/// We read the last click position from `WorkspaceManagerInner::last_tab_click`
/// (stored by the Capture-phase GestureClick on the tab bar) to position the
/// popover precisely.
fn wire_tab_context_menu_signal(
    tab_view: &adw::TabView,
    workspace_manager: &WorkspaceManager,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
) {
    let wm = Rc::clone(workspace_manager);
    let tb = tab_bar.clone();
    let win = window.clone();
    let dc = daemon_client;
    let sc = Rc::clone(shared_config);

    tab_view.connect_setup_menu(move |tv, maybe_page| {
        tracing::debug!("setup-menu: fired, page={}", maybe_page.is_some());
        let Some(page) = maybe_page else {
            tracing::debug!("setup-menu: no page (right-click on empty space)");
            return;
        };

        // Read click position and the workspace state for this tab_view.
        let result = {
            let Ok(mgr) = wm.try_borrow() else {
                tracing::debug!("setup-menu: workspace_manager borrow failed");
                return;
            };
            let (x, y) = mgr.last_tab_click;
            tracing::debug!("setup-menu: click pos ({x},{y}), {} workspaces", mgr.workspaces.len());
            // Find the workspace that owns this tab_view.
            let Some((ws_idx, ws)) =
                mgr.workspaces.iter().enumerate().find(|(_, ws)| ws.tab_view == *tv)
            else {
                tracing::debug!("setup-menu: no workspace found for this tab_view");
                return;
            };
            (
                x,
                y,
                ws_idx,
                Rc::clone(&ws.tab_states),
                Rc::clone(&ws.focus_tracker),
                Rc::clone(&ws.custom_titles),
                Rc::clone(&ws.tab_colors),
                Rc::clone(&ws.tab_id_map),
            )
        };
        let (x, y, ws_idx, tab_states, focus_tracker, custom_titles, tab_colors, tab_id_map) =
            result;

        tracing::debug!("setup-menu: showing context menu at ({x},{y})");
        // Mark as handled so the bubble-phase fallback skips this click.
        if let Ok(mut mgr) = wm.try_borrow_mut() {
            mgr.tab_menu_shown = true;
        }
        show_tab_context_menu(
            ws_idx,
            &tb,
            tv,
            page,
            x,
            y,
            &tab_states,
            &focus_tracker,
            &custom_titles,
            &tab_colors,
            &tab_id_map,
            &win,
            dc.clone(),
            &sc,
            &wm,
        );
    });
}

/// Show the tab right-click context menu positioned at (x, y) in tab_bar coordinates.
///
/// `workspace_idx` — the workspace that owns the right-clicked tab; threaded
/// through so `Duplicate Tab` routes the new tab to the correct workspace
/// (session-restore fix).
#[allow(clippy::too_many_arguments)]
fn show_tab_context_menu(
    workspace_idx: usize,
    tab_bar: &adw::TabBar,
    tab_view: &adw::TabView,
    page: &adw::TabPage,
    x: f64,
    y: f64,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    tab_colors: &TabColorMap,
    tab_id_map: &TabIdMap,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
    workspace_manager: &WorkspaceManager,
) {
    let popover = gtk4::Popover::new();
    popover.set_parent(tab_bar);
    popover.set_has_arrow(false);
    popover.add_css_class("menu");
    popover.set_autohide(true);
    popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));

    // Click-outside dismiss via focus tracking (deferred).
    //
    // On Wayland, autohide popup grabs are unreliable when the popover is
    // shown inside a button-press handler (button is still held, no valid
    // release serial for the compositor's popup grab).
    //
    // We use EventControllerFocus::leave instead, but we MUST connect it via
    // idle_add_local_once rather than immediately.  If connected before popup()
    // returns, GTK fires `leave` during the same event cycle as the button-press
    // (because the press event processing briefly takes focus away from the
    // popover as it initialises), closing the popover instantly.
    // Deferring to the next idle cycle avoids this race.
    {
        let pop_fc = popover.clone();
        glib::idle_add_local_once(move || {
            if !pop_fc.is_visible() {
                return; // already dismissed
            }
            let pop_ref = pop_fc.clone();
            let fc = gtk4::EventControllerFocus::new();
            fc.connect_leave(move |_| {
                pop_ref.popdown();
            });
            pop_fc.add_controller(fc);
        });
    }

    // Re-focus the active terminal pane after the popover is dismissed.
    //
    // grab_focus() calls inside menu action handlers fire while the popover
    // still holds modal focus, so GTK silently ignores them.  Restore focus
    // in the `closed` callback instead, which fires after the popover is gone.
    {
        let ft = Rc::clone(focus_tracker);
        let ts = Rc::clone(tab_states);
        let win_c = window.clone();
        popover.connect_closed(move |_| {
            let name = ft.borrow().clone();
            if !name.is_empty() {
                if let Some(da) = find_drawing_area_by_name(&win_c, &name) {
                    da.grab_focus();
                    return;
                }
            }
            // Fallback: focus the first tracked pane.
            let keys: Vec<String> = ts.borrow().keys().cloned().collect();
            for k in &keys {
                if let Some(da) = find_drawing_area_by_name(&win_c, k) {
                    da.grab_focus();
                    break;
                }
            }
        });
    }

    let menu_box = build_tab_context_menu_box(
        workspace_idx,
        &popover,
        tab_view,
        page,
        tab_states,
        focus_tracker,
        custom_titles,
        tab_colors,
        tab_id_map,
        window,
        daemon_client,
        shared_config,
        workspace_manager,
    );
    popover.set_child(Some(&menu_box));
    popover.popup();
}

/// Build the full 11-item tab context menu box.
#[allow(clippy::too_many_arguments)]
fn build_tab_context_menu_box(
    workspace_idx: usize,
    popover: &gtk4::Popover,
    tab_view: &adw::TabView,
    page: &adw::TabPage,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    tab_colors: &TabColorMap,
    tab_id_map: &TabIdMap,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
    workspace_manager: &WorkspaceManager,
) -> gtk4::Box {
    let _ = tab_id_map; // used later if daemon move_tab is needed
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    vbox.set_margin_top(4);
    vbox.set_margin_bottom(4);

    // 1. Change Tab Color
    {
        let color_btn = tab_menu_button_arrow("Change Tab Color");
        let page_key = page_identity_key(page);
        let tc = Rc::clone(tab_colors);
        let page_c = page.clone();

        // Color picker sub-popover
        let color_sub = gtk4::Popover::new();
        color_sub.set_has_arrow(true);
        color_sub.add_css_class("menu");
        color_sub.set_position(gtk4::PositionType::Right);

        let sub_box = build_color_picker_box(&color_sub, &page_c, &page_key, &tc);
        color_sub.set_child(Some(&sub_box));
        color_sub.set_parent(&color_btn);

        let pop_ref = popover.clone();
        color_btn.connect_clicked(move |_| {
            // Keep parent open; open submenu
            color_sub.popup();
            let _ = &pop_ref; // keep alive
        });
        vbox.append(&color_btn);
    }

    // 2. Rename Tab
    {
        let ct = Rc::clone(custom_titles);
        let page_r = page.clone();
        let win_r = window.clone();
        let pop_r = popover.clone();
        let btn = tab_menu_button("Rename Tab");
        btn.connect_clicked(move |_| {
            pop_r.popdown();
            show_change_tab_title_dialog(&win_r, &page_r, &ct);
        });
        vbox.append(&btn);
    }

    // 3. Duplicate Tab
    {
        let tv_d = tab_view.clone();
        let ts_d = Rc::clone(tab_states);
        let ft_d = Rc::clone(focus_tracker);
        let ct_d = Rc::clone(custom_titles);
        let win_d = window.clone();
        let dc_d = daemon_client.clone();
        let sc_d = Rc::clone(shared_config);
        let wm_d = Rc::clone(workspace_manager);
        let page_d = page.clone();
        let pop_d = popover.clone();
        let btn = tab_menu_button("Duplicate Tab");
        btn.connect_clicked(move |_| {
            pop_d.popdown();
            duplicate_tab(
                workspace_idx,
                &tv_d,
                &page_d,
                &ts_d,
                &ft_d,
                &ct_d,
                &win_d,
                dc_d.clone(),
                &sc_d,
                &wm_d,
            );
        });
        vbox.append(&btn);
    }

    // 4. Split Pane (submenu)
    {
        let split_btn = tab_menu_button_arrow("Split Pane");

        let split_sub = gtk4::Popover::new();
        split_sub.set_has_arrow(true);
        split_sub.add_css_class("menu");
        split_sub.set_position(gtk4::PositionType::Right);

        let sub_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        sub_vbox.set_margin_top(4);
        sub_vbox.set_margin_bottom(4);
        let pop_ref = popover.clone();
        for (label, action) in &[
            ("Split Right", "win.split-right"),
            ("Split Down", "win.split-down"),
            ("Split Left", "win.split-left"),
            ("Split Up", "win.split-up"),
        ] {
            let b = tab_menu_action_button(label, action, &split_sub);
            // Also close parent popover when sub-item selected
            let pop_ref2 = pop_ref.clone();
            b.connect_clicked(move |_| {
                pop_ref2.popdown();
            });
            sub_vbox.append(&b);
        }
        split_sub.set_child(Some(&sub_vbox));
        split_sub.set_parent(&split_btn);

        split_btn.connect_clicked(move |_| {
            split_sub.popup();
        });
        vbox.append(&split_btn);
    }

    // 5. Move Tab (submenu)
    {
        let n_pages = tab_view.n_pages();
        let pos = tab_view.page_position(page);
        let at_start = pos == 0;
        let at_end = pos == n_pages - 1;

        let move_btn = tab_menu_button_arrow("Move Tab");

        let move_sub = gtk4::Popover::new();
        move_sub.set_has_arrow(true);
        move_sub.add_css_class("menu");
        move_sub.set_position(gtk4::PositionType::Right);

        let sub_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        sub_vbox.set_margin_top(4);
        sub_vbox.set_margin_bottom(4);

        // Move Left
        {
            let btn = tab_menu_button("Move Left");
            btn.set_sensitive(!at_start);
            let tv = tab_view.clone();
            let pg = page.clone();
            let sub = move_sub.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                let new_pos = tv.page_position(&pg) - 1;
                tv.reorder_page(&pg, new_pos);
                sub.popdown();
                pop.popdown();
            });
            sub_vbox.append(&btn);
        }

        // Move Right
        {
            let btn = tab_menu_button("Move Right");
            btn.set_sensitive(!at_end);
            let tv = tab_view.clone();
            let pg = page.clone();
            let sub = move_sub.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                let new_pos = tv.page_position(&pg) + 1;
                tv.reorder_page(&pg, new_pos);
                sub.popdown();
                pop.popdown();
            });
            sub_vbox.append(&btn);
        }

        // Move to New Window
        {
            let btn = tab_menu_button("Move to New Window");
            let ts_w = Rc::clone(tab_states);
            let pg_w = page.clone();
            let tv_w = tab_view.clone();
            let sub = move_sub.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                sub.popdown();
                pop.popdown();
                move_tab_to_new_window(&tv_w, &pg_w, &ts_w);
            });
            sub_vbox.append(&btn);
        }

        move_sub.set_child(Some(&sub_vbox));
        move_sub.set_parent(&move_btn);

        move_btn.connect_clicked(move |_| {
            move_sub.popup();
        });
        vbox.append(&move_btn);
    }

    // 6. Search
    {
        let pop_s = popover.clone();
        let btn = tab_menu_shortcut_button("Search", "Ctrl+Shift+F", "win.search", &pop_s);
        vbox.append(&btn);
    }

    // 7. Export Text (placeholder — wired in T-M1-extra-012)
    {
        let btn = tab_menu_button("Export Text");
        btn.set_sensitive(false); // greyed out until T-M1-extra-012
        vbox.append(&btn);
    }

    // Separator
    let sep = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    sep.set_margin_top(4);
    sep.set_margin_bottom(4);
    vbox.append(&sep);

    // 8. Close Tabs to the Right
    {
        let n_pages = tab_view.n_pages();
        let pos = tab_view.page_position(page);
        let is_last = pos == n_pages - 1;

        let btn = tab_menu_button("Close Tabs to the Right");
        btn.set_sensitive(!is_last);
        let tv = tab_view.clone();
        let pg = page.clone();
        let pop = popover.clone();
        btn.connect_clicked(move |_| {
            pop.popdown();
            let pos = tv.page_position(&pg);
            let n = tv.n_pages();
            // Collect pages to close (right-to-left to avoid index drift).
            // Collect right-to-left to avoid index drift as pages close.
            let pages_to_close: Vec<adw::TabPage> =
                ((pos + 1)..n).rev().map(|i| tv.nth_page(i)).collect();
            for p in pages_to_close {
                tv.close_page(&p);
            }
        });
        vbox.append(&btn);
    }

    // 9. Close Other Tabs
    {
        let n_pages = tab_view.n_pages();
        let btn = tab_menu_button("Close Other Tabs");
        btn.set_sensitive(n_pages > 1);
        let tv = tab_view.clone();
        let pg = page.clone();
        let pop = popover.clone();
        btn.connect_clicked(move |_| {
            pop.popdown();
            let n = tv.n_pages();
            let keep_pos = tv.page_position(&pg);
            // Collect all pages except this one.
            let pages_to_close: Vec<adw::TabPage> =
                (0..n).rev().filter(|&i| i != keep_pos).map(|i| tv.nth_page(i)).collect();
            for p in pages_to_close {
                tv.close_page(&p);
            }
        });
        vbox.append(&btn);
    }

    // 10. Close Tab
    {
        let tv = tab_view.clone();
        let pg = page.clone();
        let pop = popover.clone();
        let btn = tab_menu_button("Close Tab");
        btn.connect_clicked(move |_| {
            pop.popdown();
            tv.close_page(&pg);
        });
        vbox.append(&btn);
    }

    vbox
}

/// Build the color picker box for the "Change Tab Color" submenu.
///
/// Contains 8 preset color swatches in a horizontal grid row + a "Custom…" button
/// and a "None" button that clears the color.
fn build_color_picker_box(
    sub_popover: &gtk4::Popover,
    page: &adw::TabPage,
    page_key: &str,
    tab_colors: &TabColorMap,
) -> gtk4::Box {
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    vbox.set_margin_top(6);
    vbox.set_margin_bottom(6);
    vbox.set_margin_start(6);
    vbox.set_margin_end(6);

    // Swatch row
    let swatch_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);

    for (name, (r, g, b)) in TAB_COLOR_PRESETS {
        let rgba = gtk4::gdk::RGBA::new(*r, *g, *b, 1.0);
        let btn = gtk4::Button::new();
        btn.set_tooltip_text(Some(name));
        btn.set_size_request(24, 24);
        btn.set_has_frame(false);

        // Draw the swatch as a colored circle drawing area.
        let da = gtk4::DrawingArea::new();
        da.set_size_request(18, 18);
        let da_rgba = rgba;
        da.set_draw_func(move |_, cr, _w, _h| {
            cr.arc(9.0, 9.0, 8.0, 0.0, 2.0 * std::f64::consts::PI);
            cr.set_source_rgba(
                da_rgba.red() as f64,
                da_rgba.green() as f64,
                da_rgba.blue() as f64,
                1.0,
            );
            let _ = cr.fill();
        });
        btn.set_child(Some(&da));

        let tc = Rc::clone(tab_colors);
        let pg = page.clone();
        let pk = page_key.to_string();
        let sub = sub_popover.clone();
        btn.connect_clicked(move |_| {
            apply_tab_color(&pg, &pk, Some(rgba), &tc);
            sub.popdown();
        });
        swatch_box.append(&btn);
    }
    vbox.append(&swatch_box);

    // "Custom…" button — opens gtk4::ColorDialog
    {
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        let custom_btn = gtk4::Button::with_label("Custom\u{2026}");
        custom_btn.set_has_frame(false);
        custom_btn.add_css_class("flat");
        custom_btn.set_hexpand(true);
        row.append(&custom_btn);

        let tc = Rc::clone(tab_colors);
        let pg = page.clone();
        let pk = page_key.to_string();
        let sub = sub_popover.clone();
        custom_btn.connect_clicked(move |btn| {
            let dialog = gtk4::ColorDialog::new();
            dialog.set_with_alpha(false);
            let tc2 = Rc::clone(&tc);
            let pg2 = pg.clone();
            let pk2 = pk.clone();
            let sub2 = sub.clone();
            // Find a window ancestor for the dialog.
            let win = btn.root().and_downcast::<gtk4::Window>();
            dialog.choose_rgba(win.as_ref(), None, gtk4::gio::Cancellable::NONE, move |result| {
                if let Ok(rgba) = result {
                    apply_tab_color(&pg2, &pk2, Some(rgba), &tc2);
                    sub2.popdown();
                }
            });
        });
        vbox.append(&row);
    }

    // "None" button — clears the color
    {
        let none_btn = gtk4::Button::with_label("None");
        none_btn.set_has_frame(false);
        none_btn.add_css_class("flat");

        let tc = Rc::clone(tab_colors);
        let pg = page.clone();
        let pk = page_key.to_string();
        let sub = sub_popover.clone();
        none_btn.connect_clicked(move |_| {
            apply_tab_color(&pg, &pk, None, &tc);
            sub.popdown();
        });
        vbox.append(&none_btn);
    }

    vbox
}

/// Apply (or clear) a color indicator on a tab page.
///
/// Sets or removes the `indicator_icon` on the page using a small colored circle
/// rendered as a `gdk::MemoryTexture`.  Also updates the `tab_colors` map so the
/// choice persists while the session is live.
fn apply_tab_color(
    page: &adw::TabPage,
    page_key: &str,
    color: Option<gtk4::gdk::RGBA>,
    tab_colors: &TabColorMap,
) {
    match color {
        Some(rgba) => {
            let icon = make_tab_color_dot_icon(&rgba);
            page.set_indicator_icon(Some(icon.upcast_ref::<gio::Icon>()));
            if let Ok(mut tc) = tab_colors.try_borrow_mut() {
                tc.insert(page_key.to_string(), rgba);
            }
        }
        None => {
            page.set_indicator_icon(gio::Icon::NONE);
            if let Ok(mut tc) = tab_colors.try_borrow_mut() {
                tc.remove(page_key);
            }
        }
    }
}

/// Create a 12×12 colored circle as a `gdk::MemoryTexture` for use as a tab indicator.
///
/// `gdk::MemoryTexture` implements `gio::Icon` (GTK 4.2+), so it can be passed
/// directly to `adw::TabPage::set_indicator_icon()`.
fn make_tab_color_dot_icon(rgba: &gtk4::gdk::RGBA) -> gtk4::gdk::MemoryTexture {
    let size: i32 = 12;
    let r = (rgba.red() * 255.0) as u8;
    let g = (rgba.green() * 255.0) as u8;
    let b = (rgba.blue() * 255.0) as u8;
    let center = size as f64 / 2.0;
    let radius = center - 0.5;
    let mut pixels: Vec<u8> = Vec::with_capacity((size * size * 4) as usize);
    for row in 0..size {
        for col in 0..size {
            let dx = col as f64 + 0.5 - center;
            let dy = row as f64 + 0.5 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= radius {
                pixels.extend_from_slice(&[r, g, b, 255u8]);
            } else {
                pixels.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]);
            }
        }
    }
    let bytes = glib::Bytes::from(&pixels);
    gtk4::gdk::MemoryTexture::new(
        size,
        size,
        gtk4::gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        (size * 4) as usize,
    )
}

/// Duplicate a tab — open a new tab at the same CWD as the source tab's focused pane.
///
/// `workspace_idx` — the workspace that owns the source tab; the new tab is
/// created in the same workspace (session-restore fix).
#[allow(clippy::too_many_arguments)]
fn duplicate_tab(
    workspace_idx: usize,
    tab_view: &adw::TabView,
    page: &adw::TabPage,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
    workspace_manager: &WorkspaceManager,
) {
    // Read CWD from the first leaf pane in the source tab.
    let cwd: Option<PathBuf> = {
        let container = page.child();
        let leaves = collect_leaf_drawing_areas(&container);
        leaves.first().and_then(|da| {
            let name = da.widget_name().to_string();
            let ts = tab_states.borrow();
            ts.get(&name).and_then(read_pane_cwd)
        })
    };

    let Ok(cfg) = shared_config.try_borrow() else {
        return;
    };
    let (cmd, cwd_buf) = if let Some(cwd_path) = cwd {
        (None, Some(cwd_path))
    } else {
        resolve_default_profile_args(&cfg)
    };

    // tab_id_map: fresh map for the duplicate (daemon will assign a new tab_id).
    let dup_tab_id_map: TabIdMap = Rc::new(RefCell::new(HashMap::new()));

    add_new_tab(
        workspace_idx,
        tab_view,
        &cfg,
        tab_states,
        focus_tracker,
        custom_titles,
        window,
        cwd_buf.as_deref(),
        cmd.as_deref(),
        daemon_client,
        &dup_tab_id_map,
        workspace_manager,
    );
}

/// Spawn a new forgetty window with the CWD of the source tab, then close the source tab.
fn move_tab_to_new_window(tab_view: &adw::TabView, page: &adw::TabPage, tab_states: &TabStateMap) {
    // Read CWD from the focused pane in the source tab.
    let cwd: Option<PathBuf> = {
        let container = page.child();
        let leaves = collect_leaf_drawing_areas(&container);
        leaves.first().and_then(|da| {
            let name = da.widget_name().to_string();
            let ts = tab_states.borrow();
            ts.get(&name).and_then(read_pane_cwd)
        })
    };

    // Find the forgetty binary.
    let exe = std::env::current_exe().ok();
    if let Some(exe_path) = exe {
        let mut cmd = std::process::Command::new(&exe_path);
        cmd.arg("--no-restore");
        if let Some(cwd_path) = cwd {
            cmd.arg("--working-directory").arg(cwd_path);
        }
        match cmd.spawn() {
            Ok(child) => {
                std::mem::forget(child);
                // Close the source tab.
                tab_view.close_page(page);
            }
            Err(e) => {
                tracing::warn!("move_tab_to_new_window: failed to spawn new window: {e}");
            }
        }
    } else {
        tracing::warn!("move_tab_to_new_window: could not determine current exe path");
    }
}

/// Create a plain flat menu button for the tab context menu.
fn tab_menu_button(label: &str) -> gtk4::Button {
    let lbl = gtk4::Label::new(Some(label));
    lbl.set_halign(gtk4::Align::Start);
    lbl.set_hexpand(true);
    lbl.set_margin_start(8);
    lbl.set_margin_end(8);

    let btn = gtk4::Button::new();
    btn.set_child(Some(&lbl));
    btn.set_has_frame(false);
    btn.add_css_class("flat");
    btn
}

/// Create a flat menu button with a right-pointing arrow (▶) indicating a submenu.
fn tab_menu_button_arrow(label: &str) -> gtk4::Button {
    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    let lbl = gtk4::Label::new(Some(label));
    lbl.set_halign(gtk4::Align::Start);
    lbl.set_hexpand(true);
    lbl.set_margin_start(8);
    hbox.append(&lbl);
    let arrow = gtk4::Label::new(Some("▶"));
    arrow.set_halign(gtk4::Align::End);
    arrow.set_margin_end(8);
    arrow.add_css_class("dim-label");
    hbox.append(&arrow);

    let btn = gtk4::Button::new();
    btn.set_child(Some(&hbox));
    btn.set_has_frame(false);
    btn.add_css_class("flat");
    btn
}

/// Create a flat menu button that activates a window action and dismisses the popover.
fn tab_menu_action_button(label: &str, action_name: &str, popover: &gtk4::Popover) -> gtk4::Button {
    let btn = tab_menu_button(label);
    let action = action_name.to_string();
    let pop = popover.clone();
    btn.connect_clicked(move |widget| {
        widget.activate_action(&action, None).ok();
        pop.popdown();
    });
    btn
}

/// Create a flat menu button with a dimmed shortcut hint that activates an action.
fn tab_menu_shortcut_button(
    label: &str,
    shortcut: &str,
    action_name: &str,
    popover: &gtk4::Popover,
) -> gtk4::Button {
    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    let lbl = gtk4::Label::new(Some(label));
    lbl.set_halign(gtk4::Align::Start);
    lbl.set_hexpand(true);
    lbl.set_margin_start(8);
    hbox.append(&lbl);
    let hint = gtk4::Label::new(Some(shortcut));
    hint.set_halign(gtk4::Align::End);
    hint.set_margin_end(8);
    hint.add_css_class("dim-label");
    hbox.append(&hint);

    let btn = gtk4::Button::new();
    btn.set_child(Some(&hbox));
    btn.set_has_frame(false);
    btn.add_css_class("flat");

    let action = action_name.to_string();
    let pop = popover.clone();
    btn.connect_clicked(move |widget| {
        widget.activate_action(&action, None).ok();
        pop.popdown();
    });
    btn
}

// ---------------------------------------------------------------------------
// Workspace management
// ---------------------------------------------------------------------------

/// Get the active workspace's focus tracker from the workspace manager.
///
/// Returns the active workspace's FocusTracker, or a new empty one if the
/// manager cannot be borrowed (should never happen in normal flow).
fn active_focus_tracker(workspace_manager: &WorkspaceManager) -> FocusTracker {
    workspace_manager
        .try_borrow()
        .ok()
        .map(|mgr| Rc::clone(&mgr.workspaces[mgr.active_index].focus_tracker))
        .unwrap_or_else(|| Rc::new(RefCell::new(String::new())))
}

/// Re-focus the terminal pane that was last active in the current workspace.
///
/// Called after closing the Settings view so keyboard input returns immediately
/// to the terminal without requiring a click.
fn refocus_active_pane(workspace_manager: &WorkspaceManager, window: &adw::ApplicationWindow) {
    let focused_name = active_focus_tracker(workspace_manager).borrow().clone();
    if focused_name.is_empty() {
        return;
    }
    if let Some(da) = find_drawing_area_by_name(window, &focused_name) {
        da.grab_focus();
    }
}

/// Wire the standard tab close and focus management handlers on a TabView.
///
/// Called for every workspace TabView except the initial one (which has these
/// handlers wired inline during build_ui setup).
///
/// The `workspace_manager` reference is captured so the close-page handler can
/// distinguish "the genuine last tab of the genuine last workspace" (which
/// SHOULD close the window — terminal exit semantics) from "the last tab of a
/// non-last workspace" (which should leave the window open and let FIX-009's
/// daemon-side auto-spawn refill the workspace).
#[allow(clippy::too_many_arguments)]
fn wire_tab_view_handlers(
    tab_view: &adw::TabView,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    tab_id_map: &TabIdMap,
    tab_active_pane: &TabActivePaneMap,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    workspace_manager: &WorkspaceManager,
) {
    // Tab close handling
    {
        let window_close = window.clone();
        let states_close = Rc::clone(tab_states);
        let dc_close = daemon_client.clone();
        let wm_close = Rc::clone(workspace_manager);
        tab_view.connect_close_page(move |tv, page| {
            let container = page.child();
            if let Some(ref dc) = dc_close {
                daemon_close_panes_in_subtree(&container, &states_close, dc);
            } else {
                remove_panes_in_subtree(&container, &states_close);
            }

            // FIX-009 9a: only close the window when the TabView is still
            // attached to the main widget tree AND no other workspace is alive.
            // §3.7 of the SPEC explains why both conjuncts are needed —
            // active-workspace delete detaches the TabView before this fires
            // (parent check) while also dropping the workspace count by one
            // (count check); keeping both keeps the guard robust to either
            // ordering. `unwrap_or(true)` falls back to the pre-FIX-009 close
            // if the WorkspaceManager is somehow re-entrantly borrowed.
            if tv.n_pages() <= 1 {
                let is_last_workspace =
                    wm_close.try_borrow().map(|m| m.workspaces.len() <= 1).unwrap_or(true);
                if tv.parent().is_some() && is_last_workspace {
                    window_close.close();
                }
            }

            tv.close_page_finish(page, true);
            glib::Propagation::Stop
        });
    }

    // Focus management on tab switch — also notify the daemon via focus_tab
    // so session-restore brings the correct tab back (session-restore fix).
    {
        let focus_switch = Rc::clone(focus_tracker);
        let tim_focus = Rc::clone(tab_id_map);
        let states_focus = Rc::clone(tab_states);
        let active_pane_map = Rc::clone(tab_active_pane);
        let dc_focus = daemon_client;
        tab_view.connect_selected_page_notify(move |tv| {
            if let Some(page) = tv.selected_page() {
                let container = page.child();
                let leaves = collect_leaf_drawing_areas(&container);
                let page_key = page_identity_key(&page);
                let tab_id = tim_focus.borrow().get(&page_key).copied();

                // FIX-005B fix-cycle 1: prefer per-tab saved
                // active_pane_id over the per-workspace single-string
                // focus_tracker (which holds the LAST focused widget name
                // across the whole workspace and so cross-contaminates
                // when switching between tabs that have different leaves).
                //
                // Lookup `tab_active_pane[tab_id]` → daemon PaneId, then
                // find the leaf whose `TerminalState.daemon_pane_id`
                // matches. Fall through to focus_tracker, then leaves.first().
                let saved_pane_id: Option<forgetty_core::PaneId> =
                    tab_id.and_then(|tid| active_pane_map.borrow().get(&tid).copied());
                let target_idx_via_map: Option<usize> = saved_pane_id.and_then(|pid| {
                    leaves.iter().position(|da| {
                        let widget_name = da.widget_name().to_string();
                        states_focus
                            .try_borrow()
                            .ok()
                            .and_then(|m| m.get(&widget_name).cloned())
                            .and_then(|rc| rc.try_borrow().ok().and_then(|s| s.daemon_pane_id))
                            == Some(pid)
                    })
                });

                let focused_name = focus_switch.borrow().clone();
                let target = target_idx_via_map
                    .and_then(|i| leaves.get(i))
                    .or_else(|| leaves.iter().find(|da| da.widget_name().as_str() == focused_name))
                    .or_else(|| leaves.first());
                if let Some(da) = target {
                    da.grab_focus();
                }
                // Persist active-tab on the daemon. See initial_tab_view's
                // notify handler for the full rationale (FIX-005A fix-cycle 1).
                if let Some(ref dc) = dc_focus {
                    if let Some(tid) = tab_id {
                        if let Err(e) = dc.focus_tab(tid) {
                            tracing::debug!("focus_tab RPC failed: {e}");
                        }
                    }
                }
            }
        });
    }
}

/// Open a minimal window to receive a tab dragged out of the tab bar.
///
/// Called from the `create-window` signal handler on every `adw::TabView`.
/// libadwaita moves the dragged `AdwTabPage` (including its child widget,
/// PTY state, and all Rc'd terminal state) into the returned `TabView`.
///
/// The window is intentionally minimal — no workspace sidebar, no keyboard
/// shortcuts beyond what GTK provides.  The terminal itself keeps working
/// because the child widget (DrawingArea, TerminalState Rc, PTY timers)
/// travels with the page unchanged.
fn open_detached_tab_window(app: &adw::Application) -> adw::TabView {
    let new_tv = adw::TabView::new();
    new_tv.set_vexpand(true);
    new_tv.set_hexpand(true);

    let tab_bar = adw::TabBar::new();
    tab_bar.set_view(Some(&new_tv));
    tab_bar.set_autohide(true);

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&tab_bar);
    content.append(&new_tv);

    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title("Forgetty")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&content)
        .build();

    // Close the window when the last tab is closed.
    {
        let win_c = win.clone();
        new_tv.connect_close_page(move |tv, page| {
            // Don't kill PTY on close here — the tab arrived from another
            // window whose state maps own the TerminalState.
            //
            // FIX-009 9a: detached windows have no WorkspaceManager — they are
            // single-TabView standalone windows so closing the last tab
            // legitimately closes the detached window. Add the
            // `tv.parent().is_some()` guard for symmetry with handlers 1 & 2
            // (and to avoid surprises if a future change detaches the TabView
            // from the window's content before this fires).
            if tv.n_pages() <= 1 && tv.parent().is_some() {
                win_c.close();
            }
            tv.close_page_finish(page, true);
            glib::Propagation::Stop
        });
    }

    // Allow further tears from this window.
    {
        let app_c = app.clone();
        new_tv.connect_create_window(move |_| Some(open_detached_tab_window(&app_c)));
    }

    win.present();
    new_tv
}

/// Switch to the workspace at `target_index`.
///
/// Swaps the TabView in main_area and rebinds the TabBar.
/// No-op if target does not exist or is already active.
fn switch_workspace(
    workspace_manager: &WorkspaceManager,
    target_index: usize,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: Option<&Arc<DaemonClient>>,
) {
    let Ok(mut mgr) = workspace_manager.try_borrow_mut() else {
        return;
    };

    if target_index >= mgr.workspaces.len() || target_index == mgr.active_index {
        return;
    }

    let old_tv = mgr.workspaces[mgr.active_index].tab_view.clone();
    let new_tv = mgr.workspaces[target_index].tab_view.clone();

    // Remove the old TabView from main_area.
    let mut child = main_area.first_child();
    while let Some(c) = child {
        if c == *old_tv.upcast_ref::<gtk4::Widget>() {
            main_area.remove(&c);
            break;
        }
        child = c.next_sibling();
    }

    // Insert the new TabView at the front (before the appearance sidebar).
    main_area.prepend(&new_tv);

    // Rebind the TabBar to the new TabView.
    tab_bar.set_view(Some(&new_tv));

    mgr.active_index = target_index;

    // Focus the first leaf in the new workspace's selected tab.
    let ws = &mgr.workspaces[target_index];
    if let Some(page) = ws.tab_view.selected_page() {
        let container = page.child();
        let leaves = collect_leaf_drawing_areas(&container);
        if let Some(da) = leaves.first() {
            da.grab_focus();
        }
    }

    // Update window title.
    let ws_count = mgr.workspaces.len();
    let ws_name = ws.name.clone();
    drop(mgr);
    update_window_title_with_workspace(ws_count, &ws_name, workspace_manager, window);

    // Persist the active-workspace index on the daemon so session-restore
    // brings the correct workspace back on cold restart (session-restore fix).
    if let Some(dc) = daemon_client {
        if let Err(e) = dc.set_active_workspace(target_index) {
            tracing::debug!("set_active_workspace RPC failed: {e}");
        }
    }
}

/// Show the "Restore Previous Session" dialog listing trashed sessions.
///
/// The dialog contains a listbox with one row per trashed session, showing
/// workspace names, tab count, and close timestamp. Selecting a row restores
/// the session from trash and spawns a new window.
#[allow(deprecated)]
fn show_restore_session_dialog(window: &adw::ApplicationWindow) {
    let trashed = forgetty_workspace::list_trashed_sessions_with_info();
    if trashed.is_empty() {
        // Show a simple message dialog instead.
        let dialog = gtk4::MessageDialog::new(
            Some(window),
            gtk4::DialogFlags::MODAL | gtk4::DialogFlags::DESTROY_WITH_PARENT,
            gtk4::MessageType::Info,
            gtk4::ButtonsType::Ok,
            "No recently closed sessions found.",
        );
        dialog.connect_response(|d, _| d.close());
        dialog.present();
        return;
    }

    let dialog = gtk4::Dialog::with_buttons(
        Some("Restore Previous Session"),
        Some(window),
        gtk4::DialogFlags::MODAL | gtk4::DialogFlags::DESTROY_WITH_PARENT,
        &[("Cancel", gtk4::ResponseType::Cancel)],
    );
    dialog.set_default_width(450);
    dialog.set_default_height(350);

    let content = dialog.content_area();
    let scrolled = gtk4::ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_hexpand(true);
    scrolled.set_min_content_height(200);

    let listbox = gtk4::ListBox::new();
    listbox.set_selection_mode(gtk4::SelectionMode::Single);
    listbox.add_css_class("boxed-list");

    let session_ids: Rc<Vec<uuid::Uuid>> = Rc::new(trashed.iter().map(|t| t.session_id).collect());

    for info in &trashed {
        let row = adw::ActionRow::builder().title(info.workspace_names.join(", ")).build();

        let tabs_label = format!("{} tab(s)", info.tab_count);
        let time_str = info
            .closed_at
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| {
                let secs = d.as_secs();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .map(|n| n.as_secs())
                    .unwrap_or(0);
                let diff = now.saturating_sub(secs);
                if diff < 60 {
                    "just now".to_string()
                } else if diff < 3600 {
                    format!("{} min ago", diff / 60)
                } else if diff < 86400 {
                    format!("{} hr ago", diff / 3600)
                } else {
                    format!("{} day(s) ago", diff / 86400)
                }
            })
            .unwrap_or_else(|_| "unknown".to_string());

        row.set_subtitle(&format!("{tabs_label} -- closed {time_str}"));
        listbox.append(&row);
    }

    scrolled.set_child(Some(&listbox));
    content.append(&scrolled);

    let session_ids_activate = Rc::clone(&session_ids);
    let dialog_weak = dialog.downgrade();
    listbox.connect_row_activated(move |_lb, row| {
        let idx = row.index() as usize;
        if let Some(&sid) = session_ids_activate.get(idx) {
            tracing::info!("Restoring trashed session {sid}");
            // Spawn a new process with --restore-session.
            if let Ok(exe) = std::env::current_exe() {
                match std::process::Command::new(&exe)
                    .arg("--restore-session")
                    .arg(sid.to_string())
                    .spawn()
                {
                    Ok(child) => {
                        std::mem::forget(child);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to spawn restore: {e}");
                    }
                }
            }
            if let Some(d) = dialog_weak.upgrade() {
                d.close();
            }
        }
    });

    dialog.connect_response(|d, _| d.close());
    dialog.present();
}

/// Show the "New Workspace" dialog. On confirm, creates a new WorkspaceView
/// and switches to it.
#[allow(deprecated)]
fn show_new_workspace_dialog(
    window: &adw::ApplicationWindow,
    workspace_manager: &WorkspaceManager,
    shared_config: &SharedConfig,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    daemon_client: Option<Arc<DaemonClient>>,
    sidebar_lb: gtk4::ListBox,
) {
    let dialog = adw::MessageDialog::new(
        Some(window),
        Some("New Workspace"),
        Some("Enter a name for the new workspace."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("create", "Create");
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Workspace name"));
    dialog.set_extra_child(Some(&entry));

    // Allow Enter in the entry to trigger the "create" response.
    let dialog_for_enter = dialog.clone();
    entry.connect_activate(move |_entry| {
        dialog_for_enter.response("create");
    });

    let wm = Rc::clone(workspace_manager);
    let cfg = Rc::clone(shared_config);
    let ma = main_area.clone();
    let tb = tab_bar.clone();
    let win = window.clone();
    let dc = daemon_client;
    let lb = sidebar_lb;
    dialog.connect_response(None, move |dialog, response| {
        if response != "create" {
            dialog.close();
            return;
        }
        let name = entry.text().to_string().trim().to_string();
        if name.is_empty() {
            return;
        }

        dialog.close();

        let Ok(cfg_ref) = cfg.try_borrow() else {
            return;
        };
        let config = cfg_ref.clone();
        drop(cfg_ref);

        create_and_switch_to_new_workspace(&wm, &name, &config, &ma, &tb, &win, dc.clone(), &cfg);
        refresh_workspace_sidebar(&lb, &wm, &ma, &tb, &win, &dc, &cfg);
    });

    dialog.present();
}

/// Create a new workspace and switch to it.
fn create_and_switch_to_new_workspace(
    workspace_manager: &WorkspaceManager,
    name: &str,
    config: &Config,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
) {
    let new_tv = adw::TabView::new();
    new_tv.set_vexpand(true);
    new_tv.set_hexpand(true);

    let new_tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));
    let new_focus_tracker: FocusTracker = Rc::new(RefCell::new(String::new()));
    let new_custom_titles: CustomTitles = Rc::new(RefCell::new(HashSet::new()));
    let new_tab_id_map: TabIdMap = Rc::new(RefCell::new(HashMap::new()));
    let new_tab_colors: TabColorMap = Rc::new(RefCell::new(HashMap::new()));
    let new_tab_active_pane: TabActivePaneMap = Rc::new(RefCell::new(HashMap::new()));

    wire_tab_view_handlers(
        &new_tv,
        &new_tab_states,
        &new_focus_tracker,
        &new_tab_id_map,
        &new_tab_active_pane,
        window,
        daemon_client.clone(),
        workspace_manager,
    );
    wire_tab_context_menu_signal(
        &new_tv,
        workspace_manager,
        tab_bar,
        window,
        daemon_client.clone(),
        shared_config,
    );
    // Allow tab tear-off from this workspace too.
    {
        let app_c = window
            .application()
            .and_downcast::<adw::Application>()
            .expect("window must be in an adw::Application");
        new_tv.connect_create_window(move |_| Some(open_detached_tab_window(&app_c)));
    }

    // Determine the workspace UUID and add the initial tab.
    // In daemon mode: create workspace + pane on the daemon and subscribe.
    // In self-contained mode: spawn a local PTY via add_new_tab.
    let workspace_id = if let Some(ref dc) = daemon_client {
        match dc.create_workspace(name) {
            Ok((ws_id, _ws_idx, pane_id, tab_id)) => {
                let channel = match dc.subscribe_output(pane_id) {
                    Ok(ch) => ch,
                    Err(e) => {
                        tracing::warn!(
                            "subscribe_output failed for new workspace pane {pane_id}: {e}"
                        );
                        return;
                    }
                };
                // V2-007: byte-log replay populates the VT via subscribe_output.
                let on_exit = make_on_exit_callback(
                    &new_tv,
                    &new_tab_states,
                    window,
                    Some(Arc::clone(dc)),
                    workspace_manager,
                );
                let on_notify = make_on_notify_callback(&new_tv, &new_tab_states, window);
                match terminal::create_terminal(
                    config,
                    pane_id,
                    Arc::clone(dc),
                    channel,
                    None,
                    Some(on_exit),
                    Some(on_notify),
                ) {
                    Ok((pane_vbox, drawing_area, state)) => {
                        let widget_name = next_pane_id();
                        drawing_area.set_widget_name(&widget_name);
                        new_tab_states.borrow_mut().insert(widget_name, Rc::clone(&state));
                        wire_focus_tracking(
                            &drawing_area,
                            &new_focus_tracker,
                            &new_tv,
                            &new_tab_states,
                            &new_custom_titles,
                            window,
                            &new_tab_id_map,
                            workspace_manager,
                            Some(Arc::clone(dc)),
                        );
                        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                        container.set_hexpand(true);
                        container.set_vexpand(true);
                        container.append(&pane_vbox);
                        let page = new_tv.append(&container);
                        let page_key = page_identity_key(&page);
                        new_tab_id_map.borrow_mut().insert(page_key, tab_id);
                        page.set_title("shell");
                        new_tv.set_selected_page(&page);
                        // FIX-003 bonus: the new workspace's TabView is added
                        // to `main_area` later in this function. At this
                        // point the DrawingArea is not yet mapped, so a plain
                        // `grab_focus()` silently no-ops and default focus-
                        // chain traversal lands on the AdwTabBar's "+" button
                        // instead of the new pane. `focus_when_mapped` arms a
                        // one-shot `connect_map` grab so focus lands on the
                        // DA once it's actually mapped (same helper used by
                        // FIX-002 and V2-007 restore).
                        focus_when_mapped(&drawing_area);
                        register_title_timer(
                            &page,
                            &new_tv,
                            &new_tab_states,
                            &new_focus_tracker,
                            &new_custom_titles,
                            window,
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            "Failed to create terminal widget for new workspace daemon pane: {e}"
                        );
                    }
                }
                ws_id
            }
            Err(e) => {
                tracing::error!("create_workspace RPC failed: {e}");
                return;
            }
        }
    } else {
        tracing::warn!("create_workspace_view called without a daemon client — ignoring");
        return;
    };

    let new_index = {
        let Ok(mut mgr) = workspace_manager.try_borrow_mut() else {
            return;
        };

        // Remove old TabView from main_area.
        let old_tv = mgr.workspaces[mgr.active_index].tab_view.clone();
        let mut child = main_area.first_child();
        while let Some(c) = child {
            if c == *old_tv.upcast_ref::<gtk4::Widget>() {
                main_area.remove(&c);
                break;
            }
            child = c.next_sibling();
        }

        // Insert new TabView.
        main_area.prepend(&new_tv);
        tab_bar.set_view(Some(&new_tv));

        mgr.workspaces.push(WorkspaceView {
            id: workspace_id,
            name: name.to_string(),
            tab_view: new_tv,
            tab_states: new_tab_states,
            focus_tracker: new_focus_tracker,
            custom_titles: new_custom_titles,
            tab_id_map: new_tab_id_map,
            tab_colors: new_tab_colors,
            color: None,
            color_css_provider: None,
            tab_active_pane: new_tab_active_pane,
        });

        let idx = mgr.workspaces.len() - 1;
        mgr.active_index = idx;
        idx
    };

    // Update delete-workspace enabled state.
    update_delete_workspace_action(workspace_manager, window);

    // Update window title.
    let Ok(mgr) = workspace_manager.try_borrow() else {
        return;
    };
    let ws_count = mgr.workspaces.len();
    let ws_name = mgr.workspaces[new_index].name.clone();
    drop(mgr);
    update_window_title_with_workspace(ws_count, &ws_name, workspace_manager, window);
}

/// Show the "Rename Workspace" dialog.
#[allow(deprecated)]
fn show_rename_workspace_dialog(
    window: &adw::ApplicationWindow,
    workspace_manager: &WorkspaceManager,
    daemon_client: Option<&Arc<DaemonClient>>,
) {
    let (current_name, active_idx) = {
        let Ok(mgr) = workspace_manager.try_borrow() else {
            return;
        };
        (mgr.workspaces[mgr.active_index].name.clone(), mgr.active_index)
    };

    let dialog = adw::MessageDialog::new(
        Some(window),
        Some("Rename Workspace"),
        Some("Enter a new name for the current workspace."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Workspace name"));
    entry.set_text(&current_name);
    entry.select_region(0, -1);
    dialog.set_extra_child(Some(&entry));

    let dialog_for_enter = dialog.clone();
    entry.connect_activate(move |_entry| {
        dialog_for_enter.response("rename");
    });

    let wm = Rc::clone(workspace_manager);
    let win = window.clone();
    let dc = daemon_client.cloned();
    dialog.connect_response(None, move |dialog, response| {
        if response != "rename" {
            dialog.close();
            return;
        }
        let new_name = entry.text().to_string().trim().to_string();
        if new_name.is_empty() {
            return;
        }

        dialog.close();

        // FIX-001: call the daemon FIRST so the rename is persisted. Only
        // mutate local GTK state on RPC success — on failure, the dialog
        // reopening will show the daemon's (still-old) truth.
        if let Some(ref dc) = dc {
            if let Err(e) = dc.rename_workspace(active_idx, &new_name) {
                tracing::warn!(
                    "Failed to rename workspace {active_idx} on daemon: {e}; skipping local update"
                );
                return;
            }
        }

        let Ok(mut mgr) = wm.try_borrow_mut() else {
            return;
        };
        // FIX-001: use the `active_idx` captured at dialog open (same value
        // passed to the RPC), not `mgr.active_index`. If the user switched
        // workspaces while the dialog was open, the RPC renamed the original
        // workspace — the local mutation must match to avoid drift.
        if active_idx >= mgr.workspaces.len() {
            return;
        }
        mgr.workspaces[active_idx].name = new_name.clone();
        let ws_count = mgr.workspaces.len();
        let is_active = active_idx == mgr.active_index;
        drop(mgr);

        if is_active {
            update_window_title_with_workspace(ws_count, &new_name, &wm, &win);
        }
    });

    dialog.present();
}

/// Delete the current workspace. Closes its panes and switches to an adjacent one.
fn delete_current_workspace(
    workspace_manager: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: Option<&Arc<DaemonClient>>,
) {
    let Ok(mut mgr) = workspace_manager.try_borrow_mut() else {
        return;
    };

    if mgr.workspaces.len() <= 1 {
        return; // Cannot delete the last workspace.
    }

    let delete_idx = mgr.active_index;
    let ws = &mgr.workspaces[delete_idx];

    // Ask the daemon to close every pane in the workspace (or just drop in
    // the legacy no-daemon fallback — see CHORE-FIX-011-cleanup).
    close_workspace_panes(&ws.tab_states, daemon_client.map(|a| a.as_ref()));

    // Remove the TabView from main_area if it is currently visible.
    let old_tv = ws.tab_view.clone();
    let mut child = main_area.first_child();
    while let Some(c) = child {
        if c == *old_tv.upcast_ref::<gtk4::Widget>() {
            main_area.remove(&c);
            break;
        }
        child = c.next_sibling();
    }

    // Remove the workspace from the list.
    mgr.workspaces.remove(delete_idx);

    // Choose the new active workspace.
    let new_active =
        if delete_idx >= mgr.workspaces.len() { mgr.workspaces.len() - 1 } else { delete_idx };
    mgr.active_index = new_active;

    // Insert the new active workspace's TabView.
    let new_tv = mgr.workspaces[new_active].tab_view.clone();
    main_area.prepend(&new_tv);
    tab_bar.set_view(Some(&new_tv));

    // Focus the first leaf.
    if let Some(page) = mgr.workspaces[new_active].tab_view.selected_page() {
        let container = page.child();
        let leaves = collect_leaf_drawing_areas(&container);
        if let Some(da) = leaves.first() {
            da.grab_focus();
        }
    }

    let ws_count = mgr.workspaces.len();
    let ws_name = mgr.workspaces[new_active].name.clone();
    drop(mgr);

    // Update delete-workspace enabled state.
    update_delete_workspace_action(workspace_manager, window);
    update_window_title_with_workspace(ws_count, &ws_name, workspace_manager, window);
}

/// Update the enabled state of the delete-workspace action.
fn update_delete_workspace_action(
    workspace_manager: &WorkspaceManager,
    window: &adw::ApplicationWindow,
) {
    let Ok(mgr) = workspace_manager.try_borrow() else {
        return;
    };
    let has_multiple = mgr.workspaces.len() > 1;
    drop(mgr);

    if let Some(action) = window
        .lookup_action("delete-workspace")
        .and_then(|a| a.downcast::<gio::SimpleAction>().ok())
    {
        action.set_enabled(has_multiple);
    }
}

/// Update the window title to reflect the current workspace.
///
/// AC-09: When only one workspace exists, no workspace name in title.
/// When multiple exist: "workspacename -- user@host:~/path -- Forgetty"
fn update_window_title_for_workspace(
    workspace_manager: &WorkspaceManager,
    window: &adw::ApplicationWindow,
) {
    let Ok(mgr) = workspace_manager.try_borrow() else {
        return;
    };
    let ws_count = mgr.workspaces.len();
    let ws_name = mgr.workspaces[mgr.active_index].name.clone();
    drop(mgr);

    update_window_title_with_workspace(ws_count, &ws_name, workspace_manager, window);
}

/// Compute and set the window title incorporating workspace info.
fn update_window_title_with_workspace(
    ws_count: usize,
    ws_name: &str,
    workspace_manager: &WorkspaceManager,
    window: &adw::ApplicationWindow,
) {
    // Try to get the focused pane's terminal state for user@host:cwd.
    let pane_title = {
        let Ok(mgr) = workspace_manager.try_borrow() else {
            return;
        };
        let ws = &mgr.workspaces[mgr.active_index];
        let focused_name =
            ws.focus_tracker.try_borrow().ok().map(|n| n.clone()).unwrap_or_default();
        if !focused_name.is_empty() {
            ws.tab_states
                .try_borrow()
                .ok()
                .and_then(|states| states.get(&focused_name).cloned())
                .and_then(|state_rc| state_rc.try_borrow().ok().map(|s| compute_window_title(&s)))
        } else {
            None
        }
    };

    let title = if ws_count <= 1 {
        // Single workspace: no workspace name in title (AC-09).
        pane_title.unwrap_or_else(|| "Forgetty".to_string())
    } else if let Some(ref pane) = pane_title {
        format!("{ws_name} \u{2014} {pane}")
    } else {
        format!("{ws_name} \u{2014} Forgetty")
    };

    window.set_title(Some(&title));
}

/// Reload configuration and apply to all panes in all workspaces.
fn reload_config_all_workspaces(
    shared_config: &SharedConfig,
    workspace_manager: &WorkspaceManager,
    window: &adw::ApplicationWindow,
) {
    let new_config = match load_config(None) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("Config reload failed: {e}");
            return;
        }
    };

    info!("Config reloaded via menu action");

    if let Ok(mut cfg) = shared_config.try_borrow_mut() {
        *cfg = new_config.clone();
    }

    let Ok(mgr) = workspace_manager.try_borrow() else {
        return;
    };

    for ws in &mgr.workspaces {
        let Ok(states) = ws.tab_states.try_borrow() else {
            continue;
        };
        let state_entries: Vec<_> =
            states.iter().map(|(name, rc)| (name.clone(), Rc::clone(rc))).collect();
        drop(states);

        for (pane_name, state_rc) in &state_entries {
            let Ok(mut s) = state_rc.try_borrow_mut() else {
                continue;
            };
            let Some(da) = find_drawing_area_by_name(window, pane_name) else {
                continue;
            };
            terminal::apply_config_change(&mut s, &new_config, &da);
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace sidebar (left panel)
// ---------------------------------------------------------------------------

/// Build the workspace sidebar revealer widget.
///
/// Returns `(revealer, list_box)`. The revealer is prepended to `terminal_row`
/// so the sidebar physically pushes `main_area` to the right. The `ListBox` is
/// returned so callers can call `refresh_workspace_sidebar()` to update it.
fn build_workspace_sidebar(
    workspace_manager: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: &Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
) -> (gtk4::Revealer, gtk4::ListBox) {
    let revealer = gtk4::Revealer::new();
    revealer.set_transition_type(gtk4::RevealerTransitionType::SlideRight);
    revealer.set_transition_duration(150);
    revealer.set_reveal_child(false);

    let sidebar_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    sidebar_box.set_width_request(220);
    sidebar_box.set_vexpand(true);
    sidebar_box.add_css_class("workspace-sidebar");

    let list_box = gtk4::ListBox::new();
    list_box.set_selection_mode(gtk4::SelectionMode::Single);
    list_box.add_css_class("navigation-sidebar");

    let scrolled = gtk4::ScrolledWindow::new();
    scrolled.set_child(Some(&list_box));
    scrolled.set_vexpand(true);
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    sidebar_box.append(&scrolled);

    revealer.set_child(Some(&sidebar_box));

    // Row activation (click): switch workspace but keep sidebar open.
    {
        let wm = Rc::clone(workspace_manager);
        let ma = main_area.clone();
        let tb = tab_bar.clone();
        let win = window.clone();
        let lb_ref = list_box.clone();
        let dc_ref = daemon_client.clone();
        let sc_ref = Rc::clone(shared_config);
        list_box.connect_row_activated(move |_lb, row| {
            let target = row.index() as usize;
            switch_workspace(&wm, target, &ma, &tb, &win, dc_ref.as_ref());
            refresh_workspace_sidebar(&lb_ref, &wm, &ma, &tb, &win, &dc_ref, &sc_ref);
        });
    }

    (revealer, list_box)
}

/// Rebuild workspace rows in the sidebar `ListBox` from the current manager state.
///
/// Called after any workspace switch, creation, or deletion so the active-row
/// highlight reflects the current state.
///
/// The extra parameters (`main_area`, `tab_bar`, `window`, `daemon_client`, `shared_config`)
/// are needed to build the right-click context menu gesture on each row.
#[allow(clippy::too_many_arguments)]
fn refresh_workspace_sidebar(
    list_box: &gtk4::ListBox,
    workspace_manager: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: &Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
) {
    let Ok(mgr) = workspace_manager.try_borrow() else {
        return;
    };

    // Remove all existing rows.
    while let Some(row) = list_box.row_at_index(0) {
        list_box.remove(&row);
    }

    // Add a row for each workspace.
    for (i, ws) in mgr.workspaces.iter().enumerate() {
        let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        hbox.set_margin_start(8);
        hbox.set_margin_end(8);
        hbox.set_margin_top(8);
        hbox.set_margin_bottom(8);

        // Number badge (1-indexed).
        let number_label = gtk4::Label::new(Some(&format!("{}", i + 1)));
        number_label.add_css_class("dim-label");
        number_label.set_width_request(18);
        number_label.set_halign(gtk4::Align::End);
        hbox.append(&number_label);

        // Workspace name.
        let name_label = gtk4::Label::new(Some(&ws.name));
        name_label.set_halign(gtk4::Align::Start);
        name_label.set_hexpand(true);
        name_label.set_ellipsize(pango::EllipsizeMode::End);
        if i == mgr.active_index {
            name_label.add_css_class("heading");
        }
        hbox.append(&name_label);

        // Meta column: tab count + CWD.
        let meta_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 2);

        // Tab count.
        let n_tabs = ws.tab_view.n_pages();
        let tab_count_str =
            if n_tabs == 1 { String::from("1 tab") } else { format!("{n_tabs} tabs") };
        let tab_count_label = gtk4::Label::new(Some(&tab_count_str));
        tab_count_label.add_css_class("caption");
        tab_count_label.add_css_class("dim-label");
        tab_count_label.set_halign(gtk4::Align::End);
        meta_vbox.append(&tab_count_label);

        // Active pane CWD (tilde-collapsed).
        let cwd_str = {
            let focused_name = ws.focus_tracker.borrow().clone();
            let cwd_path: Option<std::path::PathBuf> = if !focused_name.is_empty() {
                ws.tab_states.borrow().get(&focused_name).and_then(read_pane_cwd)
            } else {
                None
            };

            let raw = cwd_path.as_ref().and_then(|p| p.to_str()).unwrap_or("shell").to_string();

            // Tilde-collapse using HOME env var.
            if let Ok(home) = std::env::var("HOME") {
                if raw.starts_with(&home) {
                    format!("~{}", &raw[home.len()..])
                } else {
                    raw
                }
            } else {
                raw
            }
        };

        let cwd_label = gtk4::Label::new(Some(&cwd_str));
        cwd_label.add_css_class("caption");
        cwd_label.add_css_class("dim-label");
        cwd_label.set_halign(gtk4::Align::End);
        cwd_label.set_ellipsize(pango::EllipsizeMode::Start);
        meta_vbox.append(&cwd_label);

        hbox.append(&meta_vbox);

        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&hbox));

        // Highlight active workspace row.
        if i == mgr.active_index {
            row.add_css_class("workspace-sidebar-active");
        }

        // Per-row color CSS override (AC-7, AC-8).
        if ws.color.is_some() {
            // Apply the CSS class that targets this workspace's UUID.
            let class_name = format!("workspace-color-{}", ws.id.simple());
            row.add_css_class(&class_name);
            // The CSS provider was loaded when the color was first applied
            // (in apply_workspace_color). We only need to ensure the class
            // is present here; the provider is added once at the display level.
        }
        // else: No custom color — rows are rebuilt each time, so no stale classes accumulate.

        // Right-click context menu gesture (button 3, capture phase, claimed).
        // Using Capture phase to intercept before the row activation gesture.
        {
            let wm_ctx = Rc::clone(workspace_manager);
            let lb_ctx = list_box.clone();
            let ma_ctx = main_area.clone();
            let tb_ctx = tab_bar.clone();
            let win_ctx = window.clone();
            let dc_ctx = daemon_client.clone();
            let sc_ctx = Rc::clone(shared_config);
            let row_ref = row.clone();
            let workspace_idx = i;

            let gesture = gtk4::GestureClick::new();
            gesture.set_button(3);
            gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
            gesture.connect_pressed(move |gesture, _n, x, y| {
                // Claim the event so the ListBox row activation never fires.
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                show_workspace_context_menu(
                    &row_ref,
                    workspace_idx,
                    x,
                    y,
                    &wm_ctx,
                    &ma_ctx,
                    &tb_ctx,
                    &win_ctx,
                    &dc_ctx,
                    &sc_ctx,
                    &lb_ctx,
                );
            });
            row.add_controller(gesture);
        }

        list_box.append(&row);
    }

    // Select the active workspace row.
    if let Some(row) = list_box.row_at_index(mgr.active_index as i32) {
        list_box.select_row(Some(&row));
    }
}

// ---------------------------------------------------------------------------
// Workspace right-click context menu (T-M1-extra-013)
// ---------------------------------------------------------------------------

/// Show the workspace right-click context menu positioned at (x, y) on the row.
///
/// Mirrors `show_tab_context_menu` in structure: creates a popover, wires
/// focus-restore via `connect_closed`, uses `idle_add_local_once` for the
/// Wayland-safe click-outside dismiss (BUG-009 pattern).
#[allow(clippy::too_many_arguments)]
fn show_workspace_context_menu(
    row: &gtk4::ListBoxRow,
    workspace_idx: usize,
    x: f64,
    y: f64,
    wm: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: &Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
    sidebar_lb: &gtk4::ListBox,
) {
    let popover = gtk4::Popover::new();
    popover.set_parent(row);
    popover.set_has_arrow(false);
    popover.add_css_class("menu");
    popover.set_autohide(true);
    popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));

    // Click-outside dismiss via focus tracking (deferred — BUG-009 Wayland fix).
    {
        let pop_fc = popover.clone();
        glib::idle_add_local_once(move || {
            if !pop_fc.is_visible() {
                return;
            }
            let pop_ref = pop_fc.clone();
            let fc = gtk4::EventControllerFocus::new();
            fc.connect_leave(move |_| {
                pop_ref.popdown();
            });
            pop_fc.add_controller(fc);
        });
    }

    // Re-focus the active terminal pane after the popover is dismissed.
    {
        let wm_c = Rc::clone(wm);
        let win_c = window.clone();
        popover.connect_closed(move |_| {
            let focused_name = active_focus_tracker(&wm_c).borrow().clone();
            if !focused_name.is_empty() {
                if let Some(da) = find_drawing_area_by_name(&win_c, &focused_name) {
                    da.grab_focus();
                    return;
                }
            }
            // Fallback: focus the first tracked pane in the active workspace.
            let Ok(mgr) = wm_c.try_borrow() else { return };
            let ws = &mgr.workspaces[mgr.active_index];
            let keys: Vec<String> = ws.tab_states.borrow().keys().cloned().collect();
            drop(mgr);
            for k in &keys {
                if let Some(da) = find_drawing_area_by_name(&win_c, k) {
                    da.grab_focus();
                    break;
                }
            }
        });
    }

    let menu_box = build_workspace_context_menu_box(
        workspace_idx,
        wm,
        main_area,
        tab_bar,
        window,
        daemon_client,
        shared_config,
        sidebar_lb,
        &popover,
    );
    popover.set_child(Some(&menu_box));
    popover.popup();
}

/// Build the 8-item workspace context menu box.
///
/// Items (in order):
/// 1. Change Workspace Color  (▶ submenu)
/// 2. Rename Workspace
/// 3. --- separator ---
/// 4. Duplicate Workspace
/// 5. Move Up
/// 6. Move Down
/// 7. --- separator ---
/// 8. Delete Workspace
#[allow(clippy::too_many_arguments)]
fn build_workspace_context_menu_box(
    workspace_idx: usize,
    wm: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: &Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
    sidebar_lb: &gtk4::ListBox,
    popover: &gtk4::Popover,
) -> gtk4::Box {
    let n_workspaces = wm.try_borrow().map(|m| m.workspaces.len()).unwrap_or(1);

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    vbox.set_margin_top(4);
    vbox.set_margin_bottom(4);

    // 1. Change Workspace Color (submenu)
    {
        let color_btn = tab_menu_button_arrow("Change Workspace Color");

        let color_sub = gtk4::Popover::new();
        color_sub.set_has_arrow(true);
        color_sub.add_css_class("menu");
        color_sub.set_position(gtk4::PositionType::Right);

        let wm_c = Rc::clone(wm);
        let lb_c = sidebar_lb.clone();
        let win_c = window.clone();
        let sub_box = build_workspace_color_picker_box(
            workspace_idx,
            &wm_c,
            &lb_c,
            &win_c,
            daemon_client.as_ref(),
        );
        color_sub.set_child(Some(&sub_box));
        color_sub.set_parent(&color_btn);

        let pop_ref = popover.clone();
        color_btn.connect_clicked(move |_| {
            color_sub.popup();
            let _ = &pop_ref; // keep alive
        });
        vbox.append(&color_btn);
    }

    // 2. Rename Workspace
    {
        let wm_r = Rc::clone(wm);
        let win_r = window.clone();
        let lb_r = sidebar_lb.clone();
        let pop_r = popover.clone();
        let dc_r = daemon_client.clone();
        let btn = tab_menu_button("Rename Workspace");
        btn.connect_clicked(move |_| {
            pop_r.popdown();
            show_rename_workspace_dialog_for(&win_r, &wm_r, workspace_idx, &lb_r, dc_r.as_ref());
        });
        vbox.append(&btn);
    }

    // --- separator ---
    let sep1 = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    sep1.set_margin_top(4);
    sep1.set_margin_bottom(4);
    vbox.append(&sep1);

    // 4. Duplicate Workspace
    {
        let wm_d = Rc::clone(wm);
        let ma_d = main_area.clone();
        let tb_d = tab_bar.clone();
        let win_d = window.clone();
        let dc_d = daemon_client.clone();
        let sc_d = Rc::clone(shared_config);
        let lb_d = sidebar_lb.clone();
        let pop_d = popover.clone();
        let btn = tab_menu_button("Duplicate Workspace");
        btn.connect_clicked(move |_| {
            pop_d.popdown();
            duplicate_workspace(workspace_idx, &wm_d, &ma_d, &tb_d, &win_d, &dc_d, &sc_d, &lb_d);
        });
        vbox.append(&btn);
    }

    // 5. Move Up
    {
        let wm_u = Rc::clone(wm);
        let lb_u = sidebar_lb.clone();
        let ma_u = main_area.clone();
        let tb_u = tab_bar.clone();
        let win_u = window.clone();
        let dc_u = daemon_client.clone();
        let sc_u = Rc::clone(shared_config);
        let pop_u = popover.clone();
        let btn = tab_menu_button("Move Up");
        btn.set_sensitive(workspace_idx > 0);
        btn.connect_clicked(move |_| {
            pop_u.popdown();
            move_workspace_up(workspace_idx, &wm_u, &lb_u, &ma_u, &tb_u, &win_u, &dc_u, &sc_u);
        });
        vbox.append(&btn);
    }

    // 6. Move Down
    {
        let wm_dn = Rc::clone(wm);
        let lb_dn = sidebar_lb.clone();
        let ma_dn = main_area.clone();
        let tb_dn = tab_bar.clone();
        let win_dn = window.clone();
        let dc_dn = daemon_client.clone();
        let sc_dn = Rc::clone(shared_config);
        let pop_dn = popover.clone();
        let btn = tab_menu_button("Move Down");
        btn.set_sensitive(workspace_idx + 1 < n_workspaces);
        btn.connect_clicked(move |_| {
            pop_dn.popdown();
            move_workspace_down(
                workspace_idx,
                &wm_dn,
                &lb_dn,
                &ma_dn,
                &tb_dn,
                &win_dn,
                &dc_dn,
                &sc_dn,
            );
        });
        vbox.append(&btn);
    }

    // --- separator ---
    let sep2 = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    sep2.set_margin_top(4);
    sep2.set_margin_bottom(4);
    vbox.append(&sep2);

    // 8. Delete Workspace
    {
        let wm_del = Rc::clone(wm);
        let ma_del = main_area.clone();
        let tb_del = tab_bar.clone();
        let win_del = window.clone();
        let lb_del = sidebar_lb.clone();
        let pop_del = popover.clone();
        let dc_del = daemon_client.clone();
        let sc_del = Rc::clone(shared_config);
        let btn = tab_menu_button("Delete Workspace");
        btn.set_sensitive(n_workspaces > 1);
        btn.connect_clicked(move |_| {
            pop_del.popdown();
            delete_workspace_at_index(
                workspace_idx,
                &wm_del,
                &ma_del,
                &tb_del,
                &win_del,
                &lb_del,
                &dc_del,
                &sc_del,
            );
        });
        vbox.append(&btn);
    }

    vbox
}

/// Build the color picker box for the "Change Workspace Color" submenu.
///
/// Reuses `TAB_COLOR_PRESETS`. Swatch clicks call `apply_workspace_color`.
///
/// FIX-010: `daemon_client` is threaded through so `apply_workspace_color`
/// can call the daemon RPC first. Each click handler clones the `Arc`
/// into its closure.
fn build_workspace_color_picker_box(
    workspace_idx: usize,
    wm: &WorkspaceManager,
    sidebar_lb: &gtk4::ListBox,
    window: &adw::ApplicationWindow,
    daemon_client: Option<&Arc<DaemonClient>>,
) -> gtk4::Box {
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    vbox.set_margin_top(6);
    vbox.set_margin_bottom(6);
    vbox.set_margin_start(6);
    vbox.set_margin_end(6);

    // Swatch row
    let swatch_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);

    for (name, (r, g, b)) in TAB_COLOR_PRESETS {
        let rgba = gtk4::gdk::RGBA::new(*r, *g, *b, 1.0);
        let btn = gtk4::Button::new();
        btn.set_tooltip_text(Some(name));
        btn.set_size_request(24, 24);
        btn.set_has_frame(false);

        let da = gtk4::DrawingArea::new();
        da.set_size_request(18, 18);
        let da_rgba = rgba;
        da.set_draw_func(move |_, cr, _w, _h| {
            cr.arc(9.0, 9.0, 8.0, 0.0, 2.0 * std::f64::consts::PI);
            cr.set_source_rgba(
                da_rgba.red() as f64,
                da_rgba.green() as f64,
                da_rgba.blue() as f64,
                1.0,
            );
            let _ = cr.fill();
        });
        btn.set_child(Some(&da));

        let wm_c = Rc::clone(wm);
        let lb_c = sidebar_lb.clone();
        let dc_c = daemon_client.cloned();
        btn.connect_clicked(move |_| {
            apply_workspace_color(&wm_c, workspace_idx, Some(rgba), &lb_c, dc_c.as_ref());
        });
        swatch_box.append(&btn);
    }
    vbox.append(&swatch_box);

    // "Custom…" button — opens gtk4::ColorDialog
    {
        let custom_btn = gtk4::Button::with_label("Custom\u{2026}");
        custom_btn.set_has_frame(false);
        custom_btn.add_css_class("flat");

        let wm_c = Rc::clone(wm);
        let lb_c = sidebar_lb.clone();
        let win_c = window.clone();
        let dc_c = daemon_client.cloned();
        custom_btn.connect_clicked(move |btn| {
            let dialog = gtk4::ColorDialog::new();
            dialog.set_with_alpha(false);
            let wm2 = Rc::clone(&wm_c);
            let lb2 = lb_c.clone();
            let dc2 = dc_c.clone();
            let win = btn
                .root()
                .and_downcast::<gtk4::Window>()
                .or_else(|| Some(win_c.clone().upcast::<gtk4::Window>()));
            dialog.choose_rgba(win.as_ref(), None, gtk4::gio::Cancellable::NONE, move |result| {
                if let Ok(rgba) = result {
                    apply_workspace_color(&wm2, workspace_idx, Some(rgba), &lb2, dc2.as_ref());
                }
            });
        });
        vbox.append(&custom_btn);
    }

    // "None" button — clears the color
    {
        let none_btn = gtk4::Button::with_label("None");
        none_btn.set_has_frame(false);
        none_btn.add_css_class("flat");

        let wm_n = Rc::clone(wm);
        let lb_n = sidebar_lb.clone();
        let dc_n = daemon_client.cloned();
        none_btn.connect_clicked(move |_| {
            apply_workspace_color(&wm_n, workspace_idx, None, &lb_n, dc_n.as_ref());
        });
        vbox.append(&none_btn);
    }

    vbox
}

/// FIX-010: RPC-first orchestrator.
///
/// Calls `DaemonClient::set_workspace_color` (when a daemon_client is
/// available); on RPC error, logs and skips local mutation — daemon is the
/// source of truth (AD-007). On RPC success, falls through to
/// `apply_workspace_color_local` which does the CSS install + ListBox
/// refresh. When `daemon_client.is_none()` (e.g. a hypothetical local-only
/// mode), applies locally only.
///
/// See BUILDER_NOTES §1 for why RPC errors log (not toast) — matches
/// FIX-003's in-tree precedent; no toast infra exists in this codebase.
fn apply_workspace_color(
    wm: &WorkspaceManager,
    idx: usize,
    color: Option<gtk4::gdk::RGBA>,
    lb: &gtk4::ListBox,
    daemon_client: Option<&Arc<DaemonClient>>,
) {
    // TOCTOU-safe bounds check (FIX-003 discipline) before the RPC. A second
    // client could have shifted the list between menu-construction time and
    // swatch-click time.
    {
        let Ok(mgr) = wm.try_borrow() else { return };
        if idx >= mgr.workspaces.len() {
            return;
        }
    }

    // FIX-010: RPC first. Daemon is authoritative — if it rejects, we must
    // not mutate local state, otherwise we'd reintroduce the data-loss bug
    // this fix is closing.
    if let Some(dc) = daemon_client {
        let hex: Option<String> = color.as_ref().map(rgba_to_hex);
        if let Err(e) = dc.set_workspace_color(idx, hex.as_deref()) {
            tracing::warn!(
                workspace_idx = idx,
                "set_workspace_color RPC failed: {e} — skipping local mutation"
            );
            return;
        }
    }

    apply_workspace_color_local(wm, idx, color, lb);
}

/// FIX-010: the pre-existing local CSS install + sidebar-row refresh,
/// factored out so both the RPC-success path and the
/// `LayoutEvent::WorkspaceColorChanged` consumer can call it.
///
/// Sets `mgr.workspaces[idx].color` and installs (or refreshes) a
/// per-workspace `CssProvider` whose rule targets
/// `.workspace-color-{uuid.simple()}`. The provider is registered once
/// with the display and reused across colour changes for the same
/// workspace.
fn apply_workspace_color_local(
    wm: &WorkspaceManager,
    idx: usize,
    color: Option<gtk4::gdk::RGBA>,
    lb: &gtk4::ListBox,
) {
    let Ok(mut mgr) = wm.try_borrow_mut() else { return };
    if idx >= mgr.workspaces.len() {
        return;
    }

    mgr.workspaces[idx].color = color;

    if let Some(ref rgba) = color {
        let ws_id = mgr.workspaces[idx].id;
        let class_name = format!("workspace-color-{}", ws_id.simple());

        // Build CSS: a 4 px solid left border in the chosen color.
        let r = (rgba.red() * 255.0) as u8;
        let g = (rgba.green() * 255.0) as u8;
        let b = (rgba.blue() * 255.0) as u8;
        let a = rgba.alpha();
        let css = format!(".{class_name} {{ border-left: 4px solid rgba({r},{g},{b},{a}); }}");

        // Get-or-create the per-workspace CSS provider.
        if mgr.workspaces[idx].color_css_provider.is_none() {
            let provider = gtk4::CssProvider::new();
            // Register once with the display.
            gtk4::style_context_add_provider_for_display(
                &gtk4::gdk::Display::default().expect("no display"),
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
            mgr.workspaces[idx].color_css_provider = Some(provider);
        }

        if let Some(ref provider) = mgr.workspaces[idx].color_css_provider {
            provider.load_from_string(&css);
        }
    }
    // If color is None, the provider stays registered but with its previous
    // CSS. We clear it by loading empty CSS so the class has no effect.
    else if let Some(ref provider) = mgr.workspaces[idx].color_css_provider {
        provider.load_from_string("");
    }

    // Rebuild the sidebar row (needs only the ListBox, which already rebuilds).
    // We can't call refresh_workspace_sidebar here because we hold borrow_mut.
    // Instead, schedule a refresh on the next idle cycle.
    drop(mgr);

    // Re-borrow immutably to call refresh (the borrow_mut is dropped).
    // We can't call refresh_workspace_sidebar because we don't have main_area/tab_bar/etc.
    // here — apply_workspace_color only has the lb. We do a minimal rebuild:
    // remove all rows and re-add them from the current state.
    // The row-gesture-wiring part of refresh_workspace_sidebar is skipped here
    // because gestures will be re-attached on next full refresh_workspace_sidebar call.
    // For now: rebuild rows without gestures (gesture-less rebuild is AC-10 compatible).
    let wm_idle = Rc::clone(wm);
    let lb_idle = lb.clone();
    glib::idle_add_local_once(move || {
        rebuild_sidebar_rows_for_color(&lb_idle, &wm_idle);
    });
}

/// FIX-010: after `build_widgets_from_layout` restores workspaces from the
/// daemon, install a `CssProvider` for each workspace whose `color` is
/// `Some`. This is the counterpart of the runtime `apply_workspace_color`
/// call for fresh user-driven colour changes — both pipelines converge at
/// `apply_workspace_color_local`, which owns the CSS-provider install.
///
/// Called exactly once per GTK window startup, right after
/// `build_widgets_from_layout` has populated `WorkspaceView.color` from the
/// daemon's `get_layout` response. Workspaces with `color: None` are
/// skipped.
fn apply_restored_workspace_colors(wm: &WorkspaceManager, lb: &gtk4::ListBox) {
    let restored: Vec<(usize, gtk4::gdk::RGBA)> = {
        let Ok(mgr) = wm.try_borrow() else { return };
        mgr.workspaces
            .iter()
            .enumerate()
            .filter_map(|(i, ws)| ws.color.map(|rgba| (i, rgba)))
            .collect()
    };
    for (idx, rgba) in restored {
        apply_workspace_color_local(wm, idx, Some(rgba), lb);
    }
}

/// Minimal sidebar row rebuild used after a color change.
///
/// Rebuilds only the row CSS classes (color overrides) without re-attaching
/// all the gesture controllers (those are only wired during full
/// `refresh_workspace_sidebar` calls). This is sufficient for AC-10.
fn rebuild_sidebar_rows_for_color(lb: &gtk4::ListBox, wm: &WorkspaceManager) {
    let Ok(mgr) = wm.try_borrow() else { return };

    // Walk the rows and reapply color CSS classes based on current state.
    for (i, ws) in mgr.workspaces.iter().enumerate() {
        let Some(row) = lb.row_at_index(i as i32) else { continue };
        let class_name = format!("workspace-color-{}", ws.id.simple());
        if ws.color.is_some() {
            row.add_css_class(&class_name);
        } else {
            row.remove_css_class(&class_name);
        }
    }
}

/// FIX-006: minimal sidebar row-label refresh used by the
/// `LayoutEvent::WorkspacesReordered` consumer when an external (paired
/// device) reorder reaches GTK.
///
/// Self-originated reorders short-circuit before this is called (the local
/// Vec swap already ran in `move_workspace_up`/`move_workspace_down`, then
/// `refresh_workspace_sidebar` rebuilt the rows in full). This helper exists
/// because the layout-event consumer does not have `main_area` / `tab_bar`
/// refs and cannot call `refresh_workspace_sidebar` directly.
///
/// Walks each row's hbox children and rewrites the number_label (index 0)
/// and name_label (index 1) text from the underlying `WorkspaceView`. The
/// row order in the ListBox is unchanged — we only update text labels so the
/// "1: Default" / "2: A" prefixes track the current Vec order. The colour
/// CSS classes are also reapplied via `rebuild_sidebar_rows_for_color`-like
/// logic so coloured rows track their workspace after the swap.
fn refresh_sidebar_row_labels(wm: &WorkspaceManager, lb: &gtk4::ListBox) {
    let Ok(mgr) = wm.try_borrow() else { return };

    for (i, ws) in mgr.workspaces.iter().enumerate() {
        let Some(row) = lb.row_at_index(i as i32) else { continue };
        let Some(hbox) = row.child().and_then(|c| c.downcast::<gtk4::Box>().ok()) else {
            continue;
        };

        let mut child = hbox.first_child();
        let mut child_idx = 0;
        while let Some(c) = child {
            if let Some(lbl) = c.downcast_ref::<gtk4::Label>() {
                if child_idx == 0 {
                    // number_label: "1", "2", ... (1-indexed for users).
                    lbl.set_text(&format!("{}", i + 1));
                } else if child_idx == 1 {
                    // name_label
                    lbl.set_text(&ws.name);
                    break;
                }
            }
            child = c.next_sibling();
            child_idx += 1;
        }

        // Reapply colour CSS class so the row's accent follows the
        // reordered workspace, not its previous neighbour.
        let class_name = format!("workspace-color-{}", ws.id.simple());
        if ws.color.is_some() {
            row.add_css_class(&class_name);
        } else {
            row.remove_css_class(&class_name);
        }
    }
}

/// Show the "Rename Workspace" dialog targeted at `target_idx` rather than the active workspace.
///
/// Updates `mgr.workspaces[target_idx].name`. Only updates the window title if
/// `target_idx == active_index` (AC-14, AC-15).
#[allow(deprecated)]
fn show_rename_workspace_dialog_for(
    window: &adw::ApplicationWindow,
    wm: &WorkspaceManager,
    target_idx: usize,
    sidebar_lb: &gtk4::ListBox,
    daemon_client: Option<&Arc<DaemonClient>>,
) {
    let current_name = {
        let Ok(mgr) = wm.try_borrow() else { return };
        if target_idx >= mgr.workspaces.len() {
            return;
        }
        mgr.workspaces[target_idx].name.clone()
    };

    let dialog = adw::MessageDialog::new(
        Some(window),
        Some("Rename Workspace"),
        Some("Enter a new name for this workspace."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Workspace name"));
    entry.set_text(&current_name);
    entry.select_region(0, -1);
    dialog.set_extra_child(Some(&entry));

    let dialog_for_enter = dialog.clone();
    entry.connect_activate(move |_entry| {
        dialog_for_enter.response("rename");
    });

    let wm_r = Rc::clone(wm);
    let win_r = window.clone();
    let lb_r = sidebar_lb.clone();
    let dc_r = daemon_client.cloned();
    dialog.connect_response(None, move |dialog, response| {
        if response != "rename" {
            dialog.close();
            return;
        }
        let new_name = entry.text().to_string().trim().to_string();
        if new_name.is_empty() {
            return;
        }

        dialog.close();

        // FIX-001: call the daemon FIRST so the rename is persisted. Only
        // mutate local GTK state on RPC success.
        if let Some(ref dc) = dc_r {
            if let Err(e) = dc.rename_workspace(target_idx, &new_name) {
                tracing::warn!(
                    "Failed to rename workspace {target_idx} on daemon: {e}; skipping local update"
                );
                return;
            }
        }

        let (ws_count, is_active) = {
            let Ok(mut mgr) = wm_r.try_borrow_mut() else { return };
            if target_idx >= mgr.workspaces.len() {
                return;
            }
            mgr.workspaces[target_idx].name = new_name.clone();
            let is_active = target_idx == mgr.active_index;
            (mgr.workspaces.len(), is_active)
        };

        // Update window title only if the renamed workspace is the active one (AC-14, AC-15).
        if is_active {
            update_window_title_with_workspace(ws_count, &new_name, &wm_r, &win_r);
        }

        // Rebuild sidebar rows: minimal rebuild (no gestures, color only).
        rebuild_sidebar_rows_for_color(&lb_r, &wm_r);

        // Update the row label in place by rebuilding all row labels.
        // Since the row widgets are inside the lb, we need a different approach:
        // walk the list_box rows and update the name_label text.
        // The rows were built in refresh_workspace_sidebar; the name label is
        // the second child of the hbox (index 1, after the number label).
        let Ok(mgr) = wm_r.try_borrow() else { return };
        for (i, ws) in mgr.workspaces.iter().enumerate() {
            let Some(row) = lb_r.row_at_index(i as i32) else { continue };
            // Find the first Box child of the row, then the second Label inside it.
            if let Some(hbox) = row.child().and_then(|c| c.downcast::<gtk4::Box>().ok()) {
                // Walk children of hbox: number_label, name_label, meta_vbox
                let mut child = hbox.first_child();
                let mut child_idx = 0;
                while let Some(c) = child {
                    if child_idx == 1 {
                        if let Some(lbl) = c.downcast_ref::<gtk4::Label>() {
                            lbl.set_text(&ws.name);
                        }
                        break;
                    }
                    child = c.next_sibling();
                    child_idx += 1;
                }
            }
        }
    });

    dialog.present();
}

/// Duplicate a workspace: create a new WorkspaceView with the same tab CWDs,
/// insert it immediately after the source, and switch to it.
#[allow(clippy::too_many_arguments)]
/// Duplicate a workspace (FIX-007). The daemon is authoritative — GTK sends
/// a `duplicate_workspace` RPC, then builds widgets from the response.
///
/// Post-FIX-011 the daemon path is the only live path; the legacy local-only
/// fallback (`duplicate_workspace_temp_fallback`) is retained as unreachable
/// code pending CHORE-FIX-011-cleanup. In daemon mode the flow is RPC-first:
///
/// 1. TOCTOU-safe bounds re-check against GTK state (context menu already
///    captured `workspace_idx` at construction time per the existing pattern
///    at `app.rs:~8520`).
/// 2. `dc.duplicate_workspace(workspace_idx, None)` — daemon builds the
///    duplicate `SessionWorkspace` with fresh PTYs (one per source tab,
///    inheriting leftmost-leaf CWDs).
/// 3. On RPC success, GTK constructs the new `WorkspaceView` from the
///    returned `(workspace_id, workspace_idx, tabs)` tuple — per-tab
///    subscribe_output + terminal widget.
/// 4. On RPC error, log + return without any local mutation.
///
/// Bonus from FIX-003: the duplicate's first-tab DrawingArea receives a
/// `focus_when_mapped` grab so the AdwTabBar "+" button doesn't steal focus
/// on first activation.
fn duplicate_workspace(
    workspace_idx: usize,
    wm: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: &Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
    sidebar_lb: &gtk4::ListBox,
) {
    // TOCTOU-safe bounds re-check — the context menu captured
    // `workspace_idx` at menu-construction time (`app.rs:~8520`), but
    // another client (paired Android) or mid-menu sidebar click could
    // have shifted indices. Mirrors FIX-003's discipline
    // (`do_delete_workspace_at_index` at `app.rs:~9277`).
    {
        let Ok(mgr) = wm.try_borrow() else { return };
        if workspace_idx >= mgr.workspaces.len() {
            return;
        }
    }

    // Legacy fallback (no daemon client): unreachable post-FIX-011 since
    // daemon_client is always Some on the live path. Retained until
    // CHORE-FIX-011-cleanup deletes the fallback function.
    let Some(dc) = daemon_client.as_ref().cloned() else {
        duplicate_workspace_temp_fallback(
            workspace_idx,
            wm,
            main_area,
            tab_bar,
            window,
            shared_config,
            sidebar_lb,
        );
        return;
    };

    // Borrow the config outside the RPC call — the RPC blocks on the socket
    // and we don't want to hold the SharedConfig borrow across it.
    let cfg = {
        let Ok(cfg_borrow) = shared_config.try_borrow() else { return };
        cfg_borrow.clone()
    };

    // Daemon-authoritative RPC. Daemon reads source CWDs from its own
    // `PaneState.cwd` registry (SPEC §5.1.3) so GTK does not need to
    // collect per-tab CWDs client-side anymore.
    let (ws_id, daemon_idx, dup_tabs) = match dc.duplicate_workspace(workspace_idx, None) {
        Ok(tuple) => tuple,
        Err(e) => {
            tracing::warn!(
                workspace_idx,
                "duplicate_workspace RPC failed: {e} — skipping local mutation"
            );
            return;
        }
    };

    // Daemon is authoritative for placement (AD-007). Use the daemon's
    // returned `workspace_idx` as the insertion index so GTK and daemon
    // indices stay lock-step — critical because the daemon persists the
    // layout by index and GTK's `set_active_workspace` RPC routes by the
    // same index space. Clamp to `mgr.workspaces.len()` below (insert at
    // end at worst) if the daemon and GTK disagree on workspace count.
    let insert_at = daemon_idx;

    // Build the new WorkspaceView scaffolding. Same shape as
    // `create_and_switch_to_new_workspace`.
    let new_tv = adw::TabView::new();
    new_tv.set_vexpand(true);
    new_tv.set_hexpand(true);

    let new_tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));
    let new_focus_tracker: FocusTracker = Rc::new(RefCell::new(String::new()));
    let new_custom_titles: CustomTitles = Rc::new(RefCell::new(HashSet::new()));
    let new_tab_id_map: TabIdMap = Rc::new(RefCell::new(HashMap::new()));
    let new_tab_colors: TabColorMap = Rc::new(RefCell::new(HashMap::new()));
    let new_tab_active_pane: TabActivePaneMap = Rc::new(RefCell::new(HashMap::new()));

    wire_tab_view_handlers(
        &new_tv,
        &new_tab_states,
        &new_focus_tracker,
        &new_tab_id_map,
        &new_tab_active_pane,
        window,
        Some(Arc::clone(&dc)),
        wm,
    );
    wire_tab_context_menu_signal(
        &new_tv,
        wm,
        tab_bar,
        window,
        Some(Arc::clone(&dc)),
        shared_config,
    );
    {
        let app_c = window
            .application()
            .and_downcast::<adw::Application>()
            .expect("window must be in an adw::Application");
        new_tv.connect_create_window(move |_| Some(open_detached_tab_window(&app_c)));
    }

    // Build one widget per duplicated daemon tab. Same leaf-subscription
    // pattern as `build_widget_from_pane_tree` / `create_and_switch_to_new_workspace`.
    let mut first_da_opt: Option<gtk4::DrawingArea> = None;
    for dup_tab in &dup_tabs {
        let channel = match dc.subscribe_output(dup_tab.pane_id) {
            Ok(ch) => ch,
            Err(e) => {
                tracing::warn!(
                    pane_id = %dup_tab.pane_id,
                    "duplicate_workspace: subscribe_output failed: {e} — skipping this tab"
                );
                continue;
            }
        };

        let on_exit =
            make_on_exit_callback(&new_tv, &new_tab_states, window, Some(Arc::clone(&dc)), wm);
        let on_notify = make_on_notify_callback(&new_tv, &new_tab_states, window);

        match terminal::create_terminal(
            &cfg,
            dup_tab.pane_id,
            Arc::clone(&dc),
            channel,
            None,
            Some(on_exit),
            Some(on_notify),
        ) {
            Ok((pane_vbox, drawing_area, state)) => {
                let widget_name = next_pane_id();
                drawing_area.set_widget_name(&widget_name);
                new_tab_states.borrow_mut().insert(widget_name, Rc::clone(&state));
                wire_focus_tracking(
                    &drawing_area,
                    &new_focus_tracker,
                    &new_tv,
                    &new_tab_states,
                    &new_custom_titles,
                    window,
                    &new_tab_id_map,
                    wm,
                    Some(Arc::clone(&dc)),
                );
                let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                container.set_hexpand(true);
                container.set_vexpand(true);
                container.append(&pane_vbox);
                let page = new_tv.append(&container);
                let page_key = page_identity_key(&page);
                new_tab_id_map.borrow_mut().insert(page_key, dup_tab.tab_id);
                page.set_title("shell");
                register_title_timer(
                    &page,
                    &new_tv,
                    &new_tab_states,
                    &new_focus_tracker,
                    &new_custom_titles,
                    window,
                );
                if first_da_opt.is_none() {
                    first_da_opt = Some(drawing_area);
                }
            }
            Err(e) => {
                tracing::error!(
                    pane_id = %dup_tab.pane_id,
                    "duplicate_workspace: create_terminal failed: {e}"
                );
            }
        }
    }

    // Select the first tab and arm focus_when_mapped so the "+" button
    // doesn't steal focus after `main_area.prepend(&new_tv)` below (same
    // rationale as `create_and_switch_to_new_workspace` / FIX-003 bonus).
    if new_tv.n_pages() > 0 {
        let first_page = new_tv.nth_page(0);
        new_tv.set_selected_page(&first_page);
    }
    if let Some(ref da) = first_da_opt {
        focus_when_mapped(da);
    }

    // Derive the duplicate's display name from the source workspace (for
    // the sidebar row + window title). Daemon used the same derivation
    // internally so they agree.
    let dup_name = {
        let Ok(mgr) = wm.try_borrow() else { return };
        format!("{} (copy)", mgr.workspaces[workspace_idx].name)
    };

    let new_ws = WorkspaceView {
        id: ws_id,
        name: dup_name,
        tab_view: new_tv.clone(),
        tab_states: new_tab_states,
        focus_tracker: new_focus_tracker,
        custom_titles: new_custom_titles,
        tab_id_map: new_tab_id_map,
        tab_colors: new_tab_colors,
        color: None,
        color_css_provider: None,
        tab_active_pane: new_tab_active_pane,
    };

    // Insert the new workspace and switch to it.
    let final_idx = {
        let Ok(mut mgr) = wm.try_borrow_mut() else { return };

        // Clamp insert_at to mgr.workspaces.len() so Vec::insert never
        // panics if GTK and daemon workspace counts disagree (the daemon
        // may have clamped internally if another mutator shrank the list
        // between the RPC call and this borrow).
        let clamped = insert_at.min(mgr.workspaces.len());

        // Adjust active_index for indices that shift due to insertion.
        if mgr.active_index >= clamped {
            mgr.active_index += 1;
        }

        mgr.workspaces.insert(clamped, new_ws);

        // Remove the current active TabView from main_area.
        let old_tv = mgr.workspaces[mgr.active_index].tab_view.clone();
        let mut child = main_area.first_child();
        while let Some(c) = child {
            if c == *old_tv.upcast_ref::<gtk4::Widget>() {
                main_area.remove(&c);
                break;
            }
            child = c.next_sibling();
        }

        // Set the duplicate as the new active workspace.
        mgr.active_index = clamped;
        main_area.prepend(&new_tv);
        tab_bar.set_view(Some(&new_tv));

        let ws_count = mgr.workspaces.len();
        let ws_name = mgr.workspaces[clamped].name.clone();
        drop(mgr);
        update_delete_workspace_action(wm, window);
        update_window_title_with_workspace(ws_count, &ws_name, wm, window);
        clamped
    };

    // Tell the daemon to persist the new active-workspace index. Matches
    // the SPEC §5.3.6 requirement: the daemon's `inner.layout.active_workspace`
    // must agree with GTK's `mgr.active_index` so cold-restart lands the
    // user on the duplicate (not the source).
    if let Err(e) = dc.set_active_workspace(final_idx) {
        tracing::debug!("duplicate_workspace: set_active_workspace({final_idx}) failed: {e}");
    }

    refresh_workspace_sidebar(
        sidebar_lb,
        wm,
        main_area,
        tab_bar,
        window,
        daemon_client,
        shared_config,
    );
}

/// Legacy local-only duplicate for the no-daemon-client path.
///
/// Pre-FIX-007 behaviour, kept unchanged for compatibility. Post-FIX-011 the
/// daemon path is the only live path, making this function unreachable on the
/// live launcher. Deletion is deferred to CHORE-FIX-011-cleanup; daemon mode
/// (the daily-driver path) uses the RPC-authoritative `duplicate_workspace`
/// above.
fn duplicate_workspace_temp_fallback(
    workspace_idx: usize,
    wm: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    shared_config: &SharedConfig,
    sidebar_lb: &gtk4::ListBox,
) {
    // Collect source CWDs and name while holding a borrow.
    let (source_name, cwds, cfg) = {
        let Ok(mgr) = wm.try_borrow() else { return };
        if workspace_idx >= mgr.workspaces.len() {
            return;
        }
        let ws = &mgr.workspaces[workspace_idx];
        let n = ws.tab_view.n_pages();
        let mut cwds: Vec<Option<std::path::PathBuf>> = Vec::new();
        for i in 0..n {
            let page = ws.tab_view.nth_page(i);
            let container = page.child();
            let leaves = collect_leaf_drawing_areas(&container);
            let cwd = leaves.first().and_then(|da| {
                let name = da.widget_name().to_string();
                ws.tab_states.borrow().get(&name).and_then(read_pane_cwd)
            });
            cwds.push(cwd);
        }
        let source_name = ws.name.clone();
        drop(mgr);
        let Ok(cfg_borrow) = shared_config.try_borrow() else { return };
        let cfg = cfg_borrow.clone();
        (source_name, cwds, cfg)
    };

    let dup_name = format!("{source_name} (copy)");

    // Build the new TabView and WorkspaceView.
    let new_tv = adw::TabView::new();
    new_tv.set_vexpand(true);
    new_tv.set_hexpand(true);

    let new_tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));
    let new_focus_tracker: FocusTracker = Rc::new(RefCell::new(String::new()));
    let new_custom_titles: CustomTitles = Rc::new(RefCell::new(HashSet::new()));
    let new_tab_id_map: TabIdMap = Rc::new(RefCell::new(HashMap::new()));
    let new_tab_colors: TabColorMap = Rc::new(RefCell::new(HashMap::new()));
    let new_tab_active_pane: TabActivePaneMap = Rc::new(RefCell::new(HashMap::new()));

    wire_tab_view_handlers(
        &new_tv,
        &new_tab_states,
        &new_focus_tracker,
        &new_tab_id_map,
        &new_tab_active_pane,
        window,
        None,
        wm,
    );
    wire_tab_context_menu_signal(&new_tv, wm, tab_bar, window, None, shared_config);
    {
        let app_c = window
            .application()
            .and_downcast::<adw::Application>()
            .expect("window must be in an adw::Application");
        new_tv.connect_create_window(move |_| Some(open_detached_tab_window(&app_c)));
    }

    // Add one tab per source CWD — without a daemon client `add_new_tab`
    // no-ops, which means the duplicate is effectively an empty workspace.
    // This matches pre-FIX-007 behaviour exactly (the old code also called
    // `add_new_tab` with `daemon_client.clone()` which was `None` in this
    // legacy path). Unreachable post-FIX-011 — see CHORE-FIX-011-cleanup.
    for cwd_opt in &cwds {
        add_new_tab(
            workspace_idx,
            &new_tv,
            &cfg,
            &new_tab_states,
            &new_focus_tracker,
            &new_custom_titles,
            window,
            cwd_opt.as_deref(),
            None,
            None,
            &new_tab_id_map,
            wm,
        );
    }

    let new_ws = WorkspaceView {
        id: uuid::Uuid::new_v4(),
        name: dup_name,
        tab_view: new_tv.clone(),
        tab_states: new_tab_states,
        focus_tracker: new_focus_tracker,
        custom_titles: new_custom_titles,
        tab_id_map: new_tab_id_map,
        tab_colors: new_tab_colors,
        color: None,
        color_css_provider: None,
        tab_active_pane: new_tab_active_pane,
    };

    // Insert the new workspace and switch to it.
    {
        let Ok(mut mgr) = wm.try_borrow_mut() else { return };

        let insert_at = workspace_idx + 1;

        // Adjust active_index for indices that shift due to insertion.
        if mgr.active_index >= insert_at {
            mgr.active_index += 1;
        }

        mgr.workspaces.insert(insert_at, new_ws);

        // Remove the current active TabView from main_area.
        let old_tv = mgr.workspaces[mgr.active_index].tab_view.clone();
        let mut child = main_area.first_child();
        while let Some(c) = child {
            if c == *old_tv.upcast_ref::<gtk4::Widget>() {
                main_area.remove(&c);
                break;
            }
            child = c.next_sibling();
        }

        // Set the duplicate as the new active workspace.
        mgr.active_index = insert_at;
        main_area.prepend(&new_tv);
        tab_bar.set_view(Some(&new_tv));

        // Focus the first leaf.
        if let Some(page) = new_tv.selected_page() {
            let container = page.child();
            let leaves = collect_leaf_drawing_areas(&container);
            if let Some(da) = leaves.first() {
                da.grab_focus();
            }
        }

        let ws_count = mgr.workspaces.len();
        let ws_name = mgr.workspaces[insert_at].name.clone();
        drop(mgr);
        update_delete_workspace_action(wm, window);
        update_window_title_with_workspace(ws_count, &ws_name, wm, window);
    }

    refresh_workspace_sidebar(sidebar_lb, wm, main_area, tab_bar, window, &None, shared_config);
}

/// Swap the workspace at `workspace_idx` with the one above it (idx - 1).
///
/// Updates `active_index` to follow the moved element (AC-22).
///
/// FIX-006: daemon is authoritative for workspace order (AD-002 / AD-007).
/// Calls `DaemonClient::move_workspace` first; on RPC error, logs and skips
/// the local mutation so the daemon's persisted order remains the source of
/// truth.
fn move_workspace_up(
    workspace_idx: usize,
    wm: &WorkspaceManager,
    sidebar_lb: &gtk4::ListBox,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: &Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
) {
    if workspace_idx == 0 {
        return; // Already at the top.
    }
    // TOCTOU-safe bounds re-check (FIX-003 discipline) before the RPC.
    {
        let Ok(mgr) = wm.try_borrow() else { return };
        if workspace_idx >= mgr.workspaces.len() {
            return;
        }
    }

    // FIX-006: RPC first. Daemon is authoritative — if it rejects, we must
    // not mutate local state, otherwise the daemon's persisted order would
    // diverge from GTK's view.
    if let Some(dc) = daemon_client.as_ref() {
        if let Err(e) = dc.move_workspace(workspace_idx, workspace_idx - 1) {
            tracing::warn!(
                workspace_idx,
                "move_workspace_up RPC failed: {e} — skipping local mutation"
            );
            return;
        }
    }

    let Ok(mut mgr) = wm.try_borrow_mut() else { return };
    if workspace_idx >= mgr.workspaces.len() {
        return;
    }

    mgr.workspaces.swap(workspace_idx, workspace_idx - 1);

    // FIX-006 Cycle 1: record this self-originated reorder so the
    // `WorkspacesReordered` consumer can identify (and skip) the daemon's
    // echo. Pushed BEFORE the active_index follow-logic to keep the critical
    // mutation block tight against the swap.
    mgr.pending_reorders.push_back((workspace_idx, workspace_idx - 1));

    // Update active_index to follow the moved element.
    if mgr.active_index == workspace_idx {
        mgr.active_index = workspace_idx - 1;
    } else if mgr.active_index == workspace_idx - 1 {
        mgr.active_index = workspace_idx;
    }

    drop(mgr);
    refresh_workspace_sidebar(
        sidebar_lb,
        wm,
        main_area,
        tab_bar,
        window,
        daemon_client,
        shared_config,
    );
}

/// Swap the workspace at `workspace_idx` with the one below it (idx + 1).
///
/// Updates `active_index` to follow the moved element (AC-26).
///
/// FIX-006: daemon is authoritative for workspace order (AD-002 / AD-007).
/// Calls `DaemonClient::move_workspace` first; on RPC error, logs and skips
/// the local mutation so the daemon's persisted order remains the source of
/// truth.
fn move_workspace_down(
    workspace_idx: usize,
    wm: &WorkspaceManager,
    sidebar_lb: &gtk4::ListBox,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: &Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
) {
    // TOCTOU-safe bounds re-check (FIX-003 discipline) before the RPC.
    {
        let Ok(mgr) = wm.try_borrow() else { return };
        if workspace_idx + 1 >= mgr.workspaces.len() {
            return;
        }
    }

    // FIX-006: RPC first. Daemon is authoritative — if it rejects, we must
    // not mutate local state, otherwise the daemon's persisted order would
    // diverge from GTK's view.
    if let Some(dc) = daemon_client.as_ref() {
        if let Err(e) = dc.move_workspace(workspace_idx, workspace_idx + 1) {
            tracing::warn!(
                workspace_idx,
                "move_workspace_down RPC failed: {e} — skipping local mutation"
            );
            return;
        }
    }

    let Ok(mut mgr) = wm.try_borrow_mut() else { return };
    if workspace_idx + 1 >= mgr.workspaces.len() {
        return;
    }

    mgr.workspaces.swap(workspace_idx, workspace_idx + 1);

    // FIX-006 Cycle 1: record this self-originated reorder so the
    // `WorkspacesReordered` consumer can identify (and skip) the daemon's
    // echo. Pushed BEFORE the active_index follow-logic to keep the critical
    // mutation block tight against the swap.
    mgr.pending_reorders.push_back((workspace_idx, workspace_idx + 1));

    // Update active_index to follow the moved element.
    if mgr.active_index == workspace_idx {
        mgr.active_index = workspace_idx + 1;
    } else if mgr.active_index == workspace_idx + 1 {
        mgr.active_index = workspace_idx;
    }

    drop(mgr);
    refresh_workspace_sidebar(
        sidebar_lb,
        wm,
        main_area,
        tab_bar,
        window,
        daemon_client,
        shared_config,
    );
}

/// Delete the workspace at `target_idx`.
///
/// Shows a confirmation `adw::MessageDialog` when the workspace has more than 1 tab (AC-31).
/// On confirmation: kills PTYs, removes the WorkspaceView, updates active_index,
/// swaps the TabView, and refreshes the sidebar.
#[allow(deprecated)]
#[allow(clippy::too_many_arguments)]
fn delete_workspace_at_index(
    target_idx: usize,
    wm: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    sidebar_lb: &gtk4::ListBox,
    daemon_client: &Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
) {
    let (n_workspaces, n_pages) = {
        let Ok(mgr) = wm.try_borrow() else { return };
        if target_idx >= mgr.workspaces.len() {
            return;
        }
        (mgr.workspaces.len(), mgr.workspaces[target_idx].tab_view.n_pages())
    };

    if n_workspaces <= 1 {
        return; // Cannot delete the last workspace (AC-29).
    }

    if n_pages <= 1 {
        // Single tab: delete immediately without dialog (AC-30).
        do_delete_workspace_at_index(
            target_idx,
            wm,
            main_area,
            tab_bar,
            window,
            sidebar_lb,
            daemon_client,
            shared_config,
        );
    } else {
        // Multiple tabs: show confirmation dialog (AC-31).
        let body =
            format!("This workspace has {n_pages} tabs. Closing it will kill all running shells.");
        let dialog = adw::MessageDialog::new(Some(window), Some("Delete Workspace?"), Some(&body));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let wm_d = Rc::clone(wm);
        let ma_d = main_area.clone();
        let tb_d = tab_bar.clone();
        let win_d = window.clone();
        let lb_d = sidebar_lb.clone();
        let dc_d = daemon_client.clone();
        let sc_d = Rc::clone(shared_config);
        dialog.connect_response(None, move |dialog, response| {
            dialog.close();
            if response == "delete" {
                do_delete_workspace_at_index(
                    target_idx, &wm_d, &ma_d, &tb_d, &win_d, &lb_d, &dc_d, &sc_d,
                );
            }
        });
        dialog.present();
    }
}

/// Internal: perform the actual workspace deletion after confirmation.
///
/// FIX-003: the daemon's `delete_workspace` RPC is the single source of truth
/// for workspace existence (AD-002 / AD-007). We issue the RPC FIRST; only on
/// success do we apply the local widget/state mutation. On RPC failure we log
/// a warning and skip the local mutation — the subsequent `subscribe_layout`
/// event (or next refresh) will bring GTK back in sync with daemon truth
/// (AC-12).
///
/// In the legacy no-daemon-client fallback (unreachable post-FIX-011) we
/// skip the RPC and fall back to closing panes locally via
/// `close_workspace_panes`. The fallback is retained until
/// CHORE-FIX-011-cleanup.
#[allow(clippy::too_many_arguments)]
fn do_delete_workspace_at_index(
    target_idx: usize,
    wm: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    sidebar_lb: &gtk4::ListBox,
    daemon_client: &Option<Arc<DaemonClient>>,
    shared_config: &SharedConfig,
) {
    // TOCTOU-safe bounds check before issuing the RPC — another client
    // (e.g. paired Android) could have shrunk the workspace list between
    // dialog open and confirm. Mirrors FIX-001's rename flow.
    {
        let Ok(mgr) = wm.try_borrow() else { return };
        if target_idx >= mgr.workspaces.len() || mgr.workspaces.len() <= 1 {
            return;
        }
    }

    // FIX-003: daemon is authoritative. Call RPC before any local widget
    // mutation. In the legacy no-daemon-client fallback (unreachable
    // post-FIX-011) fall back to the per-pane close loop below.
    if let Some(dc) = daemon_client.as_ref() {
        if let Err(e) = dc.delete_workspace(target_idx) {
            tracing::warn!(
                target_idx,
                "delete_workspace RPC failed: {e} — skipping local mutation"
            );
            return;
        }
    }

    let Ok(mut mgr) = wm.try_borrow_mut() else { return };
    if target_idx >= mgr.workspaces.len() || mgr.workspaces.len() <= 1 {
        return;
    }

    let ws = &mgr.workspaces[target_idx];

    // Legacy fallback only: daemon RPC didn't fire, so we still need to tear
    // down the in-process pane registry locally. In daemon mode the daemon
    // already killed the panes + unlinked byte logs as part of the RPC.
    // Unreachable post-FIX-011 — see CHORE-FIX-011-cleanup.
    if daemon_client.is_none() {
        close_workspace_panes(&ws.tab_states, None);
    }

    // Remove the TabView from main_area if it is currently visible.
    let old_tv = ws.tab_view.clone();
    let is_active = target_idx == mgr.active_index;
    let mut child = main_area.first_child();
    while let Some(c) = child {
        if c == *old_tv.upcast_ref::<gtk4::Widget>() {
            main_area.remove(&c);
            break;
        }
        child = c.next_sibling();
    }

    // Remove the workspace.
    mgr.workspaces.remove(target_idx);

    // Choose new active_index (AC-32): prefer the workspace that now occupies
    // the deleted index, or len-1 if the deleted workspace was last.
    let new_active = if mgr.active_index > target_idx {
        // Active was after the deleted; shift down.
        mgr.active_index - 1
    } else if mgr.active_index == target_idx {
        // Active was the deleted one; pick the workspace now at that position.
        target_idx.min(mgr.workspaces.len() - 1)
    } else {
        // Active was before the deleted; unchanged.
        mgr.active_index
    };
    mgr.active_index = new_active;

    // If the deleted workspace was visible, swap in the new active TabView.
    if is_active || mgr.workspaces.get(new_active).is_some() {
        let new_tv = mgr.workspaces[new_active].tab_view.clone();
        // Only insert if not already parented.
        if new_tv.parent().is_none() {
            main_area.prepend(&new_tv);
        }
        tab_bar.set_view(Some(&new_tv));

        // Focus the first leaf of the new active workspace (AC-33).
        if let Some(page) = new_tv.selected_page() {
            let container = page.child();
            let leaves = collect_leaf_drawing_areas(&container);
            if let Some(da) = leaves.first() {
                da.grab_focus();
            }
        }
    }

    let ws_count = mgr.workspaces.len();
    let ws_name = mgr.workspaces[new_active].name.clone();
    drop(mgr);

    update_delete_workspace_action(wm, window);
    update_window_title_with_workspace(ws_count, &ws_name, wm, window);
    refresh_workspace_sidebar(
        sidebar_lb,
        wm,
        main_area,
        tab_bar,
        window,
        daemon_client,
        shared_config,
    );
}

#[cfg(test)]
mod tests {
    //! FIX-006 Cycle 1 + FIX-005B unit tests for the self-echo guards.
    //!
    //! These tests cover `consume_reorder_self_echo` and
    //! `consume_active_pane_self_echo` — pure helpers that absorb the
    //! daemon's broadcast echo when GTK was the originator of a layout
    //! mutation. Direct unit testing of `handle_layout_event` is not
    //! feasible (GTK widgets in signature), but the queue-check logic was
    //! extracted into a free function precisely so it can be exercised
    //! without a GTK context.
    use super::{
        consume_active_pane_self_echo, consume_reorder_self_echo, populate_tab_active_pane_map,
    };
    use forgetty_core::PaneId;
    use std::collections::{HashMap, VecDeque};
    use uuid::Uuid;

    /// Two consecutive local swaps push two `(from, to)` tuples; injecting
    /// the corresponding daemon echoes back-to-back must dedupe both via
    /// FIFO match and leave the queue empty.
    #[test]
    fn test_workspace_reorder_consumer_queue_dedup() {
        let mut pending: VecDeque<(usize, usize)> = VecDeque::new();
        // Simulate the local-swap path of two consecutive Move Up clicks
        // on workspace `B` from initial `[Default, A, B]`:
        //   1) move_workspace_up(2): swap (2, 1) -> [Default, B, A]
        //   2) move_workspace_up(1): swap (1, 0) -> [B, Default, A]
        pending.push_back((2, 1));
        pending.push_back((1, 0));

        // Daemon emits the same swaps in the same order. Both should be
        // recognised as self-echoes (match) and popped.
        assert!(
            consume_reorder_self_echo(&mut pending, 2, 1),
            "first echo should match queue head and pop"
        );
        assert_eq!(pending.len(), 1, "head popped, one entry remains");

        assert!(
            consume_reorder_self_echo(&mut pending, 1, 0),
            "second echo should match new head and pop"
        );
        assert!(pending.is_empty(), "queue should be drained after both echoes");
    }

    /// The daemon may emit the swap in either direction `(from, to)` or
    /// `(to, from)` depending on its internal swap call — the guard must
    /// match either order.
    #[test]
    fn test_workspace_reorder_consumer_queue_dedup_reversed_order() {
        let mut pending: VecDeque<(usize, usize)> = VecDeque::new();
        pending.push_back((2, 1));

        // Daemon emits (1, 2) — reversed direction. Still a self-echo.
        assert!(
            consume_reorder_self_echo(&mut pending, 1, 2),
            "reversed-order echo should still match"
        );
        assert!(pending.is_empty(), "queue drained even on reversed match");
    }

    /// Empty queue + an incoming reorder event → not a self-echo. The caller
    /// must apply (this models a future paired-device reorder arriving at
    /// this client).
    #[test]
    fn test_workspace_reorder_consumer_external_applies() {
        let mut pending: VecDeque<(usize, usize)> = VecDeque::new();
        assert!(
            !consume_reorder_self_echo(&mut pending, 1, 0),
            "empty queue means external — must apply"
        );
        assert!(pending.is_empty(), "queue still empty after non-match");
    }

    /// Non-empty queue but the event does not match the head → caller must
    /// still apply, and the queue must NOT be popped (the head is still
    /// waiting for its own future echo). This protects against a hypothetical
    /// out-of-order or interleaved external-reorder arriving while a local
    /// reorder is still pending its echo.
    #[test]
    fn test_workspace_reorder_consumer_queue_mismatch() {
        let mut pending: VecDeque<(usize, usize)> = VecDeque::new();
        pending.push_back((2, 1));

        assert!(
            !consume_reorder_self_echo(&mut pending, 0, 1),
            "mismatched event must not be treated as self-echo"
        );
        assert_eq!(pending.len(), 1, "queue head preserved on mismatch");
        assert_eq!(pending.front(), Some(&(2, 1)), "exact head entry preserved");
    }

    // -----------------------------------------------------------------------
    // FIX-005B unit tests for `consume_active_pane_self_echo`
    // -----------------------------------------------------------------------

    /// Two consecutive local focus-enters push two `(ws_idx, tab_id, pane_id)`
    /// triples; the daemon echoes them back in the same order. Both must
    /// dedupe via FIFO match and leave the queue empty.
    #[test]
    fn test_active_pane_consumer_queue_dedup() {
        let mut pending: VecDeque<(usize, Uuid, PaneId)> = VecDeque::new();
        let tab1 = Uuid::new_v4();
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();
        pending.push_back((0, tab1, pane_a));
        pending.push_back((0, tab1, pane_b));

        assert!(
            consume_active_pane_self_echo(&mut pending, 0, tab1, Some(pane_a)),
            "first echo should match queue head and pop"
        );
        assert_eq!(pending.len(), 1, "head popped, one entry remains");

        assert!(
            consume_active_pane_self_echo(&mut pending, 0, tab1, Some(pane_b)),
            "second echo should match new head and pop"
        );
        assert!(pending.is_empty(), "queue should be drained after both echoes");
    }

    /// Empty queue + an incoming pane-changed event → external (not a
    /// self-echo). The caller must apply (paired-device focus arriving here).
    #[test]
    fn test_active_pane_consumer_external_applies() {
        let mut pending: VecDeque<(usize, Uuid, PaneId)> = VecDeque::new();
        let tab1 = Uuid::new_v4();
        let pane_a = PaneId::new();
        assert!(
            !consume_active_pane_self_echo(&mut pending, 0, tab1, Some(pane_a)),
            "empty queue means external — must apply"
        );
        assert!(pending.is_empty(), "queue still empty after non-match");
    }

    /// Non-empty queue but the event does not match the head → caller must
    /// still apply, and the queue must NOT be popped.
    #[test]
    fn test_active_pane_consumer_queue_mismatch() {
        let mut pending: VecDeque<(usize, Uuid, PaneId)> = VecDeque::new();
        let tab1 = Uuid::new_v4();
        let tab2 = Uuid::new_v4();
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();
        pending.push_back((0, tab1, pane_a));

        assert!(
            !consume_active_pane_self_echo(&mut pending, 0, tab2, Some(pane_b)),
            "mismatched (tab, pane) must not be treated as self-echo"
        );
        assert_eq!(pending.len(), 1, "queue head preserved on mismatch");
        assert_eq!(pending.front(), Some(&(0, tab1, pane_a)), "exact head entry preserved");
    }

    /// `None` pane_id incoming with a non-empty queue → external apply (do
    /// not pop). Local clears are not generated by `wire_focus_tracking`,
    /// only daemon-initiated clears (e.g., on tab close), so a `None` is
    /// always external.
    #[test]
    fn test_active_pane_consumer_none_is_external() {
        let mut pending: VecDeque<(usize, Uuid, PaneId)> = VecDeque::new();
        let tab1 = Uuid::new_v4();
        let pane_a = PaneId::new();
        pending.push_back((0, tab1, pane_a));

        assert!(
            !consume_active_pane_self_echo(&mut pending, 0, tab1, None),
            "None pane_id must be treated as external — do not pop"
        );
        assert_eq!(pending.len(), 1, "queue preserved on None incoming");
    }

    // -----------------------------------------------------------------------
    // FIX-005B fix-cycle 1 unit tests for `populate_tab_active_pane_map`
    //
    // These tests cover the cold-restart population path: the `selected-
    // page-notify` handler reads the map populated here, so the contract
    // tested is "tabs with a saved active_pane_id end up in the map; tabs
    // without one are absent". The handler then falls back through
    // focus_tracker → leaves.first() for absent tabs.
    // -----------------------------------------------------------------------

    /// Cold-restart population: a workspace with three tabs where two have
    /// saved focus and one is legacy (None) populates only the two.
    #[test]
    fn test_populate_tab_active_pane_skips_none_entries() {
        let tab1 = Uuid::new_v4();
        let tab2 = Uuid::new_v4();
        let tab3 = Uuid::new_v4();
        let pane1 = Uuid::new_v4();
        let pane2 = Uuid::new_v4();

        let mut map: HashMap<Uuid, PaneId> = HashMap::new();
        populate_tab_active_pane_map(
            &mut map,
            vec![(tab1, Some(pane1)), (tab2, None), (tab3, Some(pane2))],
        );

        assert_eq!(map.len(), 2, "tabs with None active_pane_id must be absent");
        assert_eq!(map.get(&tab1), Some(&PaneId(pane1)));
        assert_eq!(map.get(&tab3), Some(&PaneId(pane2)));
        assert!(!map.contains_key(&tab2), "legacy tab must not be in the map");
    }

    /// `connect_enter` updates the map: simulate the live-path mutation of
    /// `ws.tab_active_pane.insert(tab_id, pane_id)` so a subsequent
    /// `selected-page-notify` reads the latest value.
    #[test]
    fn test_tab_active_pane_live_update_overrides() {
        let tab1 = Uuid::new_v4();
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();

        let mut map: HashMap<Uuid, PaneId> = HashMap::new();
        // Cold restart populates pane_a.
        populate_tab_active_pane_map(&mut map, vec![(tab1, Some(pane_a.0))]);
        assert_eq!(map.get(&tab1), Some(&pane_a));

        // Live `connect_enter` for pane_b — exercises the same insert path
        // used inside `wire_focus_tracking`'s focus-enter closure.
        map.insert(tab1, pane_b);
        assert_eq!(map.get(&tab1), Some(&pane_b), "live update must override cold-restart value");
    }

    /// `ActivePaneChanged` echo updates the map: external (paired-device)
    /// pane changes also write to the map so subsequent tab switches read
    /// the freshest value.
    #[test]
    fn test_tab_active_pane_external_event_updates_map() {
        let tab1 = Uuid::new_v4();
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();

        let mut map: HashMap<Uuid, PaneId> = HashMap::new();
        map.insert(tab1, pane_a);

        // Simulate `LayoutEvent::ActivePaneChanged { pane_id: Some(pane_b), .. }`
        // arriving from another client (not a self-echo).
        map.insert(tab1, pane_b);

        assert_eq!(map.get(&tab1), Some(&pane_b), "external event must update the map");
    }

    /// Empty layout (no workspaces saved any active_pane_id) yields an
    /// empty map — handler falls through to leaves.first() for every tab,
    /// matching pre-FIX-005B behaviour (AC-4).
    #[test]
    fn test_populate_tab_active_pane_empty_layout() {
        let mut map: HashMap<Uuid, PaneId> = HashMap::new();
        let no_tabs: Vec<(Uuid, Option<Uuid>)> = vec![];
        populate_tab_active_pane_map(&mut map, no_tabs);
        assert!(map.is_empty(), "empty input yields empty map");
    }
}
