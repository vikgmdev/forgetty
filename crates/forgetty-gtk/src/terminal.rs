//! Terminal grid rendering with Pango + Cairo.
//!
//! Provides `create_terminal()` which returns a `(gtk::Box, DrawingArea, State)`
//! triple: the Box contains the DrawingArea (terminal grid) and a vertical
//! gtk::Scrollbar on the right edge. The DrawingArea renders the terminal grid
//! from `forgetty_vt::Terminal`'s screen state using Cairo for drawing and
//! Pango for text layout.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use libc;

use forgetty_config::{BellMode, Config, CursorStyle, NotificationMode};
use forgetty_core::Rgba;
// OSC 9/99/777 notification detection is client-owned (AD-007/AD-008).
// V2-006 moved the scanner from `forgetty-session` into this crate.
use crate::osc_notification::scan_osc_notification;
pub use crate::osc_notification::{NotificationPayload, NotificationSource};
use forgetty_vt::screen::Color;
use forgetty_vt::selection::{Selection, SelectionMode};
use forgetty_vt::TerminalEvent;
use gtk4::cairo;
use gtk4::gdk;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{glib, DrawingArea};

use crate::code_block::{self, CodeBlock};
use crate::daemon_client::{DaemonClient, DaemonOutputMessage};
use crate::input::{GhosttyInput, ScrollAction};

/// Search state for in-terminal text search (Ctrl+Shift+F).
///
/// Tracks the current query, all match positions across the entire scrollback,
/// and the index of the currently focused match for navigation.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    /// Whether search mode is active (search bar visible).
    pub active: bool,
    /// The current search query (lowercased for case-insensitive matching).
    pub query: String,
    /// ALL matches across the entire scrollback: (absolute_row, start_col, match_len).
    /// Sorted by absolute_row ascending (natural order from scanning top to bottom).
    pub all_matches: Vec<(usize, usize, usize)>,
    /// Viewport-relative matches for drawing: (screen_row, start_col, match_len).
    /// Recomputed each frame from `all_matches` by filtering to the visible window.
    pub matches: Vec<(usize, usize, usize)>,
    /// Index into `all_matches` for the currently focused match.
    pub current_index: usize,
    /// Index within `matches` (viewport-relative) that corresponds to the
    /// currently focused match (`current_index` in `all_matches`), or `None`
    /// if the focused match is not in the current viewport.
    pub current_viewport_index: Option<usize>,
}

/// A URL detected under the mouse cursor, with its screen position.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HoverUrl {
    url: String,
    screen_row: usize,
    col_start: usize,
    col_end: usize, // exclusive
}

/// Shared terminal state accessible from multiple GTK callbacks.
///
/// All access happens on the GTK main thread via `Rc<RefCell<>>`.
pub struct TerminalState {
    pub terminal: forgetty_vt::Terminal,
    pub input: GhosttyInput,
    pub config: Config,
    pub cell_width: f64,
    pub cell_height: f64,
    pub cols: usize,
    pub rows: usize,
    /// Current text selection, if any.
    pub selection: Option<Selection>,
    /// Whether the user is actively dragging a selection.
    pub selecting: bool,
    /// The anchor word boundaries for word-mode drag extension (start_col, end_col).
    pub word_anchor: Option<(usize, usize)>,
    /// Deferred drag origin: (row, col) saved on press, selection created on first motion.
    /// Prevents flicker on single clicks (selection appears then immediately clears).
    pub drag_origin: Option<(usize, usize)>,
    /// Suppress selection clearing for N ticks after a resize.
    /// Shell redraws on SIGWINCH, which would otherwise clear the selection.
    /// Each resize resets this to a grace period; decremented each tick.
    pub suppress_selection_clear_ticks: u8,
    /// Guard flag: true while we are programmatically updating the scrollbar
    /// adjustment, to suppress the `value-changed` signal handler and prevent
    /// a feedback loop (terminal -> adjustment -> scroll -> terminal -> ...).
    pub updating_scrollbar: bool,
    /// True when the user has explicitly scrolled up (away from bottom).
    /// Auto-scroll to bottom is suppressed while this is set.
    /// Cleared when the user scrolls back to the exact bottom, sends input,
    /// or the viewport is programmatically snapped to bottom.
    pub scroll_lock: bool,
    /// Cached viewport offset from the 16ms scrollbar timer.
    /// Used by mouse handlers to avoid expensive FFI calls on every event.
    pub viewport_offset: u64,
    /// In-terminal search state (Ctrl+Shift+F).
    pub search: SearchState,
    /// Current font size in points (may differ from config default after zoom).
    pub font_size: f32,
    /// The configured default font size, for Ctrl+0 reset.
    pub default_font_size: f32,
    /// Currently hovered URL (if any) for underline rendering and Ctrl+Click.
    hover_url: Option<HoverUrl>,
    /// Last cell checked for hover URL (row, col). Avoids re-scanning the same cell on sub-pixel motion.
    last_hover_cell: (usize, usize),
    /// Current blink phase: true = cursor drawn, false = cursor hidden.
    pub cursor_blink_visible: bool,
    /// Timestamp of the last blink phase toggle (or last keypress).
    pub last_blink_toggle: Instant,
    /// Whether the terminal was on the alternate screen last tick.
    /// Used to detect alt→primary transitions and restore blink default.
    pub was_alternate_screen: bool,
    /// Deadline for the visual bell flash overlay. While `Instant::now()` is
    /// before this deadline, `draw_terminal()` paints a semi-transparent white
    /// overlay over the pane.
    pub bell_flash_until: Option<Instant>,
    /// Timestamp of the last bell response, for rate limiting (200ms cooldown).
    pub last_bell: Instant,
    /// Suppress BEL flash until this instant. Set when Ctrl+C is written so the
    /// shell's readline BEL response (zsh beeps on SIGINT) doesn't cause a flash.
    pub suppress_bell_until: Option<Instant>,
    /// Timestamp of the last PTY data received. Used to detect idle periods
    /// for calling `malloc_trim(0)` to return freed memory to the OS (T-028).
    pub last_pty_data: Instant,
    /// Whether `malloc_trim` has already been called for the current idle
    /// period. Reset to `false` when new PTY data arrives. Prevents calling
    /// `malloc_trim` repeatedly during sustained idle periods.
    pub malloc_trimmed: bool,
    /// Whether this pane currently has a notification ring drawn around it.
    /// Set when an OSC or BEL notification fires on an unfocused pane.
    /// Cleared when the user focuses this pane.
    pub notification_ring: bool,
    /// Timestamp of the last desktop notification sent from this pane.
    /// Used for 2-second rate limiting to suppress notification spam.
    pub last_notification: Instant,
    /// Callback invoked when a notification triggers (OSC 9/99/777 or BEL).
    /// Wired by `app.rs` to handle tab badge updates and desktop notifications.
    /// `None` until the caller registers it via `set_on_notify()`.
    pub on_notify: Option<Rc<dyn Fn(NotificationPayload)>>,
    /// Currently hovered code block (if any) for overlay rendering and copy button.
    pub hovered_code_block: Option<CodeBlock>,
    /// All code blocks detected in the current viewport.
    pub detected_code_blocks: Vec<CodeBlock>,
    /// Screen generation when code blocks were last scanned.
    pub code_blocks_generation: u64,
    /// Deadline for showing checkmark icon after a successful copy.
    pub copy_confirmed_until: Option<Instant>,
    /// Last cell checked for code block hover (row, col). Avoids re-scanning
    /// on sub-pixel motion within the same cell.
    pub last_hover_block_cell: (usize, usize),
    /// For daemon-backed panes: the remote pane ID in the daemon.
    pub daemon_pane_id: Option<forgetty_core::PaneId>,
    /// For daemon-backed panes: handle for routing write-pty responses.
    pub daemon_client: Option<Arc<DaemonClient>>,
    /// For daemon-backed panes: the CWD from `PaneInfo` at connect time.
    /// Used as a fallback tab title until the shell emits OSC 0/2.
    pub daemon_cwd: Option<PathBuf>,
}

/// Measure the cell dimensions for a monospace font using Pango.
fn measure_cell(pango_ctx: &pango::Context, font_desc: &pango::FontDescription) -> (f64, f64) {
    let layout = pango::Layout::new(pango_ctx);
    layout.set_font_description(Some(font_desc));
    layout.set_text("M");
    let (w, h) = layout.pixel_size();
    (w as f64, h as f64)
}

/// Build a `pango::FontDescription` from the config with an explicit size.
///
/// Used by `draw_terminal()` and `apply_font_zoom()` so the font size
/// reflects the current zoom level rather than the static config default.
fn font_description_with_size(config: &Config, size: f32) -> pango::FontDescription {
    let mut desc = pango::FontDescription::new();
    desc.set_family(&config.font_family);
    desc.set_size((size as i32) * pango::SCALE);
    desc
}

/// Create a daemon-backed terminal widget for an existing daemon pane.
///
/// Connects to a remote pane managed by `forgetty-daemon`. PTY I/O goes through
/// the daemon's JSON-RPC API (`send_input` / `subscribe_output`).
///
/// `daemon_channel` carries the mpsc receiver for decoded output frames plus
/// the wake-pipe read fd used by `glib::unix_fd_add_local` to trigger the
/// GLib handler on new data (no polling — see AD-009).
///
/// Initial screen content is populated by the daemon's byte-log replay
/// (V2-007 / AD-013): `subscribe_output` delivers the recent PTY bytes as its
/// first frames, which the VT parser below feeds itself.
pub fn create_terminal(
    config: &Config,
    pane_id: forgetty_core::PaneId,
    daemon_client: Arc<DaemonClient>,
    daemon_channel: crate::daemon_client::DaemonOutputChannel,
    cwd: Option<PathBuf>,
    on_exit: Option<Rc<dyn Fn(String)>>,
    on_notify: Option<Rc<dyn Fn(NotificationPayload)>>,
) -> Result<(gtk4::Box, DrawingArea, Rc<RefCell<TerminalState>>), String> {
    use std::os::unix::io::AsRawFd as _;
    // Destructure the daemon output channel: mpsc receiver + wake pipe read end.
    let crate::daemon_client::DaemonOutputChannel { rx: daemon_rx, wake_read_fd } = daemon_channel;
    let wake_read_raw = wake_read_fd.as_raw_fd();

    // Over-estimate rows so the first-draw resize is always a SHRINK.
    // libghostty-vt shrinks by trimming trailing blank rows from the BOTTOM;
    // byte-log replay writes content from the top, so the blank rows sit
    // below it and get trimmed cleanly on the first resize.
    // 80 rows covers any realistic monitor+font combination.
    let initial_rows: usize = 80;
    let initial_cols: usize = 240;

    // Create terminal VT state (no local PTY)
    let mut terminal =
        forgetty_vt::Terminal::new(initial_rows, initial_cols, config.theme.ansi_colors);
    terminal.feed(b"\x1b[1 q");

    let input = GhosttyInput::new();

    let state = Rc::new(RefCell::new(TerminalState {
        terminal,
        input,
        config: config.clone(),
        cell_width: 8.0,
        cell_height: 16.0,
        cols: initial_cols,
        rows: initial_rows,
        selection: None,
        selecting: false,
        word_anchor: None,
        drag_origin: None,
        suppress_selection_clear_ticks: 0,
        updating_scrollbar: false,
        scroll_lock: false,
        viewport_offset: 0,
        search: SearchState::default(),
        font_size: config.font_size,
        default_font_size: config.font_size,
        hover_url: None,
        last_hover_cell: (usize::MAX, usize::MAX),
        cursor_blink_visible: true,
        last_blink_toggle: Instant::now(),
        was_alternate_screen: false,
        bell_flash_until: None,
        last_bell: Instant::now() - Duration::from_secs(1),
        suppress_bell_until: None,
        last_pty_data: Instant::now(),
        malloc_trimmed: false,
        notification_ring: false,
        last_notification: Instant::now() - Duration::from_secs(10),
        on_notify,
        hovered_code_block: None,
        detected_code_blocks: Vec::new(),
        code_blocks_generation: u64::MAX, // force re-scan on first motion
        copy_confirmed_until: None,
        last_hover_block_cell: (usize::MAX, usize::MAX),
        daemon_pane_id: Some(pane_id),
        daemon_client: Some(daemon_client),
        daemon_cwd: cwd,
    }));

    // Create DrawingArea
    let drawing_area = DrawingArea::new();
    drawing_area.set_hexpand(true);
    drawing_area.set_vexpand(true);
    drawing_area.set_focusable(true);
    drawing_area.set_can_focus(true);
    drawing_area.set_cursor_from_name(Some("text"));

    // Track whether cell dimensions have been measured from an actual Pango context
    let cell_measured = Rc::new(RefCell::new(false));

    // --- Draw callback ---
    {
        let state = Rc::clone(&state);
        let cell_measured = Rc::clone(&cell_measured);
        drawing_area.set_draw_func(move |da, ctx, width, height| {
            draw_terminal(da, ctx, width, height, &state, &cell_measured);
        });
    }

    // --- Event-driven daemon output (pipe + mpsc, zero polling) ---
    //
    // Replaces the 8 ms `timeout_add_local` poll.  The tokio `subscribe_output`
    // task writes one wake byte to the write end of an OS pipe after each send;
    // `glib::unix_fd_add_local` fires this callback from the GLib main loop only
    // when the read end becomes readable — no periodic wakeup.
    // (AD-009: no polling on the hot path).
    {
        let state = Rc::clone(&state);
        let da_weak = drawing_area.downgrade();
        let on_exit = on_exit.map(|cb| Rc::new(std::cell::Cell::new(Some(cb))));
        glib::unix_fd_add_local(wake_read_raw, glib::IOCondition::IN, move |fd, _| {
            // Captures wake_read_fd for RAII (keeps pipe read end open).
            debug_assert_eq!(fd, wake_read_fd.as_raw_fd());
            // Drain wake bytes (one per DaemonOutputMessage sent by the tokio task).
            let mut drain_buf = [0u8; 256];
            let _ = unsafe { libc::read(fd, drain_buf.as_mut_ptr() as _, 256) };

            let Some(da) = da_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };

            while let Ok(msg) = daemon_rx.try_recv() {
                match msg {
                    DaemonOutputMessage::StreamEnded => {
                        // Same logic as the old `pty_exited` branch: if the daemon is
                        // still alive, schedule on_exit (pane shell exited naturally).
                        // If the daemon itself died, keep the pane open to preserve session.
                        let daemon_alive = {
                            let Ok(s) = state.try_borrow() else {
                                return glib::ControlFlow::Break;
                            };
                            s.daemon_client
                                .as_ref()
                                .map(|dc| dc.list_tabs().is_ok())
                                .unwrap_or(true)
                        };

                        if daemon_alive {
                            tracing::debug!(
                                "Daemon pane {:?} exited (daemon alive), scheduling close",
                                da.widget_name()
                            );
                            if let Some(ref exit_cell) = on_exit {
                                if let Some(cb) = exit_cell.take() {
                                    let pane_name = da.widget_name().to_string();
                                    glib::idle_add_local_once(move || {
                                        cb(pane_name);
                                    });
                                }
                            }
                        } else {
                            tracing::info!(
                                "Daemon died — keeping pane {:?} open to preserve session",
                                da.widget_name()
                            );
                        }
                        return glib::ControlFlow::Break;
                    }

                    DaemonOutputMessage::Data(data) => {
                        let Ok(mut s) = state.try_borrow_mut() else {
                            continue;
                        };

                        let (_, offset, _) = s.terminal.scrollbar_state();
                        s.viewport_offset = offset;

                        // Scan for OSC notification sequences BEFORE feeding to VT parser.
                        let osc_notification = scan_osc_notification(&data);

                        s.terminal.feed(&data);
                        s.last_pty_data = Instant::now();
                        s.malloc_trimmed = false;

                        // Drain write-PTY responses (DA responses, mode queries, etc.)
                        let responses = s.terminal.drain_write_pty();
                        for chunk in responses {
                            if let Some(ref dc) = s.daemon_client {
                                if let Some(pane_id) = s.daemon_pane_id {
                                    let _ = dc.send_input(pane_id, &chunk);
                                }
                            }
                        }

                        // Detect alternate screen → primary screen transitions (e.g. htop exit).
                        let is_alt = s.terminal.is_alternate_screen();
                        if s.was_alternate_screen && !is_alt {
                            s.terminal.feed(b"\x1b[1 q");
                        }
                        s.was_alternate_screen = is_alt;

                        let events = s.terminal.drain_events();
                        let mut bell_notify_payload: Option<NotificationPayload> = None;
                        let mut bell_flash_scheduled = false;
                        for event in events {
                            if let TerminalEvent::Bell = event {
                                let now = Instant::now();
                                if now.duration_since(s.last_bell) < Duration::from_millis(200) {
                                    continue;
                                }
                                if s.suppress_bell_until.is_some_and(|t| now < t) {
                                    s.suppress_bell_until = None;
                                    continue;
                                }
                                s.last_bell = now;

                                match s.config.bell_mode {
                                    BellMode::Visual => {
                                        s.bell_flash_until =
                                            Some(Instant::now() + Duration::from_millis(150));
                                        bell_flash_scheduled = true;
                                    }
                                    BellMode::Audio => {
                                        da.error_bell();
                                    }
                                    BellMode::Both => {
                                        s.bell_flash_until =
                                            Some(Instant::now() + Duration::from_millis(150));
                                        bell_flash_scheduled = true;
                                        da.error_bell();
                                    }
                                    BellMode::None => {}
                                }

                                if !da.has_focus()
                                    && s.config.notification_mode != NotificationMode::None
                                {
                                    s.notification_ring = true;
                                    bell_notify_payload = Some(NotificationPayload {
                                        title: String::new(),
                                        body: String::new(),
                                        pane_name: da.widget_name().to_string(),
                                        source: None,
                                    });
                                }
                            }
                        }

                        let osc_notify_payload = if let Some(mut payload) = osc_notification {
                            if !da.has_focus()
                                && s.config.notification_mode != NotificationMode::None
                            {
                                s.notification_ring = true;
                                payload.pane_name = da.widget_name().to_string();
                                Some(payload)
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        let on_notify_cb = s.on_notify.clone();
                        let last_notif = s.last_notification;
                        let can_send_desktop = last_notif.elapsed() >= Duration::from_secs(2);
                        if can_send_desktop
                            && (bell_notify_payload.is_some() || osc_notify_payload.is_some())
                        {
                            s.last_notification = Instant::now();
                        }

                        // V2-006: capture daemon handles so we can fire the advisory
                        // `notify` RPC after `drop(s)` below.  Kept in scope across
                        // the drop via a clone of the Arc.
                        let notify_rpc_handles = if can_send_desktop {
                            match (s.daemon_client.clone(), s.daemon_pane_id) {
                                (Some(dc), Some(pid)) => Some((dc, pid)),
                                _ => None,
                            }
                        } else {
                            None
                        };

                        if !s.scroll_lock {
                            s.terminal.scroll_viewport_bottom();
                            let (_, off, _) = s.terminal.scrollbar_state();
                            s.viewport_offset = off;
                        }

                        // Selection is NOT cleared on output — cleared on keypress instead.

                        if s.search.active && !s.search.all_matches.is_empty() {
                            s.search.all_matches.clear();
                            s.search.matches.clear();
                            s.search.current_index = 0;
                            s.search.current_viewport_index = None;
                        }

                        drop(s);
                        da.queue_draw();

                        // If a visual bell flash was triggered, schedule a clear redraw
                        // after the 150 ms flash window so the overlay disappears promptly
                        // rather than waiting for the next 600 ms blink tick.
                        if bell_flash_scheduled {
                            let da_bell = da.clone();
                            glib::timeout_add_local_once(Duration::from_millis(200), move || {
                                da_bell.queue_draw();
                            });
                        }

                        if let Some(payload) = bell_notify_payload {
                            if let Some(ref cb) = on_notify_cb {
                                cb(payload);
                            }
                        }
                        if let Some(payload) = osc_notify_payload {
                            // V2-006: advisory log RPC for daemon-side observability.
                            // Fire BEFORE the callback consumes `payload`, and only
                            // when `source` is set (BEL notifications are source=None
                            // and skip the RPC per SPEC AC-18).  Rate-limited by the
                            // same 2-second `can_send_desktop` gate so a misbehaving
                            // tool cannot flood the daemon log.
                            if let (Some(src), Some((ref dc, pid))) =
                                (payload.source, notify_rpc_handles.as_ref())
                            {
                                let src_str = match src {
                                    NotificationSource::Osc9 => "Osc9",
                                    NotificationSource::Osc99 => "Osc99",
                                    NotificationSource::Osc777 => "Osc777",
                                };
                                dc.notify(*pid, &payload.title, &payload.body, src_str);
                            }
                            if let Some(ref cb) = on_notify_cb {
                                let mut p = payload;
                                if !can_send_desktop {
                                    p.source = None;
                                }
                                cb(p);
                            }
                        }
                    }
                } // end match msg
            } // end while let

            glib::ControlFlow::Continue
        });
    }

    // --- Cursor blink, bell-flash expiry, and malloc_trim (low-frequency timer) ---
    //
    // Cursor blink no longer piggybacks on the output-poll timer. A dedicated
    // 600 ms timer toggles the blink state and redraws.  This matches the
    // actual state-change frequency of the old 8 ms timer (which checked
    // `duration_since(last_blink_toggle) >= 600ms` every tick) while firing
    // 74× less often.
    {
        let state = Rc::clone(&state);
        let da_weak = drawing_area.downgrade();
        glib::timeout_add_local(Duration::from_millis(600), move || {
            let Some(da) = da_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Ok(mut s) = state.try_borrow_mut() else {
                return glib::ControlFlow::Continue;
            };

            // Toggle cursor blink phase.
            s.cursor_blink_visible = !s.cursor_blink_visible;
            s.last_blink_toggle = Instant::now();

            // Return freed heap memory to the OS after 5 s of no PTY data.
            #[cfg(target_os = "linux")]
            if !s.malloc_trimmed
                && Instant::now().duration_since(s.last_pty_data) >= Duration::from_secs(5)
            {
                s.malloc_trimmed = true;
                unsafe {
                    libc::malloc_trim(0);
                }
                tracing::debug!("malloc_trim(0) called after 5s idle (daemon pane)");
            }

            drop(s);
            da.queue_draw();

            glib::ControlFlow::Continue
        });
    }

    // --- Keyboard input (via ghostty key encoder) ---
    {
        let key_controller = gtk4::EventControllerKey::new();

        {
            let state = Rc::clone(&state);
            let da_for_key = drawing_area.clone();
            key_controller.connect_key_pressed(move |_controller, keyval, keycode, modifier| {
                if is_app_shortcut(keyval, modifier) {
                    return glib::Propagation::Proceed;
                }

                let Ok(mut s) = state.try_borrow_mut() else {
                    return glib::Propagation::Proceed;
                };

                let ctrl_only = modifier
                    & (gdk::ModifierType::CONTROL_MASK
                        | gdk::ModifierType::SHIFT_MASK
                        | gdk::ModifierType::ALT_MASK)
                    == gdk::ModifierType::CONTROL_MASK;
                if ctrl_only && (keyval == gdk::Key::c || keyval == gdk::Key::C) {
                    if s.selection.is_some() {
                        drop(s);
                        da_for_key.activate_action("win.copy", None).ok();
                    } else {
                        // In daemon mode, send SIGINT via daemon RPC.
                        if let (Some(ref dc), Some(pid)) =
                            (s.daemon_client.clone(), s.daemon_pane_id)
                        {
                            let _ = dc.send_sigint(pid);
                        }
                        // Scroll back to bottom so the user sees the shell prompt
                        // after interrupting a process from scrollback position.
                        s.terminal.scroll_viewport_bottom();
                        let (_, off, _) = s.terminal.scrollbar_state();
                        s.viewport_offset = off;
                        s.scroll_lock = false;
                        s.cursor_blink_visible = true;
                        s.last_blink_toggle = Instant::now();
                        s.suppress_bell_until = Some(Instant::now() + Duration::from_millis(300));
                    }
                    return glib::Propagation::Stop;
                }

                if keyval == gdk::Key::Escape
                    && s.selection.is_some()
                    && !s.terminal.is_mouse_tracking()
                {
                    s.selection = None;
                    s.selecting = false;
                    s.word_anchor = None;
                    drop(s);
                    da_for_key.queue_draw();
                    return glib::Propagation::Stop;
                }

                let terminal_handle = s.terminal.raw_handle();
                if let Some(bytes) =
                    s.input.encode_key_press(keyval, keycode, modifier, terminal_handle)
                {
                    if let (Some(ref dc), Some(pid)) = (s.daemon_client.clone(), s.daemon_pane_id) {
                        let _ = dc.send_input(pid, &bytes);
                    }
                    let was_scroll_locked = s.scroll_lock;
                    let had_selection = s.selection.is_some();
                    // Scroll back to bottom on any keypress so typing / Ctrl+L
                    // always brings the viewport back from scrollback position.
                    s.terminal.scroll_viewport_bottom();
                    let (_, off, _) = s.terminal.scrollbar_state();
                    s.viewport_offset = off;
                    s.scroll_lock = false; // resume auto-scroll now that user is typing
                                           // Clear any active selection — user is typing, selection is stale.
                    s.selection = None;
                    s.selecting = false;
                    s.word_anchor = None;
                    s.drag_origin = None;
                    s.cursor_blink_visible = true;
                    s.last_blink_toggle = Instant::now();
                    // Only redraw immediately if visible state changed — same logic as inline mode.
                    if was_scroll_locked || had_selection {
                        da_for_key.queue_draw();
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }

        {
            let state = Rc::clone(&state);
            let da_for_release = drawing_area.clone();
            key_controller.connect_key_released(move |_controller, keyval, keycode, modifier| {
                let Ok(mut s) = state.try_borrow_mut() else {
                    return;
                };
                let terminal_handle = s.terminal.raw_handle();
                if let Some(bytes) =
                    s.input.encode_key_release(keyval, keycode, modifier, terminal_handle)
                {
                    if let (Some(ref dc), Some(pid)) = (s.daemon_client.clone(), s.daemon_pane_id) {
                        let _ = dc.send_input(pid, &bytes);
                    }
                    da_for_release.queue_draw();
                }
            });
        }

        drawing_area.add_controller(key_controller);
    }

    // --- Focus controller ---
    {
        let focus_controller = gtk4::EventControllerFocus::new();

        {
            let state = Rc::clone(&state);
            let da_focus = drawing_area.clone();
            focus_controller.connect_enter(move |_controller| {
                let Ok(s) = state.try_borrow() else {
                    return;
                };
                if s.terminal.is_focus_reporting() {
                    if let Some(bytes) = GhosttyInput::encode_focus(true) {
                        if let (Some(ref dc), Some(pid)) =
                            (s.daemon_client.clone(), s.daemon_pane_id)
                        {
                            let _ = dc.send_input(pid, &bytes);
                        }
                        da_focus.queue_draw();
                    }
                }
            });
        }

        {
            let state = Rc::clone(&state);
            let da_focus = drawing_area.clone();
            focus_controller.connect_leave(move |_controller| {
                let Ok(s) = state.try_borrow() else {
                    return;
                };
                if s.terminal.is_focus_reporting() {
                    if let Some(bytes) = GhosttyInput::encode_focus(false) {
                        if let (Some(ref dc), Some(pid)) =
                            (s.daemon_client.clone(), s.daemon_pane_id)
                        {
                            let _ = dc.send_input(pid, &bytes);
                        }
                        da_focus.queue_draw();
                    }
                }
            });
        }

        drawing_area.add_controller(focus_controller);
    }

    // --- Mouse gesture controller ---
    {
        let gesture = gtk4::GestureClick::new();
        gesture.set_button(0);

        {
            let state = Rc::clone(&state);
            let da_click = drawing_area.clone();
            gesture.connect_pressed(move |gesture, n_press, x, y| {
                // Clicking on a pane should focus it (for split pane navigation).
                da_click.grab_focus();

                let button = gesture.current_button();
                let modifier = gesture.current_event_state();
                let Ok(mut s) = state.try_borrow_mut() else {
                    return;
                };

                // Right-click: show context menu (same as local PTY pane).
                // Button 3 always opens the menu even in mouse-tracking mode.
                if button == 3 {
                    let (screen_row, col) =
                        pixel_to_cell(x, y, s.cell_width, s.cell_height, s.cols, s.rows);
                    let screen = s.terminal.screen();
                    let hover = detect_url_at(screen, screen_row, col);
                    let url_str = hover.map(|h| h.url);
                    let has_selection = s.selection.is_some();
                    drop(s);
                    if let Some(popover) = find_context_popover(&da_click) {
                        let menu_box =
                            build_context_menu_box(&popover, url_str.as_deref(), has_selection);
                        popover.set_child(Some(&menu_box));
                        popover
                            .set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                        popover.popup();
                    }
                    return;
                }

                // Ctrl+Click to open URL (only when mouse tracking is off).
                if button == 1
                    && modifier.contains(gdk::ModifierType::CONTROL_MASK)
                    && !modifier.contains(gdk::ModifierType::SHIFT_MASK)
                    && !s.terminal.is_mouse_tracking()
                {
                    let (screen_row, col) =
                        pixel_to_cell(x, y, s.cell_width, s.cell_height, s.cols, s.rows);
                    // Scope `screen` so its borrow of `s.terminal` ends before
                    // we potentially move `s` below.
                    let hover = {
                        let screen = s.terminal.screen();
                        detect_url_at(screen, screen_row, col)
                    };
                    if let Some(hu) = hover {
                        drop(s);
                        crate::app::open_url_in_browser(&hu.url);
                        return;
                    }
                }

                // --- Code block copy button click (T-032) ---
                if button == 1 && n_press == 1 {
                    if let Some(ref block) = s.hovered_code_block {
                        let cell_w = s.cell_width;
                        let cell_h = s.cell_height;
                        let scale = da_click.scale_factor() as f64;
                        let btn_size = (24.0 * scale).max(24.0);
                        let btn_x = (block.right_col as f64 + 1.0) * cell_w - btn_size;
                        let btn_y = block.top_row as f64 * cell_h;
                        if x >= btn_x
                            && x <= btn_x + btn_size
                            && y >= btn_y
                            && y <= btn_y + btn_size
                        {
                            let screen = s.terminal.screen();
                            let content = code_block::extract_content(screen, block);
                            if !content.is_empty() {
                                let display = da_click.display();
                                display.clipboard().set_text(&content);
                                s.copy_confirmed_until =
                                    Some(Instant::now() + Duration::from_millis(1500));
                                drop(s);
                                da_click.queue_draw();
                            }
                            return;
                        }
                    }
                }

                if button != 1 {
                    return;
                }

                let shift_held = modifier.contains(gdk::ModifierType::SHIFT_MASK);
                let use_selection = !s.terminal.is_mouse_tracking() || shift_held;

                if button == 1 && use_selection {
                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                    let (screen_row, col) =
                        pixel_to_cell(x, y, s.cell_width, s.cell_height, s.cols, s.rows);
                    let vp_offset = s.viewport_offset;
                    let abs_row = screen_row + vp_offset as usize;

                    match n_press {
                        1 => {
                            if shift_held {
                                if let Some(ref mut sel) = s.selection {
                                    sel.update(abs_row, col);
                                    s.selecting = false;
                                    drop(s);
                                    da_click.queue_draw();
                                    return;
                                }
                            }
                            s.selection = None;
                            s.selecting = true;
                            s.word_anchor = None;
                            s.drag_origin = Some((abs_row, col));
                        }
                        2 => {
                            let screen = s.terminal.screen();
                            let (word_start, word_end) =
                                find_word_boundaries(screen, screen_row, col);
                            let mut sel = Selection::new(abs_row, word_start, SelectionMode::Word);
                            sel.update(abs_row, word_end);
                            s.selection = Some(sel);
                            s.selecting = true;
                            s.word_anchor = Some((word_start, word_end));
                        }
                        3 => {
                            let screen = s.terminal.screen();
                            let last_col = last_non_whitespace_col(screen, screen_row);
                            let mut sel = Selection::new(abs_row, 0, SelectionMode::Line);
                            sel.update(abs_row, last_col);
                            s.selection = Some(sel);
                            s.selecting = false;
                            s.word_anchor = None;
                        }
                        _ => {}
                    }
                    drop(s);
                    da_click.queue_draw();
                    return;
                }

                let terminal_handle = s.terminal.raw_handle();
                let screen_size =
                    ((s.cols as f64 * s.cell_width) as u32, (s.rows as f64 * s.cell_height) as u32);
                let cell_size = (s.cell_width as u32, s.cell_height as u32);

                if let Some(bytes) = s.input.encode_mouse_button(
                    button,
                    true,
                    (x, y),
                    modifier,
                    terminal_handle,
                    screen_size,
                    cell_size,
                ) {
                    if let (Some(ref dc), Some(pid)) = (s.daemon_client.clone(), s.daemon_pane_id) {
                        let _ = dc.send_input(pid, &bytes);
                    }
                    da_click.queue_draw();
                }
            });
        }

        {
            let state = Rc::clone(&state);
            let da_release = drawing_area.clone();
            gesture.connect_released(move |gesture, _n_press, x, y| {
                let button = gesture.current_button();
                let modifier = gesture.current_event_state();
                let Ok(mut s) = state.try_borrow_mut() else {
                    return;
                };

                if button == 1 && s.selecting {
                    s.selecting = false;
                    s.word_anchor = None;
                    s.drag_origin = None;

                    if let Some(ref sel) = s.selection {
                        if sel.is_empty() && sel.mode == SelectionMode::Normal {
                            s.selection = None;
                        }
                    }

                    drop(s);
                    da_release.queue_draw();
                    return;
                }

                let terminal_handle = s.terminal.raw_handle();
                let screen_size =
                    ((s.cols as f64 * s.cell_width) as u32, (s.rows as f64 * s.cell_height) as u32);
                let cell_size = (s.cell_width as u32, s.cell_height as u32);

                if let Some(bytes) = s.input.encode_mouse_button(
                    button,
                    false,
                    (x, y),
                    modifier,
                    terminal_handle,
                    screen_size,
                    cell_size,
                ) {
                    if let (Some(ref dc), Some(pid)) = (s.daemon_client.clone(), s.daemon_pane_id) {
                        let _ = dc.send_input(pid, &bytes);
                    }
                    da_release.queue_draw();
                }
            });
        }

        drawing_area.add_controller(gesture);
    }

    // --- Motion controller ---
    {
        let motion_controller = gtk4::EventControllerMotion::new();

        {
            let state = Rc::clone(&state);
            let da_motion = drawing_area.clone();
            motion_controller.connect_motion(move |controller, x, y| {
                let modifier = controller.current_event_state();
                let Ok(mut s) = state.try_borrow_mut() else {
                    return;
                };

                // If we are actively dragging a selection, update the endpoint
                if s.selecting {
                    // Auto-scroll when dragging past viewport edges
                    let viewport_height = s.rows as f64 * s.cell_height;
                    if y < 0.0 {
                        s.terminal.scroll_viewport_delta(-3);
                        let (_, off, _) = s.terminal.scrollbar_state();
                        s.viewport_offset = off;
                    } else if y > viewport_height {
                        s.terminal.scroll_viewport_delta(3);
                        let (_, off, _) = s.terminal.scrollbar_state();
                        s.viewport_offset = off;
                    }

                    let (screen_row, col) =
                        pixel_to_cell(x, y, s.cell_width, s.cell_height, s.cols, s.rows);

                    let vp_offset = s.viewport_offset;
                    let abs_row = screen_row + vp_offset as usize;

                    if s.selection.is_none() {
                        if let Some((origin_row, origin_col)) = s.drag_origin.take() {
                            s.selection =
                                Some(Selection::new(origin_row, origin_col, SelectionMode::Normal));
                        }
                    }

                    let sel_mode = s.selection.as_ref().map(|sel| sel.mode);
                    let word_anchor = s.word_anchor;
                    let anchor_row = s.selection.as_ref().map(|sel| sel.start.0);

                    let word_bounds = if sel_mode == Some(SelectionMode::Word) {
                        let screen = s.terminal.screen();
                        Some(find_word_boundaries(screen, screen_row, col))
                    } else {
                        None
                    };

                    if let Some(ref mut sel) = s.selection {
                        match sel.mode {
                            SelectionMode::Word => {
                                let (drag_word_start, drag_word_end) =
                                    word_bounds.unwrap_or((col, col));

                                if let (Some((anchor_start, anchor_end)), Some(a_row)) =
                                    (word_anchor, anchor_row)
                                {
                                    if abs_row < a_row
                                        || (abs_row == a_row && drag_word_start < anchor_start)
                                    {
                                        sel.start = (a_row, anchor_end);
                                        sel.end = (abs_row, drag_word_start);
                                    } else {
                                        sel.start = (a_row, anchor_start);
                                        sel.end = (abs_row, drag_word_end);
                                    }
                                } else {
                                    sel.update(abs_row, drag_word_end);
                                }
                            }
                            _ => {
                                sel.update(abs_row, col);
                            }
                        }
                    }

                    drop(s);
                    da_motion.queue_draw();
                    return;
                }

                // Forward motion to mouse encoder if not selecting
                let terminal_handle = s.terminal.raw_handle();
                let screen_size =
                    ((s.cols as f64 * s.cell_width) as u32, (s.rows as f64 * s.cell_height) as u32);
                let cell_size = (s.cell_width as u32, s.cell_height as u32);

                if let Some(bytes) = s.input.encode_mouse_motion(
                    (x, y),
                    modifier,
                    terminal_handle,
                    screen_size,
                    cell_size,
                ) {
                    if let (Some(ref dc), Some(pid)) = (s.daemon_client.clone(), s.daemon_pane_id) {
                        let _ = dc.send_input(pid, &bytes);
                    }
                    da_motion.queue_draw();
                }

                // --- Hover URL detection ---
                let (screen_row, col) =
                    pixel_to_cell(x, y, s.cell_width, s.cell_height, s.cols, s.rows);
                if s.last_hover_cell == (screen_row, col) {
                    return;
                }
                s.last_hover_cell = (screen_row, col);
                let screen = s.terminal.screen();
                let new_hover = detect_url_at(screen, screen_row, col);
                let url_changed = s.hover_url != new_hover;
                if url_changed {
                    let want_pointer = new_hover.is_some();
                    s.hover_url = new_hover;
                    if want_pointer {
                        da_motion.set_cursor_from_name(Some("pointer"));
                    } else {
                        da_motion.set_cursor_from_name(Some("text"));
                    }
                }

                // --- Code block hover detection (T-032) ---
                let mut block_changed = false;
                if !s.terminal.is_mouse_tracking() && !s.terminal.is_alternate_screen() {
                    let gen = s.terminal.screen().generation();
                    if gen != s.code_blocks_generation {
                        let screen = s.terminal.screen();
                        s.detected_code_blocks = code_block::detect_code_blocks(screen);
                        s.code_blocks_generation = gen;
                    }
                    if s.last_hover_block_cell != (screen_row, col) {
                        s.last_hover_block_cell = (screen_row, col);
                        let new_block = s
                            .detected_code_blocks
                            .iter()
                            .find(|b| b.contains(screen_row, col))
                            .cloned();
                        if s.hovered_code_block != new_block {
                            s.hovered_code_block = new_block;
                            block_changed = true;
                        }
                    }
                } else if s.hovered_code_block.is_some() {
                    s.hovered_code_block = None;
                    block_changed = true;
                }

                if url_changed || block_changed {
                    drop(s);
                    da_motion.queue_draw();
                }
            });
        }

        {
            let state = Rc::clone(&state);
            let da_leave = drawing_area.clone();
            motion_controller.connect_leave(move |_controller| {
                let Ok(mut s) = state.try_borrow_mut() else {
                    return;
                };
                let had_url = s.hover_url.is_some();
                let had_block = s.hovered_code_block.is_some();
                if had_url {
                    s.hover_url = None;
                }
                if had_block {
                    s.hovered_code_block = None;
                    s.last_hover_block_cell = (usize::MAX, usize::MAX);
                }
                if had_url || had_block {
                    drop(s);
                    da_leave.set_cursor_from_name(Some("text"));
                    da_leave.queue_draw();
                }
            });
        }

        drawing_area.add_controller(motion_controller);
    }

    // --- Scroll controller ---
    {
        let scroll_controller = gtk4::EventControllerScroll::new(
            gtk4::EventControllerScrollFlags::VERTICAL | gtk4::EventControllerScrollFlags::DISCRETE,
        );

        {
            let state = Rc::clone(&state);
            let da_scroll = drawing_area.clone();
            scroll_controller.connect_scroll(move |controller, _dx, dy| {
                let modifier = controller.current_event_state();
                let Ok(mut s) = state.try_borrow_mut() else {
                    return glib::Propagation::Proceed;
                };

                let terminal_handle = s.terminal.raw_handle();
                let mouse_tracking = s.terminal.is_mouse_tracking();
                let screen_size =
                    ((s.cols as f64 * s.cell_width) as u32, (s.rows as f64 * s.cell_height) as u32);
                let cell_size = (s.cell_width as u32, s.cell_height as u32);

                let position = (0.0, 0.0);

                let action = s.input.encode_scroll(
                    dy,
                    position,
                    modifier,
                    terminal_handle,
                    mouse_tracking,
                    screen_size,
                    cell_size,
                );

                match action {
                    ScrollAction::WriteBytes(bytes) => {
                        if let (Some(ref dc), Some(pid)) =
                            (s.daemon_client.clone(), s.daemon_pane_id)
                        {
                            let _ = dc.send_input(pid, &bytes);
                        }
                    }
                    ScrollAction::ScrollViewport(delta) => {
                        s.terminal.scroll_viewport_delta(delta);
                        let (total, off, len) = s.terminal.scrollbar_state();
                        s.viewport_offset = off;
                        s.scroll_lock = !(total <= len || off + len >= total);
                    }
                }

                s.hover_url = None;
                s.last_hover_cell = (usize::MAX, usize::MAX);
                s.hovered_code_block = None;
                s.last_hover_block_cell = (usize::MAX, usize::MAX);

                drop(s);
                da_scroll.set_cursor_from_name(Some("text"));
                da_scroll.queue_draw();
                glib::Propagation::Stop
            });
        }

        drawing_area.add_controller(scroll_controller);
    }

    // --- Resize handler ---
    {
        let state = Rc::clone(&state);
        let cell_measured_resize = Rc::clone(&cell_measured);
        drawing_area.connect_resize(move |da, width, height| {
            if !*cell_measured_resize.borrow() {
                return;
            }

            let Ok(s) = state.try_borrow() else {
                return;
            };
            let (cw, ch) = (s.cell_width, s.cell_height);
            drop(s);

            if cw < 1.0 || ch < 1.0 {
                return;
            }

            let new_cols = ((width as f64) / cw).max(1.0) as usize;
            let new_rows = ((height as f64) / ch).max(1.0) as usize;

            let Ok(mut s) = state.try_borrow_mut() else {
                return;
            };
            if new_cols != s.cols || new_rows != s.rows {
                s.cols = new_cols;
                s.rows = new_rows;
                s.terminal.resize(new_rows, new_cols);
                let (_, off, _) = s.terminal.scrollbar_state();
                s.viewport_offset = off;
                if let (Some(ref dc), Some(pane_id)) = (s.daemon_client.clone(), s.daemon_pane_id) {
                    let _ = dc.resize_pane(pane_id, new_rows as u16, new_cols as u16);
                }
                drop(s);
                da.queue_draw();
            }
        });
    }

    // --- Scrollbar ---
    let adjustment = gtk4::Adjustment::new(0.0, 0.0, 0.0, 1.0, 10.0, 0.0);
    let scrollbar = gtk4::Scrollbar::new(gtk4::Orientation::Vertical, Some(&adjustment));
    scrollbar.set_vexpand(true);
    scrollbar.set_visible(false);

    {
        let state = Rc::clone(&state);
        let da_scroll = drawing_area.clone();
        adjustment.connect_value_changed(move |adj| {
            let Ok(mut s) = state.try_borrow_mut() else {
                return;
            };
            if s.updating_scrollbar {
                return;
            }
            let new_offset = adj.value() as i64;
            let (_total, current_offset, _len) = s.terminal.scrollbar_state();
            let delta = new_offset - current_offset as i64;
            if delta != 0 {
                s.terminal.scroll_viewport_delta(delta as isize);
                s.viewport_offset = new_offset as u64;
                drop(s);
                da_scroll.queue_draw();
            }
        });
    }

    {
        let state = Rc::clone(&state);
        let adj = adjustment.clone();
        let sb_widget = scrollbar.clone();
        let da_weak = drawing_area.downgrade();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(_da) = da_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Ok(mut s) = state.try_borrow_mut() else {
                return glib::ControlFlow::Continue;
            };
            let (total, offset, len) = s.terminal.scrollbar_state();
            s.viewport_offset = offset;
            let has_scrollback = total > len;
            sb_widget.set_visible(has_scrollback);
            if has_scrollback {
                s.updating_scrollbar = true;
                adj.set_lower(0.0);
                adj.set_upper(total as f64);
                adj.set_page_size(len as f64);
                adj.set_value(offset as f64);
                s.updating_scrollbar = false;
            }
            glib::ControlFlow::Continue
        });
    }

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    hbox.set_hexpand(true);
    hbox.set_vexpand(true);
    hbox.append(&drawing_area);
    hbox.append(&scrollbar);

    // --- Search bar ---
    let search_entry = gtk4::SearchEntry::new();
    search_entry.set_hexpand(true);
    search_entry.set_placeholder_text(Some("Search..."));

    let match_label = gtk4::Label::new(Some(""));
    match_label.add_css_class("dim-label");

    let search_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    search_box.append(&search_entry);
    search_box.append(&match_label);
    search_box.set_margin_start(6);
    search_box.set_margin_end(6);

    let search_bar = gtk4::SearchBar::new();
    search_bar.set_child(Some(&search_box));
    search_bar.connect_entry(&search_entry);
    search_bar.set_show_close_button(false);
    search_bar.set_search_mode(false);

    {
        let state = Rc::clone(&state);
        let da_search = drawing_area.clone();
        let ml = match_label.clone();
        search_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_string();
            let Ok(mut s) = state.try_borrow_mut() else {
                return;
            };
            s.search.query = query.to_lowercase();
            recompute_all_search_matches(&mut s);
            update_match_label(&s.search, &ml);
            drop(s);
            da_search.queue_draw();
        });
    }

    // --- Wire Enter (activate) for next-match navigation ---
    {
        let state = Rc::clone(&state);
        let da_nav = drawing_area.clone();
        let ml = match_label.clone();
        search_entry.connect_activate(move |_entry| {
            let Ok(mut s) = state.try_borrow_mut() else {
                return;
            };
            if s.search.query.is_empty() {
                return;
            }
            navigate_search_forward(&mut s);
            update_match_label(&s.search, &ml);
            drop(s);
            da_nav.queue_draw();
        });
    }

    // --- Wire Shift+Enter for previous-match and Escape for close ---
    {
        let key_controller = gtk4::EventControllerKey::new();
        let state = Rc::clone(&state);
        let da_key = drawing_area.clone();
        let ml = match_label.clone();
        let sb = search_bar.clone();
        key_controller.connect_key_pressed(move |_ctrl, keyval, _keycode, modifier| {
            // Shift+Enter: navigate to previous match
            if keyval == gdk::Key::Return && modifier.contains(gdk::ModifierType::SHIFT_MASK) {
                let Ok(mut s) = state.try_borrow_mut() else {
                    return glib::Propagation::Stop;
                };
                if s.search.query.is_empty() {
                    return glib::Propagation::Stop;
                }
                navigate_search_backward(&mut s);
                update_match_label(&s.search, &ml);
                drop(s);
                da_key.queue_draw();
                return glib::Propagation::Stop;
            }

            // Escape: close search bar, clear highlights, return focus to terminal
            if keyval == gdk::Key::Escape {
                let Ok(mut s) = state.try_borrow_mut() else {
                    return glib::Propagation::Stop;
                };
                s.search.active = false;
                s.search.query.clear();
                s.search.all_matches.clear();
                s.search.matches.clear();
                s.search.current_index = 0;
                drop(s);
                sb.set_search_mode(false);
                da_key.grab_focus();
                da_key.queue_draw();
                return glib::Propagation::Stop;
            }

            glib::Propagation::Proceed
        });
        search_entry.add_controller(key_controller);
    }

    search_bar.set_widget_name("forgetty-search-bar");

    // --- Context menu ---
    let context_popover = gtk4::Popover::new();
    context_popover.set_parent(&drawing_area);
    context_popover.set_has_arrow(false);
    context_popover.set_halign(gtk4::Align::Start);
    context_popover.add_css_class("menu");
    context_popover.set_widget_name("forgetty-context-menu");

    // --- Vertical container ---
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    vbox.set_hexpand(true);
    vbox.set_vexpand(true);
    vbox.append(&search_bar);
    vbox.append(&hbox);

    Ok((vbox, drawing_area, state))
}

/// Helper: convert a `Color` to `(r, g, b)` f64 values (0.0..1.0), using
/// the given default `Rgba` for `Color::Default`.
#[inline]
fn color_to_rgb(color: &Color, default: &Rgba) -> (f64, f64, f64) {
    match color {
        Color::Rgb(r, g, b) => (*r as f64 / 255.0, *g as f64 / 255.0, *b as f64 / 255.0),
        Color::Default => {
            (default.r as f64 / 255.0, default.g as f64 / 255.0, default.b as f64 / 255.0)
        }
    }
}

/// Check if a key combination is an app-level shortcut that should NOT be
/// consumed by the terminal's key encoder. These shortcuts must propagate
/// to GTK accelerators (defined in app.rs) instead.
fn is_app_shortcut(keyval: gdk::Key, modifier: gdk::ModifierType) -> bool {
    // Mask to only the modifier bits we care about (ignore NumLock, etc.)
    let mods = modifier
        & (gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::ALT_MASK);

    let alt_shift = gdk::ModifierType::ALT_MASK | gdk::ModifierType::SHIFT_MASK;
    let ctrl_shift = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;

    // Split right: Alt+Shift+= (may arrive as keyval `equal` or `plus`)
    if mods == alt_shift && (keyval == gdk::Key::equal || keyval == gdk::Key::plus) {
        return true;
    }
    // Split down: Alt+Shift+- (may arrive as keyval `minus` or `underscore`)
    if mods == alt_shift && (keyval == gdk::Key::minus || keyval == gdk::Key::underscore) {
        return true;
    }
    // Close pane: Ctrl+Shift+W
    if mods == ctrl_shift && (keyval == gdk::Key::w || keyval == gdk::Key::W) {
        return true;
    }
    // Copy selection: Ctrl+Shift+C (Ctrl+C is handled after state borrow — only when selection exists)
    if mods == ctrl_shift && (keyval == gdk::Key::c || keyval == gdk::Key::C) {
        return true;
    }
    // Search in terminal: Ctrl+Shift+F
    if mods == ctrl_shift && (keyval == gdk::Key::f || keyval == gdk::Key::F) {
        return true;
    }
    // Paste: Ctrl+V or Ctrl+Shift+V (always intercept — paste beats ^V literal-next)
    if (mods == gdk::ModifierType::CONTROL_MASK || mods == ctrl_shift)
        && (keyval == gdk::Key::v || keyval == gdk::Key::V)
    {
        return true;
    }
    // Pane navigation: Alt+Arrow
    if mods == gdk::ModifierType::ALT_MASK
        && (keyval == gdk::Key::Left
            || keyval == gdk::Key::Right
            || keyval == gdk::Key::Up
            || keyval == gdk::Key::Down)
    {
        return true;
    }

    // Zoom in: Ctrl+= or Ctrl++ (Ctrl+Shift+=)
    if mods == gdk::ModifierType::CONTROL_MASK
        && (keyval == gdk::Key::equal || keyval == gdk::Key::plus)
    {
        return true;
    }
    // Ctrl+Shift+= also arrives with SHIFT set; match that too
    if mods == ctrl_shift && (keyval == gdk::Key::equal || keyval == gdk::Key::plus) {
        return true;
    }
    // Zoom out: Ctrl+-
    if mods == gdk::ModifierType::CONTROL_MASK && keyval == gdk::Key::minus {
        return true;
    }
    // Zoom reset: Ctrl+0
    if mods == gdk::ModifierType::CONTROL_MASK && keyval == gdk::Key::_0 {
        return true;
    }

    // Appearance sidebar: Ctrl+,
    if mods == gdk::ModifierType::CONTROL_MASK && keyval == gdk::Key::comma {
        return true;
    }

    // Shortcuts window: F1 (no modifiers)
    if mods.is_empty() && keyval == gdk::Key::F1 {
        return true;
    }
    // Shortcuts window: Ctrl+? (Ctrl+Shift+/ on US keyboards)
    // GTK may report keyval as `question` or `slash` with SHIFT modifier.
    if mods == ctrl_shift && (keyval == gdk::Key::question || keyval == gdk::Key::slash) {
        return true;
    }

    let ctrl_alt = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK;

    // Workspace switching: Ctrl+Alt+1 through Ctrl+Alt+9
    if mods == ctrl_alt
        && (keyval == gdk::Key::_1
            || keyval == gdk::Key::_2
            || keyval == gdk::Key::_3
            || keyval == gdk::Key::_4
            || keyval == gdk::Key::_5
            || keyval == gdk::Key::_6
            || keyval == gdk::Key::_7
            || keyval == gdk::Key::_8
            || keyval == gdk::Key::_9)
    {
        return true;
    }
    // New workspace: Ctrl+Alt+N
    if mods == ctrl_alt && (keyval == gdk::Key::n || keyval == gdk::Key::N) {
        return true;
    }
    // Workspace selector: Ctrl+Alt+W
    if mods == ctrl_alt && (keyval == gdk::Key::w || keyval == gdk::Key::W) {
        return true;
    }
    // Previous/Next workspace: Ctrl+Alt+PageUp/PageDown
    if mods == ctrl_alt && (keyval == gdk::Key::Page_Up || keyval == gdk::Key::Page_Down) {
        return true;
    }

    false
}

/// Build the context menu as a vertical `gtk4::Box` with button items.
///
/// Uses a plain Box (not gio::Menu / PopoverMenu) to avoid GTK4's internal
/// ScrolledWindow that constrains height and adds unwanted scrollbars.
///
/// Items:
/// 1. Copy (sensitive only when text is selected), Paste
/// 2. Separator
/// 3. Select All, Search
/// 4. (Optional) Separator + Open URL
///
/// Each button activates the corresponding `win.*` action and closes the popover.
fn build_context_menu_box(
    popover: &gtk4::Popover,
    url: Option<&str>,
    has_selection: bool,
) -> gtk4::Box {
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    // --- Copy ---
    let copy_btn = make_menu_button("Copy", Some("Ctrl+C"), "win.copy", None, popover);
    copy_btn.set_sensitive(has_selection);
    vbox.append(&copy_btn);

    // --- Paste ---
    let paste_btn = make_menu_button("Paste", Some("Ctrl+V"), "win.paste", None, popover);
    vbox.append(&paste_btn);

    // --- Separator ---
    let sep1 = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    vbox.append(&sep1);

    // --- Select All ---
    let select_all_btn = make_menu_button("Select All", None, "win.select-all", None, popover);
    vbox.append(&select_all_btn);

    // --- Search ---
    let search_btn = make_menu_button("Search", Some("Shift+Ctrl+F"), "win.search", None, popover);
    vbox.append(&search_btn);

    // --- Open URL (conditional) ---
    if let Some(url_str) = url {
        let sep2 = gtk4::Separator::new(gtk4::Orientation::Horizontal);
        vbox.append(&sep2);

        let label = format!("Open {}", url_str);
        let url_btn =
            make_menu_button(&label, None, "win.open-url", Some(&url_str.to_variant()), popover);
        vbox.append(&url_btn);
    }

    vbox
}

/// Create a single flat button for the context menu.
///
/// The button contains a horizontal box with the label on the left and an
/// optional dimmed shortcut hint on the right. Clicking the button activates
/// the given action on the window and closes the popover.
fn make_menu_button(
    label_text: &str,
    shortcut: Option<&str>,
    action_name: &str,
    action_target: Option<&glib::Variant>,
    popover: &gtk4::Popover,
) -> gtk4::Button {
    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);

    let label = gtk4::Label::new(Some(label_text));
    label.set_halign(gtk4::Align::Start);
    label.set_hexpand(true);
    hbox.append(&label);

    if let Some(hint) = shortcut {
        let hint_label = gtk4::Label::new(Some(hint));
        hint_label.set_halign(gtk4::Align::End);
        hint_label.add_css_class("dim-label");
        hbox.append(&hint_label);
    }

    let btn = gtk4::Button::new();
    btn.set_child(Some(&hbox));
    btn.set_has_frame(false);
    btn.add_css_class("flat");

    // On click: activate the action, then close the popover.
    let action_name = action_name.to_string();
    let action_target = action_target.cloned();
    let popover = popover.clone();
    btn.connect_clicked(move |widget| {
        widget.activate_action(&action_name, action_target.as_ref()).ok();
        popover.popdown();
    });

    btn
}

/// Detect a URL at the given cell position by scanning the row text.
///
/// Looks for `http://` or `https://` URLs in the row containing the click.
/// If the clicked column falls within a URL match, returns a `HoverUrl`
/// with the URL string and its column span.
fn detect_url_at(screen: &forgetty_vt::Screen, screen_row: usize, col: usize) -> Option<HoverUrl> {
    if screen_row >= screen.rows() {
        return None;
    }

    let cells = screen.row(screen_row);
    let num_cols = screen.cols().min(cells.len());

    // Build the line text and track byte-to-col and col-to-byte mappings
    let mut line = String::new();
    let mut col_to_byte_start: Vec<usize> = Vec::with_capacity(num_cols);
    for cell in cells.iter().take(num_cols) {
        col_to_byte_start.push(line.len());
        let g = &cell.grapheme;
        if g.is_empty() {
            line.push(' ');
        } else {
            line.push_str(g);
        }
    }

    // URL-breaking characters (stop the URL match when encountered)
    let url_break_chars: &[char] =
        &[' ', '\t', '"', '\'', '<', '>', '{', '}', '|', '\\', '^', '`', '[', ']'];

    // Scan for URLs in the line
    let mut search_start = 0;
    while let Some(pos) = line[search_start..].find("http") {
        let abs_pos = search_start + pos;

        // Check for http:// or https://
        let rest = &line[abs_pos..];
        let scheme_ok = rest.starts_with("https://") || rest.starts_with("http://");
        if !scheme_ok {
            search_start = abs_pos + 1;
            continue;
        }

        // Find the end of the URL (stop at whitespace or URL-breaking chars)
        let url_end =
            abs_pos + rest.find(|c: char| url_break_chars.contains(&c)).unwrap_or(rest.len());

        // Strip trailing punctuation that is unlikely part of the URL
        let mut end = url_end;
        while end > abs_pos {
            let last_char = line.as_bytes()[end - 1];
            if matches!(last_char, b'.' | b',' | b';' | b':' | b')' | b']') {
                end -= 1;
            } else {
                break;
            }
        }

        let url = &line[abs_pos..end];

        // Find the column range that corresponds to [abs_pos..end)
        let url_col_start =
            col_to_byte_start.iter().position(|&b| b >= abs_pos).unwrap_or(num_cols);
        let url_col_end = col_to_byte_start.iter().position(|&b| b >= end).unwrap_or(num_cols);

        if col >= url_col_start && col < url_col_end {
            return Some(HoverUrl {
                url: url.to_string(),
                screen_row,
                col_start: url_col_start,
                col_end: url_col_end,
            });
        }

        search_start = end;
    }

    None
}

/// Find the context Popover attached to a DrawingArea.
///
/// Searches the DrawingArea's children for a Popover with the expected
/// widget name.
fn find_context_popover(da: &DrawingArea) -> Option<gtk4::Popover> {
    // Popover set_parent attaches it as a child widget
    let mut child = da.first_child();
    while let Some(c) = child {
        if let Some(popover) = c.downcast_ref::<gtk4::Popover>() {
            if popover.widget_name().as_str() == "forgetty-context-menu" {
                return Some(popover.clone());
            }
        }
        child = c.next_sibling();
    }
    None
}

/// Select all visible text in the viewport (for the "Select All" context menu action).
///
/// Creates a selection covering row 0 to `rows-1` in the current viewport,
/// using absolute row coordinates for proper integration with the selection system.
pub fn select_all_visible(da: &DrawingArea, state: &Rc<RefCell<TerminalState>>) {
    let Ok(mut s) = state.try_borrow_mut() else {
        return;
    };

    let rows = s.rows;
    let cols = s.cols;
    let vp_offset = s.viewport_offset as usize;

    if rows == 0 || cols == 0 {
        return;
    }

    // Find the last non-whitespace column on the last row for a clean selection
    let screen = s.terminal.screen();
    let last_row = rows.saturating_sub(1);
    let last_col = last_non_whitespace_col(screen, last_row).max(cols.saturating_sub(1));

    let abs_start = vp_offset;
    let abs_end = vp_offset + last_row;

    let mut sel = Selection::new(abs_start, 0, SelectionMode::Normal);
    sel.update(abs_end, last_col);
    s.selection = Some(sel);
    s.selecting = false;
    s.word_anchor = None;

    drop(s);
    da.queue_draw();
}

/// Toggle the search bar for a pane identified by its TerminalState.
///
/// Called from app.rs when the `win.search` action fires (Ctrl+Shift+F).
/// Finds the SearchBar in the widget tree above the DrawingArea and toggles
/// search mode. When opening, focuses the SearchEntry. When closing, clears
/// state and returns focus to the DrawingArea.
pub fn toggle_search(da: &DrawingArea, state: &Rc<RefCell<TerminalState>>) {
    // Walk up from DrawingArea -> hbox -> vbox to find the SearchBar
    let Some(hbox_widget) = da.parent() else {
        return;
    };
    let Some(vbox_widget) = hbox_widget.parent() else {
        return;
    };
    let Some(vbox) = vbox_widget.downcast_ref::<gtk4::Box>() else {
        return;
    };

    // The SearchBar is the first child of the vbox
    let Some(first_child) = vbox.first_child() else {
        return;
    };
    let Some(search_bar) = first_child.downcast_ref::<gtk4::SearchBar>() else {
        return;
    };

    let is_active = search_bar.is_search_mode();

    if is_active {
        // Close search
        if let Ok(mut s) = state.try_borrow_mut() {
            s.search.active = false;
            s.search.query.clear();
            s.search.all_matches.clear();
            s.search.matches.clear();
            s.search.current_index = 0;
        }
        search_bar.set_search_mode(false);
        da.grab_focus();
        da.queue_draw();
    } else {
        // Open search
        if let Ok(mut s) = state.try_borrow_mut() {
            s.search.active = true;
        }
        search_bar.set_search_mode(true);
        // Find the SearchEntry inside the SearchBar and focus it
        if let Some(sb_child) = search_bar.child() {
            if let Some(sb_box) = sb_child.downcast_ref::<gtk4::Box>() {
                if let Some(entry_widget) = sb_box.first_child() {
                    entry_widget.grab_focus();
                }
            }
        }
        da.queue_draw();
    }
}

/// Recalculate cell dimensions and grid size after a font size change.
///
/// Called after modifying `state.font_size`. Measures the new cell size
/// via Pango, updates `cell_width`/`cell_height`, recalculates cols/rows
/// from the current widget pixel size, and resizes the VT terminal.
/// The daemon PTY is resized via `dc.resize_pane` from the `connect_resize`
/// handler in `create_terminal`.
pub fn apply_font_zoom(state: &mut TerminalState, da: &DrawingArea) {
    let font_desc = font_description_with_size(&state.config, state.font_size);
    let pango_ctx = da.pango_context();
    let (cw, ch) = measure_cell(&pango_ctx, &font_desc);
    if cw < 1.0 || ch < 1.0 {
        return;
    }
    state.cell_width = cw;
    state.cell_height = ch;

    let width = da.width();
    let height = da.height();
    let new_cols = ((width as f64) / cw).max(1.0) as usize;
    let new_rows = ((height as f64) / ch).max(1.0) as usize;

    state.cols = new_cols;
    state.rows = new_rows;
    state.terminal.resize(new_rows, new_cols);
    state.suppress_selection_clear_ticks = 12;

    // Recompute search highlights so they render at the new cell dimensions.
    if !state.search.query.is_empty() {
        recompute_all_search_matches(state);
    }
}

/// Apply a config change to a live terminal pane.
///
/// Compares the new config against the pane's current config and applies
/// any differences:
/// - **Font family/size:** Updates font metrics, cell dimensions, grid size,
///   and PTY dimensions (programs see SIGWINCH). Resets the zoom delta.
/// - **Theme colors:** Updates the theme so the next `draw_terminal()` frame
///   uses the new colors.
/// - **Bell mode:** Updates the bell mode for immediate effect on next BEL.
///
/// Follows the same pattern as `apply_font_zoom()` for font changes.
pub fn apply_config_change(state: &mut TerminalState, new_config: &Config, da: &DrawingArea) {
    let font_changed = state.config.font_family != new_config.font_family
        || (state.config.font_size - new_config.font_size).abs() > f32::EPSILON;

    // Always update theme -- it's cheap and the next draw picks it up.
    state.config.theme = new_config.theme.clone();
    // Propagate the new ANSI palette to the VT so that palette color indices
    // 0–15 resolve via the new theme on the next sync_screen (AC-07).
    state.terminal.set_ansi_palette(new_config.theme.ansi_colors);

    // Update bell mode -- takes effect on next BEL event.
    state.config.bell_mode = new_config.bell_mode;

    // Update notification mode -- takes effect on next OSC/BEL notification.
    state.config.notification_mode = new_config.notification_mode;

    // Update font family (needed even if size didn't change, for font_description_with_size).
    state.config.font_family = new_config.font_family.clone();

    if font_changed {
        // Reset zoom: the user explicitly changed their config, so the zoom
        // delta is cleared. Ctrl+0 now resets to the new config value.
        state.config.font_size = new_config.font_size;
        state.font_size = new_config.font_size;
        state.default_font_size = new_config.font_size;

        apply_font_zoom(state, da);
    }

    da.queue_draw();
}

/// Characters that act as word delimiters for double-click word selection.
const WORD_DELIMITERS: &[char] =
    &[' ', '\t', '"', '\'', '`', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';', '|', ':'];

/// Convert pixel coordinates to cell coordinates, clamped to the grid.
fn pixel_to_cell(
    x: f64,
    y: f64,
    cell_width: f64,
    cell_height: f64,
    cols: usize,
    rows: usize,
) -> (usize, usize) {
    let col = (x / cell_width).floor().max(0.0) as usize;
    let row = (y / cell_height).floor().max(0.0) as usize;
    (row.min(rows.saturating_sub(1)), col.min(cols.saturating_sub(1)))
}

/// Find the word boundaries around a cell position.
///
/// Expands outward from (row, col) until hitting a delimiter character.
/// Returns (start_col, end_col) of the word.
fn find_word_boundaries(screen: &forgetty_vt::Screen, row: usize, col: usize) -> (usize, usize) {
    let num_cols = screen.cols();
    if row >= screen.rows() || col >= num_cols {
        return (col, col);
    }

    let cells = screen.row(row);

    // Check if the clicked cell itself is a delimiter
    let clicked_grapheme = &cells[col].grapheme;
    if clicked_grapheme.len() == 1
        && WORD_DELIMITERS.contains(&clicked_grapheme.chars().next().unwrap_or(' '))
    {
        return (col, col);
    }

    // Expand left
    let mut start = col;
    while start > 0 {
        let g = &cells[start - 1].grapheme;
        if g.is_empty()
            || (g.len() == 1 && WORD_DELIMITERS.contains(&g.chars().next().unwrap_or(' ')))
        {
            break;
        }
        start -= 1;
    }

    // Expand right
    let mut end = col;
    while end + 1 < num_cols.min(cells.len()) {
        let g = &cells[end + 1].grapheme;
        if g.is_empty()
            || (g.len() == 1 && WORD_DELIMITERS.contains(&g.chars().next().unwrap_or(' ')))
        {
            break;
        }
        end += 1;
    }

    (start, end)
}

/// Find the last non-whitespace column in a row (for line selection).
fn last_non_whitespace_col(screen: &forgetty_vt::Screen, row: usize) -> usize {
    if row >= screen.rows() {
        return 0;
    }
    let cells = screen.row(row);
    let mut last = 0;
    for (i, cell) in cells.iter().enumerate() {
        let g = &cell.grapheme;
        if !g.is_empty() && g != " " {
            last = i;
        }
    }
    last
}

/// Scan a single viewport page and collect matches with the given row offset.
///
/// Reads the current screen (after caller has scrolled to the desired position
/// and ensured `screen()` is up-to-date) and appends `(abs_row, start_col, match_len)`
/// to `out` for every case-insensitive occurrence of `query`.
fn collect_matches_from_viewport(
    screen: &forgetty_vt::screen::Screen,
    query: &str,
    row_offset: usize,
    rows: usize,
    cols: usize,
    out: &mut Vec<(usize, usize, usize)>,
) {
    let num_rows = screen.rows().min(rows);
    let num_cols = screen.cols().min(cols);

    for row in 0..num_rows {
        let cells = screen.row(row);
        // Build the lowercased line string and a mapping from byte offset
        // in the lowercased string to column index.
        let mut line_lower = String::new();
        let mut byte_to_col: Vec<usize> = Vec::new();
        for (col, cell) in cells.iter().enumerate().take(num_cols.min(cells.len())) {
            let g = &cell.grapheme;
            let start_byte = line_lower.len();
            if g.is_empty() {
                line_lower.push(' ');
            } else {
                for ch in g.chars() {
                    for lc in ch.to_lowercase() {
                        line_lower.push(lc);
                    }
                }
            }
            for _ in start_byte..line_lower.len() {
                byte_to_col.push(col);
            }
        }

        let mut search_start = 0;
        while let Some(byte_pos) = line_lower[search_start..].find(query) {
            let abs_byte_pos = search_start + byte_pos;
            let end_byte_pos = abs_byte_pos + query.len();

            if abs_byte_pos < byte_to_col.len() && end_byte_pos > 0 {
                let start_col = byte_to_col[abs_byte_pos];
                let end_col = if end_byte_pos <= byte_to_col.len() {
                    byte_to_col[end_byte_pos.saturating_sub(1)]
                } else {
                    byte_to_col[byte_to_col.len() - 1]
                };
                let match_len = end_col - start_col + 1;
                let abs_row = row_offset + row;
                out.push((abs_row, start_col, match_len));
            }

            search_start = abs_byte_pos + 1;
            if search_start >= line_lower.len() {
                break;
            }
        }
    }
}

/// Scan the entire scrollback and visible area to find ALL matches.
///
/// Temporarily scrolls the viewport page-by-page from the top of scrollback
/// to the bottom, collecting matches with absolute row positions, then restores
/// the viewport to the original offset. The result is stored in
/// `s.search.all_matches` sorted by absolute row (natural scan order).
fn recompute_all_search_matches(s: &mut TerminalState) {
    s.search.all_matches.clear();
    s.search.matches.clear();

    if s.search.query.is_empty() {
        s.search.current_index = 0;
        return;
    }

    let query = s.search.query.clone();
    let rows = s.rows;
    let cols = s.cols;

    // Save the current viewport offset so we can restore it after scanning.
    let (total, original_offset, len) = s.terminal.scrollbar_state();
    if total == 0 || len == 0 {
        return;
    }

    let page_size = len as usize;
    let max_offset = if total > len { (total - len) as usize } else { 0 };

    // Scroll to the very top of scrollback.
    if original_offset > 0 {
        s.terminal.scroll_viewport_delta(-(original_offset as isize));
    }

    let mut all = Vec::new();
    let mut current_offset: usize = 0;

    loop {
        // Read screen at the current position.
        let screen = s.terminal.screen();
        collect_matches_from_viewport(screen, &query, current_offset, rows, cols, &mut all);

        // Are we at the bottom?
        if current_offset >= max_offset {
            break;
        }

        // Advance by one page (but don't overshoot past the bottom).
        let remaining = max_offset - current_offset;
        let step = page_size.min(remaining);
        if step == 0 {
            break;
        }
        s.terminal.scroll_viewport_delta(step as isize);
        let (_, new_off, _) = s.terminal.scrollbar_state();
        current_offset = new_off as usize;
    }

    // Deduplicate: since pages can overlap (page_size > step at the end),
    // the same absolute row may be scanned twice. Remove duplicates while
    // preserving sort order.
    all.sort_by_key(|&(abs_row, col, _)| (abs_row, col));
    all.dedup();

    // Restore the original viewport offset.
    let (_, after_offset, _) = s.terminal.scrollbar_state();
    let restore_delta = original_offset as isize - after_offset as isize;
    if restore_delta != 0 {
        s.terminal.scroll_viewport_delta(restore_delta);
    }
    let (_, off, _) = s.terminal.scrollbar_state();
    s.viewport_offset = off;

    s.search.all_matches = all;

    // Clamp current_index to valid range.
    if s.search.all_matches.is_empty() || s.search.current_index >= s.search.all_matches.len() {
        s.search.current_index = 0;
    }

    // Compute the viewport-relative matches for drawing.
    filter_viewport_matches(s);
}

/// Filter `all_matches` to the current viewport, producing `matches`
/// with screen-relative row coordinates for the drawing pass.
/// Also computes `current_viewport_index` so the drawing pass can
/// identify which viewport match (if any) is the focused one.
fn filter_viewport_matches(s: &mut TerminalState) {
    s.search.matches.clear();
    s.search.current_viewport_index = None;

    let (_, offset, len) = s.terminal.scrollbar_state();
    let vp_start = offset as usize;
    let vp_end = vp_start + len as usize; // exclusive

    // The currently focused match in all_matches (for comparison).
    let current_abs = if !s.search.all_matches.is_empty()
        && s.search.current_index < s.search.all_matches.len()
    {
        Some(s.search.all_matches[s.search.current_index])
    } else {
        None
    };

    for (global_idx, &(abs_row, col, match_len)) in s.search.all_matches.iter().enumerate() {
        if abs_row >= vp_start && abs_row < vp_end {
            let screen_row = abs_row - vp_start;
            let vp_idx = s.search.matches.len();
            s.search.matches.push((screen_row, col, match_len));

            // Check if this global match is the focused one.
            if global_idx == s.search.current_index {
                if let Some(cur) = current_abs {
                    if cur == (abs_row, col, match_len) {
                        s.search.current_viewport_index = Some(vp_idx);
                    }
                }
            }
        }
    }
}

/// Update the match count label text (e.g., "3 of 15" or "0 of 0").
///
/// Uses `all_matches` for the total count so the label reflects every match
/// across the entire scrollback, not just the visible viewport.
fn update_match_label(search: &SearchState, label: &gtk4::Label) {
    if search.query.is_empty() || search.all_matches.is_empty() {
        label.set_text("0 of 0");
    } else {
        label.set_text(&format!("{} of {}", search.current_index + 1, search.all_matches.len()));
    }
}

/// Scroll the viewport so the currently focused search match is visible.
///
/// Reads the absolute row from `all_matches[current_index]` and scrolls the
/// viewport so that row is roughly at 1/3 from the top of the screen.
// Why: viewport_rows > 0 is caller-enforced; saturating-sub semantics would hide the invariant.
#[allow(clippy::implicit_saturating_sub)]
fn scroll_to_current_match(s: &mut TerminalState) {
    if s.search.all_matches.is_empty() {
        return;
    }

    let (abs_row, _, _) = s.search.all_matches[s.search.current_index];

    // Get current viewport state.
    let (total, offset, len) = s.terminal.scrollbar_state();
    let viewport_rows = len as usize;

    // Target: place the match row at roughly 1/3 from the top.
    let target_offset = if abs_row > viewport_rows / 3 { abs_row - viewport_rows / 3 } else { 0 };

    // Clamp to valid range.
    let max_offset = if total > len { (total - len) as usize } else { 0 };
    let target_offset = target_offset.min(max_offset);

    let delta = target_offset as isize - offset as isize;
    if delta != 0 {
        s.terminal.scroll_viewport_delta(delta);
        let (_, off, _) = s.terminal.scrollbar_state();
        s.viewport_offset = off;
    }

    // Refresh viewport-relative matches after scrolling.
    filter_viewport_matches(s);
}

/// Navigate to the next search match (Enter in search bar).
///
/// Advances `current_index` in `all_matches` (wrapping around to the first
/// match at the end) and scrolls the viewport to show it.
fn navigate_search_forward(s: &mut TerminalState) {
    if s.search.query.is_empty() || s.search.all_matches.is_empty() {
        return;
    }

    // Advance index, wrapping around.
    if s.search.current_index + 1 < s.search.all_matches.len() {
        s.search.current_index += 1;
    } else {
        s.search.current_index = 0; // wrap to first match
    }

    scroll_to_current_match(s);
}

/// Navigate to the previous search match (Shift+Enter in search bar).
///
/// Decrements `current_index` in `all_matches` (wrapping around to the last
/// match at the beginning) and scrolls the viewport to show it.
fn navigate_search_backward(s: &mut TerminalState) {
    if s.search.query.is_empty() || s.search.all_matches.is_empty() {
        return;
    }

    // Decrement index, wrapping around.
    if s.search.current_index > 0 {
        s.search.current_index -= 1;
    } else {
        s.search.current_index = s.search.all_matches.len() - 1; // wrap to last match
    }

    scroll_to_current_match(s);
}

/// The main draw function called by GTK on every frame.
fn draw_terminal(
    da: &DrawingArea,
    ctx: &cairo::Context,
    width: i32,
    height: i32,
    state: &Rc<RefCell<TerminalState>>,
    cell_measured: &Rc<RefCell<bool>>,
) {
    let Ok(mut s) = state.try_borrow_mut() else {
        // Another callback holds the borrow -- skip this frame.
        // The next queue_draw() will catch up.
        return;
    };

    // Clone theme colors up front to avoid borrow conflicts
    let bg_color = s.config.theme.background;
    let fg_color = s.config.theme.foreground;
    let theme_cursor_color = s.config.theme.cursor;
    let selection_color = s.config.theme.selection;
    let search_match_color = s.config.theme.search_match;
    let search_current_color = s.config.theme.search_current;

    // Read cursor visual style from the render state (DECSCUSR).
    // Map the FFI enum int to CursorStyle; unknown values default to Block.
    let cursor_style = match s.terminal.cursor_visual_style() {
        0 => CursorStyle::Bar,         // BAR
        1 => CursorStyle::Block,       // BLOCK
        2 => CursorStyle::Underline,   // UNDERLINE
        3 => CursorStyle::BlockHollow, // BLOCK_HOLLOW
        _ => CursorStyle::Block,       // fallback for unknown values
    };

    // Blink logic: cursor_blinking() defaults to true (we feed DECSCUSR 1
    // at terminal creation). Apps can disable via steady DECSCUSR (2/4/6).
    // Unfocused panes hide the cursor entirely (see draw_cursor below).
    let cursor_blinking = s.terminal.cursor_blinking();
    let pane_has_focus = da.has_focus();
    let cursor_blink_visible = s.cursor_blink_visible;

    // Prefer terminal-provided cursor color (OSC 12) over theme default.
    let cursor_color = match s.terminal.cursor_color() {
        Some((r, g, b)) => Rgba { r, g, b, a: 255 },
        None => theme_cursor_color,
    };

    // Clone selection state for rendering
    let selection = s.selection.clone();

    // Clone hover URL state for rendering
    let hover_url = s.hover_url.clone();

    // Clone code block hover state for rendering
    let hovered_code_block = s.hovered_code_block.clone();
    let copy_confirmed_until = s.copy_confirmed_until;

    // Filter all_matches to the current viewport for drawing.
    // The full scrollback scan (all_matches) is done once when the query
    // changes.  Here we just recompute the viewport-relative subset so
    // highlights track scrolling, new PTY output, etc.
    if s.search.active && !s.search.query.is_empty() {
        filter_viewport_matches(&mut s);
    }
    // Clone search state for rendering
    let search = s.search.clone();

    // Query viewport offset for converting absolute selection rows to screen rows.
    // Selection coordinates are stored as absolute scrollback positions; we need
    // the viewport offset to map them back to screen-space for drawing.
    let viewport_offset = s.viewport_offset as usize;

    // Build font description using the current zoom level
    let font_desc = font_description_with_size(&s.config, s.font_size);

    // Measure cell dimensions from the actual Pango context if not yet done
    if !*cell_measured.borrow() {
        let pango_ctx = da.pango_context();
        let (cw, ch) = measure_cell(&pango_ctx, &font_desc);
        if cw > 0.0 && ch > 0.0 {
            s.cell_width = cw;
            s.cell_height = ch;
            *cell_measured.borrow_mut() = true;

            // Recalculate grid dimensions
            let new_cols = ((width as f64) / cw).max(1.0) as usize;
            let new_rows = ((height as f64) / ch).max(1.0) as usize;
            if new_cols != s.cols || new_rows != s.rows {
                s.cols = new_cols;
                s.rows = new_rows;
                s.terminal.resize(new_rows, new_cols);
                // Notify the daemon so the PTY gets the real size.
                // The connect_resize handler skips until cell_measured is true, so
                // this is the only place the initial size reaches the daemon PTY.
                if let (Some(ref dc), Some(pane_id)) = (s.daemon_client.clone(), s.daemon_pane_id) {
                    let _ = dc.resize_pane(pane_id, new_rows as u16, new_cols as u16);
                }
            }
        }
    }

    let cell_w = s.cell_width;
    let cell_h = s.cell_height;

    // 1. Fill entire area with theme background
    ctx.set_source_rgb(
        bg_color.r as f64 / 255.0,
        bg_color.g as f64 / 255.0,
        bg_color.b as f64 / 255.0,
    );
    ctx.paint().ok();

    // 2. Get screen state (calls render_state_update internally)
    let screen = s.terminal.screen();
    let (cursor_row, cursor_col) = s.terminal.cursor();
    let cursor_visible = s.terminal.cursor_visible();

    let pango_ctx = da.pango_context();
    let num_rows = screen.rows().min(s.rows);
    let num_cols = screen.cols().min(s.cols);

    // 3. Draw cells
    //
    // Reuse a single Pango layout and pre-built font variants to avoid
    // allocating a new layout + cloning fonts for every cell.  With an
    // 80×24 grid that is ~1,920 cells per frame — per pane.  Concurrent
    // panes multiply this, so reuse is critical for responsiveness.
    let layout = pango::Layout::new(&pango_ctx);
    let mut font_bold = font_desc.clone();
    font_bold.set_weight(pango::Weight::Bold);
    let mut font_italic = font_desc.clone();
    font_italic.set_style(pango::Style::Italic);
    let mut font_bold_italic = font_desc.clone();
    font_bold_italic.set_weight(pango::Weight::Bold);
    font_bold_italic.set_style(pango::Style::Italic);

    for row in 0..num_rows {
        let y = row as f64 * cell_h;
        let cells = screen.row(row);
        let num = num_cols.min(cells.len());

        // --- Pass 1: backgrounds ---
        //
        // Group consecutive cells that share the same explicit background color
        // into a single wide rectangle and draw it with ONE fill() call.
        //
        // Per-cell fill() creates a visible grid-line artifact: Cairo composites
        // each rectangle independently, and at fractional cell widths the
        // sub-pixel edges receive slightly different anti-aliasing coverage,
        // leaving a faint 1-px seam between adjacent cells.  A single fill()
        // over the merged rectangle makes those edges interior — no anti-aliasing
        // applied, no seam.
        {
            let mut col = 0;
            while col < num {
                if let Color::Rgb(r, g, b) = cells[col].attrs.bg {
                    let run_start = col;
                    col += 1;
                    while col < num {
                        match cells[col].attrs.bg {
                            Color::Rgb(rr, gg, bb) if rr == r && gg == g && bb == b => {
                                col += 1;
                            }
                            _ => break,
                        }
                    }
                    ctx.set_source_rgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
                    ctx.rectangle(
                        run_start as f64 * cell_w,
                        y,
                        (col - run_start) as f64 * cell_w,
                        cell_h,
                    );
                    ctx.fill().ok();
                } else {
                    col += 1;
                }
            }
        }

        // --- Pass 2: foreground (text, underline, strikethrough) ---
        //
        // Full-block characters (█ U+2588) are merged into runs and drawn as
        // filled rectangles — identical technique to Pass 1 — to avoid
        // per-glyph sub-pixel anti-aliasing seams at cell boundaries.
        {
            let mut col = 0;
            while col < num {
                let cell = &cells[col];
                let grapheme = &cell.grapheme;

                // Skip empty/space cells — background already drawn in pass 1
                if grapheme == " " || grapheme.is_empty() {
                    col += 1;
                    continue;
                }

                // Full-block character: merge consecutive same-color run into a
                // single rectangle so Cairo treats shared edges as interior —
                // no anti-aliasing applied, no seam.
                if grapheme == "█" {
                    let fg_val = cell.attrs.fg;
                    let dim = cell.attrs.dim;
                    let run_start = col;
                    col += 1;
                    while col < num {
                        let next = &cells[col];
                        if next.grapheme == "█" && next.attrs.fg == fg_val && next.attrs.dim == dim
                        {
                            col += 1;
                        } else {
                            break;
                        }
                    }
                    let (fr, fg_g, fb) = color_to_rgb(&fg_val, &fg_color);
                    let (fr, fg_g, fb) =
                        if dim { (fr * 0.5, fg_g * 0.5, fb * 0.5) } else { (fr, fg_g, fb) };
                    ctx.set_source_rgb(fr, fg_g, fb);
                    ctx.rectangle(
                        run_start as f64 * cell_w,
                        y,
                        (col - run_start) as f64 * cell_w,
                        cell_h,
                    );
                    ctx.fill().ok();
                    continue;
                }

                // Regular character: render as glyph
                let x = col as f64 * cell_w;

                // Compute foreground color
                let (fr, fg_g, fb) = color_to_rgb(&cell.attrs.fg, &fg_color);

                // Apply dim attribute (reduce intensity by 50%)
                let (fr, fg_g, fb) =
                    if cell.attrs.dim { (fr * 0.5, fg_g * 0.5, fb * 0.5) } else { (fr, fg_g, fb) };

                ctx.set_source_rgb(fr, fg_g, fb);

                // Pick the pre-built font variant for this cell
                let cell_font = match (cell.attrs.bold, cell.attrs.italic) {
                    (true, true) => &font_bold_italic,
                    (true, false) => &font_bold,
                    (false, true) => &font_italic,
                    (false, false) => &font_desc,
                };

                layout.set_font_description(Some(cell_font));
                layout.set_text(grapheme);

                // Render text
                ctx.move_to(x, y);
                pangocairo::functions::show_layout(ctx, &layout);

                // Underline
                if cell.attrs.underline {
                    ctx.set_line_width(1.0);
                    ctx.move_to(x, y + cell_h - 1.0);
                    ctx.line_to(x + cell_w, y + cell_h - 1.0);
                    ctx.stroke().ok();
                }

                // Strikethrough
                if cell.attrs.strikethrough {
                    ctx.set_line_width(1.0);
                    ctx.move_to(x, y + cell_h / 2.0);
                    ctx.line_to(x + cell_w, y + cell_h / 2.0);
                    ctx.stroke().ok();
                }

                col += 1;
            }
        }
    }

    // 3b. Draw hover URL underline (AC-1, AC-5, AC-6)
    if let Some(ref hu) = hover_url {
        if hu.screen_row < num_rows {
            let y = hu.screen_row as f64 * cell_h;
            let cells = screen.row(hu.screen_row);
            for col in hu.col_start..hu.col_end.min(num_cols) {
                // Skip cells that already have SGR underline (AC-6)
                if col < cells.len() && cells[col].attrs.underline {
                    continue;
                }
                let (fr, fg_g, fb) = if col < cells.len() {
                    color_to_rgb(&cells[col].attrs.fg, &fg_color)
                } else {
                    color_to_rgb(&Color::Default, &fg_color)
                };
                ctx.set_source_rgb(fr, fg_g, fb);
                ctx.set_line_width(1.0);
                let x = col as f64 * cell_w;
                ctx.move_to(x, y + cell_h - 1.0);
                ctx.line_to(x + cell_w, y + cell_h - 1.0);
                ctx.stroke().ok();
            }
        }
    }

    // 4. Draw cursor
    //
    // Skip drawing when:
    //   - pane is unfocused (hide cursor entirely in inactive panes)
    //   - cursor is hidden by the terminal (DECTCEM off)
    //   - cursor is in the blink "off" phase and the terminal requests blinking
    let draw_cursor = pane_has_focus
        && cursor_visible
        && cursor_row < num_rows
        && cursor_col < num_cols
        && (!cursor_blinking || cursor_blink_visible);

    if draw_cursor {
        let cx = cursor_col as f64 * cell_w;
        let cy = cursor_row as f64 * cell_h;

        ctx.set_source_rgb(
            cursor_color.r as f64 / 255.0,
            cursor_color.g as f64 / 255.0,
            cursor_color.b as f64 / 255.0,
        );

        match cursor_style {
            CursorStyle::Block => {
                // Fill the cursor cell
                ctx.rectangle(cx, cy, cell_w, cell_h);
                ctx.fill().ok();

                // Redraw the character in the background color (inverted)
                if cursor_row < screen.rows() && cursor_col < screen.cols() {
                    let cell = screen.cell(cursor_row, cursor_col);
                    let grapheme = &cell.grapheme;
                    if grapheme != " " && !grapheme.is_empty() {
                        ctx.set_source_rgb(
                            bg_color.r as f64 / 255.0,
                            bg_color.g as f64 / 255.0,
                            bg_color.b as f64 / 255.0,
                        );
                        let layout = pango::Layout::new(&pango_ctx);
                        layout.set_font_description(Some(&font_desc));
                        layout.set_text(grapheme);
                        ctx.move_to(cx, cy);
                        pangocairo::functions::show_layout(ctx, &layout);
                    }
                }
            }
            CursorStyle::Bar => {
                ctx.set_line_width(2.0);
                ctx.move_to(cx, cy);
                ctx.line_to(cx, cy + cell_h);
                ctx.stroke().ok();
            }
            CursorStyle::Underline => {
                ctx.set_line_width(2.0);
                ctx.move_to(cx, cy + cell_h - 1.0);
                ctx.line_to(cx + cell_w, cy + cell_h - 1.0);
                ctx.stroke().ok();
            }
            CursorStyle::BlockHollow => {
                // Outline only — no fill, character remains in normal color
                ctx.set_line_width(1.0);
                ctx.rectangle(cx + 0.5, cy + 0.5, cell_w - 1.0, cell_h - 1.0);
                ctx.stroke().ok();
            }
        }
    }

    // 5. Draw selection overlay (semi-transparent, preserves underlying text)
    //
    // Selection coordinates are stored as ABSOLUTE scrollback rows.
    // We convert them to screen-relative rows by subtracting viewport_offset.
    // Only draw the portion of the selection that overlaps the current viewport.
    if let Some(ref sel) = selection {
        // iTerm2-format themes (which all our bundled themes use) specify selection
        // as a plain #rrggbb hex — no alpha channel — so Rgba::from_hex returns a=255.
        // Rendering that at full opacity produces an opaque block that hides the text.
        // Treat a=255 as "theme didn't specify opacity" and fall back to 40% opacity.
        // Themes that explicitly set alpha via #rrggbbaa 8-char hex will have a < 255
        // and their value is respected.
        let sel_alpha =
            if selection_color.a == 255 { 0.4 } else { selection_color.a as f64 / 255.0 };
        ctx.set_source_rgba(
            selection_color.r as f64 / 255.0,
            selection_color.g as f64 / 255.0,
            selection_color.b as f64 / 255.0,
            sel_alpha,
        );

        // Determine the absolute row range of the selection
        let ((sr, sc), (er, ec)) = sel.ordered();

        // Viewport shows absolute rows [viewport_offset .. viewport_offset + num_rows - 1].
        // Clamp the selection range to the visible viewport.
        let vp_start = viewport_offset;
        let vp_end = viewport_offset + num_rows.saturating_sub(1);

        // Skip rendering entirely if selection is outside the viewport
        if er >= vp_start && sr <= vp_end {
            let abs_start = sr.max(vp_start);
            let abs_end = er.min(vp_end);

            // Draw one rectangle per row (not per cell) and ONE fill() for all rows.
            //
            // Per-cell fills create a grid-line artifact: each cell rectangle is
            // composited independently, and at fractional cell widths the adjacent
            // sub-pixel edges receive slightly different anti-aliasing coverage,
            // leaving a faint 1px grid between cells.
            //
            // With a single path + single fill(), Cairo fills all rectangles in one
            // pass. Adjacent cell edges within the same row become interior (no
            // anti-aliasing). Adjacent row edges share the exact same y coordinate,
            // giving complementary anti-aliasing coverage that adds to 1.0 — seamless.
            ctx.new_path();
            for abs_row in abs_start..=abs_end {
                let screen_row = abs_row - viewport_offset;
                let sy = screen_row as f64 * cell_h;

                let (row_start_col, row_end_col) = match sel.mode {
                    SelectionMode::Normal | SelectionMode::Word => {
                        let start = if abs_row == sr { sc } else { 0 };
                        let end = if abs_row == er { ec + 1 } else { num_cols };
                        (start, end)
                    }
                    SelectionMode::Line => (0, num_cols),
                    SelectionMode::Block => (sc.min(ec), sc.max(ec) + 1),
                };

                if row_end_col > row_start_col {
                    ctx.rectangle(
                        row_start_col as f64 * cell_w,
                        sy,
                        (row_end_col - row_start_col) as f64 * cell_w,
                        cell_h,
                    );
                }
            }
            ctx.fill().ok();
        }
    }

    // 6. Draw search match highlights (between selection and dim overlays).
    //
    // Non-focused matches get a warm amber overlay; the currently focused match
    // gets a brighter orange overlay so the user can distinguish it.
    if search.active && !search.matches.is_empty() {
        for (idx, &(match_row, match_col, match_len)) in search.matches.iter().enumerate() {
            let is_current = search.current_viewport_index == Some(idx);
            let color = if is_current { &search_current_color } else { &search_match_color };
            ctx.set_source_rgba(
                color.r as f64 / 255.0,
                color.g as f64 / 255.0,
                color.b as f64 / 255.0,
                color.a as f64 / 255.0,
            );

            for c in match_col..match_col + match_len {
                if match_row < num_rows && c < num_cols {
                    let sx = c as f64 * cell_w;
                    let sy = match_row as f64 * cell_h;
                    ctx.rectangle(sx, sy, cell_w, cell_h);
                    ctx.fill().ok();
                }
            }
        }
    }

    // 7. Dim unfocused panes with a semi-transparent overlay (Ghostty style).
    // Focused pane renders at full brightness; unfocused panes get a dark wash.
    if !da.has_focus() {
        ctx.set_source_rgba(0.0, 0.0, 0.0, 0.12);
        ctx.rectangle(0.0, 0.0, width as f64, height as f64);
        ctx.fill().ok();
    }

    // 8. Visual bell flash overlay.
    // While the bell flash deadline is in the future, paint a semi-transparent
    // white wash over the pane. Once the deadline passes, clear the field.
    if let Some(deadline) = s.bell_flash_until {
        if Instant::now() < deadline {
            ctx.set_source_rgba(1.0, 1.0, 1.0, 0.15);
            ctx.rectangle(0.0, 0.0, width as f64, height as f64);
            ctx.fill().ok();
        } else {
            s.bell_flash_until = None;
        }
    }

    // 9. Notification ring -- amber border when this pane has a pending notification.
    // Drawn AFTER step 7 (dim overlay) so it is visible on unfocused panes,
    // and AFTER step 8 (bell flash) so it persists through bell events.
    let has_notification_ring = s.notification_ring;
    if has_notification_ring {
        ctx.set_source_rgba(1.0, 0.78, 0.0, 0.9); // amber
        ctx.set_line_width(3.0);
        ctx.rectangle(1.5, 1.5, width as f64 - 3.0, height as f64 - 3.0);
        ctx.stroke().ok();
    }

    // 10. Code block hover overlay + copy button (T-032).
    //
    // When the mouse hovers over a detected code block (box-drawing bordered
    // region), draw a subtle highlight on the border cells and a copy button
    // in the top-right corner. Clicking the button copies the inner content.
    if let Some(ref block) = hovered_code_block {
        // 10a. Subtle highlight on border cells.
        // Determine overlay color based on theme brightness:
        // dark themes get a white wash, light themes get a dark wash.
        let bg_luma =
            bg_color.r as f64 * 0.299 + bg_color.g as f64 * 0.587 + bg_color.b as f64 * 0.114;
        let is_dark_theme = bg_luma < 128.0;
        if is_dark_theme {
            ctx.set_source_rgba(1.0, 1.0, 1.0, 0.08);
        } else {
            ctx.set_source_rgba(0.0, 0.0, 0.0, 0.06);
        }

        // Top border row
        if block.top_row < num_rows {
            let bx = block.left_col as f64 * cell_w;
            let by = block.top_row as f64 * cell_h;
            let bw = (block.right_col - block.left_col + 1) as f64 * cell_w;
            ctx.rectangle(bx, by, bw, cell_h);
            ctx.fill().ok();
        }
        // Bottom border row
        if block.bottom_row < num_rows {
            let bx = block.left_col as f64 * cell_w;
            let by = block.bottom_row as f64 * cell_h;
            let bw = (block.right_col - block.left_col + 1) as f64 * cell_w;
            ctx.rectangle(bx, by, bw, cell_h);
            ctx.fill().ok();
        }
        // Left border column (excluding corners already drawn)
        for row in (block.top_row + 1)..block.bottom_row {
            if row < num_rows {
                ctx.rectangle(block.left_col as f64 * cell_w, row as f64 * cell_h, cell_w, cell_h);
                ctx.fill().ok();
            }
        }
        // Right border column (excluding corners already drawn)
        for row in (block.top_row + 1)..block.bottom_row {
            if row < num_rows {
                ctx.rectangle(block.right_col as f64 * cell_w, row as f64 * cell_h, cell_w, cell_h);
                ctx.fill().ok();
            }
        }

        // 10b. Copy button in the top-right corner of the block.
        let scale = da.scale_factor() as f64;
        let btn_size = (24.0 * scale).max(24.0);
        let btn_x = (block.right_col as f64 + 1.0) * cell_w - btn_size;
        let btn_y = block.top_row as f64 * cell_h;

        // Button background: rounded rectangle
        let btn_radius = 4.0;
        draw_rounded_rect(ctx, btn_x, btn_y, btn_size, btn_size, btn_radius);
        if is_dark_theme {
            ctx.set_source_rgba(0.0, 0.0, 0.0, 0.6);
        } else {
            ctx.set_source_rgba(1.0, 1.0, 1.0, 0.7);
        }
        ctx.fill().ok();

        // Button icon: clipboard (copy) icon or checkmark (after successful copy)
        let show_checkmark =
            copy_confirmed_until.map(|deadline| Instant::now() < deadline).unwrap_or(false);

        if !show_checkmark {
            // Clear the confirmed state if the deadline has passed
            if copy_confirmed_until.is_some() {
                s.copy_confirmed_until = None;
            }
        }

        let icon_color = if is_dark_theme { (1.0, 1.0, 1.0, 0.9) } else { (0.0, 0.0, 0.0, 0.8) };
        ctx.set_source_rgba(icon_color.0, icon_color.1, icon_color.2, icon_color.3);

        if show_checkmark {
            // Draw a checkmark icon
            let cx = btn_x + btn_size * 0.5;
            let cy = btn_y + btn_size * 0.5;
            let s_icon = btn_size * 0.3;
            ctx.set_line_width(2.0);
            ctx.move_to(cx - s_icon * 0.6, cy);
            ctx.line_to(cx - s_icon * 0.1, cy + s_icon * 0.5);
            ctx.line_to(cx + s_icon * 0.7, cy - s_icon * 0.4);
            ctx.stroke().ok();
        } else {
            // Draw a clipboard/copy icon (two overlapping rectangles)
            let pad = btn_size * 0.22;
            let icon_w = btn_size - pad * 2.0;
            let icon_h = btn_size - pad * 2.0;
            let offset = icon_w * 0.2;

            // Back rectangle (slightly offset)
            ctx.set_line_width(1.5);
            ctx.rectangle(btn_x + pad + offset, btn_y + pad, icon_w - offset, icon_h - offset);
            ctx.stroke().ok();

            // Front rectangle (overlapping)
            // Fill with button background so it looks like it's in front
            draw_rounded_rect(
                ctx,
                btn_x + pad,
                btn_y + pad + offset,
                icon_w - offset,
                icon_h - offset,
                1.5,
            );
            if is_dark_theme {
                ctx.set_source_rgba(0.0, 0.0, 0.0, 0.6);
            } else {
                ctx.set_source_rgba(1.0, 1.0, 1.0, 0.7);
            }
            ctx.fill().ok();

            // Front rectangle stroke
            ctx.set_source_rgba(icon_color.0, icon_color.1, icon_color.2, icon_color.3);
            ctx.set_line_width(1.5);
            ctx.rectangle(btn_x + pad, btn_y + pad + offset, icon_w - offset, icon_h - offset);
            ctx.stroke().ok();
        }
    }
}

/// Draw a rounded rectangle path (does not fill or stroke).
fn draw_rounded_rect(ctx: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    ctx.new_path();
    ctx.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    ctx.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    ctx.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    ctx.arc(x + r, y + r, r, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
    ctx.close_path();
}
