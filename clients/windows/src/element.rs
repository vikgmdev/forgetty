//! The custom GPUI element that paints the terminal cell grid:
//! background quads (merged runs), selection, cursor, one shaped line per row,
//! and the IME preedit. Rows are re-shaped only when their content changed.

use std::sync::Arc;

use gpui::{
    fill, outline, CursorStyle, DispatchPhase, MouseMoveEvent, MouseUpEvent, point, relative, size, App, Bounds, BorderStyle, Element, ElementId,
    ElementInputHandler, Entity, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Style, TextAlign, TextRun, UnderlineStyle, Window, px,
};

use forgetty_shell_core::codeblock::CodeBlock;

use crate::view::{hsla, shade, RowShaped, TerminalView, CODE_BTN};

pub struct TerminalElement {
    view: Entity<TerminalView>,
}

impl TerminalElement {
    pub fn new(view: Entity<TerminalView>) -> Self {
        Self { view }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

#[derive(Clone, Copy)]
enum CursorShape {
    Block,
    Bar,
    Underline,
    Hollow,
}

pub struct Prepaint {
    rows: Vec<Option<Arc<RowShaped>>>,
    cursor: Option<(Bounds<Pixels>, CursorShape)>,
    selection: Vec<(usize, usize, usize)>,
    /// (row, first col, cells, is the current match)
    search: Vec<(usize, usize, usize, bool)>,
    /// Link underline: (row, first col, one past the last col)
    links: Vec<(usize, usize, usize)>,
    /// The hovered code block, and whether its button shows the check mark.
    block: Option<(CodeBlock, bool)>,
    flash: bool,
    /// Cursor color a program set with OSC 12.
    cursor_rgb: Option<(u8, u8, u8)>,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let focused = self.view.read(cx).focus.is_focused(window);
        let active = window.is_window_active();
        self.view.update(cx, |v, _cx| {
            v.window_active = active;
            // One resize per frame: this runs once per frame.
            v.apply_bounds(bounds);
            v.prepare_rows(window);
            v.after_layout_change(_cx);
            // A smaller PTY grid sits in the middle of the pane (letterbox).
            let pad = v.letterbox_pad();
            let bounds = Bounds::new(bounds.origin + point(px(pad.0), px(pad.1)), bounds.size);

            let m = &v.metrics;
            let cursor = if v.term.cursor_visible() && (v.cursor_on || !focused) {
                let (r, c) = v.term.cursor();
                let wide = r < v.rows && c < v.cols && v.term.screen().cell(r, c).attrs.wide;
                let cells = if wide { 2.0 } else { 1.0 };
                let b = Bounds::new(
                    point(bounds.origin.x + m.cell_w * c as f32, bounds.origin.y + m.line_h * r as f32),
                    size(m.cell_w * cells, m.line_h),
                );
                // DECSCUSR from the program, else `cursor_style`.
                let shape = match forgetty_shell_core::cursor::shape(v.term.cursor_visual_style(), v.cfg.cursor_style, focused) {
                    forgetty_shell_core::cursor::Shape::Block => CursorShape::Block,
                    forgetty_shell_core::cursor::Shape::Bar => CursorShape::Bar,
                    forgetty_shell_core::cursor::Shape::Underline => CursorShape::Underline,
                    forgetty_shell_core::cursor::Shape::Hollow => CursorShape::Hollow,
                };
                Some((b, shape))
            } else {
                None
            };
            Prepaint {
                rows: v.rows_shaped.clone(),
                cursor,
                selection: v.selection_spans(),
                search: v.search_spans(),
                links: v.link_spans(),
                block: v.block_highlight(),
                flash: v.bell_flash(),
                cursor_rgb: v.term.cursor_color(),
            }
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let pad = self.view.read(cx).letterbox_pad();
        let bounds = Bounds::new(bounds.origin + point(px(pad.0), px(pad.1)), bounds.size);
        let k = 1.0 - self.view.read(cx).dim;
        let (cw, lh, font, font_size, fg, bg, cur_col, sel_col, focus, marked, cursor_cell) = {
            let v = self.view.read(cx);
            (
                v.metrics.cell_w,
                v.metrics.line_h,
                v.metrics.font.clone(),
                v.metrics.font_size,
                shade(hsla(v.cfg.foreground), k),
                shade(hsla(v.cfg.background), k),
                shade(hsla(v.cfg.cursor), k),
                shade(hsla(v.cfg.selection), k),
                v.focus.clone(),
                v.marked.clone(),
                v.term.cursor(),
            )
        };
        // A selection drag goes on outside the pane (the pointer is captured while the button is
        // held): the view hears every move and release of the window, not only those over it.
        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _w, cx| {
            if phase == DispatchPhase::Capture {
                view.update(cx, |v, cx| v.on_pointer_move(ev, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseUpEvent, phase, _w, cx| {
            if phase == DispatchPhase::Capture {
                view.update(cx, |v, cx| v.on_pointer_up(ev, cx));
            }
        });
        // IME: register the handler so TSF/IMM talks to the view.
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.view.clone()), cx);
        // Pass 1: backgrounds.
        for (r, row) in prepaint.rows.iter().enumerate() {
            let Some(row) = row else { continue };
            let y = bounds.origin.y + lh * r as f32;
            for &(c0, c1, color) in &row.bgs {
                let x = bounds.origin.x + cw * c0 as f32;
                window.paint_quad(fill(Bounds::new(point(x, y), size(cw * (c1 - c0) as f32, lh)), color));
            }
        }
        // Pass 2: selection.
        for &(r, c0, c1) in &prepaint.selection {
            let x = bounds.origin.x + cw * c0 as f32;
            let y = bounds.origin.y + lh * r as f32;
            window.paint_quad(fill(Bounds::new(point(x, y), size(cw * (c1 + 1 - c0) as f32, lh)), sel_col));
        }
        // Pass 2b: search matches (GTK colors: amber, brighter for the current one).
        for &(r, c0, n, current) in &prepaint.search {
            let color = if current { gpui::rgba(0xfab387a0) } else { gpui::rgba(0xf9e2af50) };
            let x = bounds.origin.x + cw * c0 as f32;
            let y = bounds.origin.y + lh * r as f32;
            window.paint_quad(fill(Bounds::new(point(x, y), size(cw * n as f32, lh)), color));
        }
        // The color a program set (OSC 12) wins over the theme's.
        let cur_col = match prepaint.cursor_rgb {
            Some((r, g, b)) => shade(hsla(((r as u32) << 16) | ((g as u32) << 8) | b as u32), k),
            None => cur_col,
        };
        // Pass 3: block cursor under the text.
        if let Some((b, CursorShape::Block)) = prepaint.cursor {
            let mut c = cur_col;
            c.a = 0.7;
            window.paint_quad(fill(b, c));
        }
        // Pass 4: text, one shaped line per row.
        for (r, row) in prepaint.rows.iter().enumerate() {
            if let Some(row) = row {
                let origin = point(bounds.origin.x, bounds.origin.y + lh * r as f32);
                let t_row = std::time::Instant::now();
                let _ = row.line.paint(origin, lh, TextAlign::Left, None, window, cx);
                let us = t_row.elapsed().as_micros() as usize;
                if us > 3000 {
                }
            }
        }
        // Pass 4b: link underline.
        for &(r, c0, c1) in &prepaint.links {
            let x = bounds.origin.x + cw * c0 as f32;
            let y = bounds.origin.y + lh * (r + 1) as f32 - px(2.0);
            window.paint_quad(fill(Bounds::new(point(x, y), size(cw * (c1 - c0) as f32, px(1.0))), fg));
        }
        if !prepaint.links.is_empty() {
            window.set_window_cursor_style(CursorStyle::PointingHand);
        }
        // Pass 5: thin cursors over the text.
        if let Some((b, shape)) = prepaint.cursor {
            match shape {
                CursorShape::Block => {}
                CursorShape::Bar => window.paint_quad(fill(
                    Bounds::new(b.origin, size(px(2.0), b.size.height)),
                    cur_col,
                )),
                CursorShape::Underline => window.paint_quad(fill(
                    Bounds::new(
                        point(b.origin.x, b.origin.y + b.size.height - px(2.0)),
                        size(b.size.width, px(2.0)),
                    ),
                    cur_col,
                )),
                CursorShape::Hollow => window.paint_quad(outline(b, cur_col, BorderStyle::Solid)),
            }
        }
        // Pass 5b: visual bell, a white wash over the pane (GTK: 15% white for 150 ms).
        if prepaint.flash {
            window.paint_quad(fill(bounds, gpui::rgba(0xffffff26)));
        }
        // Pass 5c: code block under the pointer: a wash on its border cells and a copy button.
        if let Some((block, copied)) = &prepaint.block {
            paint_code_block(window, cx, bounds, block, *copied, (cw, lh), (&font, font_size), bg);
        }
        // Pass 6: IME preedit at the cursor.
        if let Some(text) = marked {
            let (r, c) = cursor_cell;
            let origin = point(bounds.origin.x + cw * c as f32, bounds.origin.y + lh * r as f32);
            let cells: usize = text.chars().map(|ch| if (ch as u32) >= 0x1100 { 2 } else { 1 }).sum();
            window.paint_quad(fill(Bounds::new(origin, size(cw * cells as f32, lh)), bg));
            let run = TextRun {
                len: text.len(),
                font,
                color: fg,
                background_color: None,
                underline: Some(UnderlineStyle { color: Some(fg), thickness: px(1.0), wavy: false }),
                strikethrough: None,
            };
            let line = window.text_system().shape_line(text.into(), font_size, &[run], Some(cw));
            let _ = line.paint(origin, lh, TextAlign::Left, None, window, cx);
        }
        self.view.update(cx, |v, _cx| v.after_paint(window));
    }
}

/// The hover treatment of a code block (GTK `draw_terminal` step 10): a faint wash on the
/// border cells, and a copy button in the top right corner that shows a check mark for a
/// moment after a copy.
#[allow(clippy::too_many_arguments)]
fn paint_code_block(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    b: &CodeBlock,
    copied: bool,
    (cw, lh): (Pixels, Pixels),
    (font, font_size): (&gpui::Font, Pixels),
    bg: gpui::Hsla,
) {
    let dark = bg.l < 0.5;
    let wash = if dark { gpui::rgba(0xffffff14) } else { gpui::rgba(0x0000000f) };
    let cell = |r: usize, c: usize, w: usize| {
        Bounds::new(
            point(bounds.origin.x + cw * c as f32, bounds.origin.y + lh * r as f32),
            size(cw * w as f32, lh),
        )
    };
    let width = b.right_col - b.left_col + 1;
    window.paint_quad(fill(cell(b.top_row, b.left_col, width), wash));
    window.paint_quad(fill(cell(b.bottom_row, b.left_col, width), wash));
    for r in (b.top_row + 1)..b.bottom_row {
        window.paint_quad(fill(cell(r, b.left_col, 1), wash));
        window.paint_quad(fill(cell(r, b.right_col, 1), wash));
    }
    let btn = Bounds::new(
        point(
            bounds.origin.x + cw * (b.right_col + 1) as f32 - px(CODE_BTN),
            bounds.origin.y + lh * b.top_row as f32,
        ),
        size(px(CODE_BTN), px(CODE_BTN)),
    );
    let (btn_bg, ink) = if dark {
        (gpui::rgba(0x00000099), gpui::rgba(0xffffffe6))
    } else {
        (gpui::rgba(0xffffffb3), gpui::rgba(0x000000cc))
    };
    window.paint_quad(fill(btn, btn_bg).corner_radii(px(4.0)));
    if copied {
        let run = TextRun {
            len: "\u{2713}".len(),
            font: font.clone(),
            color: ink.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window.text_system().shape_line("\u{2713}".into(), font_size, &[run], None);
        let origin = point(btn.origin.x + (btn.size.width - line.width) / 2.0, btn.origin.y + (btn.size.height - lh) / 2.0);
        let _ = line.paint(origin, lh, TextAlign::Left, None, window, cx);
    } else {
        // Two overlapping outlined squares, the usual "copy" glyph.
        let pad = px(CODE_BTN * 0.22);
        let side = px(CODE_BTN) - pad * 2.0 - px(3.0);
        let back = Bounds::new(btn.origin + point(pad + px(3.0), pad), size(side, side));
        let front = Bounds::new(btn.origin + point(pad, pad + px(3.0)), size(side, side));
        window.paint_quad(outline(back, ink, BorderStyle::Solid));
        window.paint_quad(fill(front, btn_bg));
        window.paint_quad(outline(front, ink, BorderStyle::Solid));
    }
}
