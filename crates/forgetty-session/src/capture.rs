//! `capture`: the visible screen of a pane as text (AD-019, cold path).
//!
//! A `forgetty-vt` terminal is built from the pane's byte-log ring, fed, read
//! and dropped. Nothing is kept between calls, and nothing runs on the PTY
//! drain path.

use forgetty_vt::{Terminal, ANSI_PALETTE_BLACK};

/// Feed size per `vt_write` call.
const FEED_CHUNK: usize = 64 * 1024;

/// Scrollback of the throwaway terminal: none. Capture reads the visible
/// screen only, and a scrollback makes libghostty allocate, prune and recycle
/// pages for every line that scrolls off (10 MiB by default).
const SCRATCH_SCROLLBACK: usize = 0;

/// Most bytes of the ring that capture parses. The screen is a function of the
/// last full redraw, so the older part of a long ring does not change it, but
/// it costs time and memory. A pane that wrote more than this since its last
/// full-screen clear shows what the tail paints.
pub const TAIL_CAP: usize = 2 * 1024 * 1024;

/// Sequences that start a screen from scratch: reset (RIS), erase display,
/// and the alternate screen switches.
const RESET_MARKERS: [&[u8]; 4] = [b"\x1bc", b"\x1b[2J", b"\x1b[?1049h", b"\x1b[?1047h"];

/// The part of `replay` that capture parses: all of it up to [`TAIL_CAP`],
/// else the last [`TAIL_CAP`] bytes, cut at the last reset marker inside them
/// or, with none, after the first line break (and on a UTF-8 boundary).
pub fn replay_tail(replay: &[u8]) -> &[u8] {
    replay_tail_capped(replay, TAIL_CAP)
}

fn replay_tail_capped(replay: &[u8], cap: usize) -> &[u8] {
    if replay.len() <= cap {
        return replay;
    }
    let window = &replay[replay.len() - cap..];
    let mut start = None;
    for m in RESET_MARKERS {
        if let Some(i) = window.windows(m.len()).rposition(|w| w == m) {
            start = start.max(Some(i));
        }
    }
    let start = match start {
        Some(i) => i,
        None => {
            let mut i = window.iter().position(|&b| b == b'\n').map_or(0, |n| n + 1);
            while i < window.len() && (window[i] & 0xC0) == 0x80 {
                i += 1;
            }
            i
        }
    };
    &window[start..]
}

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
    let mut term =
        Terminal::with_scrollback(rows, cols, [ANSI_PALETTE_BLACK; 16], SCRATCH_SCROLLBACK);
    for chunk in replay_tail(replay).chunks(FEED_CHUNK) {
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
    fn long_ring_parses_only_the_tail() {
        // 6 MiB of short lines, then a clear and a screen. The result is the
        // same as for the screen alone, and no scrollback is kept.
        let mut ring = Vec::new();
        let mut n = 0u64;
        while ring.len() < 6 * 1024 * 1024 {
            ring.extend_from_slice(format!("{n}\r\n").as_bytes());
            n += 1;
        }
        ring.extend_from_slice(b"\x1b[2J\x1b[Hscreen one\r\nscreen two");
        let out = capture_text(&ring, 5, 40, true, None);
        assert_eq!(out, "screen one\nscreen two");
        // No clear in the tail: the cut falls after a line break, so the first
        // line is whole and the last lines are the ring's last lines.
        let mut ring = Vec::new();
        let mut n = 0u64;
        while ring.len() < 6 * 1024 * 1024 {
            ring.extend_from_slice(format!("line {n}\r\n").as_bytes());
            n += 1;
        }
        let out = capture_text(&ring, 3, 40, true, None);
        assert_eq!(out, format!("line {}\nline {}", n - 2, n - 1));
    }

    #[test]
    fn tail_cut_never_starts_inside_a_character() {
        let mut ring = "é".repeat(3 * 1024 * 1024).into_bytes();
        ring.extend_from_slice("\nend".as_bytes());
        let tail = replay_tail_capped(&ring, 1000);
        assert!(std::str::from_utf8(tail).is_ok());
        assert!(tail.ends_with(b"end"));
    }

    #[test]
    fn tail_starts_at_the_last_reset_marker() {
        let mut ring = vec![b'x'; 5000];
        ring.extend_from_slice(b"\x1b[2Jtext\r\n");
        ring.extend_from_slice(&[b'y'; 100]);
        let tail = replay_tail_capped(&ring, 1000);
        assert!(tail.starts_with(b"\x1b[2J"));
        assert_eq!(replay_tail_capped(&ring, 1_000_000).len(), ring.len());
    }

    #[test]
    fn utf8_text_is_kept() {
        let out = capture_text("héllo ✓\r\n".as_bytes(), 3, 20, true, None);
        assert_eq!(out, "héllo ✓");
    }
}
