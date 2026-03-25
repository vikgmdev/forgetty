//! Selection highlight rendering.
//!
//! Renders the visual highlight overlay for text selections in the terminal.
//! Uses the same instanced-quad approach as the background and cursor renderers.

// TODO: Phase 5 — implement SelectionRenderer
//
// The selection renderer will reuse the cell.wgsl shader and instanced rendering
// approach from BackgroundRenderer to draw semi-transparent highlight rectangles
// over the selected text region.
//
// pub struct SelectionRenderer { ... }
