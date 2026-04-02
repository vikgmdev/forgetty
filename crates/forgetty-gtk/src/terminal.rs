//! Terminal grid rendering with Pango + Cairo.
//!
//! Provides `create_terminal()` which returns a `(gtk::Box, DrawingArea, State)`
//! triple: the Box contains the DrawingArea (terminal grid) and a vertical
//! gtk::Scrollbar on the right edge. The DrawingArea renders the terminal grid
//! from `forgetty_vt::Terminal`'s screen state using Cairo for drawing and
//! Pango for text layout.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use libc;

use forgetty_config::{BellMode, Config, CursorStyle, NotificationMode};
use forgetty_core::Rgba;
// NotificationPayload, NotificationSource, DrainResult, scan_osc_notification
// live in forgetty-session (platform-agnostic types moved there in T-048).
use forgetty_session::events::scan_osc_notification;
pub use forgetty_session::{DrainResult, NotificationPayload, NotificationSource};
use forgetty_vt::screen::Color;
use forgetty_vt::selection::{Selection, SelectionMode};
use forgetty_vt::TerminalEvent;
use gtk4::cairo;
use gtk4::gdk;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{glib, DrawingArea};

use crate::daemon_client::DaemonClient;
use crate::input::{GhosttyInput, ScrollAction};
use crate::pty_bridge;

/// Search state for in-terminal text search (Ctrl+Shift+F).
///
/// Tracks the current query, all match positions across the entire scrollback,
/// and the index of the currently focused match for navigation.
#[derive(Debug, Clone)]
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

impl Default for SearchState {
    fn default() -> Self {
        Self {
            active: false,
            query: String::new(),
            all_matches: Vec::new(),
            matches: Vec::new(),
            current_index: 0,
            current_viewport_index: None,
        }
    }
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
    /// Local PTY process. `None` for daemon-backed panes.
    pub pty: Option<forgetty_pty::PtyProcess>,
    pub pty_rx: mpsc::Receiver<Vec<u8>>,
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
    /// For daemon-backed panes: the remote pane ID in the daemon.
    pub daemon_pane_id: Option<forgetty_core::PaneId>,
    /// For daemon-backed panes: handle for routing write-pty responses.
    pub daemon_client: Option<Arc<DaemonClient>>,
    /// For daemon-backed panes: the CWD from `PaneInfo` at connect time.
    /// Used as a fallback tab title until the shell emits OSC 0/2.
    pub daemon_cwd: Option<PathBuf>,
}

impl TerminalState {
    /// Drain pending PTY output from the channel and feed it to the
    /// terminal VT parser.  Caps the amount of data processed per tick
    /// to `MAX_DRAIN_BYTES` so the GTK main thread stays responsive for
    /// input events — especially when multiple panes stream output
    /// concurrently.  Any remaining data stays in the channel and will
    /// be picked up on the next 8ms cycle.
    fn drain_pty_output(&mut self) -> DrainResult {
        const MAX_DRAIN_BYTES: usize = 128 * 1024; // 128 KB per tick
        let mut had_data = false;
        let mut disconnected = false;
        let mut bytes_drained: usize = 0;
        let mut notification: Option<NotificationPayload> = None;
        loop {
            if bytes_drained >= MAX_DRAIN_BYTES {
                break; // yield back to the GTK main loop
            }
            match self.pty_rx.try_recv() {
                Ok(data) => {
                    bytes_drained += data.len();
                    had_data = true;

                    // Scan for OSC notification sequences BEFORE feeding to VT parser.
                    // We only capture the first notification per tick to keep it simple.
                    if notification.is_none() {
                        notification = scan_osc_notification(&data);
                    }

                    self.terminal.feed(&data);

                    // Drain write-PTY responses (DA responses, mode queries, etc.)
                    let responses = self.terminal.drain_write_pty();
                    for chunk in responses {
                        if let Some(ref dc) = self.daemon_client {
                            if let Some(pane_id) = self.daemon_pane_id {
                                let _ = dc.send_input(pane_id, &chunk);
                            }
                        } else if let Some(ref mut pty) = self.pty {
                            if let Err(e) = pty.write(&chunk) {
                                tracing::warn!("Failed to write PTY response: {e}");
                            }
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Channel is empty but the child process may have been
                    // killed externally. In that case orphan children can
                    // keep the PTY slave fd open, so the reader thread never
                    // gets EOF.  Detect this by checking the child status.
                    if self.daemon_pane_id.is_none() {
                        if let Some(ref mut pty) = self.pty {
                            if !pty.is_alive() {
                                disconnected = true;
                            }
                        }
                    }
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        // raw_bytes is not used by the GTK drain path (it feeds directly to
        // self.terminal above). It exists in DrainResult for session consumers.
        DrainResult { had_data, pty_exited: disconnected, notification, raw_bytes: Vec::new() }
    }
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

/// Create the terminal widget and wire up PTY I/O, rendering, input,
/// scrollbar, and resize handling.
///
/// Returns `(hbox, drawing_area, state)` where:
/// - `hbox` is a horizontal `gtk::Box` containing the DrawingArea and a
///   vertical scrollbar on the right edge.
/// - `drawing_area` is the inner `DrawingArea` (needed for `grab_focus()`,
///   widget naming, and focus tracking).
/// - `state` is the shared `TerminalState` wrapped in `Rc<RefCell<>>`.
///
/// `on_exit` is an optional callback invoked (once) when the PTY channel
/// disconnects (shell exited). The callback receives the DrawingArea's
/// widget name so the caller can close the correct pane.
///
/// `on_notify` is an optional callback invoked when a notification fires
/// (OSC 9/99/777 or BEL on an unfocused pane). Wired by `app.rs`.
///
/// `working_dir` and `command` are CLI launch overrides for the initial pane.
/// Pass `None` for both when creating tabs/splits (they use defaults).
pub fn create_terminal(
    config: &Config,
    on_exit: Option<Rc<dyn Fn(String)>>,
    on_notify: Option<Rc<dyn Fn(NotificationPayload)>>,
    working_dir: Option<&Path>,
    command: Option<&[String]>,
) -> Result<(gtk4::Box, DrawingArea, Rc<RefCell<TerminalState>>), String> {
    // Start with a generous over-estimate so the first-draw resize is always
    // a SHRINK (not a GROW).  libghostty-vt grows by adding blank rows at the
    // TOP (cursor maintains distance from bottom), which produces a large blank
    // area above the shell prompt.  Shrinking drops blank rows from the BOTTOM,
    // leaving content at the top of the screen.  These values comfortably cover
    // any realistic monitor+font combination (<80 rows, <240 cols).
    let initial_rows: usize = 80;
    let initial_cols: usize = 240;

    // Spawn PTY bridge
    let shell = config.shell.as_deref();
    let (pty, pty_rx) = pty_bridge::spawn_pty_bridge(
        initial_rows as u16,
        initial_cols as u16,
        shell,
        working_dir,
        command,
    )?;

    // Create terminal VT state
    let mut terminal = forgetty_vt::Terminal::new(initial_rows, initial_cols);

    // Set default cursor to "blinking block" (DECSCUSR 1) so that
    // cursor_blinking() returns true before the shell sends any DECSCUSR.
    // Without this, the render state defaults cursor_blinking to false and
    // the cursor wouldn't blink until an app explicitly requests it.
    terminal.feed(b"\x1b[1 q");

    let input = GhosttyInput::new();

    let state = Rc::new(RefCell::new(TerminalState {
        terminal,
        pty: Some(pty),
        pty_rx,
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
        daemon_pane_id: None,
        daemon_client: None,
        daemon_cwd: None,
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

    // --- Poll PTY data with a GLib timeout (8ms ~ 120Hz) ---
    // Uses a weak reference to the DrawingArea so the timer stops automatically
    // when the tab is closed and the widget is destroyed.
    {
        let state = Rc::clone(&state);
        let da_weak = drawing_area.downgrade();
        // Wrap on_exit in Rc so it can be moved into the closure and called once.
        let on_exit = on_exit.map(|cb| Rc::new(std::cell::Cell::new(Some(cb))));
        glib::timeout_add_local(Duration::from_millis(8), move || {
            // Stop the timer if the DrawingArea has been destroyed (tab closed)
            let Some(da) = da_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };

            let Ok(mut s) = state.try_borrow_mut() else {
                // Another callback holds the borrow -- skip this tick.
                // The PTY data will be picked up on the next 8ms cycle.
                return glib::ControlFlow::Continue;
            };

            // Check if viewport is at the bottom BEFORE draining new data.
            // If the user has manually scrolled up, we should NOT force them
            // back to the bottom when new output arrives.
            let (total, offset, len) = s.terminal.scrollbar_state();
            s.viewport_offset = offset; // keep cache fresh
            let was_at_bottom = total <= len || offset + len >= total;

            let DrainResult { had_data, pty_exited, notification: osc_notification, .. } =
                s.drain_pty_output();

            // Detect alternate screen → primary screen transitions (e.g., htop exit).
            // Re-feed DECSCUSR 1 (blinking block) to restore our blink default,
            // since the alternate screen exit may reset cursor_blinking to false.
            let is_alt = s.terminal.is_alternate_screen();
            if s.was_alternate_screen && !is_alt {
                s.terminal.feed(b"\x1b[1 q");
            }
            s.was_alternate_screen = is_alt;

            // Drain terminal events (bell, title changes) so they don't
            // accumulate unboundedly in the event buffer.
            let events = s.terminal.drain_events();
            let mut bell_notify_payload: Option<NotificationPayload> = None;
            for event in events {
                if let TerminalEvent::Bell = event {
                    let now = Instant::now();
                    // Rate limit: suppress bells within 200ms of the last one.
                    if now.duration_since(s.last_bell) < Duration::from_millis(200) {
                        continue;
                    }
                    // Suppress the shell's BEL response that follows a Ctrl+C press
                    // (zsh beeps on SIGINT, which would show an unwanted visual flash).
                    if s.suppress_bell_until.map_or(false, |t| now < t) {
                        s.suppress_bell_until = None;
                        continue;
                    }
                    s.last_bell = now;

                    match s.config.bell_mode {
                        BellMode::Visual => {
                            s.bell_flash_until = Some(Instant::now() + Duration::from_millis(150));
                        }
                        BellMode::Audio => {
                            da.error_bell();
                        }
                        BellMode::Both => {
                            s.bell_flash_until = Some(Instant::now() + Duration::from_millis(150));
                            da.error_bell();
                        }
                        BellMode::None => {}
                    }

                    // BEL triggers ring + badge but NOT a desktop notification.
                    // Only fire if the pane is not currently focused.
                    if !da.has_focus() && s.config.notification_mode != NotificationMode::None {
                        s.notification_ring = true;
                        bell_notify_payload = Some(NotificationPayload {
                            title: String::new(),
                            body: String::new(),
                            pane_name: da.widget_name().to_string(),
                            source: None, // BEL has no source
                        });
                    }
                }
            }

            // Handle OSC notification detected during drain.
            let osc_notify_payload = if let Some(mut payload) = osc_notification {
                if !da.has_focus() && s.config.notification_mode != NotificationMode::None {
                    s.notification_ring = true;
                    payload.pane_name = da.widget_name().to_string();
                    Some(payload)
                } else {
                    None
                }
            } else {
                None
            };

            // Clone on_notify Rc and rate-limit state before dropping the borrow.
            let on_notify_cb = s.on_notify.clone();
            let last_notif = s.last_notification;
            let can_send_desktop = last_notif.elapsed() >= Duration::from_secs(2);
            if can_send_desktop && (bell_notify_payload.is_some() || osc_notify_payload.is_some()) {
                s.last_notification = Instant::now();
            }

            if had_data {
                // Track when we last received PTY data for idle detection (T-028).
                s.last_pty_data = Instant::now();
                s.malloc_trimmed = false;

                // Only auto-scroll to bottom when the user was already at
                // the bottom. This lets users browse scrollback history
                // during rapid continuous output (e.g., `while true; do date; done`).
                if was_at_bottom {
                    s.terminal.scroll_viewport_bottom();
                    let (_, off, _) = s.terminal.scrollbar_state();
                    s.viewport_offset = off;
                }

                // Clear selection on new output to avoid stale highlights
                // pointing to cells that no longer contain the selected text (AC-17).
                // Skip clearing if a resize just happened — the shell redraws on
                // SIGWINCH but the selected text hasn't actually moved (AC-16).
                if s.suppress_selection_clear_ticks > 0 {
                    s.suppress_selection_clear_ticks -= 1;
                } else if s.selection.is_some() {
                    s.selection = None;
                    s.selecting = false;
                    s.word_anchor = None;
                }

                // Invalidate stale search matches when terminal content changes.
                // Match positions are stored as absolute rows which become wrong
                // after Ctrl+L (clear) or any output that shifts scrollback.
                // Without this, ghost highlight rectangles persist over empty cells.
                if s.search.active && !s.search.all_matches.is_empty() {
                    s.search.all_matches.clear();
                    s.search.matches.clear();
                    s.search.current_index = 0;
                    s.search.current_viewport_index = None;
                }

                // Advance blink phase if needed (data path already triggers redraw)
                let now = Instant::now();
                if now.duration_since(s.last_blink_toggle) >= Duration::from_millis(600) {
                    s.cursor_blink_visible = !s.cursor_blink_visible;
                    s.last_blink_toggle = now;
                }

                drop(s);
                da.queue_draw();
            } else {
                // No PTY data this tick — check cursor blink timer.
                // Toggle blink phase every 600ms when the terminal requests blinking.
                // Only trigger a redraw when the phase actually changes.
                let now = Instant::now();
                let needs_redraw =
                    if now.duration_since(s.last_blink_toggle) >= Duration::from_millis(600) {
                        s.cursor_blink_visible = !s.cursor_blink_visible;
                        s.last_blink_toggle = now;
                        true
                    } else {
                        false
                    };

                // T-028: After 5 seconds of no PTY data, call malloc_trim(0) to
                // return freed heap pages to the OS. The glibc allocator retains
                // freed memory by default, which causes the 39% post-settle RSS
                // bloat found in T-027 benchmarking. This is called once per idle
                // transition, not repeatedly.
                #[cfg(target_os = "linux")]
                if !s.malloc_trimmed
                    && now.duration_since(s.last_pty_data) >= Duration::from_secs(5)
                {
                    s.malloc_trimmed = true;
                    // Safety: malloc_trim is thread-safe and returns 1 if memory
                    // was actually released, 0 otherwise. Called on the GTK main
                    // thread during a confirmed idle period.
                    unsafe {
                        libc::malloc_trim(0);
                    }
                    tracing::debug!("malloc_trim(0) called after 5s idle");
                }

                // Also redraw while the bell flash overlay is active.
                let bell_active = s.bell_flash_until.is_some();
                // Also redraw when the notification ring state changed.
                let ring_changed = bell_notify_payload.is_some() || osc_notify_payload.is_some();
                if needs_redraw || bell_active || ring_changed {
                    drop(s);
                    da.queue_draw();
                }
            }

            // Fire notification callbacks outside the TerminalState borrow.
            // BEL: ring + badge but NO desktop notification.
            if let Some(payload) = bell_notify_payload {
                if let Some(ref cb) = on_notify_cb {
                    cb(payload);
                }
            }
            // OSC: ring + badge + desktop notification (subject to rate limiting).
            if let Some(payload) = osc_notify_payload {
                if let Some(ref cb) = on_notify_cb {
                    // Mark whether the desktop notification is rate-limited.
                    // We already updated last_notification above if can_send_desktop.
                    let mut p = payload;
                    if !can_send_desktop {
                        // Suppress desktop notification by clearing source
                        // (on_notify callback checks source == None to skip it).
                        p.source = None;
                    }
                    cb(p);
                }
            }

            // PTY channel disconnected -- shell process has exited.
            // Schedule the pane close asynchronously via idle callback to avoid
            // reentrancy issues with the timer that detected the exit.
            if pty_exited {
                tracing::debug!("PTY exited for pane {:?}, scheduling close", da.widget_name());
                if let Some(ref exit_cell) = on_exit {
                    if let Some(cb) = exit_cell.take() {
                        let pane_name = da.widget_name().to_string();
                        glib::idle_add_local_once(move || {
                            cb(pane_name);
                        });
                    }
                }
                return glib::ControlFlow::Break;
            }

            glib::ControlFlow::Continue
        });
    }

    // --- Keyboard input (via ghostty key encoder) ---
    {
        let key_controller = gtk4::EventControllerKey::new();

        // key-pressed handler (fires for both initial press and repeat)
        {
            let state = Rc::clone(&state);
            let da_for_key = drawing_area.clone();
            key_controller.connect_key_pressed(move |_controller, keyval, keycode, modifier| {
                // Let app-level shortcuts pass through to GTK accelerators.
                // Without this, the ghostty encoder would consume Alt+Shift+= etc.
                // and return Stop, preventing the accelerator from firing.
                if is_app_shortcut(keyval, modifier) {
                    return glib::Propagation::Proceed;
                }

                let Ok(mut s) = state.try_borrow_mut() else {
                    // Borrow held elsewhere -- drop this key event.
                    return glib::Propagation::Proceed;
                };

                // Ctrl+C: copy if selection exists, otherwise send SIGINT (0x03) directly.
                // We bypass the ghostty encoder for both cases — the encoder may return None
                // for Ctrl+C in some keyboard protocol modes, causing Propagation::Proceed
                // which triggers the GTK system bell and the T-014 visual flash.
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
                        // Write 0x03 for PTY echo + cooked-mode line discipline.
                        if let Some(ref mut pty) = s.pty {
                            pty.write(&[0x03]).ok();
                            // Also send SIGINT directly to the PTY's actual foreground
                            // process group via tcgetpgrp on the master PTY fd. This is
                            // necessary when a child has disabled ISIG (e.g. Node.js /
                            // pm2), preventing the line discipline from converting 0x03
                            // to SIGINT automatically.
                            send_sigint_to_fg_pgrp(pty);
                        }
                        s.cursor_blink_visible = true;
                        s.last_blink_toggle = Instant::now();
                        // Suppress the shell's BEL response (zsh beeps on SIGINT).
                        s.suppress_bell_until = Some(Instant::now() + Duration::from_millis(300));
                    }
                    return glib::Propagation::Stop;
                }

                // Escape clears selection when mouse tracking is off (AC-19).
                // When mouse tracking is on (e.g., vim), Escape should go to the app.
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
                    if let Some(ref mut pty) = s.pty {
                        if let Err(e) = pty.write(&bytes) {
                            tracing::warn!("Failed to write to PTY: {e}");
                        }
                    }
                    // Reset cursor blink on keypress: make cursor solid and
                    // restart the blink countdown (AC-2).
                    s.cursor_blink_visible = true;
                    s.last_blink_toggle = Instant::now();
                    da_for_key.queue_draw();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }

        // key-released handler (needed for Kitty keyboard protocol release events)
        {
            let state = Rc::clone(&state);
            let da_for_release = drawing_area.clone();
            key_controller.connect_key_released(move |_controller, keyval, keycode, modifier| {
                let Ok(mut s) = state.try_borrow_mut() else {
                    // Borrow held elsewhere -- drop this release event.
                    return;
                };
                let terminal_handle = s.terminal.raw_handle();
                if let Some(bytes) =
                    s.input.encode_key_release(keyval, keycode, modifier, terminal_handle)
                {
                    if let Some(ref mut pty) = s.pty {
                        if let Err(e) = pty.write(&bytes) {
                            tracing::warn!("Failed to write to PTY: {e}");
                        }
                    }
                    da_for_release.queue_draw();
                }
            });
        }

        drawing_area.add_controller(key_controller);
    }

    // --- Focus controller (for DECSET 1004 focus reporting) ---
    {
        let focus_controller = gtk4::EventControllerFocus::new();

        // Focus gained
        {
            let state = Rc::clone(&state);
            let da_focus = drawing_area.clone();
            focus_controller.connect_enter(move |_controller| {
                let Ok(mut s) = state.try_borrow_mut() else {
                    return;
                };
                if s.terminal.is_focus_reporting() {
                    if let Some(bytes) = GhosttyInput::encode_focus(true) {
                        if let Some(ref mut pty) = s.pty {
                            if let Err(e) = pty.write(&bytes) {
                                tracing::warn!("Failed to write focus-in to PTY: {e}");
                            }
                        }
                        da_focus.queue_draw();
                    }
                }
            });
        }

        // Focus lost
        {
            let state = Rc::clone(&state);
            let da_focus = drawing_area.clone();
            focus_controller.connect_leave(move |_controller| {
                let Ok(mut s) = state.try_borrow_mut() else {
                    return;
                };
                if s.terminal.is_focus_reporting() {
                    if let Some(bytes) = GhosttyInput::encode_focus(false) {
                        if let Some(ref mut pty) = s.pty {
                            if let Err(e) = pty.write(&bytes) {
                                tracing::warn!("Failed to write focus-out to PTY: {e}");
                            }
                        }
                        da_focus.queue_draw();
                    }
                }
            });
        }

        drawing_area.add_controller(focus_controller);
    }

    // --- Mouse click controller (GestureClick for button press/release) ---
    {
        let gesture = gtk4::GestureClick::new();
        // Respond to all buttons (default is button 1 only).
        gesture.set_button(0);

        // Button pressed
        {
            let state = Rc::clone(&state);
            let da_click = drawing_area.clone();
            gesture.connect_pressed(move |gesture, n_press, x, y| {
                // Clicking on a pane should focus it (for split pane navigation).
                da_click.grab_focus();

                let button = gesture.current_button();
                let modifier = gesture.current_event_state();

                // --- Right-click context menu (AC-1, AC-9, AC-22) ---
                // Button 3 always opens the context menu, even when mouse
                // tracking is active (matches Ghostty behavior).
                // IMPORTANT: do NOT clear the selection on right-click (AC-9).
                if button == 3 {
                    let Ok(s) = state.try_borrow() else {
                        return;
                    };

                    // Detect URL at the click position for conditional menu item
                    let (screen_row, col) =
                        pixel_to_cell(x, y, s.cell_width, s.cell_height, s.cols, s.rows);
                    let screen = s.terminal.screen();
                    let hover = detect_url_at(screen, screen_row, col);
                    let url_str = hover.map(|h| h.url);
                    let has_selection = s.selection.is_some();
                    drop(s);

                    // Find the Popover attached to this DrawingArea
                    if let Some(popover) = find_context_popover(&da_click) {
                        // Build button box fresh (handles dynamic URL + copy sensitivity)
                        let menu_box =
                            build_context_menu_box(&popover, url_str.as_deref(), has_selection);
                        popover.set_child(Some(&menu_box));
                        popover
                            .set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                        popover.popup();
                    }

                    return;
                }

                // --- Ctrl+Click to open URL (AC-7, AC-8) ---
                // Intercept before selection logic. Only when mouse tracking is
                // NOT active (AC-18) and Ctrl is held without Shift.
                if button == 1
                    && modifier.contains(gdk::ModifierType::CONTROL_MASK)
                    && !modifier.contains(gdk::ModifierType::SHIFT_MASK)
                {
                    let Ok(s) = state.try_borrow() else {
                        return;
                    };
                    let mouse_tracking = s.terminal.is_mouse_tracking();
                    if !mouse_tracking {
                        let (screen_row, col) =
                            pixel_to_cell(x, y, s.cell_width, s.cell_height, s.cols, s.rows);
                        let screen = s.terminal.screen();
                        let hover = detect_url_at(screen, screen_row, col);
                        drop(s);
                        if let Some(hu) = hover {
                            crate::app::open_url_in_browser(&hu.url);
                            return;
                        }
                    }
                    // If mouse tracking is active or no URL found, fall through
                }

                let Ok(mut s) = state.try_borrow_mut() else {
                    // Borrow held elsewhere -- drop this click event.
                    return;
                };

                let mouse_tracking = s.terminal.is_mouse_tracking();

                // Shift+Click overrides mouse tracking (AC-15): force selection
                // mode even when an app has mouse tracking enabled.
                let shift_held = modifier.contains(gdk::ModifierType::SHIFT_MASK);
                let use_selection = !mouse_tracking || shift_held;

                if button == 1 && use_selection {
                    // Handle text selection instead of forwarding to app
                    let (screen_row, col) =
                        pixel_to_cell(x, y, s.cell_width, s.cell_height, s.cols, s.rows);

                    // Use cached viewport offset (updated by 16ms timer) to
                    // avoid expensive FFI calls on every mouse event.
                    let vp_offset = s.viewport_offset;
                    let abs_row = screen_row + vp_offset as usize;

                    match n_press {
                        1 => {
                            // Shift+Click extends existing selection to clicked position
                            if shift_held {
                                if let Some(ref mut sel) = s.selection {
                                    sel.update(abs_row, col);
                                    s.selecting = false;
                                    drop(s);
                                    da_click.queue_draw();
                                    return;
                                }
                            }

                            // Single click: defer selection creation until motion is
                            // detected.  This avoids a visible flicker where the
                            // selection overlay renders for one frame on a plain click.
                            s.selection = None; // clear any previous selection (AC-3)
                            s.selecting = true;
                            s.word_anchor = None;
                            s.drag_origin = Some((abs_row, col));
                        }
                        2 => {
                            // Double-click: select word under cursor.
                            // Word boundaries are found on the viewport screen (screen_row),
                            // but stored as absolute coordinates.
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
                            // Triple-click: select entire line.
                            let screen = s.terminal.screen();
                            let last_col = last_non_whitespace_col(screen, screen_row);
                            let mut sel = Selection::new(abs_row, 0, SelectionMode::Line);
                            sel.update(abs_row, last_col);
                            s.selection = Some(sel);
                            s.selecting = false; // Line selection is immediate
                            s.word_anchor = None;
                        }
                        _ => {}
                    }

                    drop(s);
                    da_click.queue_draw();
                    return;
                }

                // Forward to mouse encoder (mouse tracking active)
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
                    if let Some(ref mut pty) = s.pty {
                        if let Err(e) = pty.write(&bytes) {
                            tracing::warn!("Failed to write mouse press to PTY: {e}");
                        }
                    }
                    da_click.queue_draw();
                }
            });
        }

        // Button released
        {
            let state = Rc::clone(&state);
            let da_release = drawing_area.clone();
            gesture.connect_released(move |gesture, _n_press, x, y| {
                let button = gesture.current_button();
                let modifier = gesture.current_event_state();
                let Ok(mut s) = state.try_borrow_mut() else {
                    // Borrow held elsewhere -- drop this release event.
                    return;
                };

                // If we were selecting with button 1, finalize the selection
                if button == 1 && s.selecting {
                    s.selecting = false;
                    s.word_anchor = None;
                    s.drag_origin = None;

                    // If no selection was created (click without drag), nothing to clear
                    // visually — the pressed handler already cleared any previous selection.
                    // If a selection exists but is empty, also clear it.
                    if let Some(ref sel) = s.selection {
                        if sel.is_empty() && sel.mode == SelectionMode::Normal {
                            s.selection = None;
                        }
                    }

                    drop(s);
                    da_release.queue_draw();
                    return;
                }

                // Forward release to mouse encoder
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
                    if let Some(ref mut pty) = s.pty {
                        if let Err(e) = pty.write(&bytes) {
                            tracing::warn!("Failed to write mouse release to PTY: {e}");
                        }
                    }
                    da_release.queue_draw();
                }
            });
        }

        drawing_area.add_controller(gesture);
    }

    // --- Mouse motion controller ---
    {
        let motion_controller = gtk4::EventControllerMotion::new();

        {
            let state = Rc::clone(&state);
            let da_motion = drawing_area.clone();
            motion_controller.connect_motion(move |controller, x, y| {
                let modifier = controller.current_event_state();
                let Ok(mut s) = state.try_borrow_mut() else {
                    // Borrow held elsewhere -- drop this motion event.
                    return;
                };

                // If we are actively dragging a selection, update the endpoint
                if s.selecting {
                    // Auto-scroll when dragging past viewport edges
                    let viewport_height = s.rows as f64 * s.cell_height;
                    if y < 0.0 {
                        // Dragging above the top edge — scroll up
                        s.terminal.scroll_viewport_delta(-3);
                        let (_, off, _) = s.terminal.scrollbar_state();
                        s.viewport_offset = off;
                    } else if y > viewport_height {
                        // Dragging below the bottom edge — scroll down
                        s.terminal.scroll_viewport_delta(3);
                        let (_, off, _) = s.terminal.scrollbar_state();
                        s.viewport_offset = off;
                    }

                    let (screen_row, col) =
                        pixel_to_cell(x, y, s.cell_width, s.cell_height, s.cols, s.rows);

                    // Use cached viewport offset (updated by 16ms timer)
                    let vp_offset = s.viewport_offset;
                    let abs_row = screen_row + vp_offset as usize;

                    // Deferred creation: if no Selection exists yet (single-click
                    // path), create it now that actual drag motion is detected.
                    // drag_origin already stores absolute rows.
                    if s.selection.is_none() {
                        if let Some((origin_row, origin_col)) = s.drag_origin.take() {
                            s.selection =
                                Some(Selection::new(origin_row, origin_col, SelectionMode::Normal));
                        }
                    }

                    // Read the selection mode and word anchor before mutably borrowing selection
                    let sel_mode = s.selection.as_ref().map(|sel| sel.mode);
                    let word_anchor = s.word_anchor;
                    let anchor_row = s.selection.as_ref().map(|sel| sel.start.0);

                    // For word mode, compute word boundaries using screen-relative row
                    // (since find_word_boundaries works on the viewport screen), but
                    // store the result as absolute rows.
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
                                        // Dragging before the anchor word
                                        sel.start = (a_row, anchor_end);
                                        sel.end = (abs_row, drag_word_start);
                                    } else {
                                        // Dragging after the anchor word
                                        sel.start = (a_row, anchor_start);
                                        sel.end = (abs_row, drag_word_end);
                                    }
                                } else {
                                    sel.update(abs_row, drag_word_end);
                                }
                            }
                            _ => {
                                // Normal mode: character-by-character
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
                    if let Some(ref mut pty) = s.pty {
                        if let Err(e) = pty.write(&bytes) {
                            tracing::warn!("Failed to write mouse motion to PTY: {e}");
                        }
                    }
                    da_motion.queue_draw();
                }

                // --- Hover URL detection (AC-1, AC-2, AC-3) ---
                let (screen_row, col) =
                    pixel_to_cell(x, y, s.cell_width, s.cell_height, s.cols, s.rows);
                if s.last_hover_cell == (screen_row, col) {
                    return;
                }
                s.last_hover_cell = (screen_row, col);
                let screen = s.terminal.screen();
                let new_hover = detect_url_at(screen, screen_row, col);
                let changed = s.hover_url != new_hover;
                if changed {
                    let want_pointer = new_hover.is_some();
                    s.hover_url = new_hover;
                    drop(s);
                    if want_pointer {
                        da_motion.set_cursor_from_name(Some("pointer"));
                    } else {
                        da_motion.set_cursor_from_name(Some("text"));
                    }
                    da_motion.queue_draw();
                }
            });
        }

        // Clear hover state when the mouse leaves the DrawingArea (AC-3, AC-19)
        {
            let state = Rc::clone(&state);
            let da_leave = drawing_area.clone();
            motion_controller.connect_leave(move |_controller| {
                let Ok(mut s) = state.try_borrow_mut() else {
                    return;
                };
                if s.hover_url.is_some() {
                    s.hover_url = None;
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
                    // Borrow held elsewhere -- drop this scroll event.
                    return glib::Propagation::Proceed;
                };

                let terminal_handle = s.terminal.raw_handle();
                let mouse_tracking = s.terminal.is_mouse_tracking();
                let screen_size =
                    ((s.cols as f64 * s.cell_width) as u32, (s.rows as f64 * s.cell_height) as u32);
                let cell_size = (s.cell_width as u32, s.cell_height as u32);

                // Get mouse position from the last event on the controller.
                // EventControllerScroll doesn't directly provide position, so we
                // use (0,0) as fallback -- the scroll button/position is rarely
                // critical for applications, and the cell is determined by the
                // encoder's last known position.
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
                        if let Some(ref mut pty) = s.pty {
                            if let Err(e) = pty.write(&bytes) {
                                tracing::warn!("Failed to write scroll to PTY: {e}");
                            }
                        }
                    }
                    ScrollAction::ScrollViewport(delta) => {
                        s.terminal.scroll_viewport_delta(delta);
                        let (_, off, _) = s.terminal.scrollbar_state();
                        s.viewport_offset = off;
                    }
                }

                // Clear stale hover state after scroll (content under cursor shifted)
                s.hover_url = None;
                s.last_hover_cell = (usize::MAX, usize::MAX);

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
                // Borrow held elsewhere -- skip this resize; the next
                // resize or draw will recalculate.
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
                // Refresh cached viewport offset immediately so the draw
                // callback uses the correct value (avoids 1-row selection drift).
                let (_, off, _) = s.terminal.scrollbar_state();
                s.viewport_offset = off;
                // Shell will redraw on SIGWINCH — suppress selection clearing
                // so the selection survives the resize (AC-16).
                // 12 ticks × 8ms = ~100ms grace period covers drag-resize bursts.
                s.suppress_selection_clear_ticks = 12;
                if let Some(ref mut pty) = s.pty {
                    if let Err(e) = pty.resize(forgetty_pty::PtySize {
                        rows: new_rows as u16,
                        cols: new_cols as u16,
                        pixel_width: width as u16,
                        pixel_height: height as u16,
                    }) {
                        tracing::warn!("Failed to resize PTY: {e}");
                    }
                }
                drop(s);
                da.queue_draw();
            }
        });
    }

    // --- Scrollbar (GTK native vertical scrollbar + Adjustment) ---
    let adjustment = gtk4::Adjustment::new(
        0.0,  // value (current position)
        0.0,  // lower bound
        0.0,  // upper bound (total rows -- updated per frame)
        1.0,  // step increment (1 row)
        24.0, // page increment (visible rows, updated per frame)
        24.0, // page size (visible rows, updated per frame)
    );
    let scrollbar = gtk4::Scrollbar::new(gtk4::Orientation::Vertical, Some(&adjustment));
    scrollbar.set_visible(false); // hidden until there is scrollback

    // Wire adjustment value-changed -> scroll the terminal viewport
    {
        let state = Rc::clone(&state);
        let da_scroll = drawing_area.clone();
        adjustment.connect_value_changed(move |adj| {
            let Ok(mut s) = state.try_borrow_mut() else {
                return;
            };
            // Skip if we are programmatically updating the adjustment
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

    // --- Scrollbar update timer (syncs terminal state -> adjustment) ---
    // Runs alongside the existing 8ms PTY poll timer. We piggyback on a
    // separate 16ms timer (~60Hz) to avoid querying scrollbar_state() too
    // frequently (the ghostty docs note it can be expensive).
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

            // Cache viewport offset for mouse handlers (avoids FFI calls per event)
            s.viewport_offset = offset;

            // Update scrollbar visibility: hide when no scrollback
            let has_scrollback = total > len;
            sb_widget.set_visible(has_scrollback);

            if has_scrollback {
                // Set guard to prevent feedback loop
                s.updating_scrollbar = true;
                adj.set_lower(0.0);
                adj.set_upper(total as f64);
                adj.set_page_size(len as f64);
                adj.set_page_increment(len as f64);
                adj.set_value(offset as f64);
                s.updating_scrollbar = false;
            }

            glib::ControlFlow::Continue
        });
    }

    // Package DrawingArea + Scrollbar into a horizontal Box
    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    hbox.set_hexpand(true);
    hbox.set_vexpand(true);
    hbox.append(&drawing_area);
    hbox.append(&scrollbar);

    // --- Search bar (Ctrl+Shift+F) ---
    // A gtk::SearchBar containing a gtk::SearchEntry, placed above the terminal
    // content in a vertical container. Hidden by default; revealed on action.
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
    // SearchBar starts hidden (search_mode = false)
    search_bar.set_search_mode(false);

    // --- Wire search-changed signal (recompute matches as user types) ---
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

    // --- Vertical container: SearchBar on top, HBox (DrawingArea + Scrollbar) below ---
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    vbox.set_hexpand(true);
    vbox.set_vexpand(true);
    vbox.append(&search_bar);
    vbox.append(&hbox);

    // Store search bar reference for the win.search action to toggle
    // We use widget name as a lookup key -- the action in app.rs will
    // find the SearchBar by walking the widget tree.
    search_bar.set_widget_name("forgetty-search-bar");

    // --- Context menu (right-click Popover with manual buttons) ---
    // Use a plain Popover (not PopoverMenu) to avoid GTK4's internal
    // ScrolledWindow that constrains height and adds a scrollbar.
    let context_popover = gtk4::Popover::new();
    context_popover.set_parent(&drawing_area);
    context_popover.set_has_arrow(false);
    context_popover.set_halign(gtk4::Align::Start);
    context_popover.add_css_class("menu");
    context_popover.set_widget_name("forgetty-context-menu");

    Ok((vbox, drawing_area, state))
}

/// Create a daemon-backed terminal widget for an existing daemon pane.
///
/// Like `create_terminal()` but connects to a remote pane managed by
/// `forgetty-daemon` rather than spawning a local PTY. PTY I/O goes through
/// the daemon's JSON-RPC API (`send_input` / `subscribe_output`).
///
/// `daemon_rx` is the receiver end of an mpsc channel whose sender was passed
/// to `DaemonClient::subscribe_output()`. The 8ms poll timer reads bytes from
/// this channel instead of a local PTY reader thread.
///
/// `snapshot` is an optional initial screen state fed to the VT parser to
/// make the first rendered frame show content rather than a blank terminal.
pub fn create_terminal_for_pane(
    config: &Config,
    pane_id: forgetty_core::PaneId,
    daemon_client: Arc<DaemonClient>,
    daemon_rx: mpsc::Receiver<Vec<u8>>,
    snapshot: Option<&crate::daemon_client::ScreenSnapshot>,
    cwd: Option<PathBuf>,
    on_exit: Option<Rc<dyn Fn(String)>>,
    on_notify: Option<Rc<dyn Fn(NotificationPayload)>>,
) -> Result<(gtk4::Box, DrawingArea, Rc<RefCell<TerminalState>>), String> {
    // Over-estimate rows so the first-draw resize is always a SHRINK.
    // libghostty-vt shrinks by trimming trailing blank rows from the BOTTOM;
    // snapshot content is placed at row 1 (top) so the blank rows sit below
    // it and get trimmed cleanly on the first resize.
    // 80 rows covers any realistic monitor+font combination.
    let initial_rows: usize = 80;
    let initial_cols: usize = 240;

    // Create terminal VT state (no local PTY)
    let mut terminal = forgetty_vt::Terminal::new(initial_rows, initial_cols);
    terminal.feed(b"\x1b[1 q");

    // Prime VT state with snapshot lines so the first frame shows content.
    if let Some(snap) = snapshot {
        // Strip leading blank rows.  Blank rows from the daemon are serialized as
        // "" (empty string): handle_get_screen only emits bytes up to the last
        // non-default cell, so an all-blank row produces zero bytes.
        let first_content = snap.lines.iter()
            .position(|l| !l.is_empty())
            .unwrap_or(snap.lines.len().saturating_sub(1)); // keep at least cursor row
        let effective_lines = &snap.lines[first_content..];
        let effective_cursor_row = snap.cursor_row.saturating_sub(first_content);

        // Place content at the TOP of the oversized initial VT (row 1).
        //
        // libghostty-vt (PageList::resizeWithoutReflow) shrinks by calling
        // trimTrailingBlankRows(), which removes blank rows from the BOTTOM of
        // the active area — NOT the top.  Placing content at row 1 means the
        // trailing blank rows sit below it; the first-draw resize trims them
        // cleanly and content stays visible at the top.
        //
        // The prior strategy (place at bottom, start_row = initial_rows - snap_rows + 1)
        // was wrong: with content at the bottom there are no trailing blank rows,
        // nothing gets trimmed, and blank rows above the content are pushed into
        // the visible window instead of history.
        let start_row = 1_usize; // 1-indexed; content always at top
        for (i, line) in effective_lines.iter().enumerate() {
            let row = start_row + i;
            // Explicit CUP per row avoids accidental scrolling at the boundary.
            terminal.feed(format!("\x1b[{row};1H").as_bytes());
            terminal.feed(line.as_bytes());
        }
        // Restore cursor to its position within the effective content slice.
        let cur_row = start_row + effective_cursor_row; // absolute 1-indexed row in oversized VT
        let cur_col = snap.cursor_col + 1;
        terminal.feed(format!("\x1b[{cur_row};{cur_col}H").as_bytes());
    }

    let input = GhosttyInput::new();

    let state = Rc::new(RefCell::new(TerminalState {
        terminal,
        pty: None,
        pty_rx: daemon_rx,
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

    // --- Poll daemon output with a GLib timeout (8ms ~ 120Hz) ---
    {
        let state = Rc::clone(&state);
        let da_weak = drawing_area.downgrade();
        let on_exit = on_exit.map(|cb| Rc::new(std::cell::Cell::new(Some(cb))));
        glib::timeout_add_local(Duration::from_millis(8), move || {
            let Some(da) = da_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };

            let Ok(mut s) = state.try_borrow_mut() else {
                return glib::ControlFlow::Continue;
            };

            let (total, offset, len) = s.terminal.scrollbar_state();
            s.viewport_offset = offset;
            let was_at_bottom = total <= len || offset + len >= total;

            let DrainResult { had_data, pty_exited, notification: osc_notification, .. } =
                s.drain_pty_output();

            let is_alt = s.terminal.is_alternate_screen();
            if s.was_alternate_screen && !is_alt {
                s.terminal.feed(b"\x1b[1 q");
            }
            s.was_alternate_screen = is_alt;

            let events = s.terminal.drain_events();
            let mut bell_notify_payload: Option<NotificationPayload> = None;
            for event in events {
                if let TerminalEvent::Bell = event {
                    let now = Instant::now();
                    if now.duration_since(s.last_bell) < Duration::from_millis(200) {
                        continue;
                    }
                    if s.suppress_bell_until.map_or(false, |t| now < t) {
                        s.suppress_bell_until = None;
                        continue;
                    }
                    s.last_bell = now;

                    match s.config.bell_mode {
                        BellMode::Visual => {
                            s.bell_flash_until = Some(Instant::now() + Duration::from_millis(150));
                        }
                        BellMode::Audio => {
                            da.error_bell();
                        }
                        BellMode::Both => {
                            s.bell_flash_until = Some(Instant::now() + Duration::from_millis(150));
                            da.error_bell();
                        }
                        BellMode::None => {}
                    }

                    if !da.has_focus() && s.config.notification_mode != NotificationMode::None {
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
                if !da.has_focus() && s.config.notification_mode != NotificationMode::None {
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
            if can_send_desktop && (bell_notify_payload.is_some() || osc_notify_payload.is_some()) {
                s.last_notification = Instant::now();
            }

            if had_data {
                s.last_pty_data = Instant::now();
                s.malloc_trimmed = false;

                if was_at_bottom {
                    s.terminal.scroll_viewport_bottom();
                    let (_, off, _) = s.terminal.scrollbar_state();
                    s.viewport_offset = off;
                }

                if s.suppress_selection_clear_ticks > 0 {
                    s.suppress_selection_clear_ticks -= 1;
                } else if s.selection.is_some() {
                    s.selection = None;
                    s.selecting = false;
                    s.word_anchor = None;
                }

                if s.search.active && !s.search.all_matches.is_empty() {
                    s.search.all_matches.clear();
                    s.search.matches.clear();
                    s.search.current_index = 0;
                    s.search.current_viewport_index = None;
                }

                let now = Instant::now();
                if now.duration_since(s.last_blink_toggle) >= Duration::from_millis(600) {
                    s.cursor_blink_visible = !s.cursor_blink_visible;
                    s.last_blink_toggle = now;
                }

                drop(s);
                da.queue_draw();
            } else {
                let now = Instant::now();
                let needs_redraw =
                    if now.duration_since(s.last_blink_toggle) >= Duration::from_millis(600) {
                        s.cursor_blink_visible = !s.cursor_blink_visible;
                        s.last_blink_toggle = now;
                        true
                    } else {
                        false
                    };

                #[cfg(target_os = "linux")]
                if !s.malloc_trimmed
                    && now.duration_since(s.last_pty_data) >= Duration::from_secs(5)
                {
                    s.malloc_trimmed = true;
                    unsafe {
                        libc::malloc_trim(0);
                    }
                    tracing::debug!("malloc_trim(0) called after 5s idle (daemon pane)");
                }

                let bell_active = s.bell_flash_until.is_some();
                let ring_changed = bell_notify_payload.is_some() || osc_notify_payload.is_some();
                if needs_redraw || bell_active || ring_changed {
                    drop(s);
                    da.queue_draw();
                }
            }

            if let Some(payload) = bell_notify_payload {
                if let Some(ref cb) = on_notify_cb {
                    cb(payload);
                }
            }
            if let Some(payload) = osc_notify_payload {
                if let Some(ref cb) = on_notify_cb {
                    let mut p = payload;
                    if !can_send_desktop {
                        p.source = None;
                    }
                    cb(p);
                }
            }

            // Daemon panes don't exit via pty_exited normally, but handle it gracefully.
            if pty_exited {
                tracing::debug!(
                    "Daemon pane channel closed for {:?}, scheduling close",
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
                return glib::ControlFlow::Break;
            }

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
                        if let (Some(ref dc), Some(pid)) = (s.daemon_client.clone(), s.daemon_pane_id) {
                            let _ = dc.send_sigint(pid);
                        }
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
                    s.cursor_blink_visible = true;
                    s.last_blink_toggle = Instant::now();
                    da_for_key.queue_draw();
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
                        popover.set_pointing_to(Some(&gdk::Rectangle::new(
                            x as i32, y as i32, 1, 1,
                        )));
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
                            let mut sel =
                                Selection::new(abs_row, word_start, SelectionMode::Word);
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
                    button, true, (x, y), modifier, terminal_handle, screen_size, cell_size,
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
                    button, false, (x, y), modifier, terminal_handle, screen_size, cell_size,
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
                let changed = s.hover_url != new_hover;
                if changed {
                    let want_pointer = new_hover.is_some();
                    s.hover_url = new_hover;
                    drop(s);
                    if want_pointer {
                        da_motion.set_cursor_from_name(Some("pointer"));
                    } else {
                        da_motion.set_cursor_from_name(Some("text"));
                    }
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
                if s.hover_url.is_some() {
                    s.hover_url = None;
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
                        let (_, off, _) = s.terminal.scrollbar_state();
                        s.viewport_offset = off;
                    }
                }

                s.hover_url = None;
                s.last_hover_cell = (usize::MAX, usize::MAX);

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
                if let (Some(ref dc), Some(pane_id)) =
                    (s.daemon_client.clone(), s.daemon_pane_id)
                {
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
/// Send SIGINT to the PTY's actual foreground process group.
///
/// Writing 0x03 to the PTY master is not enough when the child has put the
/// slave into raw mode (ISIG disabled). We call `tcgetpgrp` via the master
/// PTY fd (already held by `pty`) to get the current foreground process group,
/// then `kill(-pgid, SIGINT)`. This correctly handles grandchild processes
/// (Node.js, pm2, etc.) that are in their own process group.
fn send_sigint_to_fg_pgrp(pty: &forgetty_pty::PtyProcess) {
    #[cfg(target_os = "linux")]
    {
        if let Some(pgid) = pty.foreground_pgrp() {
            let my_pid = std::process::id() as libc::pid_t;
            if pgid > 0 && pgid != my_pid {
                unsafe { libc::kill(-(pgid as libc::c_int), libc::SIGINT) };
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pty;
}

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
    for c in 0..num_cols {
        col_to_byte_start.push(line.len());
        let g = &cells[c].grapheme;
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
/// via Pango, updates `cell_width`/`cell_height`, recalculates
/// cols/rows from the current widget pixel size, and resizes both the
/// VT terminal and the PTY.
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
    if let Some(ref mut pty) = state.pty {
        let _ = pty.resize(forgetty_pty::PtySize {
            rows: new_rows as u16,
            cols: new_cols as u16,
            pixel_width: width as u16,
            pixel_height: height as u16,
        });
    }

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
        for col in 0..num_cols.min(cells.len()) {
            let g = &cells[col].grapheme;
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
    if s.search.all_matches.is_empty() {
        s.search.current_index = 0;
    } else if s.search.current_index >= s.search.all_matches.len() {
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
                if let Some(ref mut pty) = s.pty {
                    if let Err(e) = pty.resize(forgetty_pty::PtySize {
                        rows: new_rows as u16,
                        cols: new_cols as u16,
                        pixel_width: width as u16,
                        pixel_height: height as u16,
                    }) {
                        tracing::warn!("Failed to resize PTY on initial measure: {e}");
                    }
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

        for col in 0..num_cols.min(cells.len()) {
            let x = col as f64 * cell_w;
            let cell = &cells[col];

            // Draw cell background if non-default
            match cell.attrs.bg {
                Color::Rgb(r, g, b) => {
                    ctx.set_source_rgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
                    ctx.rectangle(x, y, cell_w, cell_h);
                    ctx.fill().ok();
                }
                Color::Default => {}
            }

            // Skip drawing empty/space cells for performance
            let grapheme = &cell.grapheme;
            if grapheme == " " || grapheme.is_empty() {
                continue;
            }

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
        let sel_alpha = if selection_color.a == 255 {
            0.4
        } else {
            selection_color.a as f64 / 255.0
        };
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
}
