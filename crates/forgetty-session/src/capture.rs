//! `capture`: the visible screen of a pane as text (AD-019, cold path).
//!
//! A `forgetty-vt` terminal is built from the pane's byte-log ring, fed, read
//! and dropped. Nothing is kept between calls, and nothing runs on the PTY
//! drain path.

use std::time::{Duration, Instant};

use forgetty_vt::{Terminal, ANSI_PALETTE_BLACK};

/// Feed size per `vt_write` call for an 80x24 pane. The deadline is checked
/// between calls, so a call must stay cheap: a few KiB of `ESC[2J` on a huge
/// pane costs seconds. [`feed_chunk`] scales this to the pane.
const FEED_CHUNK_80X24: usize = 4 * 1024;
/// Floor of the chunk. DECALN (`ESC # 8`, 3 bytes) fills the whole screen, so a
/// chunk of 8 bytes is at most two fills.
const FEED_CHUNK_MIN: usize = 8;

/// Bytes per `vt_write` call: [`FEED_CHUNK_80X24`] scaled by the cell count of
/// the pane against 80x24, so one call costs about 10 ms at worst.
fn feed_chunk(rows: usize, cols: usize) -> usize {
    let cells = rows.saturating_mul(cols).max(1);
    (FEED_CHUNK_80X24 * (80 * 24) / cells).clamp(FEED_CHUNK_MIN, FEED_CHUNK_80X24)
}

/// How long capture may feed the ring. When it runs out the screen as far as
/// it got is returned, marked `incomplete`.
pub const CAPTURE_DEADLINE: Duration = Duration::from_secs(1);

/// A captured screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub text: String,
    /// The [`CAPTURE_DEADLINE`] ran out before the whole ring was read (also
    /// after the retry from the last clear). The screen may be older than the
    /// current one.
    pub incomplete: bool,
}

/// Most bytes of scrollback the capture terminal gets, for `last_lines` beyond
/// the screen height. libghostty counts scrollback in bytes of page memory.
const MAX_SCRATCH_SCROLLBACK: usize = 2 * 1024 * 1024;

/// Bytes of page memory budgeted for one scrollback cell, with slack for the
/// page granularity (see `Terminal::with_scrollback`).
const SCROLLBACK_BYTES_PER_CELL: usize = 24;

/// RIS: resets the screen and every mode.
const RIS: &[u8] = b"\x1bc";

/// Cursor home, then erase display.
const HOME_CLEAR: &[u8] = b"\x1b[H\x1b[2J";

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

/// Where `buf` first changes state that a clear does not reset and that
/// changes how later text lands: a scroll region (DECSTBM), a charset
/// designation or shift, or auto-wrap (DECAWM).
fn first_persistent_change(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            0x0e | 0x0f => return Some(i),
            0x1b => match buf.get(i + 1) {
                Some(b'(' | b')' | b'*' | b'+') => return Some(i),
                Some(b'[') => {
                    let mut j = i + 2;
                    let private = buf.get(j) == Some(&b'?');
                    if private {
                        j += 1;
                    }
                    let params_at = j;
                    while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                        j += 1;
                    }
                    if let Some(&fin) = buf.get(j) {
                        if !private && fin == b'r' {
                            return Some(i);
                        }
                        if private
                            && (fin == b'h' || fin == b'l')
                            && buf[params_at..j].split(|&b| b == b';').any(|p| p == b"7")
                        {
                            return Some(i);
                        }
                    }
                    i = j.max(i + 1);
                    continue;
                }
                _ => {}
            },
            _ => {}
        }
        i += 1;
    }
    None
}

/// The cut rules, shared by the first pass and the retry. The last RIS (it
/// resets every mode, so nothing before it matters) or the last `ESC[H ESC[2J`
/// at or after `min_pos`. The `ESC[H ESC[2J` counts only when no scroll region,
/// charset or auto-wrap change comes before it in the ring (a clear does not
/// reset those, so text after it would land differently). Neither counts when
/// an alternate screen switch follows it: leaving the alternate screen shows
/// the older primary screen. A bare `ESC[2J` is never a cut. `None` if no cut
/// is valid.
fn find_cut(
    replay: &[u8],
    min_pos: usize,
    first_change: &std::cell::OnceCell<Option<usize>>,
) -> Option<usize> {
    let tail = &replay[min_pos.min(replay.len())..];
    let last = |pat: &[u8]| tail.windows(pat.len()).rposition(|w| w == pat).map(|i| i + min_pos);
    let ris = last(RIS);
    // The scan of the whole ring is done once, for both passes.
    let clear = last(HOME_CLEAR).filter(|&m| {
        let first = *first_change.get_or_init(|| first_persistent_change(replay));
        first.is_none_or(|c| c >= m)
    });
    let mark = ris.max(clear)?;
    let after = &replay[mark..];
    let switched = ALT_SCREEN_PREFIXES.iter().any(|p| after.windows(p.len()).any(|w| w == *p));
    (!switched).then_some(mark)
}

/// Where parsing starts. The whole ring when scrollback is wanted (history
/// before a clear is part of it). Otherwise the last valid cut ([`find_cut`]),
/// or the ring start.
fn parse_start(
    replay: &[u8],
    want_scrollback: bool,
    first_change: &std::cell::OnceCell<Option<usize>>,
) -> usize {
    if want_scrollback {
        return 0;
    }
    find_cut(replay, 0, first_change).unwrap_or(0)
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
    capture(replay, rows, cols, trim, last_lines).text
}

/// [`capture_text`] with the [`CAPTURE_DEADLINE`]: says whether it ran out.
pub fn capture(
    replay: &[u8],
    rows: u16,
    cols: u16,
    trim: bool,
    last_lines: Option<usize>,
) -> Captured {
    capture_with_deadline(replay, rows, cols, trim, last_lines, CAPTURE_DEADLINE)
}

pub fn capture_with_deadline(
    replay: &[u8],
    rows: u16,
    cols: u16,
    trim: bool,
    last_lines: Option<usize>,
    deadline: Duration,
) -> Captured {
    let rows = rows.max(1) as usize;
    let cols = cols.max(1) as usize;
    let scrollback = scrollback_for(rows, cols, last_lines);
    let begun = Instant::now();
    // The deadline is for the whole call. Feeding stops early enough to read
    // the screen and free the terminal: that work grows with the cell count
    // ([`read_reserve`]). The first pass gets three quarters of the feed time,
    // so the retry has time left.
    let feed_end = begun + deadline.saturating_sub(read_reserve(rows, cols));
    let first_until = begun + (feed_end - begun).mul_f32(FIRST_PASS_SHARE);
    let first_change = std::cell::OnceCell::new();
    let start = parse_start(replay, scrollback > 0, &first_change);
    let (mut term, mut ran_out) = feed(replay, start, rows, cols, scrollback, first_until);
    if ran_out && scrollback == 0 {
        // Retry once from a valid cut ([`find_cut`]) in the final quarter of
        // the ring, with what is left of the budget: the screen is rebuilt
        // from there, which is cheap when the ring is full of redraws. No cut,
        // or no time left to build another terminal: the first pass stays and
        // the result is incomplete. Not with scrollback: the history before the
        // clear is what was asked for.
        let from = replay.len() - replay.len() / 4;
        if let Some(cut) = find_cut(replay, from, &first_change) {
            if Instant::now() + terminal_cost(rows, cols) < feed_end {
                drop(term);
                let (t, out) = feed(replay, cut, rows, cols, 0, feed_end);
                term = t;
                ran_out = out;
            }
        }
    }
    let mut lines: Vec<String> = Vec::with_capacity(rows);
    if scrollback > 0 {
        ran_out |= collect_with_history(&mut term, rows, &mut lines, begun + deadline);
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
    Captured { text: lines.join("\n"), incomplete: ran_out }
}

/// Share of the feed time for the first pass over the ring.
const FIRST_PASS_SHARE: f32 = 0.75;

/// Time kept back from the deadline for reading the screen and dropping the
/// terminal, which grow with the cell count (about 0.1 ms per 1000 cells in a
/// release build, more in a debug build), and for the last chunk fed.
fn read_reserve(rows: usize, cols: usize) -> Duration {
    Duration::from_nanos((rows.saturating_mul(cols) as u64).saturating_mul(READ_NS_PER_CELL))
        + Duration::from_millis(5)
}

/// What building a terminal of this size costs, about.
fn terminal_cost(rows: usize, cols: usize) -> Duration {
    Duration::from_nanos((rows.saturating_mul(cols) as u64).saturating_mul(NEW_NS_PER_CELL))
}

const READ_NS_PER_CELL: u64 = 250;
const NEW_NS_PER_CELL: u64 = 100;

/// Feed `replay[start..]` to a new terminal until `until`. True if it ran out.
fn feed(
    replay: &[u8],
    start: usize,
    rows: usize,
    cols: usize,
    scrollback: usize,
    until: Instant,
) -> (Terminal, bool) {
    let mut term = Terminal::with_scrollback(rows, cols, [ANSI_PALETTE_BLACK; 16], scrollback);
    for chunk in replay[start..].chunks(feed_chunk(rows, cols)) {
        if Instant::now() >= until {
            return (term, true);
        }
        term.feed(chunk);
    }
    (term, false)
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
/// True if `until` passed before every page was read.
fn collect_with_history(
    term: &mut Terminal,
    rows: usize,
    lines: &mut Vec<String>,
    until: Instant,
) -> bool {
    let _ = term.screen(); // refreshes the scrollbar
    let (total, _, _) = term.scrollbar_state();
    let total = total as usize;
    if total <= rows {
        collect_viewport(term, lines);
        return false;
    }
    term.scroll_viewport_delta(isize::MIN / 2);
    let _ = term.screen();
    let mut have = 0usize;
    // A page at a time; the last page overlaps the one before it.
    for _ in 0..=(total / rows + 1) {
        if Instant::now() >= until {
            return true;
        }
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
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that parse megabytes or fill huge panes take turns: the deadline
    /// tests measure wall time and the others must not eat their CPU.
    static HEAVY: Mutex<()> = Mutex::new(());

    fn heavy() -> std::sync::MutexGuard<'static, ()> {
        HEAVY.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// For tests of what the screen shows, not of the deadline.
    fn start_of(replay: &[u8], want_scrollback: bool) -> usize {
        parse_start(replay, want_scrollback, &std::cell::OnceCell::new())
    }

    /// Bound on a whole capture. Generous by default, the deadline plus 150 ms
    /// with `FORGETTY_STRICT_TIMING=1` (run alone, on a quiet machine).
    fn bound() -> Duration {
        if std::env::var_os("FORGETTY_STRICT_TIMING").is_some() {
            CAPTURE_DEADLINE + Duration::from_millis(150)
        } else {
            Duration::from_millis(1500)
        }
    }

    fn no_deadline() -> Duration {
        Duration::from_secs(60)
    }

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
        let _heavy = heavy();
        let ring = vim_like_ring(5 * 1024 * 1024);
        let out = capture_with_deadline(&ring, 24, 80, true, None, no_deadline()).text;
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
        let _heavy = heavy();
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
        let _heavy = heavy();
        let mut ring = Vec::new();
        for i in 0..2000 {
            ring.extend_from_slice(format!("{i} {}\r\n", "w".repeat(150)).as_bytes());
        }
        let out = capture_text(&ring, 24, 200, true, Some(500));
        assert_eq!(out.lines().count(), 500);
    }

    #[test]
    fn only_ris_or_home_clear_cut_the_parse() {
        let mut ring = vec![b'x'; 5000];
        // A bare erase display is no mark.
        ring.extend_from_slice(b"\x1b[2Jtext");
        assert_eq!(start_of(&ring, false), 0);
        // Home + erase is, when nothing before it changes persistent modes.
        ring.extend_from_slice(b"\x1b[H\x1b[2Jmore");
        let mark = ring.len() - "\x1b[H\x1b[2Jmore".len();
        assert_eq!(start_of(&ring, false), mark);
        assert_eq!(start_of(&ring, true), 0);
        // RIS is.
        ring.extend_from_slice(b"\x1bcafter");
        assert_eq!(start_of(&ring, false), ring.len() - "\x1bcafter".len());
        // An alternate screen switch after the mark: no cut.
        ring.extend_from_slice(b"\x1b[?1049l");
        assert_eq!(start_of(&ring, false), 0);
        assert_eq!(start_of(b"plain", false), 0);
    }

    #[test]
    fn a_persistent_mode_before_the_clear_means_no_cut() {
        for before in [
            &b"\x1b[1;23r"[..],
            b"\x1b[r",
            b"\x1b(0",
            b"\x1b)B",
            b"\x0e",
            b"\x1b[?7l",
            b"\x1b[?25;7h",
        ] {
            let mut ring = before.to_vec();
            ring.extend_from_slice(b"text\x1b[H\x1b[2Jnew");
            assert_eq!(start_of(&ring, false), 0, "{before:?}");
        }
        // Other CSI sequences do not count.
        let ring = b"\x1b[31m\x1b[?25l\x1b[10;5Htext\x1b[H\x1b[2Jnew".to_vec();
        assert_eq!(start_of(&ring, false), ring.len() - "\x1b[H\x1b[2Jnew".len());
    }

    /// apt: a scroll region keeps the last row for a status line. A clear does
    /// not reset the region, so the text after it must scroll inside it.
    #[test]
    fn an_apt_style_scroll_region_and_status_line_survive_a_clear() {
        let mut ring = Vec::new();
        ring.extend_from_slice(b"\x1b[1;23r\x1b[24;1Hstatus A\x1b[23;1Hold\r\n");
        ring.extend_from_slice(b"\x1b[H\x1b[2J");
        ring.extend_from_slice(b"\x1b[24;1HProgress: 50%\x1b[23;1Hl1\r\nl2\r\nl3");
        let out = capture_text(&ring, 24, 40, true, None);
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 24);
        assert_eq!(&lines[20..], ["l1", "l2", "l3", "Progress: 50%"]);
    }

    #[test]
    fn a_timeout_retries_from_the_last_clear_in_the_final_quarter() {
        let _heavy = heavy();
        // Redraw spam that cannot be read in time, then a clear and a short
        // screen at the end. The retry starts at that clear and finishes.
        let mut ring = b"\x1b[2J".repeat(1_500_000);
        let marker_at = ring.len();
        ring.extend_from_slice(b"\x1b[H\x1b[2J");
        for i in 0..2000 {
            ring.extend_from_slice(format!("tail line {i}\r\n").as_bytes());
        }
        assert!(marker_at >= ring.len() / 4 * 3);
        let t0 = Instant::now();
        let got = capture(&ring, 1000, 1000, true, Some(3));
        assert!(t0.elapsed() < bound(), "took {:?}", t0.elapsed());
        assert!(!got.incomplete, "the retry should finish");
        assert!(got.text.contains("tail line 1999"), "{:?}", got.text);
        // No clear in the final quarter: no retry, still incomplete.
        let mut spam = b"\x1b[2J".repeat(1_500_000);
        spam.extend_from_slice(&vec![b'x'; 4_000_000]);
        assert!(capture(&spam, 1000, 1000, true, None).incomplete);
    }

    #[test]
    fn a_ring_of_bare_clears_on_a_huge_pane_answers_in_time_and_says_incomplete() {
        let _heavy = heavy();
        // A bare `ESC[2J` is no cut: the screen cannot be rebuilt from there.
        let ring = b"\x1b[2J".repeat(1_500_000); // 7.5 MB
        let t0 = Instant::now();
        let got = capture(&ring, 1000, 1000, true, None);
        let took = t0.elapsed();
        assert!(took < bound(), "took {took:?}");
        assert!(got.incomplete);
        // A short ring is complete.
        assert!(!capture(b"hi", 24, 80, true, None).incomplete);
    }

    #[test]
    fn a_decaln_flood_on_a_huge_pane_answers_in_time() {
        let _heavy = heavy();
        // `ESC # 8` fills the whole screen with E.
        let ring = b"\x1b#8".repeat(2_500_000);
        let t0 = Instant::now();
        let got = capture(&ring, 1000, 1000, true, None);
        let took = t0.elapsed();
        assert!(took < bound(), "took {took:?}");
        assert!(got.incomplete);
    }

    /// htop or vim redraws past the first pass, then the user quits. The
    /// screen is the shell's, which the redraws do not show: no retry from a
    /// clear inside the alternate screen.
    #[test]
    fn the_retry_does_not_cut_inside_an_alternate_screen_that_was_left() {
        let _heavy = heavy();
        let mut ring = b"$ before vim\r\n\x1b[?1049h".to_vec();
        ring.extend_from_slice(&b"\x1b[H\x1b[2Jredraw".repeat(900_000));
        ring.extend_from_slice(b"\x1b[?1049l$ ");
        let t0 = Instant::now();
        let got = capture(&ring, 1000, 1000, true, None);
        let took = t0.elapsed();
        assert!(took < bound(), "took {took:?}");
        assert!(got.incomplete, "a blank retry screen must not pass for complete");
        assert!(find_cut(&ring, ring.len() - ring.len() / 4, &Default::default()).is_none());
    }

    /// apt: a scroll region set before the redraws. A clear does not reset it,
    /// so a retry from the clear would lay the text out differently.
    #[test]
    fn the_retry_does_not_cut_after_a_scroll_region_change() {
        let _heavy = heavy();
        let mut ring = b"\x1b[1;999r".to_vec();
        ring.extend_from_slice(&b"\x1b[H\x1b[2Jredraw".repeat(900_000));
        ring.extend_from_slice(b"tail");
        let t0 = Instant::now();
        let got = capture(&ring, 1000, 1000, true, None);
        let took = t0.elapsed();
        assert!(took < bound(), "took {took:?}");
        assert!(got.incomplete);
        assert!(find_cut(&ring, ring.len() - ring.len() / 4, &Default::default()).is_none());
        // RIS resets the region: a cut at it is fine.
        ring.extend_from_slice(b"\x1bcfresh");
        assert!(find_cut(&ring, ring.len() - ring.len() / 4, &Default::default()).is_some());
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
