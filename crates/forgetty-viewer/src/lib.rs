//! Embedded content viewer for Forgetty.
//!
//! Provides a webview-based viewer for rendering rich content inline
//! in the terminal, including Markdown documents, images, and HTML.

pub mod image_viewer;
pub mod markdown;
pub mod webview;

pub use image_viewer::render_image;
pub use markdown::render_markdown;
pub use webview::{content_for_file, is_previewable, ViewerContent};
