//! GTK4 application entry point.
//!
//! Creates and runs the adw::Application, managing the window lifecycle
//! (open, resize, close) with native GNOME client-side decorations.
//! Uses adw::TabBar + adw::TabView for multi-tab terminal sessions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use forgetty_config::Config;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use tracing::info;

use crate::terminal::{self, TerminalState};

/// The application ID used for D-Bus registration and desktop integration.
const APP_ID: &str = "dev.forgetty.Forgetty";

/// Default window width in pixels.
const DEFAULT_WIDTH: i32 = 960;

/// Default window height in pixels.
const DEFAULT_HEIGHT: i32 = 640;

/// Interval for polling CWD / OSC title changes (milliseconds).
const TITLE_POLL_MS: u64 = 1500;

/// Lookup table mapping each tab's DrawingArea widget name to its TerminalState.
///
/// This is NOT shared mutable terminal state -- it is a simple registry so that
/// the tab-close handler can find and kill the correct PTY. Each tab's
/// `TerminalState` is independently owned by its own closures.
type TabStateMap = Rc<RefCell<HashMap<String, Rc<RefCell<TerminalState>>>>>;

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

/// Generate a unique widget name for each tab's DrawingArea.
fn next_tab_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("forgetty-tab-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Build the main application window with tab bar and initial terminal tab.
fn build_ui(app: &adw::Application, config: &Config) {
    info!("Building Forgetty GTK4 window");

    // --- Widget hierarchy ---
    // adw::ApplicationWindow
    //   content: gtk::Box (vertical)
    //     [0] adw::HeaderBar
    //     [1] adw::TabBar (linked to tab_view)
    //     [2] adw::TabView (holds terminal DrawingAreas as pages)

    let header = adw::HeaderBar::new();

    let tab_view = adw::TabView::new();
    tab_view.set_vexpand(true);

    let tab_bar = adw::TabBar::new();
    tab_bar.set_view(Some(&tab_view));
    tab_bar.set_autohide(false);

    // "+" button for creating new tabs via mouse
    let new_tab_button = gtk4::Button::from_icon_name("tab-new-symbolic");
    new_tab_button.set_tooltip_text(Some("New Tab (Ctrl+Shift+T)"));
    tab_bar.set_end_action_widget(Some(&new_tab_button));

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

    // Tab state registry -- maps tab widget names to their TerminalState
    let tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));

    // --- Tab close handling ---
    // When a tab's close button is clicked, kill the PTY and confirm the close.
    // If it is the last tab, close the window (exits the application).
    {
        let window_close = window.clone();
        let states_close = Rc::clone(&tab_states);
        tab_view.connect_close_page(move |tv, page| {
            let child = page.child();
            if let Some(da) = child.downcast_ref::<gtk4::DrawingArea>() {
                let tab_id = da.widget_name().to_string();

                // Kill the PTY for this tab
                if let Some(state_rc) = states_close.borrow().get(&tab_id) {
                    if let Ok(mut s) = state_rc.try_borrow_mut() {
                        if let Err(e) = s.pty.kill() {
                            tracing::warn!("Failed to kill PTY on tab close: {e}");
                        }
                    }
                }

                // Remove from the registry
                states_close.borrow_mut().remove(&tab_id);
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
    {
        tab_view.connect_selected_page_notify(move |tv| {
            if let Some(page) = tv.selected_page() {
                let child = page.child();
                if let Some(da) = child.downcast_ref::<gtk4::DrawingArea>() {
                    da.grab_focus();
                }
            }
        });
    }

    // --- New tab action (Ctrl+Shift+T) ---
    {
        let config_action = config.clone();
        let tv_action = tab_view.clone();
        let states_action = Rc::clone(&tab_states);
        let action = gio::SimpleAction::new("new-tab", None);
        action.connect_activate(move |_action, _param| {
            add_new_tab(&tv_action, &config_action, &states_action);
        });
        window.add_action(&action);
    }

    // Set the keyboard shortcut for the action
    app.set_accels_for_action("win.new-tab", &["<Control><Shift>t"]);

    // --- "+" button click ---
    {
        let config_btn = config.clone();
        let tv_btn = tab_view.clone();
        let states_btn = Rc::clone(&tab_states);
        new_tab_button.connect_clicked(move |_btn| {
            add_new_tab(&tv_btn, &config_btn, &states_btn);
        });
    }

    // --- Create the first tab ---
    add_new_tab(&tab_view, config, &tab_states);

    window.present();

    // Grab focus on the first tab's DrawingArea
    if let Some(page) = tab_view.selected_page() {
        let child = page.child();
        if let Some(da) = child.downcast_ref::<gtk4::DrawingArea>() {
            da.grab_focus();
        }
    }
}

/// Add a new terminal tab to the TabView.
///
/// Creates a new DrawingArea + TerminalState pair via `create_terminal()`,
/// appends a page to the TabView, sets up title polling, and selects the
/// new tab.
fn add_new_tab(tab_view: &adw::TabView, config: &Config, tab_states: &TabStateMap) {
    match terminal::create_terminal(config) {
        Ok((drawing_area, state)) => {
            // Assign a unique widget name for registry lookup
            let tab_id = next_tab_id();
            drawing_area.set_widget_name(&tab_id);

            // Register in the tab state map
            tab_states.borrow_mut().insert(tab_id, Rc::clone(&state));

            // Append the page to the TabView
            let page = tab_view.append(&drawing_area);
            page.set_title("shell");

            // Make this the selected (active) tab
            tab_view.set_selected_page(&page);

            // Grab focus so keyboard input goes to this terminal
            drawing_area.grab_focus();

            // --- Title polling timer ---
            // Periodically update the tab title from CWD or OSC title.
            {
                let state_title = Rc::clone(&state);
                let page_weak = page.downgrade();
                let da_weak = drawing_area.downgrade();
                glib::timeout_add_local(Duration::from_millis(TITLE_POLL_MS), move || {
                    // Stop the timer if the page or drawing area has been destroyed
                    let Some(page) = page_weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    let Some(_da) = da_weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };

                    let Ok(s) = state_title.try_borrow() else {
                        // Borrow held elsewhere -- skip this tick
                        return glib::ControlFlow::Continue;
                    };

                    let title = compute_display_title(&s);
                    let current_title = page.title();
                    if current_title.as_str() != title {
                        page.set_title(&title);
                    }

                    glib::ControlFlow::Continue
                });
            }
        }
        Err(e) => {
            tracing::error!("Failed to create terminal for new tab: {e}");
        }
    }
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
