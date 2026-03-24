//! Markdown rendering.
//!
//! Converts Markdown documents to HTML using pulldown-cmark for display
//! in the embedded webview.

// TODO: Phase 6 — implement Markdown rendering
//
// pub fn render_markdown(markdown: &str) -> String {
//     let parser = pulldown_cmark::Parser::new(markdown);
//     let mut html = String::new();
//     pulldown_cmark::html::push_html(&mut html, parser);
//     html
// }
