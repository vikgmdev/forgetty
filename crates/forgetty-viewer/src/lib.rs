//! Embedded content viewer for Forgetty.
//!
//! Provides a webview-based viewer for rendering rich content inline
//! in the terminal, including Markdown documents, images, and HTML.

pub mod image_viewer;
pub mod markdown;
pub mod webview;

// TODO: Phase 6 — re-export key types once viewer is implemented
