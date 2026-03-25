//! Image viewing support.
//!
//! Handles rendering images (PNG, JPEG, SVG, etc.) within the embedded
//! webview panel.

use std::path::Path;

/// The dark theme CSS for the viewer, embedded at compile time.
const STYLE_CSS: &str = include_str!("assets/style.css");

/// Generate HTML that displays an image from a file path.
///
/// Uses a `file://` URL to reference the image. The resulting HTML page uses
/// the dark theme background and centers the image both horizontally and
/// vertically.
pub fn render_image(path: &Path) -> String {
    // Canonicalize if possible, otherwise use the path as-is.
    let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let file_url = format!("file://{}", abs_path.display());

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Image".to_string());

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>{file_name} - Forgetty Viewer</title>
    <style>
{STYLE_CSS}

body {{
    display: flex;
    align-items: center;
    justify-content: center;
    min-height: 100vh;
    padding: 0;
    margin: 0;
}}

#content {{
    text-align: center;
    padding: 16px;
}}

#content img {{
    max-width: 100%;
    max-height: 90vh;
    object-fit: contain;
    border-radius: 4px;
}}
    </style>
</head>
<body>
    <div id="content">
        <img src="{file_url}" alt="{file_name}">
    </div>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn generates_html_with_image_src() {
        let path = PathBuf::from("/tmp/test-image.png");
        let html = render_image(&path);
        assert!(html.contains("file:///tmp/test-image.png"));
        assert!(html.contains("<img"));
        assert!(html.contains("alt=\"test-image.png\""));
    }

    #[test]
    fn wraps_in_full_html_document() {
        let path = PathBuf::from("/tmp/photo.jpg");
        let html = render_image(&path);
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("<html"));
        assert!(html.contains("</html>"));
        assert!(html.contains("--bg: #181818"));
    }

    #[test]
    fn centers_image() {
        let path = PathBuf::from("/tmp/photo.jpg");
        let html = render_image(&path);
        assert!(html.contains("justify-content: center"));
        assert!(html.contains("align-items: center"));
    }

    #[test]
    fn includes_filename_in_title() {
        let path = PathBuf::from("/home/user/docs/diagram.svg");
        let html = render_image(&path);
        assert!(html.contains("<title>diagram.svg - Forgetty Viewer</title>"));
    }
}
