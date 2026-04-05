//! Full-window Settings view for Forgetty.
//!
//! Opens as a full-window takeover via the "Settings" hamburger menu item.
//! Left nav + right pane layout with sections: General, Terminal, Devices, About.
//! Appearance and Keyboard are excluded — they have dedicated views
//! (Ctrl+, sidebar and F1 ShortcutsWindow respectively).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use forgetty_config::{
    load_config_as_text, parse_and_save_config, BellMode, Config, CursorStyle, NotificationMode,
    OnLaunch,
};
use gtk4::prelude::*;
use libadwaita as adw;

use crate::daemon_client::DaemonClient;
use crate::preferences::{build_sync_section, save_in_background};

type SharedConfig = Rc<RefCell<Config>>;

/// Build the full-window settings view widget.
///
/// Returns a `gtk4::Box` suitable for adding to an outer `gtk4::Stack` as the
/// "settings" page. `on_back` is called when the user clicks "← Back".
pub fn build_settings_view(
    shared_config: &SharedConfig,
    daemon_client: Option<Arc<DaemonClient>>,
    on_back: impl Fn() + 'static,
) -> gtk4::Box {
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    // -------------------------------------------------------------------------
    // Header row: back button + "Settings" title
    // -------------------------------------------------------------------------
    let settings_header = adw::HeaderBar::new();
    settings_header.set_show_end_title_buttons(false);
    settings_header.set_show_start_title_buttons(false);

    let back_btn = gtk4::Button::with_label("← Back");
    back_btn.add_css_class("flat");
    let on_back_rc: Rc<dyn Fn()> = Rc::new(on_back);
    {
        let cb = Rc::clone(&on_back_rc);
        back_btn.connect_clicked(move |_| cb());
    }
    settings_header.pack_start(&back_btn);

    let title_lbl = gtk4::Label::new(Some("Settings"));
    title_lbl.add_css_class("title-4");
    settings_header.set_title_widget(Some(&title_lbl));

    root.append(&settings_header);

    // Escape key closes the settings view (same as clicking "← Back").
    {
        let cb = Rc::clone(&on_back_rc);
        let key_ctrl = gtk4::EventControllerKey::new();
        key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            if key == gtk4::gdk::Key::Escape {
                cb();
                gtk4::glib::Propagation::Stop
            } else {
                gtk4::glib::Propagation::Proceed
            }
        });
        root.add_controller(key_ctrl);
    }

    // -------------------------------------------------------------------------
    // Body: left nav list + vertical separator + right content area
    // -------------------------------------------------------------------------
    let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    body.set_vexpand(true);
    body.set_hexpand(true);

    // --- Section name → stack page name mapping ---
    // Appearance and Keyboard are excluded — they have dedicated views
    // (Ctrl+, sidebar and F1 ShortcutsWindow respectively).
    let nav_items: &[(&str, &str)] = &[
        ("General", "general"),
        ("Terminal", "terminal"),
        ("Devices", "devices"),
        ("About", "about"),
    ];

    // --- Right content stack (declare before nav so closures can capture it) ---
    let content_stack = gtk4::Stack::new();
    content_stack.set_transition_type(gtk4::StackTransitionType::None);
    content_stack.set_hexpand(true);
    content_stack.set_vexpand(true);

    // --- Left nav ---
    let nav_listbox = gtk4::ListBox::new();
    nav_listbox.set_selection_mode(gtk4::SelectionMode::Single);
    nav_listbox.set_width_request(200);
    nav_listbox.set_vexpand(true);
    nav_listbox.set_show_separators(false);
    nav_listbox.add_css_class("navigation-sidebar");

    for (label, _page) in nav_items {
        let row = gtk4::ListBoxRow::new();
        row.set_activatable(true);
        let lbl = gtk4::Label::new(Some(label));
        lbl.set_halign(gtk4::Align::Start);
        lbl.set_margin_top(10);
        lbl.set_margin_bottom(10);
        lbl.set_margin_start(16);
        lbl.set_margin_end(16);
        row.set_child(Some(&lbl));
        nav_listbox.append(&row);
    }

    // Select "General" by default.
    if let Some(first) = nav_listbox.row_at_index(0) {
        nav_listbox.select_row(Some(&first));
    }

    // Switch content stack when a nav row is selected.
    {
        let stk = content_stack.clone();
        let items: Vec<(&str, &str)> = nav_items.to_vec();
        nav_listbox.connect_row_selected(move |_, row| {
            if let Some(r) = row {
                if let Some((_, page)) = items.get(r.index() as usize) {
                    stk.set_visible_child_name(page);
                }
            }
        });
    }

    // -------------------------------------------------------------------------
    // Build each section page
    // -------------------------------------------------------------------------
    let current_config = shared_config.borrow().clone();

    // General
    let general_page = build_general_section(&current_config, shared_config);
    content_stack.add_named(&general_page, Some("general"));

    // Terminal
    let terminal_section = build_terminal_section(&current_config, shared_config);
    content_stack.add_named(&terminal_section, Some("terminal"));

    // Devices — reuse preferences::build_sync_section
    let devices_section = build_sync_section(daemon_client);
    content_stack.add_named(&devices_section, Some("devices"));

    // About
    let about_section = build_about_section();
    content_stack.add_named(&about_section, Some("about"));

    content_stack.set_visible_child_name("general");

    // -------------------------------------------------------------------------
    // Right side: scrollable content stack + "View JSON" toggle at the bottom.
    // A second stack lets "View JSON" overlay the full right area.
    // -------------------------------------------------------------------------
    let right_outer_stack = gtk4::Stack::new();
    right_outer_stack.set_transition_type(gtk4::StackTransitionType::None);
    right_outer_stack.set_hexpand(true);
    right_outer_stack.set_vexpand(true);

    // --- "sections" page ---
    let sections_page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    sections_page.set_vexpand(true);
    sections_page.set_hexpand(true);

    let scrolled = gtk4::ScrolledWindow::new();
    scrolled.set_child(Some(&content_stack));
    scrolled.set_vexpand(true);
    scrolled.set_hexpand(true);
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    sections_page.append(&scrolled);

    sections_page.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let view_json_btn = gtk4::Button::with_label("View JSON");
    view_json_btn.add_css_class("flat");
    view_json_btn.set_margin_top(4);
    view_json_btn.set_margin_bottom(4);
    view_json_btn.set_margin_start(8);
    view_json_btn.set_halign(gtk4::Align::Start);
    view_json_btn.set_tooltip_text(Some("View and edit the raw config.toml file"));
    sections_page.append(&view_json_btn);

    right_outer_stack.add_named(&sections_page, Some("sections"));

    // --- "json" page ---
    let json_text_buffer = gtk4::TextBuffer::new(None::<&gtk4::TextTagTable>);
    let json_page =
        build_json_editor_page(&right_outer_stack, shared_config, json_text_buffer.clone());
    right_outer_stack.add_named(&json_page, Some("json"));

    // Hook up "View JSON" button: load current config.toml text + switch page.
    {
        let ros = right_outer_stack.clone();
        let buf = json_text_buffer.clone();
        view_json_btn.connect_clicked(move |_| {
            buf.set_text(&load_config_as_text());
            ros.set_visible_child_name("json");
        });
    }

    right_outer_stack.set_visible_child_name("sections");

    // -------------------------------------------------------------------------
    // Assemble body: nav | separator | right
    // -------------------------------------------------------------------------
    body.append(&nav_listbox);
    body.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
    body.append(&right_outer_stack);

    root.append(&body);
    root
}

// ---------------------------------------------------------------------------
// Section builders
// ---------------------------------------------------------------------------

/// General section: Shell + On Launch.
fn build_general_section(current_config: &Config, shared_config: &SharedConfig) -> gtk4::Box {
    let page = make_settings_page();

    page.append(&make_section_header("General"));

    // --- Shell ---
    let shell_entry = gtk4::Entry::new();
    shell_entry.set_hexpand(true);
    shell_entry.set_max_width_chars(30);
    if let Some(ref shell) = current_config.shell {
        shell_entry.set_text(shell);
    }
    shell_entry.set_placeholder_text(Some("(uses $SHELL)"));
    {
        let shared = Rc::clone(shared_config);
        shell_entry.connect_changed(move |entry| {
            let text = entry.text().to_string();
            let shell_val = if text.is_empty() { None } else { Some(text) };
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else { return };
                if cfg.shell == shell_val {
                    return;
                }
                let mut updated = cfg.clone();
                updated.shell = shell_val;
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }
            save_in_background(new_config);
        });
    }
    page.append(&make_setting_row(
        "Shell",
        "Path to shell binary. Leave blank to use $SHELL.",
        &shell_entry,
    ));

    // --- On Launch ---
    let launch_options = gtk4::StringList::new(&["Restore previous session", "Start fresh"]);
    let launch_dd = gtk4::DropDown::new(Some(launch_options), gtk4::Expression::NONE);
    launch_dd.set_hexpand(false);
    launch_dd.set_width_request(200);
    let launch_idx = match current_config.on_launch {
        OnLaunch::Restore => 0,
        OnLaunch::New => 1,
    };
    launch_dd.set_selected(launch_idx);
    {
        let shared = Rc::clone(shared_config);
        launch_dd.connect_selected_notify(move |dd| {
            let on_launch = match dd.selected() {
                0 => OnLaunch::Restore,
                _ => OnLaunch::New,
            };
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else { return };
                if cfg.on_launch == on_launch {
                    return;
                }
                let mut updated = cfg.clone();
                updated.on_launch = on_launch;
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }
            save_in_background(new_config);
        });
    }
    page.append(&make_setting_row("On Launch", "What to do when Forgetty opens.", &launch_dd));

    page
}

/// Terminal section: Scrollback Lines, Cursor Style, Bell Mode, Notification Mode.
fn build_terminal_section(current_config: &Config, shared_config: &SharedConfig) -> gtk4::Box {
    let page = make_settings_page();

    page.append(&make_section_header("Terminal"));

    // --- Scrollback Lines ---
    let scroll_adj = gtk4::Adjustment::new(
        current_config.scrollback_lines as f64,
        0.0,
        1_000_000.0,
        100.0,
        1000.0,
        0.0,
    );
    let scrollback_spin = gtk4::SpinButton::new(Some(&scroll_adj), 100.0, 0);
    scrollback_spin.set_hexpand(false);
    scrollback_spin.set_width_request(120);
    {
        let shared = Rc::clone(shared_config);
        scrollback_spin.connect_value_changed(move |spin| {
            let lines = spin.value() as usize;
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else { return };
                if cfg.scrollback_lines == lines {
                    return;
                }
                let mut updated = cfg.clone();
                updated.scrollback_lines = lines;
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }
            save_in_background(new_config);
        });
    }
    page.append(&make_setting_row(
        "Scrollback Lines",
        "Number of lines to keep in scrollback buffer.",
        &scrollback_spin,
    ));

    // --- Cursor Style ---
    let cursor_opts = gtk4::StringList::new(&["Block", "Bar", "Underline", "Block Hollow"]);
    let cursor_dd = gtk4::DropDown::new(Some(cursor_opts), gtk4::Expression::NONE);
    cursor_dd.set_hexpand(false);
    cursor_dd.set_width_request(200);
    let cursor_idx = match current_config.cursor_style {
        CursorStyle::Block => 0,
        CursorStyle::Bar => 1,
        CursorStyle::Underline => 2,
        CursorStyle::BlockHollow => 3,
    };
    cursor_dd.set_selected(cursor_idx);
    {
        let shared = Rc::clone(shared_config);
        cursor_dd.connect_selected_notify(move |dd| {
            let style = match dd.selected() {
                0 => CursorStyle::Block,
                1 => CursorStyle::Bar,
                2 => CursorStyle::Underline,
                _ => CursorStyle::BlockHollow,
            };
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else { return };
                if cfg.cursor_style == style {
                    return;
                }
                let mut updated = cfg.clone();
                updated.cursor_style = style;
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }
            save_in_background(new_config);
        });
    }
    page.append(&make_setting_row("Cursor Style", "Shape of the terminal cursor.", &cursor_dd));

    // --- Bell Mode ---
    let bell_opts = gtk4::StringList::new(&["Visual", "Audio", "Both", "None"]);
    let bell_dd = gtk4::DropDown::new(Some(bell_opts), gtk4::Expression::NONE);
    bell_dd.set_hexpand(false);
    bell_dd.set_width_request(200);
    let bell_idx = match current_config.bell_mode {
        BellMode::Visual => 0,
        BellMode::Audio => 1,
        BellMode::Both => 2,
        BellMode::None => 3,
    };
    bell_dd.set_selected(bell_idx);
    {
        let shared = Rc::clone(shared_config);
        bell_dd.connect_selected_notify(move |dd| {
            let mode = match dd.selected() {
                0 => BellMode::Visual,
                1 => BellMode::Audio,
                2 => BellMode::Both,
                _ => BellMode::None,
            };
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else { return };
                if cfg.bell_mode == mode {
                    return;
                }
                let mut updated = cfg.clone();
                updated.bell_mode = mode;
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }
            save_in_background(new_config);
        });
    }
    page.append(&make_setting_row("Bell Mode", "How the terminal bell is triggered.", &bell_dd));

    // --- Notification Mode ---
    let notif_opts = gtk4::StringList::new(&["All", "Ring Only", "None"]);
    let notif_dd = gtk4::DropDown::new(Some(notif_opts), gtk4::Expression::NONE);
    notif_dd.set_hexpand(false);
    notif_dd.set_width_request(200);
    let notif_idx = match current_config.notification_mode {
        NotificationMode::All => 0,
        NotificationMode::RingOnly => 1,
        NotificationMode::None => 2,
    };
    notif_dd.set_selected(notif_idx);
    {
        let shared = Rc::clone(shared_config);
        notif_dd.connect_selected_notify(move |dd| {
            let mode = match dd.selected() {
                0 => NotificationMode::All,
                1 => NotificationMode::RingOnly,
                _ => NotificationMode::None,
            };
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else { return };
                if cfg.notification_mode == mode {
                    return;
                }
                let mut updated = cfg.clone();
                updated.notification_mode = mode;
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }
            save_in_background(new_config);
        });
    }
    page.append(&make_setting_row(
        "Notification Mode",
        "When to send OS notifications.",
        &notif_dd,
    ));

    page
}

/// About section: app name, version, license, links.
fn build_about_section() -> gtk4::Box {
    let page = make_settings_page();

    page.append(&make_section_header("About"));

    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    inner.set_margin_top(16);
    inner.set_margin_start(24);
    inner.set_margin_end(24);
    inner.set_valign(gtk4::Align::Start);

    let name_lbl = gtk4::Label::new(None);
    name_lbl.set_markup("<span size='x-large' weight='bold'>Forgetty</span>");
    name_lbl.set_halign(gtk4::Align::Start);
    inner.append(&name_lbl);

    let version_lbl = gtk4::Label::new(Some(&format!("Version {}", env!("CARGO_PKG_VERSION"))));
    version_lbl.add_css_class("dim-label");
    version_lbl.set_halign(gtk4::Align::Start);
    inner.append(&version_lbl);

    let license_lbl = gtk4::Label::new(Some("License: MIT"));
    license_lbl.add_css_class("dim-label");
    license_lbl.set_halign(gtk4::Align::Start);
    inner.append(&license_lbl);

    let links_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    links_box.set_margin_top(8);

    let github_btn =
        gtk4::LinkButton::with_label("https://github.com/totem-labs-forge/forgetty", "GitHub");
    links_box.append(&github_btn);

    let bug_btn = gtk4::LinkButton::with_label(
        "https://github.com/totem-labs-forge/forgetty/issues",
        "Report a Bug",
    );
    links_box.append(&bug_btn);

    inner.append(&links_box);
    page.append(&inner);

    page
}

// ---------------------------------------------------------------------------
// View JSON editor
// ---------------------------------------------------------------------------

/// Build the JSON editor page (the "json" page of the right outer stack).
fn build_json_editor_page(
    right_outer_stack: &gtk4::Stack,
    shared_config: &SharedConfig,
    json_text_buffer: gtk4::TextBuffer,
) -> gtk4::Box {
    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    page.set_vexpand(true);
    page.set_hexpand(true);

    // Editor action bar at top
    let action_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    action_bar.set_margin_top(8);
    action_bar.set_margin_bottom(8);
    action_bar.set_margin_start(12);
    action_bar.set_margin_end(12);

    let heading = gtk4::Label::new(None);
    heading.set_markup("<b>config.toml</b>");
    heading.set_halign(gtk4::Align::Start);
    heading.set_hexpand(true);
    action_bar.append(&heading);

    let cancel_btn = gtk4::Button::with_label("Cancel");
    cancel_btn.add_css_class("flat");
    let save_btn = gtk4::Button::with_label("Save");
    save_btn.add_css_class("suggested-action");
    action_bar.append(&cancel_btn);
    action_bar.append(&save_btn);

    page.append(&action_bar);
    page.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    // Error label (hidden until a parse error occurs)
    let error_lbl = gtk4::Label::new(None);
    error_lbl.add_css_class("error");
    error_lbl.set_halign(gtk4::Align::Start);
    error_lbl.set_margin_start(12);
    error_lbl.set_margin_top(4);
    error_lbl.set_margin_bottom(4);
    error_lbl.set_wrap(true);
    error_lbl.set_visible(false);
    page.append(&error_lbl);

    // Editable TextView
    let text_view = gtk4::TextView::new();
    text_view.set_buffer(Some(&json_text_buffer));
    text_view.set_monospace(true);
    text_view.set_editable(true);
    text_view.set_vexpand(true);
    text_view.set_hexpand(true);
    text_view.set_left_margin(12);
    text_view.set_right_margin(12);
    text_view.set_top_margin(8);
    text_view.set_bottom_margin(8);

    let scrolled = gtk4::ScrolledWindow::new();
    scrolled.set_child(Some(&text_view));
    scrolled.set_vexpand(true);
    scrolled.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Automatic);
    page.append(&scrolled);

    // Cancel: go back to sections, clear any error
    {
        let ros = right_outer_stack.clone();
        let err = error_lbl.clone();
        cancel_btn.connect_clicked(move |_| {
            err.set_visible(false);
            ros.set_visible_child_name("sections");
        });
    }

    // Save: parse TOML, apply to shared_config, persist, go back
    {
        let ros = right_outer_stack.clone();
        let shared = Rc::clone(shared_config);
        let buf = json_text_buffer.clone();
        let err = error_lbl.clone();
        save_btn.connect_clicked(move |_| {
            let (start, end) = buf.bounds();
            let text = buf.text(&start, &end, false);
            match parse_and_save_config(&text) {
                Ok(new_config) => {
                    if let Ok(mut cfg) = shared.try_borrow_mut() {
                        *cfg = new_config;
                    }
                    err.set_visible(false);
                    ros.set_visible_child_name("sections");
                }
                Err(e) => {
                    err.set_text(&format!("Parse error: {e}"));
                    err.set_visible(true);
                }
            }
        });
    }

    page
}

// ---------------------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------------------

/// Create a top-level section page box with standard padding.
fn make_settings_page() -> gtk4::Box {
    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    page.set_vexpand(true);
    page.set_hexpand(true);
    page
}

/// Create a section header label (bold, padded).
fn make_section_header(title: &str) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    row.set_margin_top(20);
    row.set_margin_start(16);
    row.set_margin_end(16);
    row.set_margin_bottom(4);

    let lbl = gtk4::Label::new(None);
    lbl.set_markup(&format!("<b>{title}</b>"));
    lbl.set_halign(gtk4::Align::Start);
    lbl.add_css_class("title-4");
    row.append(&lbl);

    let sep = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    sep.set_margin_top(4);
    row.append(&sep);

    row
}

/// Create a setting row: left side has label + description, right side has the control.
fn make_setting_row(label: &str, description: &str, control: &impl IsA<gtk4::Widget>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 16);
    row.set_margin_top(10);
    row.set_margin_bottom(10);
    row.set_margin_start(16);
    row.set_margin_end(16);
    row.set_valign(gtk4::Align::Center);

    let text_col = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    text_col.set_hexpand(true);
    text_col.set_valign(gtk4::Align::Center);

    let lbl = gtk4::Label::new(Some(label));
    lbl.set_halign(gtk4::Align::Start);
    text_col.append(&lbl);

    let desc = gtk4::Label::new(Some(description));
    desc.add_css_class("dim-label");
    desc.add_css_class("caption");
    desc.set_halign(gtk4::Align::Start);
    desc.set_wrap(true);
    desc.set_max_width_chars(40);
    text_col.append(&desc);

    row.append(&text_col);
    row.append(control.upcast_ref::<gtk4::Widget>());
    row
}
