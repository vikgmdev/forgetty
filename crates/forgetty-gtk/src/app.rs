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
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use std::path::PathBuf;

use crate::daemon_client::{DaemonClient, PaneInfo};
use crate::terminal::NotificationPayload;
use forgetty_config::{load_config, Config, NotificationMode};
use forgetty_watcher::ConfigWatcher;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use tracing::info;

use forgetty_workspace::{self, PaneTreeState, TabState, Workspace, WorkspaceState};

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

/// Interval for auto-saving the session file (seconds).
const AUTO_SAVE_SECS: u64 = 30;

// POSIX signal numbers — stable constants, avoids adding a `libc` dependency.
/// Hangup (terminal closed, session leader death).
const SIGHUP: i32 = 1;
/// Interrupt (Ctrl+C from controlling terminal).
const SIGINT: i32 = 2;
/// Termination request (default `kill` signal).
const SIGTERM: i32 = 15;

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
}

/// Shared state tracking all workspaces and which is active.
type WorkspaceManager = Rc<RefCell<WorkspaceManagerInner>>;

struct WorkspaceManagerInner {
    workspaces: Vec<WorkspaceView>,
    active_index: usize,
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
}

/// Determine the default socket path for the daemon.
fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("forgetty.sock")
    } else {
        PathBuf::from("/tmp/forgetty.sock")
    }
}

/// Try to connect to the running daemon; if not running, spawn it and retry.
///
/// Returns `Some(DaemonClient)` on success, `None` if daemon is unavailable
/// (in which case GTK falls back to self-contained PTY mode).
fn ensure_daemon(socket_path: &std::path::Path) -> Option<Arc<DaemonClient>> {
    // 1. Try to connect immediately (daemon may already be running).
    if let Ok(dc) = DaemonClient::connect(socket_path) {
        info!("ensure_daemon: connected to existing daemon at {:?}", socket_path);
        return Some(Arc::new(dc));
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
        tracing::warn!(
            "ensure_daemon: forgetty-daemon binary not found; falling back to local PTY mode"
        );
        return None;
    };

    // 3. Spawn the daemon (non-blocking; store child to prevent Drop from killing it).
    match std::process::Command::new(&daemon_path).spawn() {
        Ok(child) => {
            // Leak the child handle so Drop is never called, keeping the process alive.
            std::mem::forget(child);
            info!("ensure_daemon: spawned {:?}", daemon_path);
        }
        Err(e) => {
            tracing::warn!(
                "ensure_daemon: failed to spawn daemon: {e}; falling back to local PTY mode"
            );
            return None;
        }
    }

    // 4. Retry with exponential-ish backoff (up to ~1s total).
    for attempt in 0..20 {
        let delay_ms = if attempt < 5 { 25 } else { 50 };
        std::thread::sleep(Duration::from_millis(delay_ms));
        if let Ok(dc) = DaemonClient::connect(socket_path) {
            info!("ensure_daemon: connected after {} attempt(s)", attempt + 1);
            return Some(Arc::new(dc));
        }
    }

    tracing::warn!(
        "ensure_daemon: daemon did not become ready in time; falling back to local PTY mode"
    );
    None
}

/// Run the GTK4/libadwaita application.
///
/// This function blocks until the window is closed. It initialises libadwaita,
/// creates the main application window with CSD header bar, and enters the
/// GTK main loop.
pub fn run(config: Config, launch: LaunchOptions) -> Result<(), Box<dyn std::error::Error>> {
    let app_id = launch.class.as_deref().unwrap_or(APP_ID);

    // Attempt to connect to (or spawn) the daemon before entering the GTK loop.
    // This is done outside connect_activate so it runs once, not once per window.
    let socket_path = default_socket_path();
    let daemon_client: Option<Arc<DaemonClient>> = ensure_daemon(&socket_path);
    if daemon_client.is_some() {
        info!("GTK running in daemon-client mode: sessions survive window close");
    } else {
        info!("GTK running in self-contained mode: no daemon, PTYs owned locally");
    }

    let app = adw::Application::builder()
        .application_id(app_id)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(move |app| {
        build_ui(app, &config, &launch, daemon_client.clone());
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

/// Build the main application window with tab bar and initial terminal tab.
fn build_ui(
    app: &adw::Application,
    config: &Config,
    launch: &LaunchOptions,
    daemon_client: Option<Arc<DaemonClient>>,
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
    window_tab_section.append(Some("New Window"), Some("win.new-window"));
    window_tab_section.append(Some("Close Window"), Some("win.close-window"));
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
    let ws_selector_item =
        gio::MenuItem::new(Some("Workspace Selector"), Some("win.workspace-selector"));
    ws_selector_item.set_attribute_value("accel", Some(&"<Control><Alt>w".to_variant()));
    workspace_section.append_item(&ws_selector_item);
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

    // Section 5 -- Terminal operations
    let terminal_section = gio::Menu::new();
    terminal_section.append(Some("Clear"), Some("win.clear"));
    terminal_section.append(Some("Reset"), Some("win.reset"));
    menu.append_section(None, &terminal_section);

    // Section 6 -- Configuration & Help
    let config_help_section = gio::Menu::new();
    let cmd_palette_item = gio::MenuItem::new(Some("Command Palette"), Some("win.command-palette"));
    cmd_palette_item.set_attribute_value("accel", Some(&"<Control><Shift>p".to_variant()));
    config_help_section.append_item(&cmd_palette_item);
    config_help_section.append(Some("Terminal Inspector"), Some("win.terminal-inspector"));
    config_help_section.append(Some("Open Configuration"), Some("win.open-config"));
    config_help_section.append(Some("Reload Configuration"), Some("win.reload-config"));
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

    // --- Dropdown menu button (split actions + new tab) ---
    let dropdown_menu = gio::Menu::new();
    let new_tab_item = gio::MenuItem::new(Some("New Tab"), Some("win.new-tab"));
    new_tab_item.set_attribute_value("accel", Some(&"<Control><Shift>t".to_variant()));
    dropdown_menu.append_item(&new_tab_item);

    let split_section = gio::Menu::new();
    split_section.append(Some("Split Up"), Some("win.split-up"));
    let sd = gio::MenuItem::new(Some("Split Down"), Some("win.split-down"));
    sd.set_attribute_value("accel", Some(&"<Alt><Shift>minus".to_variant()));
    split_section.append_item(&sd);
    split_section.append(Some("Split Left"), Some("win.split-left"));
    let sr = gio::MenuItem::new(Some("Split Right"), Some("win.split-right"));
    sr.set_attribute_value("accel", Some(&"<Alt><Shift>equal".to_variant()));
    split_section.append_item(&sr);
    dropdown_menu.append_section(None, &split_section);

    let dropdown_button = gtk4::MenuButton::new();
    dropdown_button.set_icon_name("pan-down-symbolic");
    dropdown_button.set_menu_model(Some(&dropdown_menu));
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
    main_area.append(&initial_tab_view);

    // Wrap main_area in an Overlay so the command palette can float on top.
    let main_overlay = gtk4::Overlay::new();
    main_overlay.set_child(Some(&main_area));
    main_overlay.set_vexpand(true);

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&tab_bar);
    content.append(&main_overlay);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Forgetty")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&content)
        .build();

    // --- Workspace Manager ---
    // Holds all workspaces with their per-workspace GTK state.
    // The initial workspace is created here; additional workspaces are added
    // via the "New Workspace" action or session restore.
    let initial_tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));
    let initial_focus_tracker: FocusTracker = Rc::new(RefCell::new(String::new()));
    let initial_custom_titles: CustomTitles = Rc::new(RefCell::new(HashSet::new()));

    let workspace_manager: WorkspaceManager = Rc::new(RefCell::new(WorkspaceManagerInner {
        workspaces: vec![WorkspaceView {
            id: uuid::Uuid::new_v4(),
            name: String::from("Default"),
            tab_view: initial_tab_view.clone(),
            tab_states: Rc::clone(&initial_tab_states),
            focus_tracker: Rc::clone(&initial_focus_tracker),
            custom_titles: Rc::clone(&initial_custom_titles),
        }],
        active_index: 0,
    }));

    // Convenience aliases for the active workspace's state during initial setup.
    // These point to the initial workspace; after session restore they may be
    // replaced if the default workspace is rebuilt.
    let tab_states = Rc::clone(&initial_tab_states);
    let focus_tracker = Rc::clone(&initial_focus_tracker);
    let _custom_titles = Rc::clone(&initial_custom_titles);

    // Shared config -- updated on hot reload, read by new tab/split creation.
    // All action closures that create terminals capture a clone of this Rc.
    let shared_config: SharedConfig = Rc::new(RefCell::new(config.clone()));

    // --- Settings sidebar (right panel, built after shared state is ready) ---
    // Use the workspace manager for config apply so it hits all workspaces.
    let appearance_revealer = preferences::build_appearance_sidebar(
        &shared_config,
        &tab_states,
        &window,
        daemon_client.clone(),
    );
    main_area.append(&appearance_revealer);

    // --- Command palette overlay (built after workspace_manager is ready) ---
    let command_palette = build_command_palette(&window, &workspace_manager);
    main_overlay.add_overlay(&command_palette);

    // --- Workspace selector overlay ---
    let (workspace_selector, workspace_selector_lb) =
        build_workspace_selector(&workspace_manager, &main_area, &tab_bar, &window);
    main_overlay.add_overlay(&workspace_selector);

    // Click-outside-to-close: a GestureClick on the overlay detects clicks
    // that land outside the palette card and closes it.
    {
        let palette_ref = command_palette.clone();
        let wm_click = Rc::clone(&workspace_manager);
        let click_gesture = gtk4::GestureClick::new();
        click_gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
        click_gesture.connect_pressed(move |gesture, _n_press, x, y| {
            if !palette_ref.is_visible() {
                gesture.set_state(gtk4::EventSequenceState::None);
                return;
            }
            // Check if the click is inside the palette widget bounds
            let Some(parent) = palette_ref.parent() else {
                gesture.set_state(gtk4::EventSequenceState::None);
                return;
            };
            let Some(bounds) = palette_ref.compute_bounds(&parent) else {
                gesture.set_state(gtk4::EventSequenceState::None);
                return;
            };
            let inside = x >= bounds.x() as f64
                && x <= (bounds.x() + bounds.width()) as f64
                && y >= bounds.y() as f64
                && y <= (bounds.y() + bounds.height()) as f64;

            if !inside {
                let ft = active_focus_tracker(&wm_click);
                close_command_palette(&palette_ref, &ft);
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            } else {
                gesture.set_state(gtk4::EventSequenceState::None);
            }
        });
        main_overlay.add_controller(click_gesture);
    }

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
        initial_tab_view.connect_close_page(move |tv, page| {
            let container = page.child();
            if let Some(ref dc) = dc_close {
                // Daemon mode: send close_tab RPC for each pane in the subtree.
                daemon_close_panes_in_subtree(&container, &states_close, dc);
            } else {
                kill_all_panes_in_subtree(&container, &states_close);
            }

            // If this is the last page, close the window
            if tv.n_pages() <= 1 {
                window_close.close();
            }

            // Confirm the close
            tv.close_page_finish(page, true);

            // Inhibit default close handling since we called close_page_finish
            glib::Propagation::Stop
        });
    }

    // --- Focus management on tab switch ---
    // When switching tabs, find a leaf DrawingArea in the new tab and focus it.
    {
        let focus_switch = Rc::clone(&focus_tracker);
        initial_tab_view.connect_selected_page_notify(move |tv| {
            if let Some(page) = tv.selected_page() {
                let container = page.child();
                // Try to focus the pane that was last focused in this tab,
                // otherwise just focus the first leaf.
                let focused_name = focus_switch.borrow().clone();
                let leaves = collect_leaf_drawing_areas(&container);
                let target = leaves
                    .iter()
                    .find(|da| da.widget_name().as_str() == focused_name)
                    .or_else(|| leaves.first());
                if let Some(da) = target {
                    da.grab_focus();
                }
            }
        });
    }

    // --- New tab action (Ctrl+Shift+T) ---
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
            let ws = &mgr.workspaces[mgr.active_index];
            add_new_tab(
                &ws.tab_view,
                &cfg,
                &ws.tab_states,
                &ws.focus_tracker,
                &ws.custom_titles,
                &win_action,
                None,
                None,
                dc_newtab.clone(),
            );
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.new-tab", &["<Control><Shift>t"]);

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
            let Ok(mgr) = wm_close.try_borrow() else {
                return;
            };
            let ws = &mgr.workspaces[mgr.active_index];
            close_focused_pane(
                &ws.tab_view,
                &ws.tab_states,
                &ws.focus_tracker,
                &window_close,
                dc_closepane.clone(),
            );
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
            paste_clipboard(&ts, &ft, &window_paste, dc_paste.clone());
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
        &["F1", "<Control>question", "<Control><Shift>slash"],
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
    {
        let action = gio::SimpleAction::new("new-window", None);
        action.connect_activate(move |_action, _param| {
            if let Ok(exe) = std::env::current_exe() {
                if let Err(e) = std::process::Command::new(exe).spawn() {
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
                kill_all_panes_in_subtree(&container, &ws.tab_states);
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
            write_to_focused_pty_or_daemon(
                &ws.tab_states,
                &ws.focus_tracker,
                b"\x0c",
                dc_clear.as_deref(),
            );
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
            write_to_focused_pty_or_daemon(
                &ws.tab_states,
                &ws.focus_tracker,
                b"\x0c",
                dc_reset.as_deref(),
            );
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

    // --- Quit action (Ctrl+Shift+Q) ---
    // CRITICAL: save session BEFORE killing PTYs (CWD read needs /proc/{pid}/cwd).
    // In daemon mode: do NOT kill daemon PTYs — sessions survive the quit.
    {
        let app_quit = app.clone();
        let wm_quit = Rc::clone(&workspace_manager);
        let win_quit_save = window.clone();
        let skip_save_quit = Rc::clone(&skip_session_save);
        let dc_quit = daemon_client.clone();
        let action = gio::SimpleAction::new("quit", None);
        action.connect_activate(move |_action, _param| {
            if !skip_save_quit.get() {
                save_all_workspaces(&wm_quit, &win_quit_save);
            }
            // Only kill PTYs in self-contained mode (no daemon).
            if dc_quit.is_none() {
                kill_all_workspace_ptys(&wm_quit, "Quit action");
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
        let action = gio::SimpleAction::new("new-workspace", None);
        action.connect_activate(move |_action, _param| {
            show_new_workspace_dialog(&win_new, &wm_new, &cfg_new, &main_area_new, &tab_bar_new);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.new-workspace", &["<Control><Alt>n"]);

    // --- Rename Workspace action (menu only) ---
    {
        let wm_rename = Rc::clone(&workspace_manager);
        let win_rename = window.clone();
        let action = gio::SimpleAction::new("rename-workspace", None);
        action.connect_activate(move |_action, _param| {
            show_rename_workspace_dialog(&win_rename, &wm_rename);
        });
        window.add_action(&action);
    }

    // --- Delete Workspace action (menu only) ---
    {
        let wm_delete = Rc::clone(&workspace_manager);
        let main_area_del = main_area.clone();
        let tab_bar_del = tab_bar.clone();
        let win_del = window.clone();
        let delete_action = gio::SimpleAction::new("delete-workspace", None);
        {
            // Disable if only one workspace
            let has_multiple =
                workspace_manager.try_borrow().map(|mgr| mgr.workspaces.len() > 1).unwrap_or(false);
            delete_action.set_enabled(has_multiple);
        }
        delete_action.connect_activate(move |_action, _param| {
            delete_current_workspace(&wm_delete, &main_area_del, &tab_bar_del, &win_del);
        });
        window.add_action(&delete_action);
    }

    // --- Switch Workspace by index (Ctrl+Alt+1 through 9) ---
    for i in 1..=9u32 {
        let wm_switch = Rc::clone(&workspace_manager);
        let main_area_sw = main_area.clone();
        let tab_bar_sw = tab_bar.clone();
        let win_sw = window.clone();
        let action_name = format!("switch-workspace-{i}");
        let action = gio::SimpleAction::new(&action_name, None);
        action.connect_activate(move |_action, _param| {
            let target = (i - 1) as usize;
            switch_workspace(&wm_switch, target, &main_area_sw, &tab_bar_sw, &win_sw);
        });
        window.add_action(&action);
        app.set_accels_for_action(
            &format!("win.switch-workspace-{i}"),
            &[&format!("<Control><Alt>{i}")],
        );
    }

    // --- Previous Workspace (Ctrl+Alt+Left) ---
    {
        let wm_prev = Rc::clone(&workspace_manager);
        let main_area_prev = main_area.clone();
        let tab_bar_prev = tab_bar.clone();
        let win_prev = window.clone();
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
            switch_workspace(&wm_prev, target, &main_area_prev, &tab_bar_prev, &win_prev);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.prev-workspace", &["<Control><Alt>Page_Up"]);

    // --- Next Workspace (Ctrl+Alt+Right) ---
    {
        let wm_next = Rc::clone(&workspace_manager);
        let main_area_next = main_area.clone();
        let tab_bar_next = tab_bar.clone();
        let win_next = window.clone();
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
            switch_workspace(&wm_next, target, &main_area_next, &tab_bar_next, &win_next);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.next-workspace", &["<Control><Alt>Page_Down"]);

    // --- Workspace Selector overlay (Ctrl+Alt+W) ---
    {
        let wm_selector = Rc::clone(&workspace_manager);
        let selector_ref = workspace_selector.clone();
        let lb_ref = workspace_selector_lb.clone();
        let palette_ref_ws = command_palette.clone();
        let wm_ws = Rc::clone(&workspace_manager);
        let action = gio::SimpleAction::new("workspace-selector", None);
        action.connect_activate(move |_action, _param| {
            // Close command palette if open
            if palette_ref_ws.is_visible() {
                let ft = active_focus_tracker(&wm_ws);
                close_command_palette(&palette_ref_ws, &ft);
            }
            toggle_workspace_selector(&selector_ref, &lb_ref, &wm_selector);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.workspace-selector", &["<Control><Alt>w"]);

    // --- Command Palette action (Ctrl+Shift+P) ---
    {
        let palette_ref = command_palette.clone();
        let wm_palette = Rc::clone(&workspace_manager);
        let win_ref = window.clone();
        let action = gio::SimpleAction::new("command-palette", None);
        action.connect_activate(move |_action, _param| {
            let ft = active_focus_tracker(&wm_palette);
            toggle_command_palette(&palette_ref, &ft, &win_ref);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.command-palette", &["<Control><Shift>p"]);

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
        // Daemon mode: session-file-ordered reconnect.
        //
        // Algorithm:
        //   1. Call list_tabs → build UUID→PaneInfo map of live daemon panes.
        //   2. Load session file → ordered tab list with pane_ids.
        //   3. For each session tab:
        //      - pane_id matches live map  → reconnect it (remove from map)
        //      - pane gone / no pane_id   → create a fresh daemon pane in its slot
        //   4. Append remaining live panes (opened since last save) as extra tabs.
        //   5. No session file → fall through to "create one new tab" path.
        match dc.list_tabs() {
            Ok(live_panes) if !live_panes.is_empty() => {
                tracing::info!("Reconnecting to {} live daemon pane(s)", live_panes.len());
                let Ok(mgr) = workspace_manager.try_borrow() else {
                    tracing::warn!("Failed to borrow workspace_manager for daemon reconcile");
                    return;
                };
                let ws = &mgr.workspaces[0];

                // UUID→PaneInfo map for O(1) session-file matching.
                let mut pane_map: HashMap<uuid::Uuid, PaneInfo> =
                    live_panes.into_iter().map(|p| (p.pane_id.0, p)).collect();

                // Load the session file to get ordered tabs (may be absent).
                let session_tabs: Vec<TabState> = forgetty_workspace::load_session()
                    .ok()
                    .flatten()
                    .and_then(|s| s.workspaces.into_iter().next())
                    .map(|w| w.tabs)
                    .unwrap_or_default();

                if !session_tabs.is_empty() {
                    // Reconnect each session tab, preserving its split layout.
                    for tab in &session_tabs {
                        // Legacy compat: T-055 session files stored the pane UUID at the
                        // tab level instead of per-leaf. Pass it as fallback to
                        // reconnect_pane_tree so those files still reconnect correctly.
                        let legacy_pane_id = tab.pane_id;
                        let Some((root_widget, first_da)) = reconnect_pane_tree(
                            &tab.pane_tree,
                            &mut pane_map,
                            dc,
                            config,
                            &ws.tab_states,
                            &ws.focus_tracker,
                            &ws.custom_titles,
                            &window,
                            &ws.tab_view,
                            legacy_pane_id,
                        ) else {
                            tracing::warn!("reconnect_pane_tree failed for tab {:?}", tab.title);
                            continue;
                        };

                        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                        container.set_hexpand(true);
                        container.set_vexpand(true);
                        container.append(&root_widget);
                        let page = ws.tab_view.append(&container);
                        let tab_title = if tab.title.is_empty() { "shell" } else { &tab.title };
                        page.set_title(tab_title);
                        ws.tab_view.set_selected_page(&page);
                        first_da.grab_focus();
                        register_title_timer(
                            &page,
                            &ws.tab_view,
                            &ws.tab_states,
                            &ws.focus_tracker,
                            &ws.custom_titles,
                            &window,
                        );
                        restored = true;
                    }

                    // Append any live daemon panes not referenced by any session leaf.
                    for info in pane_map.into_values() {
                        let title = if info.title.is_empty() {
                            "shell".to_string()
                        } else {
                            info.title.clone()
                        };
                        let cwd = if info.cwd.is_empty() { None } else { Some(info.cwd.clone()) };
                        let (mpsc_tx, mpsc_rx) = std::sync::mpsc::channel::<Vec<u8>>();
                        if let Err(e) = dc.subscribe_output(info.pane_id, mpsc_tx) {
                            tracing::warn!(
                                "subscribe_output failed for orphan {}: {e}",
                                info.pane_id
                            );
                        }
                        let on_exit = make_on_exit_callback(
                            &ws.tab_view,
                            &ws.tab_states,
                            &window,
                            Some(Arc::clone(dc)),
                        );
                        let on_notify =
                            make_on_notify_callback(&ws.tab_view, &ws.tab_states, &window);
                        let snapshot = dc.get_screen(info.pane_id).ok();
                        let daemon_cwd = cwd.as_ref().map(PathBuf::from);
                        match terminal::create_terminal_for_pane(
                            config,
                            info.pane_id,
                            Arc::clone(dc),
                            mpsc_rx,
                            snapshot.as_ref(),
                            daemon_cwd,
                            Some(on_exit),
                            Some(on_notify),
                        ) {
                            Ok((pane_vbox, drawing_area, state)) => {
                                let pane_widget_name = next_pane_id();
                                drawing_area.set_widget_name(&pane_widget_name);
                                ws.tab_states
                                    .borrow_mut()
                                    .insert(pane_widget_name, Rc::clone(&state));
                                wire_focus_tracking(
                                    &drawing_area,
                                    &ws.focus_tracker,
                                    &ws.tab_view,
                                    &ws.tab_states,
                                    &ws.custom_titles,
                                );
                                let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                                container.set_hexpand(true);
                                container.set_vexpand(true);
                                container.append(&pane_vbox);
                                let page = ws.tab_view.append(&container);
                                page.set_title(&title);
                                ws.tab_view.set_selected_page(&page);
                                drawing_area.grab_focus();
                                register_title_timer(
                                    &page,
                                    &ws.tab_view,
                                    &ws.tab_states,
                                    &ws.focus_tracker,
                                    &ws.custom_titles,
                                    &window,
                                );
                                restored = true;
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to create terminal for orphan pane {}: {e}",
                                    info.pane_id
                                );
                            }
                        }
                    }
                } else {
                    // No session file or empty → use live panes in daemon order as flat tabs.
                    for info in pane_map.into_values() {
                        let title = if info.title.is_empty() {
                            "shell".to_string()
                        } else {
                            info.title.clone()
                        };
                        let cwd = if info.cwd.is_empty() { None } else { Some(info.cwd.clone()) };
                        let (mpsc_tx, mpsc_rx) = std::sync::mpsc::channel::<Vec<u8>>();
                        if let Err(e) = dc.subscribe_output(info.pane_id, mpsc_tx) {
                            tracing::warn!("subscribe_output failed for {}: {e}", info.pane_id);
                        }
                        let on_exit = make_on_exit_callback(
                            &ws.tab_view,
                            &ws.tab_states,
                            &window,
                            Some(Arc::clone(dc)),
                        );
                        let on_notify =
                            make_on_notify_callback(&ws.tab_view, &ws.tab_states, &window);
                        let snapshot = dc.get_screen(info.pane_id).ok();
                        let daemon_cwd = cwd.as_ref().map(PathBuf::from);
                        match terminal::create_terminal_for_pane(
                            config,
                            info.pane_id,
                            Arc::clone(dc),
                            mpsc_rx,
                            snapshot.as_ref(),
                            daemon_cwd,
                            Some(on_exit),
                            Some(on_notify),
                        ) {
                            Ok((pane_vbox, drawing_area, state)) => {
                                let pane_widget_name = next_pane_id();
                                drawing_area.set_widget_name(&pane_widget_name);
                                ws.tab_states
                                    .borrow_mut()
                                    .insert(pane_widget_name, Rc::clone(&state));
                                wire_focus_tracking(
                                    &drawing_area,
                                    &ws.focus_tracker,
                                    &ws.tab_view,
                                    &ws.tab_states,
                                    &ws.custom_titles,
                                );
                                let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                                container.set_hexpand(true);
                                container.set_vexpand(true);
                                container.append(&pane_vbox);
                                let page = ws.tab_view.append(&container);
                                page.set_title(&title);
                                ws.tab_view.set_selected_page(&page);
                                drawing_area.grab_focus();
                                register_title_timer(
                                    &page,
                                    &ws.tab_view,
                                    &ws.tab_states,
                                    &ws.focus_tracker,
                                    &ws.custom_titles,
                                    &window,
                                );
                                restored = true;
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to create terminal for daemon pane {}: {e}",
                                    info.pane_id
                                );
                            }
                        }
                    }
                }
            }
            Ok(_) => {
                tracing::info!("Daemon has no live panes — attempting cold-start session restore");
                let Ok(mgr) = workspace_manager.try_borrow() else {
                    return;
                };
                let ws = &mgr.workspaces[0];

                let session_tabs: Vec<TabState> = forgetty_workspace::load_session()
                    .ok()
                    .flatten()
                    .and_then(|s| s.workspaces.into_iter().next())
                    .map(|w| w.tabs)
                    .unwrap_or_default();

                if !session_tabs.is_empty() {
                    let mut pane_map: HashMap<uuid::Uuid, PaneInfo> = HashMap::new(); // empty — cold start

                    for tab in &session_tabs {
                        let legacy_pane_id = tab.pane_id;
                        let Some((root_widget, first_da)) = reconnect_pane_tree(
                            &tab.pane_tree,
                            &mut pane_map,
                            dc,
                            config,
                            &ws.tab_states,
                            &ws.focus_tracker,
                            &ws.custom_titles,
                            &window,
                            &ws.tab_view,
                            legacy_pane_id,
                        ) else {
                            tracing::warn!("reconnect_pane_tree failed for tab {:?}", tab.title);
                            continue;
                        };

                        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                        container.set_hexpand(true);
                        container.set_vexpand(true);
                        container.append(&root_widget);
                        let page = ws.tab_view.append(&container);
                        page.set_title(if tab.title.is_empty() { "shell" } else { &tab.title });
                        ws.tab_view.set_selected_page(&page);
                        first_da.grab_focus();
                        register_title_timer(
                            &page,
                            &ws.tab_view,
                            &ws.tab_states,
                            &ws.focus_tracker,
                            &ws.custom_titles,
                            &window,
                        );
                        restored = true;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("list_tabs RPC failed: {e} — creating initial tab locally");
            }
        }
    } else if !has_cli_override {
        // Self-contained mode: restore from session file.
        match forgetty_workspace::load_session() {
            Ok(Some(state)) => {
                let has_tabs = state.workspaces.iter().any(|ws| !ws.tabs.is_empty());
                if !has_tabs {
                    tracing::info!("Session file has no tabs, opening default tab");
                } else {
                    tracing::info!(
                        "Restoring saved session ({} workspace(s))",
                        state.workspaces.len()
                    );
                    restored = restore_all_workspaces(
                        &state,
                        &workspace_manager,
                        config,
                        &main_area,
                        &tab_bar,
                        &window,
                        None,
                    );
                }
            }
            Ok(None) => {
                tracing::debug!("No session file found, opening default tab");
            }
            Err(e) => {
                tracing::warn!("Failed to load session ({e}), opening default tab");
            }
        }
    }

    if !restored {
        // Add a tab to the default workspace.
        let Ok(mgr) = workspace_manager.try_borrow() else {
            return;
        };
        let ws = &mgr.workspaces[0];
        add_new_tab(
            &ws.tab_view,
            config,
            &ws.tab_states,
            &ws.focus_tracker,
            &ws.custom_titles,
            &window,
            launch.working_directory.as_deref(),
            launch.command.as_deref(),
            daemon_client.clone(),
        );
        drop(mgr);
    }

    // Update the window title based on workspace count.
    update_window_title_for_workspace(&workspace_manager, &window);

    // --- Window close request handler ---
    // Fires when the user clicks the CSD X button, when window.close() is
    // called programmatically, or when the window manager requests a close.
    // CRITICAL: In daemon mode, do NOT kill PTYs — sessions must survive the close.
    // In self-contained mode: save session then kill PTYs as before.
    {
        let wm_close = Rc::clone(&workspace_manager);
        let win_close_save = window.clone();
        let skip_save_close = Rc::clone(&skip_session_save);
        let dc_window_close = daemon_client.clone();
        window.connect_close_request(move |_win| {
            // Save session in both modes (daemon needs it for ordered reconnect).
            if !skip_save_close.get() {
                save_all_workspaces(&wm_close, &win_close_save);
            }
            if dc_window_close.is_none() {
                // Self-contained mode only: also kill PTYs.
                kill_all_workspace_ptys(&wm_close, "Window close request");
            }
            // Daemon mode: PTY sessions survive the GTK close.
            glib::Propagation::Proceed
        });
    }

    // --- Unix signal handlers (SIGTERM, SIGHUP, SIGINT) ---
    // Registered via glib::unix_signal_add_local which dispatches signals as
    // GLib source callbacks on the main thread, avoiding async-signal-safety
    // issues. Must be registered before window.present() so signals arriving
    // immediately after startup are caught.
    // In daemon mode: do NOT kill daemon PTYs on signal.
    {
        let signals: &[(i32, &str)] =
            &[(SIGTERM, "SIGTERM"), (SIGHUP, "SIGHUP"), (SIGINT, "SIGINT")];
        for &(signum, name) in signals {
            let wm_signal = Rc::clone(&workspace_manager);
            let app_signal = app.clone();
            let win_signal_save = window.clone();
            let skip_save_signal = Rc::clone(&skip_session_save);
            let dc_signal = daemon_client.clone();
            glib::unix_signal_add_local(signum, move || {
                tracing::info!("Received {name} (signal {signum}), initiating clean shutdown");
                // Save session in both modes (daemon needs it for ordered reconnect).
                if !skip_save_signal.get() {
                    save_all_workspaces(&wm_signal, &win_signal_save);
                }
                if dc_signal.is_none() {
                    // Self-contained mode only: also kill PTYs.
                    kill_all_workspace_ptys(&wm_signal, name);
                }
                app_signal.quit();
                glib::ControlFlow::Break
            });
        }
    }

    window.present();

    // Grab focus on the active workspace's selected tab's first DrawingArea
    {
        let Ok(mgr) = workspace_manager.try_borrow() else {
            return;
        };
        let ws = &mgr.workspaces[mgr.active_index];
        if let Some(page) = ws.tab_view.selected_page() {
            let container = page.child();
            let leaves = collect_leaf_drawing_areas(&container);
            if let Some(da) = leaves.first() {
                da.grab_focus();
            }
        }
    }

    // --- Auto-save timer (30s) ---
    // Periodically saves ALL workspaces so crash recovery loses at most 30s.
    // Uses a weak window reference to stop the timer after window destruction.
    // Skipped when launched with CLI overrides to avoid overwriting the real session.
    if !has_cli_override {
        let wm_autosave = Rc::clone(&workspace_manager);
        let window_weak_save = window.downgrade();
        glib::timeout_add_local(Duration::from_secs(AUTO_SAVE_SECS), move || {
            let Some(win) = window_weak_save.upgrade() else {
                return glib::ControlFlow::Break;
            };
            save_all_workspaces(&wm_autosave, &win);
            glib::ControlFlow::Continue
        });
    }

    // --- Config hot reload timer ---
    // Polls the config watcher every 500ms. On change, reloads config.toml
    // and applies diffs (font, theme, bell) to all existing panes in ALL workspaces.
    if let Some(mut config_watcher) = ConfigWatcher::new() {
        let shared_cfg = Rc::clone(&shared_config);
        let wm_reload = Rc::clone(&workspace_manager);
        let window_weak = window.downgrade();

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
fn home_dir_fallback() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

/// Read the CWD of a terminal pane.
///
/// Self-contained panes: reads `/proc/{pid}/cwd` via the local PTY PID.
/// Daemon panes: falls back to `daemon_cwd` (set at connect time from `PaneInfo`).
/// Returns `None` if neither source is available.
fn read_pane_cwd(state_rc: &Rc<RefCell<TerminalState>>) -> Option<PathBuf> {
    let s = state_rc.try_borrow().ok()?;
    // Self-contained: read live CWD from /proc
    if let Some(pid) = s.pty.as_ref().and_then(|p| p.pid()) {
        let link = format!("/proc/{pid}/cwd");
        if let Ok(path) = std::fs::read_link(&link) {
            return Some(path);
        }
    }
    // Daemon fallback: CWD from PaneInfo at connect time
    s.daemon_cwd.clone()
}

/// Walk a widget subtree and return the daemon pane ID of the first leaf found.
///
/// Used to populate `TabState.pane_id` when snapshotting in daemon mode.
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

/// Walk a widget subtree and produce a `PaneTreeState` for session persistence.
///
/// Recognises the three widget types used in the tab layout:
/// - `DrawingArea` → leaf pane
/// - `Paned` → split with two children
/// - `Box` → pane container wrapper (recurse into first child)
fn snapshot_pane_tree(widget: &gtk4::Widget, tab_states: &TabStateMap) -> Option<PaneTreeState> {
    if let Some(da) = widget.downcast_ref::<gtk4::DrawingArea>() {
        let widget_name = da.widget_name().to_string();
        let cwd = tab_states
            .try_borrow()
            .ok()
            .and_then(|states| states.get(&widget_name).cloned())
            .and_then(|state_rc| read_pane_cwd(&state_rc))
            .unwrap_or_else(|| home_dir_fallback());
        let daemon_pane_id = tab_states
            .try_borrow()
            .ok()
            .and_then(|states| states.get(&widget_name).cloned())
            .and_then(|rc| rc.try_borrow().ok().map(|s| s.daemon_pane_id))
            .flatten()
            .map(|pid| pid.0);
        return Some(PaneTreeState::Leaf { cwd, pane_id: daemon_pane_id });
    }

    if let Some(paned) = widget.downcast_ref::<gtk4::Paned>() {
        let direction = match paned.orientation() {
            gtk4::Orientation::Horizontal => "horizontal",
            _ => "vertical",
        };

        let size = match paned.orientation() {
            gtk4::Orientation::Horizontal => paned.width(),
            _ => paned.height(),
        };
        let pos = paned.position();
        let ratio = if size > 0 { pos as f32 / size as f32 } else { 0.5 };

        let first = paned.start_child().and_then(|c| snapshot_pane_tree(&c, tab_states));
        let second = paned.end_child().and_then(|c| snapshot_pane_tree(&c, tab_states));

        if let (Some(first), Some(second)) = (first, second) {
            return Some(PaneTreeState::Split {
                direction: direction.to_string(),
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            });
        }
    }

    // Box container: recurse into first child (the pane-vbox or Paned).
    if let Some(bx) = widget.downcast_ref::<gtk4::Box>() {
        let mut child = bx.first_child();
        while let Some(c) = child {
            if let Some(tree) = snapshot_pane_tree(&c, tab_states) {
                return Some(tree);
            }
            child = c.next_sibling();
        }
    }

    None
}

/// Snapshot a single workspace's layout for session persistence.
fn snapshot_single_workspace(ws: &WorkspaceView) -> Workspace {
    let n_pages = ws.tab_view.n_pages();
    let active_tab = ws
        .tab_view
        .selected_page()
        .map(|p| (0..n_pages).find(|&i| ws.tab_view.nth_page(i) == p).unwrap_or(0) as usize)
        .unwrap_or(0);

    let mut tabs = Vec::with_capacity(n_pages as usize);
    for i in 0..n_pages {
        let page = ws.tab_view.nth_page(i);
        let title = page.title().to_string();
        let container = page.child();

        let pane_tree = snapshot_pane_tree(&container, &ws.tab_states)
            .unwrap_or_else(|| PaneTreeState::Leaf { cwd: home_dir_fallback(), pane_id: None });

        // pane_id is now stored per-Leaf inside pane_tree; top-level is always None.
        tabs.push(TabState { title, pane_tree, pane_id: None });
    }

    Workspace { id: ws.id, name: ws.name.clone(), root_paths: Vec::new(), tabs, active_tab }
}

/// Snapshot ALL workspaces for session persistence.
fn snapshot_all_workspaces(
    workspace_manager: &WorkspaceManager,
    window: &adw::ApplicationWindow,
) -> WorkspaceState {
    let Ok(mgr) = workspace_manager.try_borrow() else {
        return WorkspaceState::new();
    };

    let workspaces: Vec<Workspace> = mgr.workspaces.iter().map(snapshot_single_workspace).collect();

    WorkspaceState {
        version: 1,
        workspaces,
        active_workspace: mgr.active_index,
        window_width: Some(window.width()),
        window_height: Some(window.height()),
    }
}

/// Save ALL workspaces to disk. Logs errors but does not propagate them.
fn save_all_workspaces(workspace_manager: &WorkspaceManager, window: &adw::ApplicationWindow) {
    let state = snapshot_all_workspaces(workspace_manager, window);
    if let Err(e) = forgetty_workspace::save_session(&state) {
        tracing::warn!("Failed to save session: {e}");
    } else {
        tracing::debug!("Session saved ({} workspace(s))", state.workspaces.len());
    }
}

/// Kill all PTYs across all workspaces.
fn kill_all_workspace_ptys(workspace_manager: &WorkspaceManager, reason: &str) {
    let Ok(mgr) = workspace_manager.try_borrow() else {
        tracing::warn!("kill_all_workspace_ptys: could not borrow workspace manager");
        return;
    };
    for ws in &mgr.workspaces {
        kill_all_ptys(&ws.tab_states, reason);
    }
}

/// Recursively reconnect a daemon pane tree for a single tab during GTK reopen.
///
/// Mirrors `build_pane_tree` (self-contained mode) but uses live daemon panes
/// instead of spawning new PTYs. Each `Leaf` looks up its pane UUID in
/// `pane_map`, subscribes to the daemon output stream, and creates a
/// `DrawingArea` connected to the live pane. `Split` nodes recurse and
/// produce a `gtk::Paned` with the ratio restored via `idle_add_local_once`.
///
/// `legacy_pane_id` is the `TabState.pane_id` from T-055-era session files
/// (where pane IDs were stored at the tab level, not per-leaf). It is used as
/// a fallback when the leaf's own `pane_id` is `None`.
#[allow(clippy::too_many_arguments)]
fn reconnect_pane_tree(
    tree: &PaneTreeState,
    pane_map: &mut HashMap<uuid::Uuid, PaneInfo>,
    dc: &Arc<DaemonClient>,
    config: &Config,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    window: &adw::ApplicationWindow,
    tab_view: &adw::TabView,
    legacy_pane_id: Option<uuid::Uuid>,
) -> Option<(gtk4::Widget, gtk4::DrawingArea)> {
    match tree {
        PaneTreeState::Leaf { cwd, pane_id } => {
            // Determine which daemon pane to reconnect:
            // 1. Per-leaf pane_id (new format, T-057+)
            // 2. Legacy top-level TabState.pane_id (T-055 compat)
            let uid = pane_id.or(legacy_pane_id);

            let leaf_cwd = if cwd.is_dir() { Some(cwd.as_path()) } else { None };

            let (daemon_pane_id, daemon_cwd, fresh_pane) = if let Some(uid) = uid {
                if let Some(info) = pane_map.remove(&uid) {
                    // Live daemon pane found — reconnect it.
                    let daemon_cwd =
                        if info.cwd.is_empty() { None } else { Some(PathBuf::from(&info.cwd)) };
                    (info.pane_id, daemon_cwd, false)
                } else {
                    // Pane was closed between GTK close and reopen — fresh pane.
                    tracing::info!(
                        "Daemon pane {:?} gone — creating fresh pane for leaf {:?}",
                        uid,
                        cwd
                    );
                    match dc.new_tab_with_cwd(leaf_cwd) {
                        Ok((pid, _tab_id)) => (pid, None, true),
                        Err(e) => {
                            tracing::warn!("new_tab failed for missing leaf pane: {e}");
                            return None;
                        }
                    }
                }
            } else {
                // No pane_id at all (old session format or self-contained) — fresh pane.
                match dc.new_tab_with_cwd(leaf_cwd) {
                    Ok((pid, _tab_id)) => (pid, None, false),
                    Err(e) => {
                        tracing::warn!("new_tab failed for legacy leaf: {e}");
                        return None;
                    }
                }
            };

            // Effective CWD: daemon's live CWD > saved CWD > home.
            let effective_cwd =
                daemon_cwd.or_else(|| if cwd.is_dir() { Some(cwd.clone()) } else { None });

            // Pre-seed VT buffer with saved snapshot for fresh panes that had a UUID.
            if fresh_pane {
                if let Some(old_uuid) = uid {
                    if let Err(e) = dc.preseed_snapshot(daemon_pane_id, old_uuid) {
                        tracing::warn!("preseed_snapshot failed: {e}");
                    }
                }
            }

            let (mpsc_tx, mpsc_rx) = std::sync::mpsc::channel::<Vec<u8>>();
            if let Err(e) = dc.subscribe_output(daemon_pane_id, mpsc_tx) {
                tracing::warn!("subscribe_output failed for {}: {e}", daemon_pane_id);
            }

            let on_exit = make_on_exit_callback(tab_view, tab_states, window, Some(Arc::clone(dc)));
            let on_notify = make_on_notify_callback(tab_view, tab_states, window);
            let snapshot = dc.get_screen(daemon_pane_id).ok();

            match terminal::create_terminal_for_pane(
                config,
                daemon_pane_id,
                Arc::clone(dc),
                mpsc_rx,
                snapshot.as_ref(),
                effective_cwd,
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
                    );
                    Some((pane_vbox.upcast::<gtk4::Widget>(), drawing_area))
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to create terminal for daemon pane {daemon_pane_id}: {e}"
                    );
                    None
                }
            }
        }

        PaneTreeState::Split { direction, ratio, first, second } => {
            let orientation = if direction == "horizontal" {
                gtk4::Orientation::Horizontal
            } else {
                gtk4::Orientation::Vertical
            };

            let first_result = reconnect_pane_tree(
                first,
                pane_map,
                dc,
                config,
                tab_states,
                focus_tracker,
                custom_titles,
                window,
                tab_view,
                None,
            );
            let second_result = reconnect_pane_tree(
                second,
                pane_map,
                dc,
                config,
                tab_states,
                focus_tracker,
                custom_titles,
                window,
                tab_view,
                None,
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

            // Defer set_position after realization so the widget has a size.
            let saved_ratio = *ratio;
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
                    paned.set_position((size as f32 * saved_ratio) as i32);
                }
            });

            Some((paned.upcast::<gtk4::Widget>(), first_da))
        }
    }
}

/// Recursively build the pane tree for a single tab from a `PaneTreeState`.
///
/// Returns `(root_widget, first_leaf_drawing_area)` where root_widget is either
/// the pane vbox (leaf) or a Paned (split), and first_leaf_drawing_area is the
/// leftmost/topmost leaf (used for focus).
fn build_pane_tree(
    tree: &PaneTreeState,
    config: &Config,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    tab_view: &adw::TabView,
    window: &adw::ApplicationWindow,
) -> Option<(gtk4::Widget, gtk4::DrawingArea)> {
    match tree {
        PaneTreeState::Leaf { cwd, .. } => {
            // Fall back to $HOME if saved CWD no longer exists.
            let effective_cwd = if cwd.is_dir() {
                cwd.clone()
            } else {
                tracing::warn!("Saved CWD {:?} no longer exists, falling back to $HOME", cwd);
                home_dir_fallback()
            };

            let on_exit = make_on_exit_callback(tab_view, tab_states, window, None);
            let on_notify = make_on_notify_callback(tab_view, tab_states, window);
            match terminal::create_terminal(
                config,
                Some(on_exit),
                Some(on_notify),
                Some(&effective_cwd),
                None,
            ) {
                Ok((pane_vbox, drawing_area, state)) => {
                    let pane_id = next_pane_id();
                    drawing_area.set_widget_name(&pane_id);
                    tab_states.borrow_mut().insert(pane_id, Rc::clone(&state));
                    wire_focus_tracking(
                        &drawing_area,
                        focus_tracker,
                        tab_view,
                        tab_states,
                        custom_titles,
                    );
                    Some((pane_vbox.upcast::<gtk4::Widget>(), drawing_area))
                }
                Err(e) => {
                    tracing::error!("Failed to create terminal for restored pane: {e}");
                    None
                }
            }
        }
        PaneTreeState::Split { direction, ratio, first, second } => {
            let orientation = if direction == "horizontal" {
                gtk4::Orientation::Horizontal
            } else {
                gtk4::Orientation::Vertical
            };

            let first_result = build_pane_tree(
                first,
                config,
                tab_states,
                focus_tracker,
                custom_titles,
                tab_view,
                window,
            );
            let second_result = build_pane_tree(
                second,
                config,
                tab_states,
                focus_tracker,
                custom_titles,
                tab_view,
                window,
            );

            let (Some((first_widget, first_da)), Some((second_widget, _second_da))) =
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

            // Defer set_position after realization so the widget has a size.
            let saved_ratio = *ratio;
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
                    paned.set_position((size as f32 * saved_ratio) as i32);
                }
            });

            Some((paned.upcast::<gtk4::Widget>(), first_da))
        }
    }
}

/// Restore tabs into a single workspace's TabView from a saved `Workspace`.
///
/// Returns `true` if at least one tab was successfully restored.
fn restore_workspace_tabs(
    saved: &Workspace,
    tab_view: &adw::TabView,
    config: &Config,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    window: &adw::ApplicationWindow,
) -> bool {
    if saved.tabs.is_empty() {
        return false;
    }

    for (idx, tab) in saved.tabs.iter().enumerate() {
        let result = build_pane_tree(
            &tab.pane_tree,
            config,
            tab_states,
            focus_tracker,
            custom_titles,
            tab_view,
            window,
        );

        let Some((root_widget, _leaf_da)) = result else {
            tracing::warn!("Failed to restore tab {idx}, skipping");
            continue;
        };

        // Wrap in pane container Box (same pattern as add_new_tab).
        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(true);
        container.append(&root_widget);

        let page = tab_view.append(&container);
        page.set_title(&tab.title);

        // Register title polling timer for this tab.
        register_title_timer(&page, tab_view, tab_states, focus_tracker, custom_titles, window);
    }

    // Select the saved active tab.
    let active = saved.active_tab.min(tab_view.n_pages().saturating_sub(1) as usize);
    if tab_view.n_pages() > 0 {
        let page = tab_view.nth_page(active as i32);
        tab_view.set_selected_page(&page);
    }

    tab_view.n_pages() > 0
}

/// Restore ALL workspaces from a saved session.
///
/// Replaces the workspace manager contents. The first workspace reuses the
/// initial TabView that is already parented in main_area. Additional workspaces
/// get new TabViews that are kept unparented until switched to.
/// Returns `true` if at least one workspace was restored.
fn restore_all_workspaces(
    state: &WorkspaceState,
    workspace_manager: &WorkspaceManager,
    config: &Config,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
) -> bool {
    let _ = daemon_client; // reserved for future workspace-aware daemon restore
    if state.workspaces.is_empty() {
        return false;
    }

    // Restore window dimensions.
    if let (Some(w), Some(h)) = (state.window_width, state.window_height) {
        if w > 0 && h > 0 {
            window.set_default_size(w, h);
        }
    }

    let Ok(mut mgr) = workspace_manager.try_borrow_mut() else {
        return false;
    };

    // Get a reference to the initial TabView (the one already in main_area).
    let initial_tab_view = mgr.workspaces[0].tab_view.clone();

    // Clear the existing single default workspace -- we will rebuild all from saved state.
    mgr.workspaces.clear();

    let mut any_restored = false;

    for (ws_idx, saved_ws) in state.workspaces.iter().enumerate() {
        // Backward compat: capitalize "default" -> "Default"
        let name =
            if saved_ws.name == "default" { "Default".to_string() } else { saved_ws.name.clone() };

        // First workspace reuses the initial TabView.
        // Subsequent workspaces get fresh TabViews.
        let tab_view = if ws_idx == 0 {
            initial_tab_view.clone()
        } else {
            let tv = adw::TabView::new();
            tv.set_vexpand(true);
            tv.set_hexpand(true);
            tv
        };

        let tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));
        let focus_tracker: FocusTracker = Rc::new(RefCell::new(String::new()));
        let custom_titles: CustomTitles = Rc::new(RefCell::new(HashSet::new()));

        // Wire tab close and focus management on new TabViews.
        if ws_idx > 0 {
            wire_tab_view_handlers(&tab_view, &tab_states, &focus_tracker, window);
        }

        let restored = restore_workspace_tabs(
            saved_ws,
            &tab_view,
            config,
            &tab_states,
            &focus_tracker,
            &custom_titles,
            window,
        );

        if restored {
            any_restored = true;
        }

        mgr.workspaces.push(WorkspaceView {
            id: saved_ws.id,
            name,
            tab_view,
            tab_states,
            focus_tracker,
            custom_titles,
        });
    }

    // Set the active workspace to the saved one.
    let active = state.active_workspace.min(mgr.workspaces.len().saturating_sub(1));
    mgr.active_index = active;

    // If the active workspace is not the first, swap the TabView in main_area.
    if active > 0 {
        // Remove the initial tab_view, insert the active one.
        let active_tv = mgr.workspaces[active].tab_view.clone();
        // Find and remove the initial tab_view from main_area.
        // It is the first child of main_area.
        let mut child = main_area.first_child();
        while let Some(c) = child {
            if c == *initial_tab_view.upcast_ref::<gtk4::Widget>() {
                main_area.remove(&c);
                break;
            }
            child = c.next_sibling();
        }
        main_area.prepend(&active_tv);
        tab_bar.set_view(Some(&active_tv));
    }

    // Focus the first leaf in the active workspace's selected tab.
    if let Some(ws) = mgr.workspaces.get(active) {
        if let Some(page) = ws.tab_view.selected_page() {
            let container = page.child();
            let leaves = collect_leaf_drawing_areas(&container);
            if let Some(da) = leaves.first() {
                da.grab_focus();
            }
        }
    }

    // Update delete-workspace action enabled state.
    drop(mgr);
    update_delete_workspace_action(workspace_manager, window);

    any_restored
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
/// GTK main thread and calls `close_pane_by_name()` to remove the pane.
fn make_on_exit_callback(
    tab_view: &adw::TabView,
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
) -> Rc<dyn Fn(String)> {
    let tv = tab_view.clone();
    let states = Rc::clone(tab_states);
    let win = window.clone();
    Rc::new(move |pane_name: String| {
        close_pane_by_name(&pane_name, &tv, &states, &win, daemon_client.clone());
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

fn add_new_tab(
    tab_view: &adw::TabView,
    config: &Config,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    custom_titles: &CustomTitles,
    window: &adw::ApplicationWindow,
    working_dir: Option<&std::path::Path>,
    command: Option<&[String]>,
    daemon_client: Option<Arc<DaemonClient>>,
) {
    let on_exit = make_on_exit_callback(tab_view, tab_states, window, daemon_client.clone());
    let on_notify = make_on_notify_callback(tab_view, tab_states, window);

    // --- Daemon mode: create pane via RPC and subscribe to output. ---
    if let Some(ref dc) = daemon_client {
        match dc.new_tab() {
            Ok((pane_id, _tab_id)) => {
                let (mpsc_tx, mpsc_rx) = std::sync::mpsc::channel::<Vec<u8>>();
                if let Err(e) = dc.subscribe_output(pane_id, mpsc_tx) {
                    tracing::warn!("subscribe_output failed for new pane {pane_id}: {e}");
                }
                let snapshot = dc.get_screen(pane_id).ok();
                match terminal::create_terminal_for_pane(
                    config,
                    pane_id,
                    Arc::clone(dc),
                    mpsc_rx,
                    snapshot.as_ref(),
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
                        );
                        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                        container.set_hexpand(true);
                        container.set_vexpand(true);
                        container.append(&pane_vbox);
                        let page = tab_view.append(&container);
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
                return;
            }
            Err(e) => {
                tracing::warn!("new_tab RPC failed: {e}; falling back to local PTY");
                // Fall through to local PTY creation below.
            }
        }
    }

    // --- Local / fallback mode: spawn PTY directly. ---
    match terminal::create_terminal(config, Some(on_exit), Some(on_notify), working_dir, command) {
        Ok((pane_vbox, drawing_area, state)) => {
            // Assign a unique widget name for registry lookup
            let pane_id = next_pane_id();
            drawing_area.set_widget_name(&pane_id);

            // Register in the pane state map
            tab_states.borrow_mut().insert(pane_id, Rc::clone(&state));

            // Wire up focus tracking on this pane
            wire_focus_tracking(&drawing_area, focus_tracker, tab_view, tab_states, custom_titles);

            // Wrap in a pane container Box (allows swapping root widget later)
            let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            container.set_hexpand(true);
            container.set_vexpand(true);
            container.append(&pane_vbox);

            // Append the page to the TabView
            let page = tab_view.append(&container);
            page.set_title("shell");

            // Make this the selected (active) tab
            tab_view.set_selected_page(&page);

            // Grab focus so keyboard input goes to this terminal
            drawing_area.grab_focus();

            // --- Set initial window title immediately ---
            if let Ok(s) = state.try_borrow() {
                let pane_title = compute_window_title(&s);
                set_window_title_preserving_workspace(window, &pane_title);
                page.set_title(&compute_display_title(&s));
            }

            // --- Title polling timer ---
            // Periodically update the tab title from the focused pane's CWD.
            register_title_timer(
                &page,
                tab_view,
                tab_states,
                focus_tracker,
                custom_titles,
                &window,
            );
        }
        Err(e) => {
            tracing::error!("Failed to create terminal for new tab: {e}");
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

    // Create the new terminal pane (splits always get default shell + default CWD)
    let on_exit = make_on_exit_callback(tab_view, tab_states, window, daemon_client.clone());
    let on_notify = make_on_notify_callback(tab_view, tab_states, window);

    // Determine whether to create via daemon or local PTY.
    let new_pane_result: Result<
        (gtk4::Box, gtk4::DrawingArea, Rc<RefCell<TerminalState>>),
        String,
    > = if let Some(ref dc) = daemon_client {
        match dc.new_tab() {
            Ok((pane_id, _tab_id)) => {
                let (mpsc_tx, mpsc_rx) = std::sync::mpsc::channel::<Vec<u8>>();
                if let Err(e) = dc.subscribe_output(pane_id, mpsc_tx) {
                    tracing::warn!("subscribe_output failed for split pane {pane_id}: {e}");
                }
                let snapshot = dc.get_screen(pane_id).ok();
                terminal::create_terminal_for_pane(
                    config,
                    pane_id,
                    Arc::clone(dc),
                    mpsc_rx,
                    snapshot.as_ref(),
                    None,
                    Some(on_exit),
                    Some(on_notify),
                )
            }
            Err(e) => {
                tracing::warn!("new_tab RPC failed for split: {e}; falling back to local PTY");
                terminal::create_terminal(config, Some(on_exit), Some(on_notify), None, None)
            }
        }
    } else {
        terminal::create_terminal(config, Some(on_exit), Some(on_notify), None, None)
    };

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
    wire_focus_tracking(&new_da, focus_tracker, tab_view, tab_states, custom_titles);

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

    // Give focus to the new pane
    new_da.grab_focus();
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

    close_pane_by_name(&focused_name, tab_view, tab_states, window, daemon_client);
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

    // If this is the only pane in the tab, close the tab
    if leaves.len() <= 1 {
        // Kill or close the PTY (local or daemon).
        kill_or_daemon_close_pane(pane_name, tab_states, daemon_client.as_deref());

        if tab_view.n_pages() <= 1 {
            window.close();
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
    parent_paned.set_start_child(gtk4::Widget::NONE);
    parent_paned.set_end_child(gtk4::Widget::NONE);

    // Kill or close the PTY (local or daemon) and remove from registry.
    kill_or_daemon_close_pane(pane_name, tab_states, daemon_client.as_deref());

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

        if is_valid {
            if best.is_none() || distance < best.unwrap().1 {
                best = Some((candidate, distance));
            }
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
            for col in col_start..=col_end.min(cells.len().saturating_sub(1)) {
                line.push_str(&cells[col].grapheme);
            }
            lines.push(line);
        }

        cursor = page_end + 1;
    }

    // Restore original viewport position
    let (_, cur_off, _) = s.terminal.scrollbar_state();
    let restore = orig_offset as isize - cur_off as isize;
    if restore != 0 {
        s.terminal.scroll_viewport_delta(restore);
    }

    let raw_text = lines.join("\n");

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

/// Kill all PTY processes in a widget subtree and remove their states from
/// the registry.
fn kill_all_panes_in_subtree(widget: &gtk4::Widget, tab_states: &TabStateMap) {
    let leaves = collect_leaf_drawing_areas(widget);
    for da in &leaves {
        let pane_id = da.widget_name().to_string();
        kill_pane(&pane_id, tab_states);
    }
}

/// Kill a single pane's PTY and remove it from the state registry.
fn kill_pane(pane_id: &str, tab_states: &TabStateMap) {
    if let Some(state_rc) = tab_states.borrow().get(pane_id).cloned() {
        if let Ok(mut s) = state_rc.try_borrow_mut() {
            if let Some(ref mut pty) = s.pty {
                if let Err(e) = pty.kill() {
                    tracing::warn!("Failed to kill PTY for pane {pane_id}: {e}");
                }
            }
        }
    }
    tab_states.borrow_mut().remove(pane_id);
}

/// Kill every PTY process tracked in the state registry.
///
/// This is the centralized shutdown path, called by signal handlers,
/// `connect_close_request`, and the Quit action. It iterates the data
/// structure directly (not the widget tree), so it works even after GTK
/// widgets have been destroyed.
fn kill_all_ptys(tab_states: &TabStateMap, reason: &str) {
    let Ok(states) = tab_states.try_borrow() else {
        tracing::warn!("kill_all_ptys: could not borrow tab_states (already borrowed)");
        return;
    };
    let count = states.len();
    if count > 0 {
        tracing::info!("{reason}: killing {count} PTY process(es)");
    }
    for (pane_id, state_rc) in states.iter() {
        if let Ok(mut s) = state_rc.try_borrow_mut() {
            if let Some(ref mut pty) = s.pty {
                if let Err(e) = pty.kill() {
                    tracing::warn!("Failed to kill PTY for pane {pane_id}: {e}");
                }
            }
        }
    }
}

/// Kill a local PTY or send close_tab RPC to daemon, then remove from registry.
fn kill_or_daemon_close_pane(
    pane_name: &str,
    tab_states: &TabStateMap,
    daemon_client: Option<&DaemonClient>,
) {
    if let Some(dc) = daemon_client {
        // In daemon mode: look up the PaneId from the TerminalState and send close_tab RPC.
        if let Some(state_rc) = tab_states.borrow().get(pane_name).cloned() {
            if let Ok(s) = state_rc.try_borrow() {
                if let Some(pane_id) = s.daemon_pane_id {
                    if let Err(e) = dc.close_tab_by_pane_id(pane_id) {
                        tracing::warn!("close_tab RPC failed for {pane_name}: {e}");
                    }
                }
            }
        }
        tab_states.borrow_mut().remove(pane_name);
    } else {
        kill_pane(pane_name, tab_states);
    }
}

/// Walk a widget subtree, send close_tab RPC for each pane, remove from registry.
fn daemon_close_panes_in_subtree(
    widget: &gtk4::Widget,
    tab_states: &TabStateMap,
    daemon_client: &DaemonClient,
) {
    if let Some(da) = widget.downcast_ref::<gtk4::DrawingArea>() {
        let pane_name = da.widget_name().to_string();
        kill_or_daemon_close_pane(&pane_name, tab_states, Some(daemon_client));
        return;
    }

    // Recurse into children.
    let mut child = widget.first_child();
    while let Some(c) = child {
        daemon_close_panes_in_subtree(&c, tab_states, daemon_client);
        child = c.next_sibling();
    }
}

/// Write bytes to the focused pane's PTY — either via daemon RPC or local pty.
fn write_to_focused_pty_or_daemon(
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    bytes: &[u8],
    daemon_client: Option<&DaemonClient>,
) {
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else { return };
        name.clone()
    };
    if focused_name.is_empty() {
        return;
    }
    let Ok(states) = tab_states.try_borrow() else { return };
    let Some(state_rc) = states.get(&focused_name).cloned() else { return };
    drop(states);

    if let Some(dc) = daemon_client {
        if let Ok(s) = state_rc.try_borrow() {
            if let Some(pane_id) = s.daemon_pane_id {
                if let Err(e) = dc.send_input(pane_id, bytes) {
                    tracing::warn!("send_input RPC failed: {e}");
                }
                return;
            }
        }
    }

    // Fallback: local PTY.
    let Ok(mut s) = state_rc.try_borrow_mut() else { return };
    if let Some(ref mut pty) = s.pty {
        if let Err(e) = pty.write(bytes) {
            tracing::warn!("Failed to write to PTY: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Focus tracking
// ---------------------------------------------------------------------------

/// Wire up an EventControllerFocus on a DrawingArea to update the focus tracker
/// when this pane gains focus.
///
/// Also triggers a redraw on focus change so the visual indicator updates.
fn wire_focus_tracking(
    drawing_area: &gtk4::DrawingArea,
    focus_tracker: &FocusTracker,
    tab_view: &adw::TabView,
    tab_states: &TabStateMap,
    custom_titles: &CustomTitles,
) {
    let focus_controller = gtk4::EventControllerFocus::new();

    // Focus gained -- update the tracker and tab title immediately
    {
        let tracker = Rc::clone(focus_tracker);
        let da = drawing_area.clone();
        let tv = tab_view.clone();
        let states = Rc::clone(tab_states);
        let ct = Rc::clone(custom_titles);
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

/// Compute the display title for a terminal tab.
///
/// Priority: /proc CWD basename > daemon_cwd basename > OSC title > "shell".
///
/// daemon_cwd is preferred over OSC title because OSC 0/2 from zsh/bash emits
/// the full `user@host:cwd` format (meant for the window title bar), which is
/// too verbose for a tab label.  daemon_cwd gives just the directory path whose
/// basename is a clean, short tab title.
fn compute_display_title(state: &TerminalState) -> String {
    // Try to read CWD from /proc/{pid}/cwd (self-contained panes only)
    if let Some(pid) = state.pty.as_ref().and_then(|p| p.pid()) {
        let proc_path = format!("/proc/{}/cwd", pid);
        if let Ok(target) = std::fs::read_link(&proc_path) {
            let cwd = target.to_string_lossy().to_string();
            if !cwd.is_empty() {
                // Use basename of the CWD
                return std::path::Path::new(&cwd)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| cwd.clone());
            }
        }
    }

    // Daemon fallback: CWD basename from PaneInfo (set at connect time).
    if let Some(cwd) = &state.daemon_cwd {
        if let Some(name) = cwd.file_name() {
            return name.to_string_lossy().to_string();
        }
    }

    // Fall back to OSC title if set (e.g. user@host:cwd from zsh).
    let osc_title = state.terminal.title();
    if !osc_title.is_empty() && osc_title != "shell" {
        return osc_title.to_string();
    }

    "shell".to_string()
}

/// Compute the window title bar string: `user@hostname:cwd`.
///
/// Mirrors Ghostty's title format shown in the CSD header bar.
fn compute_window_title(state: &TerminalState) -> String {
    let user = std::env::var("USER").unwrap_or_default();
    let hostname = glib::host_name().to_string();
    // Strip domain from hostname (e.g. "totemlabs-lap.local" → "totemlabs-lap")
    let short_host = hostname.split('.').next().unwrap_or(&hostname);

    if let Some(pid) = state.pty.as_ref().and_then(|p| p.pid()) {
        let proc_path = format!("/proc/{}/cwd", pid);
        if let Ok(target) = std::fs::read_link(&proc_path) {
            let cwd = target.to_string_lossy().to_string();
            // Replace /home/user with ~
            let home = std::env::var("HOME").unwrap_or_default();
            let display_cwd = if !home.is_empty() && cwd.starts_with(&home) {
                format!("~{}", &cwd[home.len()..])
            } else {
                cwd
            };
            return format!("{}@{}:{}", user, short_host, display_cwd);
        }
    }

    format!("{}@{}", user, short_host)
}

// ---------------------------------------------------------------------------
// Paste from clipboard
// ---------------------------------------------------------------------------

/// Paste the system clipboard text into the focused pane's PTY.
///
/// Reads the clipboard text asynchronously via `gdk::Clipboard::read_text_async()`,
/// then writes the text bytes to the PTY. The `TerminalState` borrow is NOT held
/// across the async boundary -- only acquired in the callback.
fn paste_clipboard(
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
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

    // Clone state_rc and daemon_client for the async callback
    let state_for_cb = Rc::clone(&state_rc);
    let dc_paste = daemon_client.clone();
    clipboard.read_text_async(gio::Cancellable::NONE, move |result| {
        let text = match result {
            Ok(Some(text)) => text.to_string(),
            Ok(None) => return, // clipboard empty (AC-11)
            Err(e) => {
                tracing::debug!("Clipboard read failed: {e}");
                return;
            }
        };

        if text.is_empty() {
            return;
        }

        // Route paste through daemon if available, else local PTY.
        if let Some(ref dc) = dc_paste {
            if let Ok(s) = state_for_cb.try_borrow() {
                if let Some(pane_id) = s.daemon_pane_id {
                    let _ = dc.send_input(pane_id, text.as_bytes());
                    return;
                }
            }
        }

        let Ok(mut s) = state_for_cb.try_borrow_mut() else {
            return;
        };

        if let Some(ref mut pty) = s.pty {
            if let Err(e) = pty.write(text.as_bytes()) {
                tracing::warn!("Failed to write paste to PTY: {e}");
            }
        }
    });
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
    workspace_group.add_shortcut(&shortcut("<Control><Alt>1", "Switch to Workspace 1\u{2013}9"));
    workspace_group.add_shortcut(&shortcut("<Control><Alt>Page_Up", "Previous Workspace"));
    workspace_group.add_shortcut(&shortcut("<Control><Alt>Page_Down", "Next Workspace"));
    workspace_group.add_shortcut(&shortcut("<Control><Alt>w", "Workspace Selector"));
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
// Command Palette
// ---------------------------------------------------------------------------

/// A single entry in the command palette registry.
struct CommandEntry {
    display_name: &'static str,
    action_name: &'static str,
    shortcut_label: &'static str,
}

/// The static list of commands shown in the command palette.
///
/// Order matches the hamburger menu grouping for discoverability.
/// Commands with parameters (e.g., open-url) and disabled placeholders
/// (terminal-inspector) are excluded.
fn command_registry() -> &'static [CommandEntry] {
    static COMMANDS: &[CommandEntry] = &[
        CommandEntry { display_name: "Copy", action_name: "win.copy", shortcut_label: "Ctrl+C" },
        CommandEntry { display_name: "Paste", action_name: "win.paste", shortcut_label: "Ctrl+V" },
        CommandEntry {
            display_name: "New Window",
            action_name: "win.new-window",
            shortcut_label: "",
        },
        CommandEntry {
            display_name: "Close Window",
            action_name: "win.close-window",
            shortcut_label: "",
        },
        CommandEntry {
            display_name: "New Tab",
            action_name: "win.new-tab",
            shortcut_label: "Ctrl+Shift+T",
        },
        CommandEntry {
            display_name: "Close Tab",
            action_name: "win.close-tab",
            shortcut_label: "",
        },
        CommandEntry {
            display_name: "Close Pane",
            action_name: "win.close-pane",
            shortcut_label: "Ctrl+Shift+W",
        },
        CommandEntry {
            display_name: "Change Tab Title",
            action_name: "win.change-tab-title",
            shortcut_label: "",
        },
        CommandEntry {
            display_name: "New Workspace",
            action_name: "win.new-workspace",
            shortcut_label: "Ctrl+Alt+N",
        },
        CommandEntry {
            display_name: "Rename Workspace",
            action_name: "win.rename-workspace",
            shortcut_label: "",
        },
        CommandEntry {
            display_name: "Delete Workspace",
            action_name: "win.delete-workspace",
            shortcut_label: "",
        },
        CommandEntry {
            display_name: "Workspace Selector",
            action_name: "win.workspace-selector",
            shortcut_label: "Ctrl+Alt+W",
        },
        CommandEntry {
            display_name: "Previous Workspace",
            action_name: "win.prev-workspace",
            shortcut_label: "Ctrl+Alt+PgUp",
        },
        CommandEntry {
            display_name: "Next Workspace",
            action_name: "win.next-workspace",
            shortcut_label: "Ctrl+Alt+PgDn",
        },
        CommandEntry { display_name: "Split Up", action_name: "win.split-up", shortcut_label: "" },
        CommandEntry {
            display_name: "Split Down",
            action_name: "win.split-down",
            shortcut_label: "Alt+Shift+\u{2212}",
        },
        CommandEntry {
            display_name: "Split Left",
            action_name: "win.split-left",
            shortcut_label: "",
        },
        CommandEntry {
            display_name: "Split Right",
            action_name: "win.split-right",
            shortcut_label: "Alt+Shift+=",
        },
        CommandEntry {
            display_name: "Focus Pane Left",
            action_name: "win.focus-pane-left",
            shortcut_label: "Alt+Left",
        },
        CommandEntry {
            display_name: "Focus Pane Right",
            action_name: "win.focus-pane-right",
            shortcut_label: "Alt+Right",
        },
        CommandEntry {
            display_name: "Focus Pane Up",
            action_name: "win.focus-pane-up",
            shortcut_label: "Alt+Up",
        },
        CommandEntry {
            display_name: "Focus Pane Down",
            action_name: "win.focus-pane-down",
            shortcut_label: "Alt+Down",
        },
        CommandEntry {
            display_name: "Find in Terminal",
            action_name: "win.search",
            shortcut_label: "Ctrl+Shift+F",
        },
        CommandEntry {
            display_name: "Zoom In",
            action_name: "win.zoom-in",
            shortcut_label: "Ctrl+=",
        },
        CommandEntry {
            display_name: "Zoom Out",
            action_name: "win.zoom-out",
            shortcut_label: "Ctrl+\u{2212}",
        },
        CommandEntry {
            display_name: "Reset Zoom",
            action_name: "win.zoom-reset",
            shortcut_label: "Ctrl+0",
        },
        CommandEntry { display_name: "Clear", action_name: "win.clear", shortcut_label: "" },
        CommandEntry { display_name: "Reset", action_name: "win.reset", shortcut_label: "" },
        CommandEntry {
            display_name: "Open Configuration",
            action_name: "win.open-config",
            shortcut_label: "",
        },
        CommandEntry {
            display_name: "Reload Configuration",
            action_name: "win.reload-config",
            shortcut_label: "",
        },
        CommandEntry {
            display_name: "Appearance",
            action_name: "win.appearance",
            shortcut_label: "Ctrl+,",
        },
        CommandEntry {
            display_name: "Keyboard Shortcuts",
            action_name: "win.show-shortcuts",
            shortcut_label: "F1",
        },
        CommandEntry {
            display_name: "About Forgetty",
            action_name: "win.show-about",
            shortcut_label: "",
        },
        CommandEntry {
            display_name: "Quit",
            action_name: "app.quit",
            shortcut_label: "Ctrl+Shift+Q",
        },
    ];
    COMMANDS
}

/// Build the command palette overlay widget.
///
/// Returns the outer container (a `gtk4::Box` used as the overlay child)
/// and internal widgets needed for wiring up actions.
///
/// The palette is a centered card with a SearchEntry at the top and a
/// scrollable ListBox below it. It starts hidden; the caller adds it
/// as an overlay child on the `main_overlay` and toggles visibility.
fn build_command_palette(
    window: &adw::ApplicationWindow,
    workspace_manager: &WorkspaceManager,
) -> gtk4::Box {
    let registry = command_registry();

    // --- Outer alignment container ---
    // This Box fills the entire overlay area. We use alignment to center the
    // palette card horizontally at the top third of the window.
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.set_halign(gtk4::Align::Center);
    outer.set_valign(gtk4::Align::Start);
    outer.set_margin_top(60);
    outer.set_hexpand(true);
    outer.set_vexpand(true);
    // Request ~55% of default window width; actual width adapts via CSS/hexpand.
    outer.set_width_request((DEFAULT_WIDTH as f64 * 0.55) as i32);
    outer.set_visible(false);
    outer.set_can_focus(false);
    // Give it a card-like look
    outer.add_css_class("card");
    outer.add_css_class("command-palette");

    // --- Search entry ---
    let search_entry = gtk4::SearchEntry::new();
    search_entry.set_placeholder_text(Some("Type a command\u{2026}"));
    search_entry.set_hexpand(true);
    search_entry.set_focusable(true);
    search_entry.set_can_focus(true);
    search_entry.set_margin_start(8);
    search_entry.set_margin_end(8);
    search_entry.set_margin_top(8);
    search_entry.set_margin_bottom(4);
    outer.append(&search_entry);

    let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    outer.append(&separator);

    // --- Scrollable command list ---
    let list_box = gtk4::ListBox::new();
    list_box.set_selection_mode(gtk4::SelectionMode::Single);
    list_box.add_css_class("navigation-sidebar");

    // Populate with all commands
    for entry in registry {
        let row = build_palette_row(entry);
        list_box.append(&row);
    }

    let scrolled = gtk4::ScrolledWindow::new();
    scrolled.set_child(Some(&list_box));
    scrolled.set_vexpand(true);
    scrolled.set_propagate_natural_height(true);
    scrolled.set_max_content_height(400);
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    outer.append(&scrolled);

    // Select the first row by default
    if let Some(first_row) = list_box.row_at_index(0) {
        list_box.select_row(Some(&first_row));
    }

    // --- Filtering logic ---
    // On search-changed, show/hide rows based on substring match and
    // auto-select the first visible row.
    {
        let lb = list_box.clone();
        search_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_string().to_lowercase();
            let registry = command_registry();
            let mut first_visible: Option<gtk4::ListBoxRow> = None;

            for (i, cmd) in registry.iter().enumerate() {
                let Some(row) = lb.row_at_index(i as i32) else {
                    continue;
                };
                let visible = query.is_empty() || cmd.display_name.to_lowercase().contains(&query);
                row.set_visible(visible);
                if visible && first_visible.is_none() {
                    first_visible = Some(row);
                }
            }

            // Auto-select first visible row
            if let Some(row) = first_visible {
                lb.select_row(Some(&row));
                // Scroll to make the selected row visible
                row.grab_focus();
                // Return focus to search entry after scroll adjustment
                entry.grab_focus();
            } else {
                lb.select_row(gtk4::ListBoxRow::NONE);
            }
        });
    }

    // --- Keyboard navigation ---
    // Up/Down arrows move selection; Enter executes; Escape closes.
    {
        let lb = list_box.clone();
        let outer_ref = outer.clone();
        let win = window.clone();
        let wm = Rc::clone(workspace_manager);
        let key_controller = gtk4::EventControllerKey::new();
        key_controller.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            match key {
                gtk4::gdk::Key::Escape => {
                    let ft = active_focus_tracker(&wm);
                    close_command_palette(&outer_ref, &ft);
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter => {
                    if let Some(row) = lb.selected_row() {
                        let index = row.index();
                        let registry = command_registry();
                        if let Some(cmd) = registry.get(index as usize) {
                            let action_name = cmd.action_name.to_string();
                            let ft = active_focus_tracker(&wm);
                            close_command_palette(&outer_ref, &ft);
                            // Defer action dispatch so the palette is fully hidden
                            // before any dialog opens (e.g. "Change Tab Title").
                            let win_deferred = win.clone();
                            glib::idle_add_local_once(move || {
                                let _ = gtk4::prelude::WidgetExt::activate_action(
                                    &win_deferred,
                                    &action_name,
                                    None,
                                );
                            });
                        }
                    }
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::Down => {
                    move_palette_selection(&lb, true);
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::Up => {
                    move_palette_selection(&lb, false);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        search_entry.add_controller(key_controller);
    }

    // --- Row activation (Enter or click on a row) ---
    {
        let outer_ref = outer.clone();
        let win = window.clone();
        let wm = Rc::clone(workspace_manager);
        list_box.connect_row_activated(move |_lb, row| {
            let index = row.index();
            let registry = command_registry();
            if let Some(cmd) = registry.get(index as usize) {
                let action_name = cmd.action_name.to_string();
                let ft = active_focus_tracker(&wm);
                close_command_palette(&outer_ref, &ft);
                // Defer action dispatch so the palette is fully hidden
                // before any dialog opens (e.g. "Change Tab Title").
                let win_deferred = win.clone();
                glib::idle_add_local_once(move || {
                    let _ = gtk4::prelude::WidgetExt::activate_action(
                        &win_deferred,
                        &action_name,
                        None,
                    );
                });
            }
        });
    }

    outer
}

/// Build a single row for the command palette list.
///
/// Each row is a horizontal Box with the command name on the left and
/// the shortcut label (if any) on the right in a muted style.
fn build_palette_row(entry: &CommandEntry) -> gtk4::ListBoxRow {
    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    hbox.set_margin_start(12);
    hbox.set_margin_end(12);
    hbox.set_margin_top(6);
    hbox.set_margin_bottom(6);

    let name_label = gtk4::Label::new(Some(entry.display_name));
    name_label.set_halign(gtk4::Align::Start);
    name_label.set_hexpand(true);
    hbox.append(&name_label);

    if !entry.shortcut_label.is_empty() {
        let shortcut_label = gtk4::Label::new(Some(entry.shortcut_label));
        shortcut_label.set_halign(gtk4::Align::End);
        shortcut_label.add_css_class("dim-label");
        hbox.append(&shortcut_label);
    }

    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&hbox));
    row
}

/// Move the palette selection up or down with wrapping.
///
/// Skips hidden (filtered-out) rows to navigate only visible entries.
fn move_palette_selection(list_box: &gtk4::ListBox, forward: bool) {
    let current_index = list_box.selected_row().map(|r| r.index()).unwrap_or(-1);

    // Collect indices of visible rows
    let mut visible_indices: Vec<i32> = Vec::new();
    let mut i = 0;
    while let Some(row) = list_box.row_at_index(i) {
        if row.is_visible() {
            visible_indices.push(i);
        }
        i += 1;
    }

    if visible_indices.is_empty() {
        return;
    }

    // Find current position in the visible list
    let current_pos = visible_indices.iter().position(|&idx| idx == current_index);
    let next_pos = match current_pos {
        Some(pos) => {
            if forward {
                (pos + 1) % visible_indices.len()
            } else if pos == 0 {
                visible_indices.len() - 1
            } else {
                pos - 1
            }
        }
        // No current selection: pick the first or last visible row
        None => {
            if forward {
                0
            } else {
                visible_indices.len() - 1
            }
        }
    };

    let target_index = visible_indices[next_pos];
    if let Some(row) = list_box.row_at_index(target_index) {
        list_box.select_row(Some(&row));
    }
}

/// Close the command palette and restore focus to the previously focused pane.
fn close_command_palette(palette: &gtk4::Box, focus_tracker: &FocusTracker) {
    palette.set_visible(false);

    // Restore focus to the DrawingArea that was focused before the palette opened
    let focused_name = {
        let Ok(name) = focus_tracker.try_borrow() else {
            return;
        };
        name.clone()
    };

    if focused_name.is_empty() {
        return;
    }

    let app =
        gtk4::gio::Application::default().and_then(|a| a.downcast::<gtk4::Application>().ok());
    let Some(app) = app else {
        return;
    };
    let Some(window) = app.active_window() else {
        return;
    };

    if let Some(da) = find_drawing_area_by_name(&window, &focused_name) {
        da.grab_focus();
    }
}

/// Open the command palette: show it, clear previous query, focus the search entry.
///
/// Resets all row visibility and selects the first row so the palette always
/// opens in a clean state.
fn open_command_palette(palette: &gtk4::Box, window: &adw::ApplicationWindow) {
    palette.set_visible(true);

    // Find the SearchEntry (first child) and clear + focus it
    let search_entry = palette.first_child().and_then(|w| w.downcast::<gtk4::SearchEntry>().ok());
    if let Some(ref entry) = search_entry {
        entry.set_text("");
    }

    // Find the ListBox (inside ScrolledWindow, third child: entry, separator, scrolled)
    // and ensure all rows are visible + first is selected.
    let mut child = palette.first_child();
    while let Some(c) = child {
        if let Some(scrolled) = c.downcast_ref::<gtk4::ScrolledWindow>() {
            if let Some(lb) = scrolled.child().and_then(|w| w.downcast::<gtk4::ListBox>().ok()) {
                let mut i = 0;
                while let Some(row) = lb.row_at_index(i) {
                    row.set_visible(true);
                    i += 1;
                }
                if let Some(first) = lb.row_at_index(0) {
                    lb.select_row(Some(&first));
                }
            }
            break;
        }
        child = c.next_sibling();
    }

    // Use the window's set_focus() to directly assign keyboard focus to the
    // search entry. This works reliably even before the widget is fully mapped,
    // unlike grab_focus() which silently fails on unrealized widgets.
    if let Some(entry) = search_entry {
        gtk4::prelude::GtkWindowExt::set_focus(window, Some(&entry));
    }
}

/// Toggle the command palette open/closed.
fn toggle_command_palette(
    palette: &gtk4::Box,
    focus_tracker: &FocusTracker,
    window: &adw::ApplicationWindow,
) {
    if palette.is_visible() {
        close_command_palette(palette, focus_tracker);
    } else {
        open_command_palette(palette, window);
    }
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

/// Wire the standard tab close and focus management handlers on a TabView.
///
/// Called for every workspace TabView except the initial one (which has these
/// handlers wired inline during build_ui setup).
fn wire_tab_view_handlers(
    tab_view: &adw::TabView,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    window: &adw::ApplicationWindow,
) {
    // Tab close handling
    {
        let window_close = window.clone();
        let states_close = Rc::clone(tab_states);
        tab_view.connect_close_page(move |tv, page| {
            let container = page.child();
            kill_all_panes_in_subtree(&container, &states_close);

            if tv.n_pages() <= 1 {
                window_close.close();
            }

            tv.close_page_finish(page, true);
            glib::Propagation::Stop
        });
    }

    // Focus management on tab switch
    {
        let focus_switch = Rc::clone(focus_tracker);
        tab_view.connect_selected_page_notify(move |tv| {
            if let Some(page) = tv.selected_page() {
                let container = page.child();
                let focused_name = focus_switch.borrow().clone();
                let leaves = collect_leaf_drawing_areas(&container);
                let target = leaves
                    .iter()
                    .find(|da| da.widget_name().as_str() == focused_name)
                    .or_else(|| leaves.first());
                if let Some(da) = target {
                    da.grab_focus();
                }
            }
        });
    }
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

        create_and_switch_to_new_workspace(&wm, &name, &config, &ma, &tb, &win);
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
) {
    let new_tv = adw::TabView::new();
    new_tv.set_vexpand(true);
    new_tv.set_hexpand(true);

    let new_tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));
    let new_focus_tracker: FocusTracker = Rc::new(RefCell::new(String::new()));
    let new_custom_titles: CustomTitles = Rc::new(RefCell::new(HashSet::new()));

    wire_tab_view_handlers(&new_tv, &new_tab_states, &new_focus_tracker, window);

    // Add a default tab to the new workspace.
    add_new_tab(
        &new_tv,
        config,
        &new_tab_states,
        &new_focus_tracker,
        &new_custom_titles,
        window,
        None,
        None,
        None, // new workspaces are always local, no daemon client
    );

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
            id: uuid::Uuid::new_v4(),
            name: name.to_string(),
            tab_view: new_tv,
            tab_states: new_tab_states,
            focus_tracker: new_focus_tracker,
            custom_titles: new_custom_titles,
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
) {
    let Ok(mgr) = workspace_manager.try_borrow() else {
        return;
    };
    let current_name = mgr.workspaces[mgr.active_index].name.clone();
    drop(mgr);

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

        let Ok(mut mgr) = wm.try_borrow_mut() else {
            return;
        };
        let active = mgr.active_index;
        mgr.workspaces[active].name = new_name.clone();
        let ws_count = mgr.workspaces.len();
        drop(mgr);

        update_window_title_with_workspace(ws_count, &new_name, &wm, &win);
    });

    dialog.present();
}

/// Delete the current workspace. Kills all its PTYs and switches to an adjacent one.
fn delete_current_workspace(
    workspace_manager: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
) {
    let Ok(mut mgr) = workspace_manager.try_borrow_mut() else {
        return;
    };

    if mgr.workspaces.len() <= 1 {
        return; // Cannot delete the last workspace.
    }

    let delete_idx = mgr.active_index;
    let ws = &mgr.workspaces[delete_idx];

    // Kill all PTYs in the workspace.
    kill_all_ptys(&ws.tab_states, "Delete workspace");

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
// Workspace selector overlay
// ---------------------------------------------------------------------------

/// Build the workspace selector overlay widget.
///
/// Shows a card with a ListBox of workspace names. Active workspace is
/// highlighted. Click or Enter switches, Escape closes.
/// Returns (outer_container, list_box) so callers can pass the ListBox directly.
fn build_workspace_selector(
    workspace_manager: &WorkspaceManager,
    main_area: &gtk4::Box,
    tab_bar: &adw::TabBar,
    window: &adw::ApplicationWindow,
) -> (gtk4::Box, gtk4::ListBox) {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.set_halign(gtk4::Align::Center);
    outer.set_valign(gtk4::Align::Start);
    outer.set_margin_top(60);
    outer.set_hexpand(true);
    outer.set_vexpand(true);
    outer.set_width_request(300);
    outer.set_visible(false);
    outer.set_can_focus(false);
    outer.add_css_class("card");

    let title_label = gtk4::Label::new(Some("Workspaces"));
    title_label.add_css_class("title-4");
    title_label.set_margin_top(12);
    title_label.set_margin_bottom(8);
    outer.append(&title_label);

    let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    outer.append(&separator);

    let list_box = gtk4::ListBox::new();
    list_box.set_selection_mode(gtk4::SelectionMode::Single);
    list_box.add_css_class("navigation-sidebar");
    list_box.set_focusable(true);
    list_box.set_can_focus(true);

    let scrolled = gtk4::ScrolledWindow::new();
    scrolled.set_child(Some(&list_box));
    scrolled.set_vexpand(true);
    scrolled.set_propagate_natural_height(true);
    scrolled.set_max_content_height(400);
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    outer.append(&scrolled);

    // Row activation (click or Enter)
    {
        let outer_ref = outer.clone();
        let wm = Rc::clone(workspace_manager);
        let ma = main_area.clone();
        let tb = tab_bar.clone();
        let win = window.clone();
        list_box.connect_row_activated(move |_lb, row| {
            let target = row.index() as usize;
            outer_ref.set_visible(false);
            switch_workspace(&wm, target, &ma, &tb, &win);
        });
    }

    // Keyboard handling on the list box
    {
        let outer_ref = outer.clone();
        let wm = Rc::clone(workspace_manager);
        let ma = main_area.clone();
        let tb = tab_bar.clone();
        let win = window.clone();
        let lb = list_box.clone();
        let key_controller = gtk4::EventControllerKey::new();
        key_controller.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            match key {
                gtk4::gdk::Key::Escape => {
                    outer_ref.set_visible(false);
                    // Restore focus to the active workspace's pane.
                    if let Ok(mgr) = wm.try_borrow() {
                        let ws = &mgr.workspaces[mgr.active_index];
                        if let Some(page) = ws.tab_view.selected_page() {
                            let container = page.child();
                            let leaves = collect_leaf_drawing_areas(&container);
                            if let Some(da) = leaves.first() {
                                da.grab_focus();
                            }
                        }
                    }
                    glib::Propagation::Stop
                }
                gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter => {
                    if let Some(row) = lb.selected_row() {
                        let target = row.index() as usize;
                        outer_ref.set_visible(false);
                        switch_workspace(&wm, target, &ma, &tb, &win);
                    }
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        list_box.add_controller(key_controller);
    }

    (outer, list_box)
}

/// Populate the workspace selector with current workspace names and show/hide it.
fn toggle_workspace_selector(
    selector: &gtk4::Box,
    lb: &gtk4::ListBox,
    workspace_manager: &WorkspaceManager,
) {
    if selector.is_visible() {
        selector.set_visible(false);
        // Restore focus to the active workspace.
        if let Ok(mgr) = workspace_manager.try_borrow() {
            let ws = &mgr.workspaces[mgr.active_index];
            if let Some(page) = ws.tab_view.selected_page() {
                let container = page.child();
                let leaves = collect_leaf_drawing_areas(&container);
                if let Some(da) = leaves.first() {
                    da.grab_focus();
                }
            }
        }
        return;
    }

    // Rebuild the list contents from the current workspace manager state.
    let Ok(mgr) = workspace_manager.try_borrow() else {
        return;
    };

    // Remove all existing rows.
    while let Some(row) = lb.row_at_index(0) {
        lb.remove(&row);
    }

    // Add a row for each workspace.
    for (i, ws) in mgr.workspaces.iter().enumerate() {
        let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        hbox.set_margin_start(12);
        hbox.set_margin_end(12);
        hbox.set_margin_top(6);
        hbox.set_margin_bottom(6);

        let label = gtk4::Label::new(Some(&ws.name));
        label.set_halign(gtk4::Align::Start);
        label.set_hexpand(true);
        if i == mgr.active_index {
            label.add_css_class("heading");
        }
        hbox.append(&label);

        // Show position number
        let pos_label = gtk4::Label::new(Some(&format!("{}", i + 1)));
        pos_label.add_css_class("dim-label");
        pos_label.set_halign(gtk4::Align::End);
        hbox.append(&pos_label);

        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&hbox));
        lb.append(&row);
    }

    // Select the active workspace row.
    if let Some(row) = lb.row_at_index(mgr.active_index as i32) {
        lb.select_row(Some(&row));
    }

    drop(mgr);

    selector.set_visible(true);

    // Defer focus grab to next idle tick so GTK finishes overlay layout first.
    let lb_focus = lb.clone();
    glib::idle_add_local_once(move || {
        lb_focus.grab_focus();
    });
}
