//! Full-window Settings view for Forgetty.
//!
//! Opens as a full-window takeover via the "Settings" hamburger menu item.
//! Left nav + right pane layout with sections: General, Terminal, Devices,
//! Keybindings, About.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use forgetty_config::{
    load_config_as_text, parse_and_save_config, save_config, BellMode, Config, CursorStyle,
    NotificationMode, OnLaunch,
};
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::MessageDialogExt;

use crate::daemon_client::DaemonClient;
use crate::preferences::{build_sync_section, save_in_background};

type SharedConfig = Rc<RefCell<Config>>;

// ---------------------------------------------------------------------------
// Keybinding action inventory (single source of truth for T-M1-extra-007)
// ---------------------------------------------------------------------------

/// Metadata for a single bindable action.
pub struct ActionDef {
    /// Human-readable label shown in the Keybindings editor.
    pub display_name: &'static str,
    /// Key used in `[keybindings]` section of config.toml.
    pub config_key: &'static str,
    /// Full GIO action name (e.g. `"win.new-tab"` or `"app.quit"`).
    pub action_name: &'static str,
    /// Default GTK accelerator strings.  Empty slice means "no default".
    pub default_accels: &'static [&'static str],
    /// Display category shown as a group header.
    pub category: &'static str,
}

/// Complete ordered inventory of every bindable action.
///
/// This array drives the Keybindings editor rows AND the conflict-detection
/// logic and the `apply_keybinding_overrides` helper in `app.rs`.
/// Add new actions here — they automatically appear in the editor.
pub static ACTION_DEFS: &[ActionDef] = &[
    // --- Clipboard ---
    ActionDef {
        display_name: "Copy",
        config_key: "copy",
        action_name: "win.copy",
        default_accels: &["<Control><Shift>c"],
        category: "Clipboard",
    },
    ActionDef {
        display_name: "Paste",
        config_key: "paste",
        action_name: "win.paste",
        default_accels: &["<Control>v", "<Control><Shift>v"],
        category: "Clipboard",
    },
    ActionDef {
        display_name: "Select All",
        config_key: "select-all",
        action_name: "win.select-all",
        default_accels: &[],
        category: "Clipboard",
    },
    // --- Tabs ---
    ActionDef {
        display_name: "New Tab",
        config_key: "new-tab",
        action_name: "win.new-tab",
        default_accels: &["<Control><Shift>t"],
        category: "Tabs",
    },
    ActionDef {
        display_name: "Close Tab",
        config_key: "close-tab",
        action_name: "win.close-tab",
        default_accels: &[],
        category: "Tabs",
    },
    ActionDef {
        display_name: "Close Pane",
        config_key: "close-pane",
        action_name: "win.close-pane",
        default_accels: &["<Control><Shift>w"],
        category: "Tabs",
    },
    ActionDef {
        display_name: "Change Tab Title",
        config_key: "change-tab-title",
        action_name: "win.change-tab-title",
        default_accels: &[],
        category: "Tabs",
    },
    // --- Panes ---
    ActionDef {
        display_name: "Split Right",
        config_key: "split-right",
        action_name: "win.split-right",
        default_accels: &["<Alt><Shift>equal"],
        category: "Panes",
    },
    ActionDef {
        display_name: "Split Down",
        config_key: "split-down",
        action_name: "win.split-down",
        default_accels: &["<Alt><Shift>minus"],
        category: "Panes",
    },
    ActionDef {
        display_name: "Split Left",
        config_key: "split-left",
        action_name: "win.split-left",
        default_accels: &[],
        category: "Panes",
    },
    ActionDef {
        display_name: "Split Up",
        config_key: "split-up",
        action_name: "win.split-up",
        default_accels: &[],
        category: "Panes",
    },
    // --- Focus Navigation ---
    ActionDef {
        display_name: "Focus Pane Left",
        config_key: "focus-pane-left",
        action_name: "win.focus-pane-left",
        default_accels: &["<Alt>Left"],
        category: "Focus Navigation",
    },
    ActionDef {
        display_name: "Focus Pane Right",
        config_key: "focus-pane-right",
        action_name: "win.focus-pane-right",
        default_accels: &["<Alt>Right"],
        category: "Focus Navigation",
    },
    ActionDef {
        display_name: "Focus Pane Up",
        config_key: "focus-pane-up",
        action_name: "win.focus-pane-up",
        default_accels: &["<Alt>Up"],
        category: "Focus Navigation",
    },
    ActionDef {
        display_name: "Focus Pane Down",
        config_key: "focus-pane-down",
        action_name: "win.focus-pane-down",
        default_accels: &["<Alt>Down"],
        category: "Focus Navigation",
    },
    // --- Zoom ---
    ActionDef {
        display_name: "Zoom In",
        config_key: "zoom-in",
        action_name: "win.zoom-in",
        default_accels: &["<Control>equal"],
        category: "Zoom",
    },
    ActionDef {
        display_name: "Zoom Out",
        config_key: "zoom-out",
        action_name: "win.zoom-out",
        default_accels: &["<Control>minus"],
        category: "Zoom",
    },
    ActionDef {
        display_name: "Reset Zoom",
        config_key: "zoom-reset",
        action_name: "win.zoom-reset",
        default_accels: &["<Control>0"],
        category: "Zoom",
    },
    // --- Search ---
    ActionDef {
        display_name: "Find in Terminal",
        config_key: "search",
        action_name: "win.search",
        default_accels: &["<Control><Shift>f"],
        category: "Search",
    },
    // --- Terminal ---
    ActionDef {
        display_name: "Clear",
        config_key: "clear",
        action_name: "win.clear",
        default_accels: &[],
        category: "Terminal",
    },
    ActionDef {
        display_name: "Reset",
        config_key: "reset",
        action_name: "win.reset",
        default_accels: &[],
        category: "Terminal",
    },
    // --- Configuration ---
    ActionDef {
        display_name: "Appearance Sidebar",
        config_key: "appearance",
        action_name: "win.appearance",
        default_accels: &["<Control>comma"],
        category: "Configuration",
    },
    ActionDef {
        display_name: "Settings",
        config_key: "open-settings",
        action_name: "win.open-settings",
        default_accels: &["<Control>period"],
        category: "Configuration",
    },
    ActionDef {
        display_name: "Open Configuration File",
        config_key: "open-config",
        action_name: "win.open-config",
        default_accels: &[],
        category: "Configuration",
    },
    ActionDef {
        display_name: "Reload Configuration",
        config_key: "reload-config",
        action_name: "win.reload-config",
        default_accels: &[],
        category: "Configuration",
    },
    ActionDef {
        display_name: "Command Palette",
        config_key: "command-palette",
        action_name: "win.command-palette",
        default_accels: &["<Control><Shift>p"],
        category: "Configuration",
    },
    ActionDef {
        display_name: "Keyboard Shortcuts (reference)",
        config_key: "show-shortcuts",
        action_name: "win.show-shortcuts",
        default_accels: &["F1"],
        category: "Configuration",
    },
    // --- Workspaces ---
    ActionDef {
        display_name: "New Workspace",
        config_key: "new-workspace",
        action_name: "win.new-workspace",
        default_accels: &["<Control><Alt>n"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Rename Workspace",
        config_key: "rename-workspace",
        action_name: "win.rename-workspace",
        default_accels: &[],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Delete Workspace",
        config_key: "delete-workspace",
        action_name: "win.delete-workspace",
        default_accels: &[],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Toggle Workspace Sidebar",
        config_key: "toggle-workspace-sidebar",
        action_name: "win.toggle-workspace-sidebar",
        default_accels: &["<Control><Alt>b"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Previous Workspace",
        config_key: "prev-workspace",
        action_name: "win.prev-workspace",
        default_accels: &["<Control><Alt>Page_Up"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Next Workspace",
        config_key: "next-workspace",
        action_name: "win.next-workspace",
        default_accels: &["<Control><Alt>Page_Down"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Switch to Workspace 1",
        config_key: "switch-workspace-1",
        action_name: "win.switch-workspace-1",
        default_accels: &["<Alt>1"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Switch to Workspace 2",
        config_key: "switch-workspace-2",
        action_name: "win.switch-workspace-2",
        default_accels: &["<Alt>2"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Switch to Workspace 3",
        config_key: "switch-workspace-3",
        action_name: "win.switch-workspace-3",
        default_accels: &["<Alt>3"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Switch to Workspace 4",
        config_key: "switch-workspace-4",
        action_name: "win.switch-workspace-4",
        default_accels: &["<Alt>4"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Switch to Workspace 5",
        config_key: "switch-workspace-5",
        action_name: "win.switch-workspace-5",
        default_accels: &["<Alt>5"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Switch to Workspace 6",
        config_key: "switch-workspace-6",
        action_name: "win.switch-workspace-6",
        default_accels: &["<Alt>6"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Switch to Workspace 7",
        config_key: "switch-workspace-7",
        action_name: "win.switch-workspace-7",
        default_accels: &["<Alt>7"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Switch to Workspace 8",
        config_key: "switch-workspace-8",
        action_name: "win.switch-workspace-8",
        default_accels: &["<Alt>8"],
        category: "Workspaces",
    },
    ActionDef {
        display_name: "Switch to Workspace 9",
        config_key: "switch-workspace-9",
        action_name: "win.switch-workspace-9",
        default_accels: &["<Alt>9"],
        category: "Workspaces",
    },
    // --- Application ---
    ActionDef {
        display_name: "New Window",
        config_key: "new-window",
        action_name: "win.new-window",
        default_accels: &[],
        category: "Application",
    },
    ActionDef {
        display_name: "New Temporary Window",
        config_key: "new-temp-window",
        action_name: "win.new-temp-window",
        default_accels: &["<Control><Shift>n"],
        category: "Application",
    },
    ActionDef {
        display_name: "Close Window",
        config_key: "close-window",
        action_name: "win.close-window",
        default_accels: &[],
        category: "Application",
    },
    ActionDef {
        display_name: "Close Window Permanently",
        config_key: "close-window-permanently",
        action_name: "win.close-window-permanently",
        default_accels: &[],
        category: "Application",
    },
    ActionDef {
        display_name: "Quit",
        config_key: "quit",
        action_name: "app.quit",
        default_accels: &["<Control><Shift>q"],
        category: "Application",
    },
    ActionDef {
        display_name: "About Forgetty",
        config_key: "show-about",
        action_name: "win.show-about",
        default_accels: &[],
        category: "Application",
    },
];

// ---------------------------------------------------------------------------
// Keybinding helpers
// ---------------------------------------------------------------------------

/// Convert a GTK accelerator string to a human-readable display string.
///
/// Examples:
/// - `"<Control><Shift>t"` → `"Ctrl+Shift+T"`
/// - `"<Alt>Left"` → `"Alt+Left"`
/// - `"F1"` → `"F1"`
/// - `""` → `"— None —"`
pub fn accel_to_display(accel: &str) -> String {
    if accel.is_empty() {
        return "— None —".to_string();
    }

    let mut parts: Vec<&str> = Vec::new();
    let mut rest = accel;

    // Parse modifier tokens in order.
    loop {
        if rest.starts_with("<Control>") {
            parts.push("Ctrl");
            rest = &rest["<Control>".len()..];
        } else if rest.starts_with("<Shift>") {
            parts.push("Shift");
            rest = &rest["<Shift>".len()..];
        } else if rest.starts_with("<Alt>") {
            parts.push("Alt");
            rest = &rest["<Alt>".len()..];
        } else if rest.starts_with("<Super>") {
            parts.push("Super");
            rest = &rest["<Super>".len()..];
        } else if rest.starts_with("<Meta>") {
            parts.push("Meta");
            rest = &rest["<Meta>".len()..];
        } else if rest.starts_with("<Hyper>") {
            parts.push("Hyper");
            rest = &rest["<Hyper>".len()..];
        } else {
            break;
        }
    }

    // Remaining part is the key name.
    let key_display = match rest {
        "equal" => "=".to_string(),
        "minus" => "-".to_string(),
        "plus" => "+".to_string(),
        "comma" => ",".to_string(),
        "period" => ".".to_string(),
        "slash" => "/".to_string(),
        "backslash" => "\\".to_string(),
        "space" => "Space".to_string(),
        "Tab" => "Tab".to_string(),
        "Return" => "Enter".to_string(),
        "Escape" | "escape" => "Esc".to_string(),
        "Delete" => "Del".to_string(),
        "BackSpace" => "Backspace".to_string(),
        "Page_Up" => "Page Up".to_string(),
        "Page_Down" => "Page Down".to_string(),
        "Home" => "Home".to_string(),
        "End" => "End".to_string(),
        "Insert" => "Ins".to_string(),
        "Left" => "Left".to_string(),
        "Right" => "Right".to_string(),
        "Up" => "Up".to_string(),
        "Down" => "Down".to_string(),
        other => {
            // Single lowercase letter → uppercase.
            if other.len() == 1 {
                other.to_uppercase()
            } else {
                other.to_string()
            }
        }
    };

    parts.push(&key_display);
    // We pushed &key_display after the loop; collect only the modifier parts first.
    // Reconstruct properly:
    let mut result_parts: Vec<String> =
        parts[..parts.len() - 1].iter().map(|s| s.to_string()).collect();
    result_parts.push(key_display);
    result_parts.join("+")
}

/// Find a conflicting action for a given accelerator string, skipping the
/// action identified by `skip_key` (the one being edited).
///
/// Returns `Some(display_name)` if another action shares the same effective
/// accelerator (custom override if set, otherwise the first default).
pub fn find_conflict(new_accel: &str, skip_key: &str, config: &Config) -> Option<String> {
    if new_accel.is_empty() {
        return None;
    }
    for def in ACTION_DEFS {
        if def.config_key == skip_key {
            continue;
        }
        // Effective accel: custom if set, otherwise first default.
        let effective = if let Some(custom) = config.keybindings.get(def.config_key) {
            custom.as_str()
        } else if !def.default_accels.is_empty() {
            def.default_accels[0]
        } else {
            continue;
        };
        if effective == new_accel {
            return Some(def.display_name.to_string());
        }
        // Also check multi-accelerator defaults (e.g. paste has two).
        for &default in def.default_accels {
            if default == new_accel {
                return Some(def.display_name.to_string());
            }
        }
    }
    None
}

/// Build the full-window settings view widget.
///
/// Returns a `gtk4::Box` suitable for adding to an outer `gtk4::Stack` as the
/// "settings" page. `on_back` is called when the user clicks "← Back".
/// `app` is the adw::Application — required by the Keybindings page to call
/// `set_accels_for_action` when the user rebinds an action.
pub fn build_settings_view(
    shared_config: &SharedConfig,
    daemon_client: Option<Arc<DaemonClient>>,
    app: adw::Application,
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
    //
    // Bug fix (Fix cycle 1): Use PropagationPhase::Bubble instead of Capture.
    // In Capture phase (outermost→innermost) this handler was firing BEFORE the
    // per-row key-capture controller attached to shortcut_lbl (a descendant).
    // The per-row controller uses Capture phase and returns Propagation::Stop,
    // which stops the event in the Capture pass — so the Bubble phase handler
    // here never fires when a row is in capture mode.  When no row is capturing,
    // the label is not the event target so the per-row controller is not in the
    // dispatch path and this handler fires normally.
    {
        let cb = Rc::clone(&on_back_rc);
        let key_ctrl = gtk4::EventControllerKey::new();
        key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Bubble);
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
    // Appearance is excluded — it has its own dedicated Ctrl+, sidebar.
    // Keyboard Shortcuts (F1 reference) is also excluded from this nav.
    // Keybindings editor is included here as a full settings page (AC-1).
    let nav_items: &[(&str, &str)] = &[
        ("General", "general"),
        ("Terminal", "terminal"),
        ("Keybindings", "keybindings"),
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

    // Keybindings (T-M1-extra-007)
    let keybindings_section = build_keybindings_section(shared_config, &app);
    content_stack.add_named(&keybindings_section, Some("keybindings"));

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

    // --- Interaction ---
    page.append(&make_section_header("Interaction"));

    // --- Warn on large paste ---
    const PASTE_SIZE_DEFAULT: usize = 5120;
    let paste_size_adj = gtk4::Adjustment::new(
        current_config.paste_warn_size as f64,
        0.0,
        65536.0,
        512.0,
        512.0,
        0.0,
    );
    let paste_size_spin = gtk4::SpinButton::new(Some(&paste_size_adj), 512.0, 0);
    paste_size_spin.set_hexpand(false);
    paste_size_spin.set_width_request(120);

    let paste_size_reset_btn = gtk4::Button::with_label("Reset");
    paste_size_reset_btn.add_css_class("flat");
    paste_size_reset_btn.add_css_class("caption");
    paste_size_reset_btn.set_tooltip_text(Some("Reset to default (5120 bytes)"));
    paste_size_reset_btn.set_sensitive(current_config.paste_warn_size != PASTE_SIZE_DEFAULT);

    {
        let shared = Rc::clone(shared_config);
        let reset_btn = paste_size_reset_btn.clone();
        paste_size_spin.connect_value_changed(move |spin| {
            let size = spin.value() as usize;
            reset_btn.set_sensitive(size != PASTE_SIZE_DEFAULT);
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else { return };
                if cfg.paste_warn_size == size {
                    return;
                }
                let mut updated = cfg.clone();
                updated.paste_warn_size = size;
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }
            save_in_background(new_config);
        });
    }
    {
        let spin = paste_size_spin.clone();
        paste_size_reset_btn.connect_clicked(move |_| {
            spin.set_value(PASTE_SIZE_DEFAULT as f64);
        });
    }

    let paste_size_ctrl = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    paste_size_ctrl.set_valign(gtk4::Align::Center);
    paste_size_ctrl.append(&paste_size_reset_btn);
    paste_size_ctrl.append(&paste_size_spin);

    page.append(&make_setting_row(
        "Large Paste Warning",
        "Show a confirmation dialog when pasting more than this many bytes (0 to disable).",
        &paste_size_ctrl,
    ));

    // --- Warn on newline paste ---
    let paste_newline_check = gtk4::CheckButton::new();
    paste_newline_check.set_hexpand(false);
    paste_newline_check.set_active(current_config.paste_warn_newline);
    {
        let shared = Rc::clone(shared_config);
        paste_newline_check.connect_toggled(move |check| {
            let enabled = check.is_active();
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else { return };
                if cfg.paste_warn_newline == enabled {
                    return;
                }
                let mut updated = cfg.clone();
                updated.paste_warn_newline = enabled;
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }
            save_in_background(new_config);
        });
    }
    page.append(&make_setting_row(
        "Newline Paste Warning",
        "Show a confirmation dialog when pasting text that contains newlines.",
        &paste_newline_check,
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
// Keybindings editor section
// ---------------------------------------------------------------------------

/// Build the Keybindings settings page.
///
/// Displays all `ACTION_DEFS` rows grouped by category with:
/// - A `gtk4::SearchEntry` for real-time filtering (AC-7/AC-8/AC-9).
/// - A "Reset all defaults" button (AC-17).
/// - Per-action rows: display name, current shortcut (or "— None —"), Edit and
///   Reset buttons (AC-4/AC-5/AC-6).
/// - Key-capture mode on Edit (AC-10/AC-11/AC-12/AC-13).
/// - Inline conflict warning after capture (AC-14/AC-15).
fn build_keybindings_section(shared_config: &SharedConfig, app: &adw::Application) -> gtk4::Box {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.set_vexpand(true);
    outer.set_hexpand(true);

    outer.append(&make_section_header("Keybindings"));

    // ---- Toolbar: search + "Reset all defaults" ----
    let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    toolbar.set_margin_top(8);
    toolbar.set_margin_bottom(4);
    toolbar.set_margin_start(16);
    toolbar.set_margin_end(16);

    let search_entry = gtk4::SearchEntry::new();
    search_entry.set_hexpand(true);
    search_entry.set_placeholder_text(Some("Filter actions…"));
    toolbar.append(&search_entry);

    let reset_all_btn = gtk4::Button::with_label("Reset all defaults");
    reset_all_btn.add_css_class("destructive-action");
    toolbar.append(&reset_all_btn);

    outer.append(&toolbar);

    // ---- Scrollable list of action rows ----
    let scrolled = gtk4::ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_hexpand(true);
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);

    let list_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    list_box.set_vexpand(true);
    list_box.set_margin_start(8);
    list_box.set_margin_end(8);
    list_box.set_margin_bottom(16);

    // "No matching actions" label — shown when search yields no results.
    let no_match_lbl = gtk4::Label::new(Some("No matching actions"));
    no_match_lbl.add_css_class("dim-label");
    no_match_lbl.set_margin_top(24);
    no_match_lbl.set_halign(gtk4::Align::Center);
    no_match_lbl.set_visible(false);
    list_box.append(&no_match_lbl);

    // Track the "currently capturing" action by config_key.
    // Only one row may be in capture mode at a time (AC-13).
    let capturing: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Collect all row widgets so search / hot-reload can iterate them.
    // Each element: (config_key, category_header_box, row_outer_box).
    // We store them in a Vec for visibility toggling.
    struct RowWidgets {
        config_key: &'static str,
        category: &'static str,
        /// The outer container for this action row (used for show/hide in search).
        row_box: gtk4::Box,
        /// The shortcut label (updated on save / reset / hot-reload).
        shortcut_lbl: gtk4::Label,
        /// Conflict warning label (shown/hidden after capture).
        conflict_lbl: gtk4::Label,
        /// Reset button (sensitive = current != default).
        reset_btn: gtk4::Button,
    }

    let mut row_widgets: Vec<RowWidgets> = Vec::new();

    // Category headers: map category name → header Box widget.
    let mut category_headers: Vec<(&'static str, gtk4::Box)> = Vec::new();

    // Build one row per ActionDef.
    let mut current_category: Option<&'static str> = None;

    for def in ACTION_DEFS {
        // Insert a category header when the category changes.
        if current_category != Some(def.category) {
            current_category = Some(def.category);

            let hdr = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            hdr.set_margin_top(16);
            hdr.set_margin_start(8);
            hdr.set_margin_end(8);
            hdr.set_margin_bottom(2);

            let hdr_lbl = gtk4::Label::new(None);
            hdr_lbl.set_markup(&format!("<b>{}</b>", def.category));
            hdr_lbl.set_halign(gtk4::Align::Start);
            hdr_lbl.add_css_class("caption");
            hdr_lbl.add_css_class("dim-label");
            hdr.append(&hdr_lbl);

            let sep = gtk4::Separator::new(gtk4::Orientation::Horizontal);
            sep.set_margin_top(2);
            hdr.append(&sep);

            list_box.append(&hdr);
            category_headers.push((def.category, hdr));
        }

        // Determine the current effective accel string to display.
        let current_cfg = shared_config.borrow().clone();
        let effective_accel: String =
            if let Some(custom) = current_cfg.keybindings.get(def.config_key) {
                custom.clone()
            } else if !def.default_accels.is_empty() {
                def.default_accels[0].to_string()
            } else {
                String::new()
            };
        let is_custom = current_cfg.keybindings.contains_key(def.config_key);
        let is_default = !is_custom;

        // ---- Row outer box (also wraps conflict label) ----
        let row_outer = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        row_outer.set_margin_top(2);
        row_outer.set_margin_bottom(2);

        // ---- Main row: name | shortcut | reset | edit ----
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        row.set_margin_start(8);
        row.set_margin_end(8);
        row.set_margin_top(4);
        row.set_margin_bottom(4);

        // Action display name.
        let name_lbl = gtk4::Label::new(Some(def.display_name));
        name_lbl.set_halign(gtk4::Align::Start);
        name_lbl.set_hexpand(true);
        row.append(&name_lbl);

        // Current shortcut label.
        let shortcut_lbl = gtk4::Label::new(Some(&accel_to_display(&effective_accel)));
        shortcut_lbl.set_halign(gtk4::Align::End);
        shortcut_lbl.set_width_request(160);
        shortcut_lbl.set_xalign(1.0);
        if is_custom {
            shortcut_lbl.add_css_class("accent");
        }
        row.append(&shortcut_lbl);

        // Reset button.
        let reset_btn = gtk4::Button::with_label("Reset");
        reset_btn.add_css_class("flat");
        reset_btn.add_css_class("caption");
        reset_btn.set_sensitive(!is_default);
        reset_btn.set_tooltip_text(Some("Reset to default"));
        row.append(&reset_btn);

        // Edit button.
        let edit_btn = gtk4::Button::with_label("Edit");
        edit_btn.add_css_class("flat");
        row.append(&edit_btn);

        row_outer.append(&row);

        // Conflict warning label (hidden by default).
        let conflict_lbl = gtk4::Label::new(None);
        conflict_lbl.set_halign(gtk4::Align::Start);
        conflict_lbl.set_margin_start(8);
        conflict_lbl.set_margin_bottom(2);
        conflict_lbl.add_css_class("warning");
        conflict_lbl.set_visible(false);
        row_outer.append(&conflict_lbl);

        list_box.append(&row_outer);

        // ---- Wire up Reset button ----
        {
            let shared = Rc::clone(shared_config);
            let app_reset = app.clone();
            let shortcut_lbl_r = shortcut_lbl.clone();
            let reset_btn_r = reset_btn.clone();
            let conflict_lbl_r = conflict_lbl.clone();
            let config_key = def.config_key;
            let default_accels = def.default_accels;
            let action_name = def.action_name;

            reset_btn.connect_clicked(move |_| {
                // Remove the custom binding from config.
                let new_config = {
                    let Ok(mut cfg) = shared.try_borrow_mut() else { return };
                    cfg.keybindings.remove(config_key);
                    cfg.clone()
                };
                // Persist to disk.
                if let Err(e) = save_config(&new_config) {
                    tracing::warn!("Failed to save config after keybinding reset: {e}");
                }
                // Restore default accelerator(s) on the app.
                let accel_strs: Vec<&str> = default_accels.iter().copied().collect();
                app_reset.set_accels_for_action(action_name, &accel_strs);
                // Update the shortcut label back to default display.
                let display = if !default_accels.is_empty() {
                    accel_to_display(default_accels[0])
                } else {
                    "— None —".to_string()
                };
                shortcut_lbl_r.set_text(&display);
                shortcut_lbl_r.remove_css_class("accent");
                // Disable reset button (now at default).
                reset_btn_r.set_sensitive(false);
                // Clear any conflict warning.
                conflict_lbl_r.set_visible(false);
            });
        }

        // ---- Wire up Edit button (dialog-based key capture) ----
        //
        // Fix cycle 2: Replaced the broken per-label EventControllerKey approach
        // with a modal adw::MessageDialog.  Dialogs are top-level windows that
        // naturally own keyboard focus — no set_focusable/grab_focus hackery needed.
        {
            let shared = Rc::clone(shared_config);
            let app_edit = app.clone();
            let shortcut_lbl_e = shortcut_lbl.clone();
            let reset_btn_e = reset_btn.clone();
            let conflict_lbl_e = conflict_lbl.clone();
            let capturing_e = Rc::clone(&capturing);
            let config_key = def.config_key;
            let action_name = def.action_name;
            let display_name: &'static str = def.display_name;

            edit_btn.connect_clicked(move |btn| {
                // Cancel any currently-capturing row first (AC-13).
                {
                    let mut cap = capturing_e.borrow_mut();
                    *cap = Some(config_key.to_string());
                }

                // Present a capture dialog.  Dialogs are top-level windows that
                // grab all keyboard input — this is the reliable way to do
                // shortcut capture in GTK4 (same approach as gnome-control-center).
                let parent_win = btn.root().and_then(|r| r.downcast::<gtk4::Window>().ok());

                #[allow(deprecated)]
                let dialog = adw::MessageDialog::new(
                    parent_win.as_ref(),
                    Some("Bind New Shortcut"),
                    Some(&format!(
                        "Press the key combination you want to assign to \"{}\".\n\nPress Escape to cancel.",
                        display_name
                    )),
                );
                // Cancel response — key press handles the confirm path.
                dialog.add_response("cancel", "Cancel");
                dialog.set_close_response("cancel");

                // Attach a Capture-phase key controller to the dialog window.
                // Dialogs receive all key events while open — no focus tricks needed.
                let key_ctrl = gtk4::EventControllerKey::new();
                key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);

                let shared_kc = Rc::clone(&shared);
                let app_kc = app_edit.clone();
                let shortcut_lbl_kc = shortcut_lbl_e.clone();
                let reset_btn_kc = reset_btn_e.clone();
                let conflict_lbl_kc = conflict_lbl_e.clone();
                let capturing_kc = Rc::clone(&capturing_e);
                let dialog_kc = dialog.clone();

                key_ctrl.connect_key_pressed(move |_ctrl, keyval, _code, state| {
                    use gtk4::gdk::ModifierType;

                    // Ignore bare modifier-only key presses (AC-10).
                    let is_modifier_only = matches!(
                        keyval,
                        gtk4::gdk::Key::Shift_L
                            | gtk4::gdk::Key::Shift_R
                            | gtk4::gdk::Key::Control_L
                            | gtk4::gdk::Key::Control_R
                            | gtk4::gdk::Key::Alt_L
                            | gtk4::gdk::Key::Alt_R
                            | gtk4::gdk::Key::Super_L
                            | gtk4::gdk::Key::Super_R
                            | gtk4::gdk::Key::Meta_L
                            | gtk4::gdk::Key::Meta_R
                            | gtk4::gdk::Key::ISO_Level3_Shift
                            | gtk4::gdk::Key::Caps_Lock
                            | gtk4::gdk::Key::Num_Lock
                            | gtk4::gdk::Key::Hyper_L
                            | gtk4::gdk::Key::Hyper_R
                    );

                    // Escape cancels capture mode (AC-11).
                    if keyval == gtk4::gdk::Key::Escape {
                        *capturing_kc.borrow_mut() = None;
                        dialog_kc.close();
                        return gtk4::glib::Propagation::Stop;
                    }

                    if is_modifier_only {
                        return gtk4::glib::Propagation::Proceed;
                    }

                    // Build GTK accel string from the pressed key + modifiers.
                    let mut accel = String::new();
                    if state.contains(ModifierType::CONTROL_MASK) {
                        accel.push_str("<Control>");
                    }
                    if state.contains(ModifierType::SHIFT_MASK) {
                        accel.push_str("<Shift>");
                    }
                    if state.contains(ModifierType::ALT_MASK) {
                        accel.push_str("<Alt>");
                    }
                    if state.contains(ModifierType::SUPER_MASK) {
                        accel.push_str("<Super>");
                    }

                    // Append the key name.
                    let key_name = keyval.name().map(|s| s.to_string()).unwrap_or_default();
                    if key_name.is_empty() {
                        // Cannot represent this key — ignore.
                        return gtk4::glib::Propagation::Stop;
                    }
                    accel.push_str(&key_name);

                    // Save the new binding.
                    let new_config = {
                        let Ok(mut cfg) = shared_kc.try_borrow_mut() else {
                            return gtk4::glib::Propagation::Stop;
                        };
                        cfg.keybindings.insert(config_key.to_string(), accel.clone());
                        cfg.clone()
                    };
                    if let Err(e) = save_config(&new_config) {
                        tracing::warn!("Failed to save keybinding: {e}");
                    }

                    // Register the new accelerator immediately (AC-12).
                    app_kc.set_accels_for_action(action_name, &[accel.as_str()]);

                    // Update row UI.
                    shortcut_lbl_kc.set_text(&accel_to_display(&accel));
                    shortcut_lbl_kc.remove_css_class("dim-label");
                    shortcut_lbl_kc.add_css_class("accent");
                    reset_btn_kc.set_sensitive(true);

                    // Conflict detection (AC-14).
                    if let Ok(cfg) = shared_kc.try_borrow() {
                        if let Some(conflict_name) = find_conflict(&accel, config_key, &cfg) {
                            conflict_lbl_kc
                                .set_text(&format!("⚠ Also assigned to: {}", conflict_name));
                            conflict_lbl_kc.set_visible(true);
                        } else {
                            conflict_lbl_kc.set_visible(false);
                        }
                    }

                    // Clear capture tracking and close dialog.
                    *capturing_kc.borrow_mut() = None;
                    dialog_kc.close();
                    gtk4::glib::Propagation::Stop
                });

                dialog.add_controller(key_ctrl);
                dialog.present();
            });
        }

        row_widgets.push(RowWidgets {
            config_key: def.config_key,
            category: def.category,
            row_box: row_outer,
            shortcut_lbl,
            conflict_lbl,
            reset_btn,
        });
    }

    // ---- Wire up search / filter (AC-7/AC-8/AC-9) ----
    {
        let row_widgets_search: Vec<_> =
            row_widgets.iter().map(|r| (r.config_key, r.category, r.row_box.clone())).collect();
        let category_headers_search = category_headers.clone();
        let no_match = no_match_lbl.clone();

        search_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_string().to_lowercase();

            // Determine visibility for each row.
            let mut any_visible = false;
            let mut category_visible: std::collections::HashMap<&str, bool> =
                std::collections::HashMap::new();

            for (config_key, category, row_box) in &row_widgets_search {
                // Find the display name from ACTION_DEFS.
                let display_name = ACTION_DEFS
                    .iter()
                    .find(|d| d.config_key == *config_key)
                    .map(|d| d.display_name.to_lowercase())
                    .unwrap_or_default();

                let visible = query.is_empty() || display_name.contains(&query);
                row_box.set_visible(visible);
                if visible {
                    any_visible = true;
                    *category_visible.entry(category).or_insert(false) = true;
                }
            }

            // Show/hide category headers.
            for (cat, hdr) in &category_headers_search {
                let vis = category_visible.get(*cat).copied().unwrap_or(false) || query.is_empty();
                hdr.set_visible(vis);
            }

            no_match.set_visible(!any_visible && !query.is_empty());
        });
    }

    // ---- "Reset all defaults" button (AC-17) ----
    {
        let shared = Rc::clone(shared_config);
        let app_ral = app.clone();
        let row_widgets_ral: Vec<_> = row_widgets
            .iter()
            .map(|r| {
                (r.shortcut_lbl.clone(), r.reset_btn.clone(), r.conflict_lbl.clone(), r.config_key)
            })
            .collect();

        reset_all_btn.connect_clicked(move |btn| {
            // Find an ancestor window for the dialog.
            let parent_win = btn.root().and_then(|r| r.downcast::<gtk4::Window>().ok());

            #[allow(deprecated)]
            let dialog = adw::MessageDialog::new(
                parent_win.as_ref(),
                Some("Reset all keybindings to defaults?"),
                Some("This will remove all custom keybindings. This cannot be undone."),
            );
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("reset", "Reset all");
            dialog.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");

            let shared_dialog = Rc::clone(&shared);
            let app_dialog = app_ral.clone();
            let rw = row_widgets_ral.clone();

            dialog.connect_response(None, move |_d, response| {
                if response != "reset" {
                    return;
                }
                // Clear all custom keybindings.
                let new_config = {
                    let Ok(mut cfg) = shared_dialog.try_borrow_mut() else { return };
                    cfg.keybindings.clear();
                    cfg.clone()
                };
                if let Err(e) = save_config(&new_config) {
                    tracing::warn!("Failed to save config after reset-all: {e}");
                }
                // Re-register all defaults.
                for def in ACTION_DEFS {
                    let accel_strs: Vec<&str> = def.default_accels.iter().copied().collect();
                    app_dialog.set_accels_for_action(def.action_name, &accel_strs);
                }
                // Update all row UIs.
                for (shortcut_lbl, reset_btn, conflict_lbl, config_key) in &rw {
                    let def = ACTION_DEFS.iter().find(|d| d.config_key == *config_key);
                    if let Some(def) = def {
                        let display = if !def.default_accels.is_empty() {
                            accel_to_display(def.default_accels[0])
                        } else {
                            "— None —".to_string()
                        };
                        shortcut_lbl.set_text(&display);
                        shortcut_lbl.remove_css_class("accent");
                        reset_btn.set_sensitive(false);
                        conflict_lbl.set_visible(false);
                    }
                }
            });

            dialog.present();
        });
    }

    scrolled.set_child(Some(&list_box));
    outer.append(&scrolled);

    outer
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
