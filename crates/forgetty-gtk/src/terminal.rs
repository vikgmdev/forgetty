//! Terminal grid rendering with Pango + Cairo.
//!
//! Provides `create_terminal()` which returns a `gtk::DrawingArea`
//! that renders the terminal grid from `forgetty_vt::Terminal`'s screen state
//! using Cairo for drawing and Pango for text layout.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use forgetty_config::{Config, CursorStyle};
use forgetty_core::Rgba;
use forgetty_vt::screen::Color;
use gtk4::cairo;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{glib, DrawingArea};

use crate::input::{GhosttyInput, ScrollAction};
use crate::pty_bridge;

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

/// Create the terminal `DrawingArea` and wire up PTY I/O, rendering, input,
/// and resize handling.
///
/// Returns `(drawing_area, state)` where `state` is the shared `TerminalState`
/// wrapped in `Rc<RefCell<>>`.
pub fn create_terminal(
    config: &Config,
) -> Result<(DrawingArea, Rc<RefCell<TerminalState>>), String> {
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
            let had_data = s.drain_pty_output();
            if had_data {
                // Scroll to bottom on new output so the user doesn't miss
                // anything after browsing scrollback (AC-9).
                s.terminal.scroll_viewport_bottom();
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
                let Ok(mut s) = state.try_borrow_mut() else {
                    // Borrow held elsewhere -- drop this key event.
                    return glib::Propagation::Proceed;
                };
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
            gesture.connect_pressed(move |gesture, _n_press, x, y| {
                let button = gesture.current_button();
                let modifier = gesture.current_event_state();
                let Ok(mut s) = state.try_borrow_mut() else {
                    // Borrow held elsewhere -- drop this click event.
                    return;
                };

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

    Ok((drawing_area, state))
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
    let cursor_style = s.config.cursor_style;

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
}
