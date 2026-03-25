//! Markdown rendering.
//!
//! Converts Markdown documents to HTML using pulldown-cmark for display
//! in the embedded webview.

use pulldown_cmark::{html, Options, Parser};

/// The dark theme CSS for the viewer, embedded at compile time.
const STYLE_CSS: &str = include_str!("assets/style.css");

/// Render markdown content to a full HTML page with dark theme styling.
///
/// Uses `pulldown-cmark` to parse markdown into HTML, then wraps it in a
/// complete HTML document with the dark theme CSS from `assets/style.css`.
/// Code blocks are emitted with `<pre><code class="language-X">` classes
/// for downstream syntax highlighting.
pub fn render_markdown(content: &str) -> String {
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_TASKLISTS;

    let parser = Parser::new_ext(content, options);
    let mut body_html = String::new();
    html::push_html(&mut body_html, parser);

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Forgetty Viewer</title>
    <style>
{STYLE_CSS}
    </style>
</head>
<body>
    <div id="content">
{body_html}
    </div>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headings() {
        let html = render_markdown("# Hello\n## World");
        assert!(html.contains("<h1>Hello</h1>"));
        assert!(html.contains("<h2>World</h2>"));
    }

    #[test]
    fn renders_code_blocks() {
        let md = "```rust\nfn main() {}\n```";
        let html = render_markdown(md);
        assert!(html.contains("<pre><code class=\"language-rust\">"));
        assert!(html.contains("fn main()"));
    }

    #[test]
    fn renders_inline_code() {
        let html = render_markdown("Use `cargo build` to compile.");
        assert!(html.contains("<code>cargo build</code>"));
    }

    #[test]
    fn renders_lists() {
        let md = "- one\n- two\n- three\n";
        let html = render_markdown(md);
        assert!(html.contains("<ul>"));
        assert!(html.contains("<li>one</li>"));
        assert!(html.contains("<li>two</li>"));
        assert!(html.contains("<li>three</li>"));
    }

    #[test]
    fn renders_links() {
        let md = "[Forgetty](https://forgetty.dev)";
        let html = render_markdown(md);
        assert!(html.contains("<a href=\"https://forgetty.dev\">Forgetty</a>"));
    }

    #[test]
    fn wraps_in_full_html_document() {
        let html = render_markdown("Hello");
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("<html"));
        assert!(html.contains("</html>"));
        assert!(html.contains("<style>"));
        assert!(html.contains("--bg: #181818"));
    }

    #[test]
    fn renders_blockquotes() {
        let md = "> This is a quote";
        let html = render_markdown(md);
        assert!(html.contains("<blockquote>"));
    }

    #[test]
    fn renders_strikethrough() {
        let md = "~~deleted~~";
        let html = render_markdown(md);
        assert!(html.contains("<del>deleted</del>"));
    }

    #[test]
    fn renders_tables() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let html = render_markdown(md);
        assert!(html.contains("<table>"));
        assert!(html.contains("<th>"));
        assert!(html.contains("<td>"));
    }
}
