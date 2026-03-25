//! Webview management for the embedded content viewer.
//!
//! Creates and manages wry webview instances for rendering HTML content
//! within Forgetty panes.

use std::path::Path;

use crate::image_viewer;
use crate::markdown;

/// Content prepared for display in a webview.
pub struct ViewerContent {
    /// The full HTML page to render.
    pub html: String,
    /// A human-readable title for the viewer tab / pane.
    pub title: String,
}

/// The set of file extensions recognized as markdown.
const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown"];

/// The set of file extensions recognized as images.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "svg", "webp"];

/// Determine the viewer content for a file path.
///
/// Returns `Some(ViewerContent)` if the file type is supported for preview,
/// or `None` for unsupported file types.
///
/// Supported types:
/// - Markdown (`.md`, `.markdown`) — rendered to themed HTML.
/// - Images (`.png`, `.jpg`, `.jpeg`, `.gif`, `.svg`, `.webp`) — displayed
///   with a centered, dark-background viewer.
pub fn content_for_file(path: &Path) -> Option<ViewerContent> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".to_string());

    if MARKDOWN_EXTENSIONS.contains(&ext.as_str()) {
        let content = std::fs::read_to_string(path).ok()?;
        let html = markdown::render_markdown(&content);
        Some(ViewerContent { html, title: file_name })
    } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        let html = image_viewer::render_image(path);
        Some(ViewerContent { html, title: file_name })
    } else {
        None
    }
}

/// Check whether a file extension is supported for preview.
pub fn is_previewable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| {
            let ext = ext.to_lowercase();
            MARKDOWN_EXTENSIONS.contains(&ext.as_str()) || IMAGE_EXTENSIONS.contains(&ext.as_str())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn markdown_is_previewable() {
        assert!(is_previewable(&PathBuf::from("README.md")));
        assert!(is_previewable(&PathBuf::from("docs/guide.markdown")));
    }

    #[test]
    fn images_are_previewable() {
        assert!(is_previewable(&PathBuf::from("photo.png")));
        assert!(is_previewable(&PathBuf::from("photo.jpg")));
        assert!(is_previewable(&PathBuf::from("photo.jpeg")));
        assert!(is_previewable(&PathBuf::from("anim.gif")));
        assert!(is_previewable(&PathBuf::from("icon.svg")));
        assert!(is_previewable(&PathBuf::from("banner.webp")));
    }

    #[test]
    fn unknown_extensions_are_not_previewable() {
        assert!(!is_previewable(&PathBuf::from("main.rs")));
        assert!(!is_previewable(&PathBuf::from("data.json")));
        assert!(!is_previewable(&PathBuf::from("Cargo.toml")));
    }

    #[test]
    fn no_extension_is_not_previewable() {
        assert!(!is_previewable(&PathBuf::from("Makefile")));
    }

    #[test]
    fn case_insensitive_extensions() {
        assert!(is_previewable(&PathBuf::from("README.MD")));
        assert!(is_previewable(&PathBuf::from("photo.PNG")));
        assert!(is_previewable(&PathBuf::from("photo.Jpg")));
    }

    #[test]
    fn content_for_unknown_file_returns_none() {
        let result = content_for_file(&PathBuf::from("/tmp/test.rs"));
        assert!(result.is_none());
    }

    #[test]
    fn content_for_missing_markdown_returns_none() {
        // File does not exist, so fs::read_to_string will fail.
        let result = content_for_file(&PathBuf::from("/tmp/nonexistent-39fj2k.md"));
        assert!(result.is_none());
    }
}
