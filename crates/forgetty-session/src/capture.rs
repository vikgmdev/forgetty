//! `capture`: the visible screen of a pane as text (AD-019, cold path).
//!
//! A `forgetty-vt` terminal is built from the pane's byte-log ring, fed, read
//! and dropped. Nothing is kept between calls, and nothing runs on the PTY
//! drain path.

use forgetty_vt::{Terminal, ANSI_PALETTE_BLACK};

/// Feed size per `vt_write` call.
const FEED_CHUNK: usize = 64 * 1024;

/// Most bytes of scrollback the capture terminal gets, for `last_lines` beyond
/// the screen height. libghostty counts scrollback in bytes of page memory.
const MAX_SCRATCH_SCROLLBACK: usize = 2 * 1024 * 1024;

/// Bytes of page memory budgeted for one scrollback cell, with slack for the
/// page granularity (see `Terminal::with_scrollback`).
const SCROLLBACK_BYTES_PER_CELL: usize = 24;

/// Sequences that clear the whole screen: reset (RIS) and erase display.
const RESET_MARKERS: [&[u8]; 2] = [b"\x1bc", b"\x1b[2J"];

/// Alternate screen mode switches (1049, 1047 and 47, set or reset).
const ALT_SCREEN_PREFIXES: [&[u8]; 3] = [b"\x1b[?1049", b"\x1b[?1047", b"\x1b[?47"];

/// Scrollback bytes for a request of `last_lines` on a `rows` x `cols` screen:
/// none unless the request reaches past the screen, then enough for the extra
/// lines, up to [`MAX_SCRATCH_SCROLLBACK`].
fn scrollback_for(rows: usize, cols: usize, last_lines: Option<usize>) -> usize {
    match last_lines {
        Some(n) if n > rows => ((n - rows) * cols * SCROLLBACK_BYTES_PER_CELL)
            .saturating_add(64 * 1024)
            .min(MAX_SCRATCH_SCROLLBACK),
        _ => 0,
    }
}

/// Where parsing starts. The whole ring when scrollback is wanted (history
/// before a clear is part of it). Otherwise the last full-screen clear, since
/// nothing before it shows on the screen, unless an alternate screen switch
/// follows it (leaving the alternate screen shows the older primary screen).
/// The ring start when there is no clear.
fn parse_start(replay: &[u8], want_scrollback: bool) -> usize {
    if want_scrollback {
        return 0;
    }
    let last = |pat: &[u8]| replay.windows(pat.len()).rposition(|w| w == pat);
    let Some(mark) = RESET_MARKERS.iter().filter_map(|m| last(m)).max() else { return 0 };
    let after = &replay[mark..];
    let switched = ALT_SCREEN_PREFIXES.iter().any(|p| after.windows(p.len()).any(|w| w == *p));
    if switched {
        0
    } else {
        mark
    }
}

/// Render `replay` (raw PTY bytes) on a `rows` x `cols` screen and return its
/// text.
///
/// - one output line per screen row, joined with `\n`
/// - `trim`: remove trailing whitespace from every line and trailing blank
///   lines from the end
/// - `last_lines`: keep only the last N lines (after `trim`). Past the screen
///   height the lines come from a scrollback sized for the request.
pub fn capture_text(
    replay: &[u8],
    rows: u16,
    cols: u16,
    trim: bool,
    last_lines: Option<usize>,
) -> String {
    let rows = rows.max(1) as usize;
    let cols = cols.max(1) as usize;
    let scrollback = scrollback_for(rows, cols, last_lines);
    let start = parse_start(replay, scrollback > 0);
    let mut term = Terminal::with_scrollback(rows, cols, [ANSI_PALETTE_BLACK; 16], scrollback);
    for chunk in replay[start..].chunks(FEED_CHUNK) {
        term.feed(chunk);
    }
    let mut lines: Vec<String> = Vec::with_capacity(rows);
    if scrollback > 0 {
        collect_with_history(&mut term, rows, &mut lines);
    } else {
        collect_viewport(&term, &mut lines);
    }
    if trim {
        for line in &mut lines {
            line.truncate(line.trim_end().len());
        }
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

/// Append the rows now in the viewport.
fn collect_viewport(term: &Terminal, lines: &mut Vec<String>) {
    let screen = term.screen();
    for r in 0..screen.rows() {
        let mut line = String::new();
        for cell in screen.row(r) {
            line.push_str(&cell.grapheme);
        }
        lines.push(line);
    }
}

/// Append every line, scrollback first: the viewport is moved to the top and
/// down a page at a time.
fn collect_with_history(term: &mut Terminal, rows: usize, lines: &mut Vec<String>) {
    let _ = term.screen(); // refreshes the scrollbar
    let (total, _, _) = term.scrollbar_state();
    let total = total as usize;
    if total <= rows {
        collect_viewport(term, lines);
        return;
    }
    term.scroll_viewport_delta(isize::MIN / 2);
    let _ = term.screen();
    let mut have = 0usize;
    // A page at a time; the last page overlaps the one before it.
    for _ in 0..=(total / rows + 1) {
        let _ = term.screen();
        let (_, offset, _) = term.scrollbar_state();
        let offset = offset as usize;
        let mut page = Vec::with_capacity(rows);
        collect_viewport(term, &mut page);
        let skip = have.saturating_sub(offset);
        lines.extend(page.into_iter().skip(skip));
        have = lines.len();
        if offset + rows >= total {
            break;
        }
        term.scroll_viewport_delta(rows as isize);
    }
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

    /// A vim-like screen: drawn once, then only cursor-addressed updates.
    fn vim_like_ring(updates_bytes: usize) -> Vec<u8> {
        let mut ring = Vec::new();
        ring.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
        for r in 1..=24 {
            ring.extend_from_slice(format!("\x1b[{r};1Hrow {r} static text").as_bytes());
        }
        let mut i = 0u64;
        let start = ring.len();
        while ring.len() - start < updates_bytes {
            // Rewrite one cell of row 24 column 60.. only.
            ring.extend_from_slice(format!("\x1b[24;60H{:>8}", i % 100_000_000).as_bytes());
            i += 1;
        }
        ring
    }

    #[test]
    fn a_screen_drawn_once_survives_megabytes_of_small_updates() {
        let ring = vim_like_ring(5 * 1024 * 1024);
        let out = capture_text(&ring, 24, 80, true, None);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 24);
        for (i, l) in lines.iter().take(23).enumerate() {
            assert_eq!(*l, format!("row {} static text", i + 1));
        }
        assert!(lines[23].starts_with("row 24 static text"));
        // The updates stay whole too.
        assert!(lines[23].trim_end().len() > "row 24 static text".len());
    }

    #[test]
    fn last_lines_past_the_screen_comes_from_history() {
        let mut ring = Vec::new();
        for i in 0..3000 {
            ring.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        ring.extend_from_slice(b"last");
        let out = capture_text(&ring, 24, 80, true, Some(500));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 500);
        assert_eq!(lines[0], "line 2501");
        assert_eq!(lines[498], "line 2999");
        assert_eq!(lines[499], "last");
        // Without history: only the screen.
        assert_eq!(capture_text(&ring, 24, 80, true, Some(24)).lines().count(), 24);
        assert_eq!(capture_text(&ring, 24, 80, true, None).lines().count(), 24);
    }

    #[test]
    fn last_lines_on_a_wide_pane_is_still_whole_up_to_the_cap() {
        let mut ring = Vec::new();
        for i in 0..2000 {
            ring.extend_from_slice(format!("{i} {}\r\n", "w".repeat(150)).as_bytes());
        }
        let out = capture_text(&ring, 24, 200, true, Some(500));
        assert_eq!(out.lines().count(), 500);
    }

    #[test]
    fn a_clear_cuts_the_parse_unless_the_alternate_screen_switches_after_it() {
        let mut ring = vec![b'x'; 5000];
        ring.extend_from_slice(b"\x1b[2Jtext");
        assert_eq!(parse_start(&ring, false), 5000);
        assert_eq!(parse_start(&ring, true), 0);
        ring.extend_from_slice(b"\x1b[?1049l");
        assert_eq!(parse_start(&ring, false), 0);
        assert_eq!(parse_start(b"plain", false), 0);
    }

    #[test]
    fn leaving_the_alternate_screen_shows_the_primary_screen() {
        let out =
            capture_text(b"\x1b[2Jprompt $\x1b[?1049h\x1b[2Jvim\x1b[?1049l", 4, 20, true, None);
        assert_eq!(out, "prompt $");
    }

    #[test]
    fn utf8_text_is_kept() {
        let out = capture_text("héllo ✓\r\n".as_bytes(), 3, 20, true, None);
        assert_eq!(out, "héllo ✓");
    }
}
