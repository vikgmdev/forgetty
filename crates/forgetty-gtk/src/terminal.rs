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

use crate::pty_bridge;

/// Shared terminal state accessible from multiple GTK callbacks.
///
/// All access happens on the GTK main thread via `Rc<RefCell<>>`.
pub struct TerminalState {
    pub terminal: forgetty_vt::Terminal,
    pub pty: forgetty_pty::PtyProcess,
    pub pty_rx: mpsc::Receiver<Vec<u8>>,
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

    let state = Rc::new(RefCell::new(TerminalState {
        terminal,
        pty,
        pty_rx,
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
    {
        let state = Rc::clone(&state);
        let da = drawing_area.clone();
        glib::timeout_add_local(Duration::from_millis(8), move || {
            let had_data = state.borrow_mut().drain_pty_output();
            if had_data {
                da.queue_draw();
            }
            glib::ControlFlow::Continue
        });
    }

    // --- Keyboard input ---
    {
        let state = Rc::clone(&state);
        let da_for_key = drawing_area.clone();
        let key_controller = gtk4::EventControllerKey::new();
        key_controller.connect_key_pressed(move |_controller, keyval, _keycode, modifier| {
            if let Some(bytes) = crate::input::key_to_pty_bytes(keyval, modifier) {
                let mut s = state.borrow_mut();
                if let Err(e) = s.pty.write(&bytes) {
                    tracing::warn!("Failed to write to PTY: {e}");
                }
                da_for_key.queue_draw();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        drawing_area.add_controller(key_controller);
    }

    // --- Resize handler ---
    {
        let state = Rc::clone(&state);
        let cell_measured_resize = Rc::clone(&cell_measured);
        drawing_area.connect_resize(move |da, width, height| {
            if !*cell_measured_resize.borrow() {
                return;
            }

            let (cw, ch) = {
                let s = state.borrow();
                (s.cell_width, s.cell_height)
            };

            if cw < 1.0 || ch < 1.0 {
                return;
            }

            let new_cols = ((width as f64) / cw).max(1.0) as usize;
            let new_rows = ((height as f64) / ch).max(1.0) as usize;

            let mut s = state.borrow_mut();
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
    let mut s = state.borrow_mut();

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
