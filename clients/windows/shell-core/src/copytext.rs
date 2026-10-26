//! Smart copy: the text that goes on the clipboard when the user copies a selection.
//!
//! Same pipeline as GTK (`forgetty-gtk/src/clipboard.rs`): strip box-drawing characters
//! (U+2500 to U+257F), strip trailing spaces on every line, normalize line endings. That
//! turns a selection from a bordered TUI box (Claude Code and the like) into plain text.
//!
//! Where this differs from GTK, on purpose:
//! - Rows are joined only where the terminal itself marked a soft wrap. GTK also treats a row
//!   that is more than 80% full as wrapped and trims the white space at the join, which merges
//!   separate output lines that happen to be long.
//! - Text at a soft wrap is kept as it is (the spaces at the end of a wrapped row and the start
//!   of the next one are text, not padding).
//! - Line endings are normalized first (GTK strips trailing space before a lone CR is split).
//! - `raw` copy skips the box-drawing strip, for a `tree` listing or a table the user wants whole.

/// Strip box-drawing characters (U+2500 to U+257F).
pub fn strip_box_drawing(text: &str) -> String {
    text.chars().filter(|&c| !('\u{2500}'..='\u{257F}').contains(&c)).collect()
}

/// Strip trailing white space from every line.
pub fn strip_trailing_whitespace(text: &str) -> String {
    text.lines().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
}

/// Line endings to `\n`.
pub fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// The full pipeline.
pub fn smart_copy(text: &str) -> String {
    strip_trailing_whitespace(&strip_box_drawing(&normalize_line_endings(text)))
}

/// What `raw` copy does: no box-drawing strip.
pub fn raw_copy(text: &str) -> String {
    strip_trailing_whitespace(&normalize_line_endings(text))
}

/// Join selected rows. Each row is its text and whether it soft-wraps into the next one. A row
/// that ends a line loses its trailing spaces (they are the terminal's padding); a wrapped row
/// keeps everything. The last row never gets a newline.
pub fn join_rows(rows: &[(String, bool)]) -> String {
    let mut out = String::new();
    for (i, (text, wrapped)) in rows.iter().enumerate() {
        if *wrapped && i + 1 < rows.len() {
            out.push_str(text);
        } else {
            out.push_str(text.trim_end());
            if i + 1 < rows.len() {
                out.push('\n');
            }
        }
    }
    out
}

/// Remove leading and trailing lines that hold only white space. Inner blank lines stay.
pub fn trim_blank_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(lines.len());
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map(|i| i + 1).unwrap_or(start);
    if start >= end {
        return String::new();
    }
    lines[start..end].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_strips_boxes_trailing_space_and_crlf() {
        let boxed = "\u{250c}\u{2500}\u{2500}\u{2500}\u{2510}\r\n\u{2502} hi  \u{2502}  \r\n\u{2502} yo  \u{2502}\r\n\u{2514}\u{2500}\u{2500}\u{2500}\u{2518}";
        assert_eq!(smart_copy(boxed), "\n hi\n yo");
        assert_eq!(smart_copy("a  \r\nb\t\rc"), "a\nb\nc");
        assert_eq!(smart_copy("plain text"), "plain text");
        assert_eq!(smart_copy(""), "");
    }

    #[test]
    fn raw_copy_keeps_the_box_characters() {
        assert_eq!(raw_copy("\u{251c}\u{2500}\u{2500} file  \r\n"), "\u{251c}\u{2500}\u{2500} file");
    }

    #[test]
    fn rows_join_only_at_soft_wraps() {
        let rows = vec![
            ("a long line that wrapped ".to_string(), true),
            ("  and went on   ".to_string(), false),
            ("next line".to_string(), false),
        ];
        assert_eq!(join_rows(&rows), "a long line that wrapped   and went on\nnext line");
        // A long row without the wrap mark is a line of its own (GTK would join it).
        let rows = vec![("x".repeat(100), false), ("y".repeat(100), false)];
        assert_eq!(join_rows(&rows), format!("{}\n{}", "x".repeat(100), "y".repeat(100)));
        // The last row is trimmed even when marked as wrapped (its continuation is not selected).
        assert_eq!(join_rows(&[("tail   ".to_string(), true)]), "tail");
        assert_eq!(join_rows(&[]), "");
    }

    #[test]
    fn blank_edges_go_but_inner_blank_lines_stay() {
        assert_eq!(trim_blank_lines("  \n  \nhello\nworld\n  \n  "), "hello\nworld");
        assert_eq!(trim_blank_lines("hello\n\nworld"), "hello\n\nworld");
        assert_eq!(trim_blank_lines("  \n  "), "");
        assert_eq!(trim_blank_lines(""), "");
    }
}
