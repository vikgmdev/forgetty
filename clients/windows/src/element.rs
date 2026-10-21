//! The custom GPUI element that paints the terminal cell grid:
//! background quads (one per merged run) + one shaped text line per row.
//! Rows are re-shaped only when libghostty-vt reports them dirty.

use gpui::{
    fill, point, relative, size, App, Bounds, Element, ElementId, Entity, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Style, TextAlign, Window,
};

use crate::view::{hsla, TerminalView};

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

pub struct Prepaint {
    cursor: Option<Bounds<Pixels>>,
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
        self.view.update(cx, |v, _cx| {
            v.prepare_rows(window);
            let cursor = if v.term.cursor_visible() {
                let (r, c) = v.term.cursor();
                let m = &v.metrics;
                Some(Bounds::new(
                    point(
                        bounds.origin.x + m.cell_w * c as f32,
                        bounds.origin.y + m.line_h * r as f32,
                    ),
                    size(m.cell_w, m.line_h),
                ))
            } else {
                None
            };
            Prepaint { cursor }
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (cw, lh, rows) = {
            let view = self.view.read(cx);
            // ShapedLine is an Arc inside; this clone is 30 refcount bumps.
            (view.metrics.cell_w, view.metrics.line_h, view.rows_cache.clone())
        };

        // Pass 1: backgrounds.
        for (r, row) in rows.iter().enumerate() {
            let y = bounds.origin.y + lh * r as f32;
            for &(c0, c1, color) in &row.bgs {
                let x = bounds.origin.x + cw * c0 as f32;
                window.paint_quad(fill(
                    Bounds::new(point(x, y), size(cw * (c1 - c0) as f32, lh)),
                    color,
                ));
            }
        }
        // Pass 2: cursor block under the text.
        if let Some(cb) = prepaint.cursor {
            let mut color = hsla(0xcccccc);
            color.a = 0.65;
            window.paint_quad(fill(cb, color));
        }
        // Pass 3: text, one shaped line per row.
        for (r, row) in rows.iter().enumerate() {
            if let Some(line) = &row.line {
                let origin = point(bounds.origin.x, bounds.origin.y + lh * r as f32);
                let _ = line.paint(origin, lh, TextAlign::Left, None, window, cx);
            }
        }

        self.view.update(cx, |v, _cx| v.after_paint());
    }
}
