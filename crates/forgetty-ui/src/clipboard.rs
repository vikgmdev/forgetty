//! Smart clipboard integration.
//!
//! Provides copy and paste functionality using the system clipboard
//! via the arboard crate, with smart text processing for terminal output.

/// A smart clipboard that can process terminal text before copying.
pub struct SmartClipboard {
    inner: arboard::Clipboard,
}

impl SmartClipboard {
    /// Create a new smart clipboard handle.
    ///
    /// Returns `None` if the clipboard cannot be accessed (e.g., no display
    /// server on Linux).
    pub fn new() -> Option<Self> {
        arboard::Clipboard::new().ok().map(|inner| Self { inner })
    }

    /// Smart copy: strip box-drawing chars, trailing whitespace, and
    /// normalize line endings before copying.
    pub fn copy_smart(&mut self, text: &str) -> bool {
        let processed =
            normalize_line_endings(&strip_trailing_whitespace(&strip_box_drawing(text)));
        self.inner.set_text(processed).is_ok()
    }

    /// Raw copy: no processing.
    pub fn copy_raw(&mut self, text: &str) -> bool {
        self.inner.set_text(text.to_string()).is_ok()
    }

    /// Paste text from the clipboard.
    pub fn paste(&mut self) -> Option<String> {
        self.inner.get_text().ok()
    }
}

/// Strip box-drawing characters (U+2500-U+257F) from text.
fn strip_box_drawing(text: &str) -> String {
    text.chars().filter(|&c| !('\u{2500}'..='\u{257F}').contains(&c)).collect()
}

/// Strip trailing whitespace from each line.
fn strip_trailing_whitespace(text: &str) -> String {
    text.lines().map(|line| line.trim_end()).collect::<Vec<_>>().join("\n")
}

/// Normalize line endings to \n.
fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_box_drawing() {
        // U+2500 = ─, U+2502 = │, U+250C = ┌, U+2510 = ┐, U+2514 = └, U+2518 = ┘
        let input = "┌──────┐\n│ hello │\n└──────┘";
        let result = strip_box_drawing(input);
        assert_eq!(result, "\n hello \n");
    }

    #[test]
    fn test_strip_box_drawing_no_box_chars() {
        let input = "plain text";
        assert_eq!(strip_box_drawing(input), "plain text");
    }

    #[test]
    fn test_strip_box_drawing_empty() {
        assert_eq!(strip_box_drawing(""), "");
    }

    #[test]
    fn test_strip_trailing_whitespace() {
        let input = "hello   \nworld  \n  foo  ";
        let result = strip_trailing_whitespace(input);
        assert_eq!(result, "hello\nworld\n  foo");
    }

    #[test]
    fn test_strip_trailing_whitespace_no_trailing() {
        let input = "hello\nworld";
        assert_eq!(strip_trailing_whitespace(input), "hello\nworld");
    }

    #[test]
    fn test_strip_trailing_whitespace_empty() {
        assert_eq!(strip_trailing_whitespace(""), "");
    }

    #[test]
    fn test_normalize_line_endings_crlf() {
        let input = "hello\r\nworld\r\n";
        assert_eq!(normalize_line_endings(input), "hello\nworld\n");
    }

    #[test]
    fn test_normalize_line_endings_cr() {
        let input = "hello\rworld\r";
        assert_eq!(normalize_line_endings(input), "hello\nworld\n");
    }

    #[test]
    fn test_normalize_line_endings_already_lf() {
        let input = "hello\nworld\n";
        assert_eq!(normalize_line_endings(input), "hello\nworld\n");
    }

    #[test]
    fn test_normalize_line_endings_mixed() {
        let input = "a\r\nb\rc\n";
        assert_eq!(normalize_line_endings(input), "a\nb\nc\n");
    }

    #[test]
    fn test_smart_pipeline() {
        // Box drawing + trailing whitespace + CRLF
        // After strip_box_drawing: " hello  \r\n world  \r\n"
        // After strip_trailing_whitespace (uses .lines() which drops trailing newline): " hello\n world"
        // After normalize_line_endings: " hello\n world"
        let input = "│ hello  \r\n│ world  \r\n";
        let result = normalize_line_endings(&strip_trailing_whitespace(&strip_box_drawing(input)));
        assert_eq!(result, " hello\n world");
    }
}
