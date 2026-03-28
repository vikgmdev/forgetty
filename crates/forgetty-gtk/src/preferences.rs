//! Appearance sidebar for live visual configuration editing.
//!
//! Provides an in-window sidebar (right-panel) with Theme, Font Family, and
//! Font Size dropdowns. Every change is applied IMMEDIATELY to all terminal
//! panes via `apply_config_change()`, then persisted to disk in the background.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use forgetty_config::{parse_theme_file, save_config, Config, Theme};
use gtk4::prelude::*;
use libadwaita as adw;

use crate::terminal::{self, TerminalState};

/// Shared config state, matching the type alias in `app.rs`.
type SharedConfig = Rc<RefCell<Config>>;

/// Pane state map, matching the type alias in `app.rs`.
type TabStateMap = Rc<RefCell<HashMap<String, Rc<RefCell<TerminalState>>>>>;

/// Build the Appearance sidebar as a `gtk4::Revealer`.
///
/// The revealer uses `SlideLeft` transition and starts hidden. The caller
/// places it in the layout and connects a menu action to toggle visibility.
///
/// Each dropdown's change handler:
/// 1. Mutates `SharedConfig` directly.
/// 2. Calls `apply_config_change()` on every pane for instant visual update.
/// 3. Fires `save_config()` in the background to persist to disk.
pub fn build_appearance_sidebar(
    shared_config: &SharedConfig,
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
) -> gtk4::Revealer {
    let revealer = gtk4::Revealer::new();
    revealer.set_transition_type(gtk4::RevealerTransitionType::None);
    revealer.set_reveal_child(false);
    revealer.set_visible(false);

    // --- Sidebar container ---
    let sidebar_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    sidebar_box.set_width_request(300);
    sidebar_box.set_size_request(300, -1);
    sidebar_box.set_hexpand(false);
    sidebar_box.set_vexpand(true);
    sidebar_box.add_css_class("sidebar");
    sidebar_box.set_margin_start(0);
    sidebar_box.set_margin_end(0);

    revealer.set_hexpand(false);
    revealer.set_overflow(gtk4::Overflow::Hidden);

    // Title bar with close button
    let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    title_bar.set_margin_top(12);
    title_bar.set_margin_bottom(8);
    title_bar.set_margin_start(12);
    title_bar.set_margin_end(12);

    let title_label = gtk4::Label::new(Some("Appearance"));
    title_label.add_css_class("title-3");
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_hexpand(true);
    title_bar.append(&title_label);

    let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
    close_button.add_css_class("flat");
    close_button.set_tooltip_text(Some("Close (Ctrl+,)"));
    {
        let rev = revealer.clone();
        close_button.connect_clicked(move |_| {
            rev.set_reveal_child(false);
        });
    }
    title_bar.append(&close_button);

    sidebar_box.append(&title_bar);

    let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    sidebar_box.append(&separator);

    // Read current config for pre-selection
    let current_config = {
        let Ok(cfg) = shared_config.try_borrow() else {
            revealer.set_child(Some(&sidebar_box));
            return revealer;
        };
        cfg.clone()
    };

    // --- Theme section ---
    let theme_section = build_theme_section(&current_config, shared_config, tab_states, window);
    sidebar_box.append(&theme_section);

    // --- Font Family section ---
    let font_family_section =
        build_font_family_section(&current_config, shared_config, tab_states, window);
    sidebar_box.append(&font_family_section);

    // --- Font Size section ---
    let font_size_section =
        build_font_size_section(&current_config, shared_config, tab_states, window);
    sidebar_box.append(&font_size_section);

    // Close sidebar on Escape when it has focus.
    {
        let rev = revealer.clone();
        let key_controller = gtk4::EventControllerKey::new();
        key_controller.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            if key == gtk4::gdk::Key::Escape {
                rev.set_reveal_child(false);
                return gtk4::glib::Propagation::Stop;
            }
            gtk4::glib::Propagation::Proceed
        });
        sidebar_box.add_controller(key_controller);
    }

    revealer.set_child(Some(&sidebar_box));
    revealer
}

// ---------------------------------------------------------------------------
// Theme section
// ---------------------------------------------------------------------------

/// Build the Theme dropdown section.
fn build_theme_section(
    config: &Config,
    shared_config: &SharedConfig,
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
) -> gtk4::Box {
    let section = make_section_box();

    let label = make_section_label("Theme");
    section.append(&label);

    // Scan for custom theme files.
    let themes_dir = forgetty_core::platform::config_dir().join("themes");
    let mut theme_entries: Vec<(String, Option<PathBuf>)> = vec![];
    theme_entries.push(("Default Dark".to_string(), None));

    if themes_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&themes_dir) {
            let mut custom: Vec<(String, PathBuf)> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
                .filter_map(|e| {
                    let path = e.path();
                    let stem = path.file_stem()?.to_string_lossy().to_string();
                    // Validate the theme parses before adding it.
                    match std::fs::read_to_string(&path) {
                        Ok(contents) => match parse_theme_file(&contents) {
                            Ok(_) => Some((stem, path)),
                            Err(e) => {
                                tracing::warn!("Skipping malformed theme {}: {e}", path.display());
                                None
                            }
                        },
                        Err(e) => {
                            tracing::warn!("Skipping unreadable theme {}: {e}", path.display());
                            None
                        }
                    }
                })
                .collect();
            custom.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, path) in custom {
                theme_entries.push((name, Some(path)));
            }
        }
    }

    // Build StringList model.
    let string_list = gtk4::StringList::new(&[]);
    for (name, _) in &theme_entries {
        string_list.append(name);
    }

    let dropdown = gtk4::DropDown::new(Some(string_list), gtk4::Expression::NONE);
    dropdown.set_margin_start(12);
    dropdown.set_margin_end(12);

    // Pre-select the current theme.
    let selected_idx = find_current_theme_index(config, &theme_entries);
    dropdown.set_selected(selected_idx);

    let theme_count = theme_entries.len() as u32;

    // Connect change handler.
    {
        let shared = Rc::clone(shared_config);
        let states = Rc::clone(tab_states);
        let win = window.clone();
        let entries = theme_entries;
        dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            if idx >= entries.len() {
                return;
            }

            let new_theme = match &entries[idx].1 {
                None => Theme::default(),
                Some(p) => {
                    match std::fs::read_to_string(p)
                        .map_err(|e| e.to_string())
                        .and_then(|s| parse_theme_file(&s).map_err(|e| e.to_string()))
                    {
                        Ok(theme) => theme,
                        Err(e) => {
                            tracing::warn!("Failed to parse theme {}: {e}", p.display());
                            return;
                        }
                    }
                }
            };

            // 1. Update SharedConfig
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else {
                    return;
                };
                let mut updated = cfg.clone();
                updated.theme = new_theme;
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }

            // 2. Apply to every pane
            apply_to_all_panes(&states, &win, &new_config);

            // 3. Persist to disk in background
            save_in_background(new_config);
        });
    }

    dropdown.set_focusable(true);
    add_inline_arrow_cycling(&dropdown, theme_count);

    section.append(&dropdown);
    section
}

// ---------------------------------------------------------------------------
// Font Family section
// ---------------------------------------------------------------------------

/// Build the Font Family dropdown section.
fn build_font_family_section(
    config: &Config,
    shared_config: &SharedConfig,
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
) -> gtk4::Box {
    let section = make_section_box();

    let label = make_section_label("Font Family");
    section.append(&label);

    // Enumerate monospace fonts via Pango FontMap.
    let font_map = pangocairo::FontMap::default();
    let all_families = font_map.list_families();
    let mut families: Vec<String> =
        all_families.iter().filter(|f| f.is_monospace()).map(|f| f.name().to_string()).collect();
    families.sort();

    // Build StringList model.
    let string_list = gtk4::StringList::new(&[]);
    for name in &families {
        string_list.append(name);
    }

    let dropdown = gtk4::DropDown::new(Some(string_list), gtk4::Expression::NONE);
    dropdown.set_margin_start(12);
    dropdown.set_margin_end(12);

    // Enable search for long font lists.
    dropdown.set_enable_search(true);

    // Pre-select the current font family.
    let selected_idx = families.iter().position(|f| f == &config.font_family).unwrap_or(0) as u32;
    dropdown.set_selected(selected_idx);

    let family_count = families.len() as u32;

    // Connect change handler.
    {
        let shared = Rc::clone(shared_config);
        let states = Rc::clone(tab_states);
        let win = window.clone();
        let family_list = families;
        dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            if idx >= family_list.len() {
                return;
            }
            let new_family = &family_list[idx];

            // 1. Update SharedConfig
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else {
                    return;
                };
                if cfg.font_family == *new_family {
                    return;
                }
                let mut updated = cfg.clone();
                updated.font_family = new_family.clone();
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }

            // 2. Apply to every pane
            apply_to_all_panes(&states, &win, &new_config);

            // 3. Persist to disk in background
            save_in_background(new_config);
        });
    }

    dropdown.set_focusable(true);
    add_inline_arrow_cycling(&dropdown, family_count);

    section.append(&dropdown);
    section
}

// ---------------------------------------------------------------------------
// Font Size section
// ---------------------------------------------------------------------------

/// Hardcoded font size options.
const FONT_SIZES: &[u32] = &[8, 9, 10, 11, 12, 13, 14, 16, 18, 20, 24, 28, 32, 36, 48, 64, 72];

/// Build the Font Size dropdown section.
fn build_font_size_section(
    config: &Config,
    shared_config: &SharedConfig,
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
) -> gtk4::Box {
    let section = make_section_box();

    let label = make_section_label("Font Size");
    section.append(&label);

    // Build StringList model.
    let string_list = gtk4::StringList::new(&[]);
    for size in FONT_SIZES {
        string_list.append(&size.to_string());
    }

    let dropdown = gtk4::DropDown::new(Some(string_list), gtk4::Expression::NONE);
    dropdown.set_margin_start(12);
    dropdown.set_margin_end(12);

    // Pre-select the closest size to the current config.
    let selected_idx = find_closest_size_index(config.font_size);
    dropdown.set_selected(selected_idx);

    // Connect change handler.
    {
        let shared = Rc::clone(shared_config);
        let states = Rc::clone(tab_states);
        let win = window.clone();
        dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            if idx >= FONT_SIZES.len() {
                return;
            }
            let new_size = FONT_SIZES[idx] as f32;

            // 1. Update SharedConfig
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else {
                    return;
                };
                if (cfg.font_size - new_size).abs() < 0.01 {
                    return;
                }
                let mut updated = cfg.clone();
                updated.font_size = new_size;
                updated
            };
            if let Ok(mut cfg) = shared.try_borrow_mut() {
                *cfg = new_config.clone();
            }

            // 2. Apply to every pane
            apply_to_all_panes(&states, &win, &new_config);

            // 3. Persist to disk in background
            save_in_background(new_config);
        });
    }

    dropdown.set_focusable(true);
    add_inline_arrow_cycling(&dropdown, FONT_SIZES.len() as u32);

    section.append(&dropdown);
    section
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Apply config changes to every terminal pane via `apply_config_change()`.
///
/// This replicates the exact pattern used by `reload_config()` in `app.rs`.
fn apply_to_all_panes(
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
    new_config: &Config,
) {
    let Ok(states) = tab_states.try_borrow() else {
        return;
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
        terminal::apply_config_change(&mut s, new_config, &da);
    }
}

/// Save config to disk in the background (fire-and-forget via idle callback).
fn save_in_background(config: Config) {
    gtk4::glib::idle_add_local_once(move || {
        if let Err(e) = save_config(&config) {
            tracing::warn!("Failed to save config: {e}");
        }
    });
}

/// Find the index of the closest font size in `FONT_SIZES` to the given value.
fn find_closest_size_index(current_size: f32) -> u32 {
    let mut best_idx = 0u32;
    let mut best_diff = f32::MAX;
    for (i, &size) in FONT_SIZES.iter().enumerate() {
        let diff = (size as f32 - current_size).abs();
        if diff < best_diff {
            best_diff = diff;
            best_idx = i as u32;
        }
    }
    best_idx
}

/// Find the index of the currently active theme in the theme entries list.
///
/// Compares the current config theme against the default and each custom file.
/// Falls back to 0 (Default Dark) if no match is found.
fn find_current_theme_index(config: &Config, entries: &[(String, Option<PathBuf>)]) -> u32 {
    let default_theme = Theme::default();
    if theme_matches(&config.theme, &default_theme) {
        return 0;
    }

    // Check custom themes by parsing each file.
    for (idx, (_, path)) in entries.iter().enumerate() {
        if let Some(p) = path {
            if let Ok(contents) = std::fs::read_to_string(p) {
                if let Ok(theme) = parse_theme_file(&contents) {
                    if theme_matches(&config.theme, &theme) {
                        return idx as u32;
                    }
                }
            }
        }
    }

    0
}

/// Rough equality check for two themes (compare foreground, background, and cursor).
fn theme_matches(a: &Theme, b: &Theme) -> bool {
    a.foreground == b.foreground && a.background == b.background && a.cursor == b.cursor
}

/// Recursively find a DrawingArea with the given widget name.
///
/// This is the same logic as `find_drawing_area_by_name` in `app.rs`,
/// duplicated here to avoid cross-module visibility issues.
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
    let mut child = widget_ref.first_child();
    while let Some(c) = child {
        if let Some(found) = find_drawing_area_by_name(&c, name) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

/// Create a section box with standard spacing and padding.
fn make_section_box() -> gtk4::Box {
    let section = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    section.set_margin_top(12);
    section.set_margin_start(12);
    section.set_margin_end(12);
    section
}

/// Create a bold section label.
fn make_section_label(text: &str) -> gtk4::Label {
    let label = gtk4::Label::new(None);
    label.set_markup(&format!("<b>{text}</b>"));
    label.set_halign(gtk4::Align::Start);
    label.set_margin_bottom(4);
    label
}

/// Add an `EventControllerKey` to a `DropDown` so Up/Down arrows cycle
/// through items inline without opening the popup.
fn add_inline_arrow_cycling(dropdown: &gtk4::DropDown, item_count: u32) {
    let dd = dropdown.clone();
    let key_ctrl = gtk4::EventControllerKey::new();
    key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _mods| match key {
        gtk4::gdk::Key::Up => {
            let cur = dd.selected();
            if cur > 0 {
                dd.set_selected(cur - 1);
            }
            gtk4::glib::Propagation::Stop
        }
        gtk4::gdk::Key::Down => {
            let cur = dd.selected();
            if cur + 1 < item_count {
                dd.set_selected(cur + 1);
            }
            gtk4::glib::Propagation::Stop
        }
        _ => gtk4::glib::Propagation::Proceed,
    });
    dropdown.add_controller(key_ctrl);
}
