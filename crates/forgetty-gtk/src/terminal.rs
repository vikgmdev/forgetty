//! Terminal grid rendering with Pango + Cairo.
//!
//! Provides `create_terminal()` which returns a `(gtk::Box, DrawingArea, State)`
//! triple: the Box contains the DrawingArea (terminal grid) and a vertical
//! gtk::Scrollbar on the right edge. The DrawingArea renders the terminal grid
//! from `forgetty_vt::Terminal`'s screen state using Cairo for drawing and
//! Pango for text layout.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use forgetty_config::{Config, CursorStyle};
use forgetty_core::Rgba;
use forgetty_vt::screen::Color;
use forgetty_vt::selection::{Selection, SelectionMode};
use gtk4::cairo;
use gtk4::gdk;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{glib, DrawingArea};

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

/// Shared terminal state accessible from multiple GTK callbacks.
///
/// All access happens on the GTK main thread via `Rc<RefCell<>>`.
pub struct TerminalState {
    pub terminal: forgetty_vt::Terminal,
    pub pty: forgetty_pty::PtyProcess,
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
}

impl TerminalState {
    /// Drain all pending PTY output from the channel and feed it to the
    /// terminal VT parser. Returns true if any data was processed.
    fn drain_pty_output(&mut self) -> bool {
        let mut had_data = false;
        loop {
            match self.pty_rx.try_recv() {
                Ok(data) => {
                    had_data = true;
                    self.terminal.feed(&data);

                    // Drain write-PTY responses (DA responses, mode queries, etc.)
                    let responses = self.terminal.drain_write_pty();
                    for chunk in responses {
                        if let Err(e) = self.pty.write(&chunk) {
                            tracing::warn!("Failed to write PTY response: {e}");
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        had_data
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

/// Build a `pango::FontDescription` from the config.
fn font_description(config: &Config) -> pango::FontDescription {
    let mut desc = pango::FontDescription::new();
    desc.set_family(&config.font_family);
    desc.set_size((config.font_size as i32) * pango::SCALE);
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
pub fn create_terminal(
    config: &Config,
) -> Result<(gtk4::Box, DrawingArea, Rc<RefCell<TerminalState>>), String> {
    let initial_rows: usize = 24;
    let initial_cols: usize = 80;

    // Spawn PTY bridge
    let shell = config.shell.as_deref();
    let (pty, pty_rx) =
        pty_bridge::spawn_pty_bridge(initial_rows as u16, initial_cols as u16, shell)?;

    // Create terminal VT state
    let terminal = forgetty_vt::Terminal::new(initial_rows, initial_cols);

    let input = GhosttyInput::new();

    let state = Rc::new(RefCell::new(TerminalState {
        terminal,
        pty,
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
    }));

    // Create DrawingArea
    let drawing_area = DrawingArea::new();
    drawing_area.set_hexpand(true);
    drawing_area.set_vexpand(true);
    drawing_area.set_focusable(true);
    drawing_area.set_can_focus(true);

    // Track whether cell dimensions have been measured from an actual Pango context
    let cell_measured = Rc::new(RefCell::new(false));

    // --- Draw callback ---
    {
        let state = Rc::clone(&state);
        let config = config.clone();
        let cell_measured = Rc::clone(&cell_measured);
        drawing_area.set_draw_func(move |da, ctx, width, height| {
            draw_terminal(da, ctx, width, height, &state, &config, &cell_measured);
        });
    }

    // --- Poll PTY data with a GLib timeout (8ms ~ 120Hz) ---
    // Uses a weak reference to the DrawingArea so the timer stops automatically
    // when the tab is closed and the widget is destroyed.
    {
        let state = Rc::clone(&state);
        let da_weak = drawing_area.downgrade();
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

            let had_data = s.drain_pty_output();
            if had_data {
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

                drop(s);
                da.queue_draw();
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
                    if let Err(e) = s.pty.write(&bytes) {
                        tracing::warn!("Failed to write to PTY: {e}");
                    }
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
                    if let Err(e) = s.pty.write(&bytes) {
                        tracing::warn!("Failed to write to PTY: {e}");
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
                        if let Err(e) = s.pty.write(&bytes) {
                            tracing::warn!("Failed to write focus-in to PTY: {e}");
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
                        if let Err(e) = s.pty.write(&bytes) {
                            tracing::warn!("Failed to write focus-out to PTY: {e}");
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
                    if let Err(e) = s.pty.write(&bytes) {
                        tracing::warn!("Failed to write mouse press to PTY: {e}");
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
                    if let Err(e) = s.pty.write(&bytes) {
                        tracing::warn!("Failed to write mouse release to PTY: {e}");
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
                    if let Err(e) = s.pty.write(&bytes) {
                        tracing::warn!("Failed to write mouse motion to PTY: {e}");
                    }
                    da_motion.queue_draw();
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
                        if let Err(e) = s.pty.write(&bytes) {
                            tracing::warn!("Failed to write scroll to PTY: {e}");
                        }
                    }
                    ScrollAction::ScrollViewport(delta) => {
                        s.terminal.scroll_viewport_delta(delta);
                        let (_, off, _) = s.terminal.scrollbar_state();
                        s.viewport_offset = off;
                    }
                }

                drop(s);
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
                if let Err(e) = s.pty.resize(forgetty_pty::PtySize {
                    rows: new_rows as u16,
                    cols: new_cols as u16,
                    pixel_width: width as u16,
                    pixel_height: height as u16,
                }) {
                    tracing::warn!("Failed to resize PTY: {e}");
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
    // Copy selection: Ctrl+Shift+C
    if mods == ctrl_shift && (keyval == gdk::Key::c || keyval == gdk::Key::C) {
        return true;
    }
    // Search in terminal: Ctrl+Shift+F
    if mods == ctrl_shift && (keyval == gdk::Key::f || keyval == gdk::Key::F) {
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

    false
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
    config: &Config,
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
    let cursor_color = s.config.theme.cursor;
    let selection_color = s.config.theme.selection;
    let search_match_color = s.config.theme.search_match;
    let search_current_color = s.config.theme.search_current;
    let cursor_style = s.config.cursor_style;

    // Clone selection state for rendering
    let selection = s.selection.clone();

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

    // Build font description
    let font_desc = font_description(config);

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
                if let Err(e) = s.pty.resize(forgetty_pty::PtySize {
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

            // Create Pango layout for this cell's text
            let layout = pango::Layout::new(&pango_ctx);
            let mut cell_font = font_desc.clone();

            if cell.attrs.bold {
                cell_font.set_weight(pango::Weight::Bold);
            }
            if cell.attrs.italic {
                cell_font.set_style(pango::Style::Italic);
            }

            layout.set_font_description(Some(&cell_font));
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

    // 4. Draw cursor
    if cursor_visible && cursor_row < num_rows && cursor_col < num_cols {
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
        }
    }

    // 5. Draw selection overlay (semi-transparent, preserves underlying text)
    //
    // Selection coordinates are stored as ABSOLUTE scrollback rows.
    // We convert them to screen-relative rows by subtracting viewport_offset.
    // Only draw the portion of the selection that overlaps the current viewport.
    if let Some(ref sel) = selection {
        ctx.set_source_rgba(
            selection_color.r as f64 / 255.0,
            selection_color.g as f64 / 255.0,
            selection_color.b as f64 / 255.0,
            selection_color.a as f64 / 255.0,
        );

        // Determine the absolute row range of the selection
        let ((sr, _), (er, _)) = sel.ordered();

        // Viewport shows absolute rows [viewport_offset .. viewport_offset + num_rows - 1].
        // Clamp the selection range to the visible viewport.
        let vp_start = viewport_offset;
        let vp_end = viewport_offset + num_rows.saturating_sub(1);

        // Skip rendering entirely if selection is outside the viewport
        if er >= vp_start && sr <= vp_end {
            let abs_start = sr.max(vp_start);
            let abs_end = er.min(vp_end);

            for abs_row in abs_start..=abs_end {
                let screen_row = abs_row - viewport_offset;
                for col in 0..num_cols {
                    if sel.contains(abs_row, col) {
                        let sx = col as f64 * cell_w;
                        let sy = screen_row as f64 * cell_h;
                        ctx.rectangle(sx, sy, cell_w, cell_h);
                        ctx.fill().ok();
                    }
                }
            }
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
}
