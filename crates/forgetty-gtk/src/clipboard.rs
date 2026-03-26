//! Smart clipboard text processing for terminal output.
//!
//! Provides the copy pipeline that strips box-drawing characters, trailing
//! whitespace, and normalizes line endings before placing text on the
//! system clipboard. This produces clean, usable text when copying from
//! TUI applications like Claude Code.

/// Strip box-drawing characters (U+2500-U+257F) from text.
pub fn strip_box_drawing(text: &str) -> String {
    text.chars().filter(|&c| !('\u{2500}'..='\u{257F}').contains(&c)).collect()
}

/// Strip trailing whitespace from each line.
pub fn strip_trailing_whitespace(text: &str) -> String {
    text.lines().map(|line| line.trim_end()).collect::<Vec<_>>().join("\n")
}

/// Normalize line endings to \n.
pub fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Run the full smart copy pipeline: strip box-drawing, trailing whitespace,
/// and normalize line endings.
pub fn smart_copy_pipeline(text: &str) -> String {
    normalize_line_endings(&strip_trailing_whitespace(&strip_box_drawing(text)))
}
