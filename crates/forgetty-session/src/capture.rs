//! `capture`: the visible screen of a pane as text (AD-019, cold path).
//!
//! A `forgetty-vt` terminal is built from the pane's byte-log ring, fed, read
//! and dropped. Nothing is kept between calls, and nothing runs on the PTY
//! drain path.

use forgetty_vt::{Terminal, ANSI_PALETTE_BLACK};

/// Feed size per `vt_write` call.
const FEED_CHUNK: usize = 64 * 1024;

/// Render `replay` (raw PTY bytes) on a `rows` x `cols` screen and return its
/// text.
///
/// - one output line per screen row, joined with `\n`
/// - `trim`: remove trailing whitespace from every line and trailing blank
///   lines from the end
/// - `last_lines`: keep only the last N lines (after `trim`)
pub fn capture_text(
    replay: &[u8],
    rows: u16,
    cols: u16,
    trim: bool,
    last_lines: Option<usize>,
) -> String {
    let rows = rows.max(1) as usize;
    let cols = cols.max(1) as usize;
    let mut term = Terminal::new(rows, cols, [ANSI_PALETTE_BLACK; 16]);
    for chunk in replay.chunks(FEED_CHUNK) {
        term.feed(chunk);
    }
    let screen = term.screen();
    let mut lines: Vec<String> = Vec::with_capacity(rows);
    for r in 0..screen.rows() {
        let mut line = String::new();
        for cell in screen.row(r) {
            line.push_str(&cell.grapheme);
        }
        if trim {
            line.truncate(line.trim_end().len());
        }
        lines.push(line);
    }
    if trim {
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
    }
    if let Some(n) = last_lines {
        let keep_from = lines.len().saturating_sub(n);
        lines.drain(..keep_from);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_lands_on_the_screen() {
        let out = capture_text(b"hello\r\nworld\r\n", 5, 20, true, None);
        assert_eq!(out, "hello\nworld");
    }

    #[test]
    fn untrimmed_keeps_the_full_grid() {
        let out = capture_text(b"hi", 3, 4, false, None);
        assert_eq!(out, "hi  \n    \n    ");
    }

    #[test]
    fn colours_and_cursor_moves_are_resolved() {
        let out = capture_text(b"\x1b[31mred\x1b[0m\r\n\x1b[2Ccol", 4, 20, true, None);
        assert_eq!(out, "red\n  col");
    }

    #[test]
    fn clear_screen_removes_earlier_text() {
        let out = capture_text(b"old\r\n\x1b[2J\x1b[Hnew", 4, 20, true, None);
        assert_eq!(out, "new");
    }

    #[test]
    fn last_lines_keeps_the_tail() {
        let out = capture_text(b"a\r\nb\r\nc\r\nd", 6, 10, true, Some(2));
        assert_eq!(out, "c\nd");
        let out = capture_text(b"a\r\nb", 6, 10, true, Some(10));
        assert_eq!(out, "a\nb");
        let out = capture_text(b"a\r\nb", 6, 10, true, Some(0));
        assert_eq!(out, "");
    }

    #[test]
    fn scrolled_off_lines_are_not_visible() {
        let out = capture_text(b"1\r\n2\r\n3\r\n4\r\n5", 3, 10, true, None);
        assert_eq!(out, "3\n4\n5");
    }

    #[test]
    fn empty_replay_is_empty_when_trimmed() {
        assert_eq!(capture_text(b"", 24, 80, true, None), "");
    }

    #[test]
    fn osc_sequences_do_not_show() {
        let out = capture_text(b"\x1b]2;title\x07\x1b]7501;state=done\x07text", 3, 20, true, None);
        assert_eq!(out, "text");
    }

    #[test]
    fn utf8_text_is_kept() {
        let out = capture_text("héllo ✓\r\n".as_bytes(), 3, 20, true, None);
        assert_eq!(out, "héllo ✓");
    }
}
