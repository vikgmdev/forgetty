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

use forgetty_config::Config;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use tracing::info;

use crate::clipboard;
use crate::terminal::{self, TerminalState};

/// The application ID used for D-Bus registration and desktop integration.
const APP_ID: &str = "dev.forgetty.Forgetty";

/// Default window width in pixels.
const DEFAULT_WIDTH: i32 = 960;

/// Default window height in pixels.
const DEFAULT_HEIGHT: i32 = 640;

/// Interval for polling CWD / OSC title changes (milliseconds).
const TITLE_POLL_MS: u64 = 1500;

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
    //     [0] adw::HeaderBar
    //     [1] adw::TabBar (linked to tab_view)
    //     [2] adw::TabView (holds pane containers as pages)
    //
    // Each tab page child is a gtk::Box (the "pane container"), which holds
    // either a single DrawingArea or a nested gtk::Paned tree.

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

    // Pane state registry -- maps pane widget names to their TerminalState
    let tab_states: TabStateMap = Rc::new(RefCell::new(HashMap::new()));

    // Focus tracker -- widget name of the currently focused DrawingArea
    let focus_tracker: FocusTracker = Rc::new(RefCell::new(String::new()));

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
        let config_action = config.clone();
        let tv_action = tab_view.clone();
        let states_action = Rc::clone(&tab_states);
        let focus_action = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("new-tab", None);
        action.connect_activate(move |_action, _param| {
            add_new_tab(&tv_action, &config_action, &states_action, &focus_action);
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.new-tab", &["<Control><Shift>t"]);

    // --- Split right action (Alt+Shift+=) ---
    {
        let config_split = config.clone();
        let tv_split = tab_view.clone();
        let states_split = Rc::clone(&tab_states);
        let focus_split = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("split-right", None);
        action.connect_activate(move |_action, _param| {
            split_pane(
                &tv_split,
                &config_split,
                &states_split,
                &focus_split,
                gtk4::Orientation::Horizontal,
            );
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.split-right", &["<Alt><Shift>equal", "<Alt>plus"]);

    // --- Split down action (Alt+Shift+-) ---
    {
        let config_split = config.clone();
        let tv_split = tab_view.clone();
        let states_split = Rc::clone(&tab_states);
        let focus_split = Rc::clone(&focus_tracker);
        let action = gio::SimpleAction::new("split-down", None);
        action.connect_activate(move |_action, _param| {
            split_pane(
                &tv_split,
                &config_split,
                &states_split,
                &focus_split,
                gtk4::Orientation::Vertical,
            );
        });
        window.add_action(&action);
    }

    app.set_accels_for_action("win.split-down", &["<Alt><Shift>minus", "<Alt>underscore"]);

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

    // --- "+" button click ---
    {
        let config_btn = config.clone();
        let tv_btn = tab_view.clone();
        let states_btn = Rc::clone(&tab_states);
        let focus_btn = Rc::clone(&focus_tracker);
        new_tab_button.connect_clicked(move |_btn| {
            add_new_tab(&tv_btn, &config_btn, &states_btn, &focus_btn);
        });
    }

    // --- Create the first tab ---
    add_new_tab(&tab_view, config, &tab_states, &focus_tracker);

    window.present();

    // Grab focus on the first tab's DrawingArea
    if let Some(page) = tab_view.selected_page() {
        let container = page.child();
        let leaves = collect_leaf_drawing_areas(&container);
        if let Some(da) = leaves.first() {
            da.grab_focus();
        }
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
) {
    match terminal::create_terminal(config) {
        Ok((hbox, drawing_area, state)) => {
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
            container.append(&hbox);

            // Append the page to the TabView
            let page = tab_view.append(&container);
            page.set_title("shell");

            // Make this the selected (active) tab
            tab_view.set_selected_page(&page);

            // Grab focus so keyboard input goes to this terminal
            drawing_area.grab_focus();

            // --- Title polling timer ---
            // Periodically update the tab title from the focused pane's CWD.
            register_title_timer(&page, tab_view, tab_states, focus_tracker);
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
fn split_pane(
    tab_view: &adw::TabView,
    config: &Config,
    tab_states: &TabStateMap,
    focus_tracker: &FocusTracker,
    orientation: gtk4::Orientation,
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
    let (new_hbox, new_da, new_state) = match terminal::create_terminal(config) {
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

    // The DrawingArea lives inside an hbox (with the scrollbar).
    // We need to operate on the hbox for tree manipulation.
    let focused_hbox: gtk4::Widget =
        focused_da.parent().expect("focused DA should have a parent hbox");

    // Determine where the hbox sits in the widget tree.
    // Detect the slot BEFORE removing the child.
    let is_direct_child = root_content == focused_hbox;
    let parent = focused_hbox.parent();
    let parent_slot = parent.as_ref().and_then(|p| {
        p.downcast_ref::<gtk4::Paned>().map(|pp| detect_paned_slot(pp, &focused_hbox))
    });

    // Remove the hbox from its current parent.
    // IMPORTANT: For Paned parents, we MUST use set_start/end_child(None)
    // instead of unparent(). Direct unparent() doesn't clear the Paned's
    // internal child pointer, so a later set_start/end_child() would
    // double-unparent the widget from its new location.
    if is_direct_child {
        focused_hbox.unparent();
    } else if let Some(ref parent_widget) = parent {
        if let Some(parent_paned) = parent_widget.downcast_ref::<gtk4::Paned>() {
            match parent_slot.unwrap_or(PanedSlot::End) {
                PanedSlot::Start => parent_paned.set_start_child(gtk4::Widget::NONE),
                PanedSlot::End => parent_paned.set_end_child(gtk4::Widget::NONE),
            }
        } else {
            focused_hbox.unparent();
        }
    }

    // Set up the Paned children: original hbox on start, new hbox on end
    paned.set_start_child(Some(&focused_hbox));
    paned.set_end_child(Some(&new_hbox));

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

    // The DrawingArea lives inside an hbox (with the scrollbar).
    // Navigate: DrawingArea -> hbox -> parent Paned.
    let Some(hbox_widget) = focused_da.parent() else {
        return;
    };
    let Some(parent_widget) = hbox_widget.parent() else {
        return;
    };

    let Some(parent_paned) = parent_widget.downcast_ref::<gtk4::Paned>() else {
        return;
    };

    // Determine the sibling (the other child of the parent Paned)
    let slot = detect_paned_slot(parent_paned, &hbox_widget);
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
    // The screen() viewport only shows a window of rows starting at the
    // current viewport offset.  To extract text from the selection, we
    // temporarily scroll the viewport so the selection start is visible,
    // extract, then restore the original viewport position.
    let sel_clone = sel.clone();
    let (_, orig_offset, _) = s.terminal.scrollbar_state();
    let sel_start_row = sel_clone.ordered().0 .0;
    let delta = sel_start_row as isize - orig_offset as isize;
    if delta != 0 {
        s.terminal.scroll_viewport_delta(delta);
    }

    // Now the viewport starts at the selection's first row.
    // Convert absolute selection rows to viewport-relative for extract_text().
    let (_, vp_offset, _) = s.terminal.scrollbar_state();
    let vp_offset = vp_offset as usize;
    let mut viewport_sel = sel_clone.clone();
    viewport_sel.start.0 = sel_clone.start.0.saturating_sub(vp_offset);
    viewport_sel.end.0 = sel_clone.end.0.saturating_sub(vp_offset);

    // Extract text from the screen at the selected cell range
    let screen = s.terminal.screen();
    let raw_text = viewport_sel.extract_text(screen);

    // Restore the original viewport position
    if delta != 0 {
        s.terminal.scroll_viewport_delta(-delta);
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
) {
    let page_weak = page.downgrade();
    let tab_states_title = Rc::clone(tab_states);
    let focus_title = Rc::clone(focus_tracker);
    let tv_weak = tab_view.downgrade();

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
