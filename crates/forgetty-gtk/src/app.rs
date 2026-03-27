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

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use forgetty_config::{load_config, Config};
use forgetty_watcher::ConfigWatcher;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use tracing::info;

use crate::clipboard;
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

/// Run the GTK4/libadwaita application.
///
/// This function blocks until the window is closed. It initialises libadwaita,
/// creates the main application window with CSD header bar, and enters the
/// GTK main loop.
pub fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        build_ui(app, &config);
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
fn build_ui(app: &adw::Application, config: &Config) {
    info!("Building Forgetty GTK4 window");

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
    let menu = gio::Menu::new();
    menu.append(Some("Keyboard Shortcuts"), Some("win.show-shortcuts"));
    menu.append(Some("About Forgetty"), Some("win.show-about"));

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

    let tab_view = adw::TabView::new();
    tab_view.set_vexpand(true);

    let tab_bar = adw::TabBar::new();
    tab_bar.set_view(Some(&tab_view));
    tab_bar.set_autohide(true);
    tab_bar.set_hexpand(true);

    // The header bar keeps its default title ("Forgetty" from the window title).
    // The tab bar lives as a separate row below the header, auto-hidden when
    // only one tab exists (matching Ghostty's two-row layout for 2+ tabs).

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&tab_bar);
    content.append(&tab_view);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Forgetty")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&content)
        .build();

    // Pane state registry -- maps pane widget names to their TerminalState
    let tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));

    // Focus tracker -- widget name of the currently focused DrawingArea
    let focus_tracker: FocusTracker = Rc::new(RefCell::new(String::new()));

    // Shared config -- updated on hot reload, read by new tab/split creation.
    // All action closures that create terminals capture a clone of this Rc.
    let shared_config: SharedConfig = Rc::new(RefCell::new(config.clone()));

    // --- Tab close handling ---
    // When a tab's close button is clicked, kill ALL PTYs in the tab's
    // widget tree (may contain nested Paned widgets with multiple panes).
    // If it is the last tab, close the window (exits the application).
    {
        let window_close = window.clone();
        let states_close = Rc::clone(&tab_states);
        tab_view.connect_close_page(move |tv, page| {
            let container = page.child();
            kill_all_panes_in_subtree(&container, &states_close);

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
        tab_view.connect_selected_page_notify(move |tv| {
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
        let tv_action = tab_view.clone();
        let states_action = Rc::clone(&tab_states);
        let focus_action = Rc::clone(&focus_tracker);
        let win_action = window.clone();
        let action = gio::SimpleAction::new("new-tab", None);
        action.connect_activate(move |_action, _param| {
            let Ok(cfg) = config_action.try_borrow() else {
                return;
            };
            add_new_tab(&tv_action, &cfg, &states_action, &focus_action, &win_action);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.new-tab", &["<Control><Shift>t"]);

    // --- Split right action (Alt+Shift+=) ---
    {
        let config_split = Rc::clone(&shared_config);
        let tv_split = tab_view.clone();
        let states_split = Rc::clone(&tab_states);
        let focus_split = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("split-right", None);
        action.connect_activate(move |_action, _param| {
            let Ok(cfg) = config_split.try_borrow() else {
                return;
            };
            split_pane(
                &tv_split,
                &cfg,
                &states_split,
                &focus_split,
                gtk4::Orientation::Horizontal,
                false,
            );
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.split-right", &["<Alt><Shift>equal", "<Alt>plus"]);

    // --- Split down action (Alt+Shift+-) ---
    {
        let config_split = Rc::clone(&shared_config);
        let tv_split = tab_view.clone();
        let states_split = Rc::clone(&tab_states);
        let focus_split = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("split-down", None);
        action.connect_activate(move |_action, _param| {
            let Ok(cfg) = config_split.try_borrow() else {
                return;
            };
            split_pane(
                &tv_split,
                &cfg,
                &states_split,
                &focus_split,
                gtk4::Orientation::Vertical,
                false,
            );
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.split-down", &["<Alt><Shift>minus", "<Alt>underscore"]);

    // --- Split left action (dropdown menu only, no default accelerator) ---
    {
        let config_split = Rc::clone(&shared_config);
        let tv_split = tab_view.clone();
        let states_split = Rc::clone(&tab_states);
        let focus_split = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("split-left", None);
        action.connect_activate(move |_action, _param| {
            let Ok(cfg) = config_split.try_borrow() else {
                return;
            };
            split_pane(
                &tv_split,
                &cfg,
                &states_split,
                &focus_split,
                gtk4::Orientation::Horizontal,
                true,
            );
        });
        window.add_action(&action);
    }

    // --- Split up action (dropdown menu only, no default accelerator) ---
    {
        let config_split = Rc::clone(&shared_config);
        let tv_split = tab_view.clone();
        let states_split = Rc::clone(&tab_states);
        let focus_split = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("split-up", None);
        action.connect_activate(move |_action, _param| {
            let Ok(cfg) = config_split.try_borrow() else {
                return;
            };
            split_pane(
                &tv_split,
                &cfg,
                &states_split,
                &focus_split,
                gtk4::Orientation::Vertical,
                true,
            );
        });
        window.add_action(&action);
    }

    // --- Close pane action (Ctrl+Shift+W) ---
    {
        let tv_close = tab_view.clone();
        let states_close = Rc::clone(&tab_states);
        let focus_close = Rc::clone(&focus_tracker);
        let window_close = window.clone();
        let action = gio::SimpleAction::new("close-pane", None);
        action.connect_activate(move |_action, _param| {
            close_focused_pane(&tv_close, &states_close, &focus_close, &window_close);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.close-pane", &["<Control><Shift>w"]);

    // --- Copy selection action (Ctrl+Shift+C) ---
    {
        let states_copy = Rc::clone(&tab_states);
        let focus_copy = Rc::clone(&focus_tracker);
        let window_copy = window.clone();
        let action = gio::SimpleAction::new("copy", None);
        action.connect_activate(move |_action, _param| {
            copy_selection(&states_copy, &focus_copy, &window_copy);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.copy", &["<Control><Shift>c"]);

    // --- Search in terminal action (Ctrl+Shift+F) ---
    {
        let states_search = Rc::clone(&tab_states);
        let focus_search = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("search", None);
        action.connect_activate(move |_action, _param| {
            toggle_search_on_focused_pane(&states_search, &focus_search);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.search", &["<Control><Shift>f"]);

    // --- Paste action (Ctrl+Shift+V) ---
    {
        let states_paste = Rc::clone(&tab_states);
        let focus_paste = Rc::clone(&focus_tracker);
        let window_paste = window.clone();
        let action = gio::SimpleAction::new("paste", None);
        action.connect_activate(move |_action, _param| {
            paste_clipboard(&states_paste, &focus_paste, &window_paste);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.paste", &["<Control><Shift>v"]);

    // --- Select All action (context menu only, no accelerator) ---
    {
        let states_sel = Rc::clone(&tab_states);
        let focus_sel = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("select-all", None);
        action.connect_activate(move |_action, _param| {
            select_all_on_focused_pane(&states_sel, &focus_sel);
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
        let tv_nav = tab_view.clone();
        let focus_nav = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_action, _param| {
            navigate_pane(&tv_nav, &focus_nav, direction);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.focus-pane-left", &["<Alt>Left"]);
    app.set_accels_for_action("win.focus-pane-right", &["<Alt>Right"]);
    app.set_accels_for_action("win.focus-pane-up", &["<Alt>Up"]);
    app.set_accels_for_action("win.focus-pane-down", &["<Alt>Down"]);

    // --- Zoom in action (Ctrl+= / Ctrl++) ---
    {
        let states_zoom = Rc::clone(&tab_states);
        let focus_zoom = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("zoom-in", None);
        action.connect_activate(move |_action, _param| {
            zoom_focused_pane(&states_zoom, &focus_zoom, ZoomDirection::In);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.zoom-in", &["<Control>equal", "<Control>plus"]);

    // --- Zoom out action (Ctrl+-) ---
    {
        let states_zoom = Rc::clone(&tab_states);
        let focus_zoom = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("zoom-out", None);
        action.connect_activate(move |_action, _param| {
            zoom_focused_pane(&states_zoom, &focus_zoom, ZoomDirection::Out);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.zoom-out", &["<Control>minus"]);

    // --- Zoom reset action (Ctrl+0) ---
    {
        let states_zoom = Rc::clone(&tab_states);
        let focus_zoom = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("zoom-reset", None);
        action.connect_activate(move |_action, _param| {
            zoom_focused_pane(&states_zoom, &focus_zoom, ZoomDirection::Reset);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.zoom-reset", &["<Control>0"]);

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

    // --- Create the first tab ---
    add_new_tab(&tab_view, config, &tab_states, &focus_tracker, &window);

    window.present();

    // Grab focus on the first tab's DrawingArea
    if let Some(page) = tab_view.selected_page() {
        let container = page.child();
        let leaves = collect_leaf_drawing_areas(&container);
        if let Some(da) = leaves.first() {
            da.grab_focus();
        }
    }

    // --- Config hot reload timer ---
    // Polls the config watcher every 500ms. On change, reloads config.toml
    // and applies diffs (font, theme, bell) to all existing panes.
    if let Some(mut config_watcher) = ConfigWatcher::new() {
        let shared_cfg = Rc::clone(&shared_config);
        let states_reload = Rc::clone(&tab_states);
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

            // Apply changes to every existing pane.
            let Ok(states) = states_reload.try_borrow() else {
                return glib::ControlFlow::Continue;
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

            glib::ControlFlow::Continue
        });
    }
}

// ---------------------------------------------------------------------------
// Tab management
// ---------------------------------------------------------------------------

/// Add a new terminal tab to the TabView.
///
/// Creates a new DrawingArea + TerminalState pair via `create_terminal()`,
/// wraps it in a pane container Box, appends a page to the TabView, sets up
/// title polling, and selects the new tab.
fn add_new_tab(
    tab_view: &adw::TabView,
    config: &Config,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    window: &adw::ApplicationWindow,
) {
    match terminal::create_terminal(config) {
        Ok((pane_vbox, drawing_area, state)) => {
            // Assign a unique widget name for registry lookup
            let pane_id = next_pane_id();
            drawing_area.set_widget_name(&pane_id);

            // Register in the pane state map
            tab_states.borrow_mut().insert(pane_id, Rc::clone(&state));

            // Wire up focus tracking on this pane
            wire_focus_tracking(&drawing_area, focus_tracker, tab_view, tab_states);

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
                window.set_title(Some(&compute_window_title(&s)));
                page.set_title(&compute_display_title(&s));
            }

            // --- Title polling timer ---
            // Periodically update the tab title from the focused pane's CWD.
            register_title_timer(&page, tab_view, tab_states, focus_tracker, &window);
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
    orientation: gtk4::Orientation,
    before: bool,
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

    // Create the new terminal pane
    let (new_pane_vbox, new_da, new_state) = match terminal::create_terminal(config) {
        Ok(triple) => triple,
        Err(e) => {
            tracing::error!("Failed to create terminal for split: {e}");
            return;
        }
    };

    let new_pane_id = next_pane_id();
    new_da.set_widget_name(&new_pane_id);
    tab_states.borrow_mut().insert(new_pane_id, new_state);
    wire_focus_tracking(&new_da, focus_tracker, tab_view, tab_states);

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
/// If the pane is the only one in the tab, close the tab.
/// If the tab is the only tab, close the window.
fn close_focused_pane(
    tab_view: &adw::TabView,
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

    // Find the focused DrawingArea
    let focused_da = leaves.iter().find(|da| da.widget_name().as_str() == focused_name);

    let Some(focused_da) = focused_da.cloned() else {
        return;
    };

    // If this is the only pane in the tab, close the tab
    if leaves.len() <= 1 {
        // Kill the PTY
        kill_pane(&focused_name, tab_states);

        if tab_view.n_pages() <= 1 {
            window.close();
        } else {
            tab_view.close_page(&page);
        }
        return;
    }

    // The DrawingArea lives inside: DA -> hbox -> vbox.
    // Navigate: DrawingArea -> hbox -> vbox -> parent Paned.
    let Some(hbox_widget) = focused_da.parent() else {
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

    // Kill the closed pane's PTY and remove from registry
    kill_pane(&focused_name, tab_states);

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
            if let Err(e) = s.pty.kill() {
                tracing::warn!("Failed to kill PTY for pane {pane_id}: {e}");
            }
        }
    }
    tab_states.borrow_mut().remove(pane_id);
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
) {
    let focus_controller = gtk4::EventControllerFocus::new();

    // Focus gained -- update the tracker and tab title immediately
    {
        let tracker = Rc::clone(focus_tracker);
        let da = drawing_area.clone();
        let tv = tab_view.clone();
        let states = Rc::clone(tab_states);
        focus_controller.connect_enter(move |_controller| {
            let pane_name = da.widget_name().to_string();
            if let Ok(mut name) = tracker.try_borrow_mut() {
                *name = pane_name.clone();
            }
            // Redraw to show the focus indicator
            da.queue_draw();
            // Update tab title immediately from this pane's CWD
            if let Some(page) = tv.selected_page() {
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
    window: &adw::ApplicationWindow,
) {
    let page_weak = page.downgrade();
    let tab_states_title = Rc::clone(tab_states);
    let focus_title = Rc::clone(focus_tracker);
    let tv_weak = tab_view.downgrade();
    let win_weak = window.downgrade();

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
            // Update window title bar with user@host:cwd for the focused pane
            if let Some(win) = win_weak.upgrade() {
                let win_title = compute_window_title(&s);
                if win.title().map(|t| t.as_str() != win_title).unwrap_or(true) {
                    win.set_title(Some(&win_title));
                }
            }
        }

        glib::ControlFlow::Continue
    });
}

/// Compute the display title for a terminal tab.
///
/// Priority: CWD basename > OSC title > "shell".
/// Adapted from `crates/forgetty-ui/src/pane.rs::display_title()`.
fn compute_display_title(state: &TerminalState) -> String {
    // Try to read CWD from /proc/{pid}/cwd
    if let Some(pid) = state.pty.pid() {
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

    // Fall back to OSC title if set
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

    if let Some(pid) = state.pty.pid() {
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

    // Clone state_rc for the async callback
    let state_for_cb = Rc::clone(&state_rc);
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

        // Write the pasted text to the PTY
        let Ok(mut s) = state_for_cb.try_borrow_mut() else {
            return;
        };

        if let Err(e) = s.pty.write(text.as_bytes()) {
            tracing::warn!("Failed to write paste to PTY: {e}");
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
    clipboard_group.add_shortcut(&shortcut("<Control><Shift>c", "Copy Selection"));
    clipboard_group.add_shortcut(&shortcut("<Control><Shift>v", "Paste"));
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
