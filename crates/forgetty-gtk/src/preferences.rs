//! Settings sidebar for live visual configuration editing and device pairing.
//!
//! Provides an in-window sidebar (right-panel) with Theme, Font Family, Font
//! Size dropdowns, and a Paired Devices / QR pairing section. All appearance
//! changes apply immediately with live preview and are saved to disk.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use base64::Engine as _;
use forgetty_config::{load_theme_by_name, load_theme_catalog, save_config, Config};
use gtk4::prelude::*;
use libadwaita as adw;

use crate::daemon_client::DaemonClient;
use crate::terminal::{self, TerminalState};

/// Shared config state, matching the type alias in `app.rs`.
type SharedConfig = Rc<RefCell<Config>>;

/// Pane state map, matching the type alias in `app.rs`.
type TabStateMap = Rc<RefCell<HashMap<String, Rc<RefCell<TerminalState>>>>>;

/// Build the Settings sidebar as a `gtk4::Revealer`.
///
/// The revealer uses `SlideLeft` transition and starts hidden. The caller
/// places it in the layout and connects a menu action to toggle visibility.
///
/// The theme browser supports:
/// - Arrow key navigation with live preview on every selection change.
/// - Enter to confirm (save to config.toml).
/// - Escape to revert to the original theme.
/// - Close sidebar (X button or toggle) also reverts if no Enter was pressed.
///
/// `daemon_client` is used to populate the "Paired Devices" sync section.
/// Pass `None` if the daemon is unavailable.
pub fn build_appearance_sidebar(
    shared_config: &SharedConfig,
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
    daemon_client: Option<Arc<DaemonClient>>,
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

    let title_label = gtk4::Label::new(Some("Settings"));
    title_label.add_css_class("title-3");
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_hexpand(true);
    title_bar.append(&title_label);

    let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
    close_button.add_css_class("flat");
    close_button.set_tooltip_text(Some("Close (Ctrl+,)"));
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

    // --- Scrollable content area (holds all sections) ---
    let content_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content_box.set_vexpand(true);

    // --- Theme dropdown section ---
    let theme_section = build_theme_section(&current_config, shared_config, tab_states, window);
    content_box.append(&theme_section);

    // --- Font Family section ---
    let font_family_section =
        build_font_family_section(&current_config, shared_config, tab_states, window);
    content_box.append(&font_family_section);

    // --- Font Size section ---
    let font_size_section =
        build_font_size_section(&current_config, shared_config, tab_states, window);
    content_box.append(&font_size_section);

    // --- Sync / Paired Devices section ---
    let sync_section = build_sync_section(daemon_client);
    content_box.append(&sync_section);

    // Wrap content in a ScrolledWindow for long option lists.
    let scrolled_sidebar = gtk4::ScrolledWindow::new();
    scrolled_sidebar.set_child(Some(&content_box));
    scrolled_sidebar.set_vexpand(true);
    scrolled_sidebar.set_propagate_natural_height(true);
    scrolled_sidebar.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    sidebar_box.append(&scrolled_sidebar);

    // --- Close button handler ---
    {
        let rev = revealer.clone();
        close_button.connect_clicked(move |_| {
            rev.set_reveal_child(false);
            rev.set_visible(false);
        });
    }

    revealer.set_child(Some(&sidebar_box));
    revealer
}

// ---------------------------------------------------------------------------
// Theme dropdown section
// ---------------------------------------------------------------------------

/// Build the Theme dropdown section (same pattern as Font Family).
fn build_theme_section(
    config: &Config,
    shared_config: &SharedConfig,
    tab_states: &TabStateMap,
    window: &adw::ApplicationWindow,
) -> gtk4::Box {
    let section = make_section_box();

    let label = make_section_label("Theme");
    section.append(&label);

    // Load the full theme catalog (bundled + custom).
    let catalog = load_theme_catalog();
    let theme_names: Vec<String> = catalog.iter().map(|e| e.name.clone()).collect();

    // Build StringList model.
    let string_list = gtk4::StringList::new(&[]);
    for name in &theme_names {
        string_list.append(name);
    }

    let dropdown = gtk4::DropDown::new(Some(string_list), gtk4::Expression::NONE);
    dropdown.set_margin_start(12);
    dropdown.set_margin_end(12);

    // Enable search for 400+ themes.
    dropdown.set_enable_search(true);

    // Pre-select the current theme.
    let current_name = config.theme_name.as_deref().unwrap_or("0x96f");
    let selected_idx = theme_names.iter().position(|n| n == current_name).unwrap_or(0) as u32;
    dropdown.set_selected(selected_idx);

    let theme_count = theme_names.len() as u32;

    // Connect change handler — immediate apply + save (same as font dropdowns).
    {
        let shared = Rc::clone(shared_config);
        let states = Rc::clone(tab_states);
        let win = window.clone();
        let names = theme_names;
        dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            if idx >= names.len() {
                return;
            }
            let theme_name = &names[idx];
            let new_theme = load_theme_by_name(theme_name).unwrap_or_default();

            // 1. Update SharedConfig
            let new_config = {
                let Ok(cfg) = shared.try_borrow() else {
                    return;
                };
                if cfg.theme_name.as_deref() == Some(theme_name.as_str()) {
                    return;
                }
                let mut updated = cfg.clone();
                updated.theme = new_theme;
                updated.theme_name = Some(theme_name.clone());
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
// Sync / Paired Devices section
// ---------------------------------------------------------------------------

/// Build the "Paired Devices" sync section.
///
/// Shows a list of paired devices with Revoke buttons and a "Pair new device"
/// button that displays the QR code. If `daemon_client` is `None`, shows a
/// placeholder message.
pub fn build_sync_section(daemon_client: Option<Arc<DaemonClient>>) -> gtk4::Box {
    let section = make_section_box();

    let label = make_section_label("Paired Devices");
    section.append(&label);

    let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    separator.set_margin_bottom(8);
    section.append(&separator);

    let Some(dc) = daemon_client else {
        let placeholder = gtk4::Label::new(Some("Daemon not connected"));
        placeholder.set_halign(gtk4::Align::Start);
        placeholder.set_margin_start(4);
        section.append(&placeholder);
        return section;
    };

    // Container that shows either the device list or the QR code view.
    let stack = gtk4::Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::None);

    // --- Device list page ---
    let list_page = build_device_list_page(Arc::clone(&dc), &stack);
    stack.add_named(&list_page, Some("devices"));

    // Show "pair new device" button below the stack on the devices page.
    section.append(&stack);

    // Pair new device button.
    let pair_btn = gtk4::Button::with_label("Pair new device");
    pair_btn.add_css_class("suggested-action");
    pair_btn.set_margin_top(8);
    pair_btn.set_halign(gtk4::Align::Start);
    {
        let stack_clone = stack.clone();
        let dc_btn = Arc::clone(&dc);
        pair_btn.connect_clicked(move |_| {
            show_qr_view(&stack_clone, Arc::clone(&dc_btn));
        });
    }
    section.append(&pair_btn);

    section
}

/// Build the device list page for the stack.
fn build_device_list_page(dc: Arc<DaemonClient>, _stack: &gtk4::Stack) -> gtk4::Box {
    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 4);

    // Populate the list with current devices.
    let devices = dc.list_devices().unwrap_or_default();

    if devices.is_empty() {
        let lbl = gtk4::Label::new(Some("No paired devices"));
        lbl.set_halign(gtk4::Align::Start);
        lbl.add_css_class("dim-label");
        lbl.set_margin_start(4);
        page.append(&lbl);
    } else {
        let list_box = gtk4::ListBox::new();
        list_box.add_css_class("boxed-list");
        list_box.set_selection_mode(gtk4::SelectionMode::None);

        for device in devices {
            let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row_box.set_margin_top(4);
            row_box.set_margin_bottom(4);
            row_box.set_margin_start(8);
            row_box.set_margin_end(8);

            let last_seen_text = device
                .last_seen
                .as_deref()
                .unwrap_or("never");
            let label_text = format!("{}  —  last seen: {}", device.name, last_seen_text);
            let dev_label = gtk4::Label::new(Some(&label_text));
            dev_label.set_halign(gtk4::Align::Start);
            dev_label.set_hexpand(true);
            dev_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            row_box.append(&dev_label);

            let revoke_btn = gtk4::Button::with_label("Revoke");
            revoke_btn.add_css_class("destructive-action");
            revoke_btn.add_css_class("flat");
            {
                let dc_rev = Arc::clone(&dc);
                let device_id = device.device_id.clone();
                let row_box_ref = row_box.clone();
                revoke_btn.connect_clicked(move |_| {
                    if let Err(e) = dc_rev.revoke_device(&device_id) {
                        tracing::warn!("revoke_device failed: {e}");
                    }
                    // Remove the row from the parent widget (hide immediately).
                    if let Some(parent) = row_box_ref.parent() {
                        if let Some(lb) = parent.downcast_ref::<gtk4::ListBox>() {
                            if let Some(row) = row_box_ref.parent().and_then(|p| p.parent()) {
                                lb.remove(&row);
                            }
                        }
                    }
                });
            }
            row_box.append(&revoke_btn);

            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&row_box));
            list_box.append(&row);
        }

        page.append(&list_box);
    }

    page
}

/// Switch the stack to show the QR code view.
fn show_qr_view(stack: &gtk4::Stack, dc: Arc<DaemonClient>) {
    // Fetch pairing info from daemon.
    let info = match dc.get_pairing_info() {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("get_pairing_info failed: {e}");
            return;
        }
    };

    // Decode QR PNG from base64.
    let png_bytes = match base64::engine::general_purpose::STANDARD.decode(&info.qr_png_base64) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("QR base64 decode failed: {e}");
            return;
        }
    };

    // Create GTK image from PNG bytes via gdk_pixbuf.
    let bytes = gtk4::glib::Bytes::from(&png_bytes);
    let stream = gtk4::gio::MemoryInputStream::from_bytes(&bytes);
    let pixbuf = match gdk_pixbuf::Pixbuf::from_stream(
        &stream,
        gtk4::gio::Cancellable::NONE,
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Pixbuf::from_stream failed: {e}");
            return;
        }
    };

    // Convert Pixbuf → Texture → Paintable to avoid the deprecated from_pixbuf path.
    let texture = gtk4::gdk::Texture::for_pixbuf(&pixbuf);
    let qr_image = gtk4::Image::from_paintable(Some(&texture));
    qr_image.set_pixel_size(250);

    // Truncate node_id to 16 chars for display.
    let node_id_short = if info.node_id.len() > 16 {
        format!("{}…", &info.node_id[..16])
    } else {
        info.node_id.clone()
    };
    let node_label = gtk4::Label::new(Some(&format!("node: {node_id_short}")));
    node_label.add_css_class("monospace");
    node_label.add_css_class("dim-label");
    node_label.set_margin_top(4);

    let done_btn = gtk4::Button::with_label("Done");
    done_btn.set_margin_top(8);
    done_btn.set_halign(gtk4::Align::Start);

    let qr_page = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    qr_page.append(&qr_image);
    qr_page.append(&node_label);
    qr_page.append(&done_btn);

    // Remove previous QR page if any, then add and show.
    if stack.child_by_name("qr").is_some() {
        if let Some(old) = stack.child_by_name("qr") {
            stack.remove(&old);
        }
    }
    stack.add_named(&qr_page, Some("qr"));
    stack.set_visible_child_name("qr");

    // Done button: go back to devices view.
    {
        let stack_done = stack.clone();
        done_btn.connect_clicked(move |_| {
            stack_done.set_visible_child_name("devices");
        });
    }
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
