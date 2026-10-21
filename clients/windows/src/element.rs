//! The custom GPUI element that paints the terminal cell grid:
//! background quads (merged runs), selection, cursor, one shaped line per row,
//! and the IME preedit. Rows are re-shaped only when their content changed.

use std::sync::Arc;

use gpui::{
    fill, outline, point, relative, size, App, Bounds, BorderStyle, Element, ElementId,
    ElementInputHandler, Entity, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Style, TextAlign, TextRun, UnderlineStyle, Window, px,
};

use crate::view::{hsla, RowShaped, TerminalView};

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
        crate::smoke::trace("e", 0);
        let focused = self.view.read(cx).focus.is_focused(window);
        let active = window.is_window_active();
        self.view.update(cx, |v, _cx| {
            v.window_active = active;
            // One resize per frame: this runs once per frame.
            v.apply_bounds(bounds);
            v.prepare_rows(window);

            let m = &v.metrics;
            let cursor = if v.term.cursor_visible() && (v.cursor_on || !focused) {
                let (r, c) = v.term.cursor();
                let wide = r < v.rows && c < v.cols && v.term.screen().cell(r, c).attrs.wide;
                let cells = if wide { 2.0 } else { 1.0 };
                let b = Bounds::new(
                    point(bounds.origin.x + m.cell_w * c as f32, bounds.origin.y + m.line_h * r as f32),
                    size(m.cell_w * cells, m.line_h),
                );
                let shape = if !focused {
                    CursorShape::Hollow
                } else {
                    // DECSCUSR: BAR=0, BLOCK=1, UNDERLINE=2, BLOCK_HOLLOW=3
                    match v.term.cursor_visual_style() {
                        0 => CursorShape::Bar,
                        2 => CursorShape::Underline,
                        3 => CursorShape::Hollow,
                        _ => CursorShape::Block,
                    }
                };
                Some((b, shape))
            } else {
                None
            };
            crate::smoke::trace("E", v.rows_reshaped_last);
            Prepaint { rows: v.rows_shaped.clone(), cursor, selection: v.selection_spans() }
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
        let (cw, lh, font, font_size, fg, bg, cur_col, sel_col, focus, marked, cursor_cell) = {
            let v = self.view.read(cx);
            (
                v.metrics.cell_w,
                v.metrics.line_h,
                v.metrics.font.clone(),
                v.metrics.font_size,
                hsla(v.cfg.foreground),
                hsla(v.cfg.background),
                hsla(v.cfg.cursor),
                hsla(v.cfg.selection),
                v.focus.clone(),
                v.marked.clone(),
                v.term.cursor(),
            )
        };

        crate::smoke::trace("g", 0);
        // IME: register the handler so TSF/IMM talks to the view.
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.view.clone()), cx);

        crate::smoke::trace("h", 0);
        // Pass 1: backgrounds.
        for (r, row) in prepaint.rows.iter().enumerate() {
            let Some(row) = row else { continue };
            let y = bounds.origin.y + lh * r as f32;
            for &(c0, c1, color) in &row.bgs {
                let x = bounds.origin.x + cw * c0 as f32;
                window.paint_quad(fill(Bounds::new(point(x, y), size(cw * (c1 - c0) as f32, lh)), color));
            }
        }
        crate::smoke::trace("1", 0);
        // Pass 2: selection.
        for &(r, c0, c1) in &prepaint.selection {
            let x = bounds.origin.x + cw * c0 as f32;
            let y = bounds.origin.y + lh * r as f32;
            window.paint_quad(fill(Bounds::new(point(x, y), size(cw * (c1 + 1 - c0) as f32, lh)), sel_col));
        }
        // Pass 3: block cursor under the text.
        if let Some((b, CursorShape::Block)) = prepaint.cursor {
            let mut c = cur_col;
            c.a = 0.7;
            window.paint_quad(fill(b, c));
        }
        crate::smoke::trace("3", 0);
        // Pass 4: text, one shaped line per row.
        for (r, row) in prepaint.rows.iter().enumerate() {
            if let Some(row) = row {
                let origin = point(bounds.origin.x, bounds.origin.y + lh * r as f32);
                let _ = row.line.paint(origin, lh, TextAlign::Left, None, window, cx);
            }
        }
        crate::smoke::trace("4", 0);
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

        crate::smoke::trace("G", 0);
        self.view.update(cx, |v, _cx| v.after_paint(window));
    }
}
