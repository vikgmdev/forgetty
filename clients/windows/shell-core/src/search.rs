//! In-terminal search over the whole scrollback. No UI and no terminal types: the caller hands
//! in a row reader, so the same code runs against libghostty rows and against test strings.
//!
//! The scan is a job that runs in slices (`SearchJob::step` with a time budget). The UI thread
//! runs one slice, paints and handles input, then runs the next, so typing never waits for a
//! big scrollback to be read. GTK scans in one blocking pass; the matching rules are the same
//! (case-insensitive, every occurrence), except that matches do not overlap.

use std::time::{Duration, Instant};

/// More matches than this are counted but not kept (the label shows "N+").
pub const MAX_MATCHES: usize = 100_000;

/// A match: `len` cells starting at `col` in the absolute row `row`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Match {
    pub row: u32,
    pub col: u16,
    pub len: u16,
}

/// The result of reading one row: `text` and `cols` as `forgetty_vt::RowText` documents them.
/// `cols` has one entry per char of `text`, plus a last entry holding the row width.
pub trait RowReader {
    /// Fill `text` and `cols` with row `abs`. False when the row does not exist.
    fn read(&mut self, abs: usize, text: &mut String, cols: &mut Vec<u16>) -> bool;
}

impl<F: FnMut(usize, &mut String, &mut Vec<u16>) -> bool> RowReader for F {
    fn read(&mut self, abs: usize, text: &mut String, cols: &mut Vec<u16>) -> bool {
        self(abs, text, cols)
    }
}

/// Lower-case a query for matching. Empty and blank-only queries search nothing.
pub fn fold_query(q: &str) -> Option<String> {
    let f: String = q.chars().flat_map(char::to_lowercase).collect();
    (!f.trim().is_empty()).then_some(f)
}

pub struct SearchJob {
    needle: String,
    next: usize,
    total: usize,
    pub matches: Vec<Match>,
    /// Matches found beyond `MAX_MATCHES` (not stored).
    pub overflow: usize,
    text: String,
    cols: Vec<u16>,
    folded: String,
    /// For a non-ASCII row: the char index in `text` of every byte of `folded`.
    byte_char: Vec<u32>,
}

impl SearchJob {
    /// A job over rows `0..total`. `None` when the query is empty.
    pub fn new(query: &str, total: usize) -> Option<Self> {
        Some(Self {
            needle: fold_query(query)?,
            next: 0,
            total,
            matches: Vec::new(),
            overflow: 0,
            text: String::new(),
            cols: Vec::new(),
            folded: String::new(),
            byte_char: Vec::new(),
        })
    }

    pub fn done(&self) -> bool {
        self.next >= self.total
    }

    /// Rows read so far and rows to read.
    pub fn progress(&self) -> (usize, usize) {
        (self.next.min(self.total), self.total)
    }

    pub fn found(&self) -> usize {
        self.matches.len() + self.overflow
    }

    /// Output arrived: the buffer now has `new_total` rows. Keep what the job found in rows
    /// before `rescan_from` and read again from there (the live screen can change in place, so
    /// the caller passes the first row of the screen as it was). A running job keeps its place
    /// when it has not reached that row. Returns false when the job cannot be continued (the
    /// buffer shrank, or matches were dropped for the cap): the caller starts a new one.
    pub fn extend(&mut self, new_total: usize, rescan_from: usize) -> bool {
        if new_total < self.total || self.overflow > 0 {
            return false;
        }
        let from = rescan_from.min(self.total);
        let keep = self.matches.partition_point(|m| (m.row as usize) < from);
        self.matches.truncate(keep);
        self.next = self.next.min(from);
        self.total = new_total;
        true
    }

    /// Read rows until the budget is used or the job is done. Returns true when done. At least
    /// one row is read per call, so a zero budget still makes progress.
    pub fn step(&mut self, reader: &mut impl RowReader, budget: Duration) -> bool {
        let t0 = Instant::now();
        let mut n = 0usize;
        while self.next < self.total {
            let abs = self.next;
            self.next += 1;
            if reader.read(abs, &mut self.text, &mut self.cols) {
                self.scan_row(abs);
            }
            n += 1;
            // Looking at the clock costs more than reading a short row: check every 32 rows.
            if n % 32 == 0 && t0.elapsed() >= budget {
                break;
            }
        }
        self.done()
    }

    fn scan_row(&mut self, abs: usize) {
        if self.text.is_empty() {
            return;
        }
        let ascii = self.text.is_ascii();
        self.folded.clear();
        if ascii {
            self.folded.extend(self.text.chars().map(|c| c.to_ascii_lowercase()));
        } else {
            self.byte_char.clear();
            for (i, ch) in self.text.chars().enumerate() {
                for lc in ch.to_lowercase() {
                    let before = self.folded.len();
                    self.folded.push(lc);
                    for _ in before..self.folded.len() {
                        self.byte_char.push(i as u32);
                    }
                }
            }
        }
        let width = self.cols.last().copied().unwrap_or(0);
        let mut from = 0;
        while let Some(pos) = self.folded[from..].find(&self.needle) {
            let b0 = from + pos;
            let b1 = b0 + self.needle.len();
            // Char index of the first and the last matched char in `text`.
            let (c0, c1) = if ascii { (b0, b1 - 1) } else { (self.byte_char[b0] as usize, self.byte_char[b1 - 1] as usize) };
            let col0 = self.cols.get(c0).copied().unwrap_or(0);
            // The match ends where the next cell starts. A wide glyph covers two cells, and
            // combining chars share their base char's column: look past them.
            let base = self.cols.get(c1).copied().unwrap_or(0);
            let mut k = c1 + 1;
            while k + 1 < self.cols.len() && self.cols[k] == base {
                k += 1;
            }
            let end = self.cols.get(k).copied().unwrap_or(width);
            let m = Match { row: abs as u32, col: col0, len: end.saturating_sub(col0).max(1) };
            if self.matches.len() < MAX_MATCHES {
                self.matches.push(m);
            } else {
                self.overflow += 1;
            }
            from = b1;
            if from >= self.folded.len() {
                break;
            }
        }
    }
}

/// Index of the first match at or after `row`; wraps to 0 past the last. `None` when empty.
pub fn first_at_or_after(matches: &[Match], row: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let i = matches.partition_point(|m| (m.row as usize) < row);
    Some(if i >= matches.len() { 0 } else { i })
}

/// The matches that start in rows `top..top + rows`. `matches` is sorted by row, then column.
pub fn visible(matches: &[Match], top: usize, rows: usize) -> &[Match] {
    let a = matches.partition_point(|m| (m.row as usize) < top);
    let b = matches.partition_point(|m| (m.row as usize) < top + rows);
    &matches[a..b]
}

pub fn next_index(cur: usize, count: usize) -> usize {
    if count == 0 {
        0
    } else {
        (cur + 1) % count
    }
}

pub fn prev_index(cur: usize, count: usize) -> usize {
    if count == 0 {
        0
    } else {
        (cur + count - 1) % count
    }
}

/// The viewport offset that puts `row` a third of the way down the viewport (the GTK rule),
/// clamped to the scrollable range.
pub fn offset_for(row: usize, viewport_rows: usize, total: usize) -> usize {
    let max = total.saturating_sub(viewport_rows);
    row.saturating_sub(viewport_rows / 3).min(max)
}

/// True when `row` is inside the viewport that starts at `offset`.
pub fn in_view(row: usize, offset: usize, viewport_rows: usize) -> bool {
    row >= offset && row < offset + viewport_rows
}

/// The label next to the search field: "3 of 15", "0 of 0", "Searching... 12 found".
pub fn label(job_running: bool, query_empty: bool, current: usize, found: usize, overflow: usize) -> String {
    if query_empty {
        return String::new();
    }
    if job_running {
        return format!("Searching... {found} found");
    }
    if found == 0 {
        return "0 of 0".to_string();
    }
    let plus = if overflow > 0 { "+" } else { "" };
    format!("{} of {}{plus}", current + 1, found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader over plain strings: one char per column, padded with spaces to `width` like a
    /// terminal row.
    fn reader<'a>(rows: &'a [&'a str], width: u16) -> impl RowReader + 'a {
        move |abs: usize, text: &mut String, cols: &mut Vec<u16>| {
            text.clear();
            cols.clear();
            let Some(r) = rows.get(abs) else { return false };
            for (i, ch) in r.chars().chain(std::iter::repeat(' ')).take(width as usize).enumerate() {
                text.push(ch);
                cols.push(i as u16);
            }
            cols.push(width);
            true
        }
    }

    fn run(q: &str, rows: &[&str]) -> Vec<Match> {
        let mut job = SearchJob::new(q, rows.len()).unwrap();
        assert!(job.step(&mut reader(rows, 40), Duration::from_secs(5)));
        job.matches
    }

    #[test]
    fn finds_every_occurrence_ignoring_case() {
        let m = run("Foo", &["a foo b FOO", "nothing", "fOo"]);
        assert_eq!(
            m,
            vec![
                Match { row: 0, col: 2, len: 3 },
                Match { row: 0, col: 8, len: 3 },
                Match { row: 2, col: 0, len: 3 },
            ]
        );
    }

    #[test]
    fn matches_do_not_overlap_and_blank_queries_search_nothing() {
        assert_eq!(run("aa", &["aaaaa"]).len(), 2);
        assert!(SearchJob::new("", 10).is_none());
        assert!(SearchJob::new("   ", 10).is_none());
        // A query with inner spaces is kept as typed.
        assert_eq!(run("a b", &["xa b a b"]).len(), 2);
    }

    #[test]
    fn columns_follow_wide_glyphs() {
        // "x日y": x at 0, the wide glyph at 1 (2 cells), y at 3.
        let mut rows_text = vec![];
        let mut job = SearchJob::new("日y", 1).unwrap();
        let mut rd = |abs: usize, text: &mut String, cols: &mut Vec<u16>| {
            if abs != 0 {
                return false;
            }
            text.clear();
            cols.clear();
            // Columns 4 to 9 are blank; the row is 10 wide.
            for (ch, c) in [('x', 0u16), ('日', 1), ('y', 3), (' ', 4), (' ', 5), (' ', 6), (' ', 7), (' ', 8), (' ', 9)] {
                text.push(ch);
                cols.push(c);
            }
            cols.push(10);
            true
        };
        assert!(job.step(&mut rd, Duration::from_secs(1)));
        rows_text.extend(job.matches.iter().copied());
        assert_eq!(rows_text, vec![Match { row: 0, col: 1, len: 3 }]);
        let mut job = SearchJob::new("日", 1).unwrap();
        assert!(job.step(&mut rd, Duration::from_secs(1)));
        assert_eq!(job.matches[0], Match { row: 0, col: 1, len: 2 });
    }

    #[test]
    fn non_ascii_case_folding_never_panics_and_maps_columns() {
        // 'İ' lower-cases to two chars; the match still lands on its own column.
        let m = run("zz", &["İİ zz"]);
        assert_eq!(m, vec![Match { row: 0, col: 3, len: 2 }]);
        let m = run("É", &["café É"]);
        assert_eq!(m, vec![Match { row: 0, col: 3, len: 1 }, Match { row: 0, col: 5, len: 1 }]);
    }

    #[test]
    fn a_job_runs_in_slices_and_resumes() {
        let rows: Vec<String> = (0..5000).map(|i| format!("row {i} needle")).collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let mut job = SearchJob::new("needle", refs.len()).unwrap();
        let mut rd = reader(&refs, 40);
        let mut slices = 0;
        // A zero budget still reads at least 32 rows per call.
        while !job.step(&mut rd, Duration::ZERO) {
            slices += 1;
            assert!(slices < 5000);
        }
        assert!(slices > 10, "the job must not finish in one call: {slices}");
        assert_eq!(job.matches.len(), 5000);
        assert_eq!(job.progress(), (5000, 5000));
    }

    #[test]
    fn the_match_list_is_capped_but_counted() {
        let rows: Vec<String> = (0..(MAX_MATCHES + 50)).map(|_| "x".to_string()).collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let mut job = SearchJob::new("x", refs.len()).unwrap();
        assert!(job.step(&mut reader(&refs, 5), Duration::from_secs(30)));
        assert_eq!(job.matches.len(), MAX_MATCHES);
        assert_eq!(job.overflow, 50);
        assert_eq!(job.found(), MAX_MATCHES + 50);
    }

    #[test]
    fn navigation_wraps_and_the_viewport_rule() {
        let m: Vec<Match> = [3u32, 10, 10, 40].iter().map(|&r| Match { row: r, col: 0, len: 1 }).collect();
        assert_eq!(first_at_or_after(&m, 0), Some(0));
        assert_eq!(first_at_or_after(&m, 10), Some(1));
        assert_eq!(first_at_or_after(&m, 11), Some(3));
        assert_eq!(first_at_or_after(&m, 41), Some(0), "wraps");
        assert_eq!(first_at_or_after(&[], 0), None);
        assert_eq!(visible(&m, 10, 5).len(), 2);
        assert_eq!(visible(&m, 11, 29).len(), 0);
        assert_eq!(visible(&m, 0, 100).len(), 4);
        assert_eq!(next_index(3, 4), 0);
        assert_eq!(prev_index(0, 4), 3);
        assert_eq!(next_index(0, 0), 0);
        // A third of the way down, never below the top or past the bottom.
        assert_eq!(offset_for(100, 30, 1000), 90);
        assert_eq!(offset_for(5, 30, 1000), 0);
        assert_eq!(offset_for(995, 30, 1000), 970);
        assert!(in_view(95, 90, 30) && !in_view(120, 90, 30));
    }

    #[test]
    fn labels() {
        assert_eq!(label(false, true, 0, 0, 0), "");
        assert_eq!(label(false, false, 0, 0, 0), "0 of 0");
        assert_eq!(label(false, false, 2, 15, 0), "3 of 15");
        assert_eq!(label(true, false, 0, 7, 0), "Searching... 7 found");
        assert_eq!(label(false, false, 0, 100_050, 50), "1 of 100050+");
    }

    /// The same job over a real terminal: rows in the scrollback, a wide glyph, no viewport move.
    #[test]
    fn over_real_libghostty_rows() {
        use forgetty_vt::{RowText, Terminal, ANSI_PALETTE_BLACK};
        let mut t = Terminal::with_scrollback(10, 40, [ANSI_PALETTE_BLACK; 16], 4 << 20);
        let mut feed = String::new();
        for i in 0..400 {
            feed.push_str(&format!("line {i} \u{65e5}\u{672c} Needle\r\n"));
        }
        t.feed(feed.as_bytes());
        let (total, off, _) = t.scrollbar_state();
        let mut rt = RowText::default();
        let mut rd = |abs: usize, text: &mut String, cols: &mut Vec<u16>| {
            if !t.row_text(abs, &mut rt) {
                return false;
            }
            std::mem::swap(text, &mut rt.text);
            std::mem::swap(cols, &mut rt.cols);
            true
        };
        let mut job = SearchJob::new("needle", total as usize).unwrap();
        assert!(job.step(&mut rd, Duration::from_secs(5)));
        assert_eq!(job.matches.len(), 400);
        // "line 0 " is 7 cells, then two wide glyphs (4 cells), a space, then the word.
        let m = job.matches[0];
        assert_eq!((m.col, m.len), (12, 6));
        assert_eq!(t.scrollbar_state().1, off);
    }

    /// Output arrives while a scan runs: the job goes on from where it is, reads only the rows
    /// added since, and re-reads the old screen rows (they change in place).
    #[test]
    fn extend_reads_only_new_rows_and_the_old_screen() {
        let mut rows: Vec<String> = (0..100).map(|i| if i % 10 == 0 { format!("hit {i}") } else { format!("row {i}") }).collect();
        let reads = std::cell::Cell::new(0usize);
        let rd = |rows: &Vec<String>| {
            let rows = rows.clone();
            let reads = &reads;
            move |abs: usize, text: &mut String, cols: &mut Vec<u16>| {
                reads.set(reads.get() + 1);
                text.clear();
                cols.clear();
                let Some(r) = rows.get(abs) else { return false };
                for (i, ch) in r.chars().enumerate() {
                    text.push(ch);
                    cols.push(i as u16);
                }
                cols.push(r.chars().count() as u16);
                true
            }
        };
        let mut job = SearchJob::new("hit", 100).unwrap();
        assert!(job.step(&mut rd(&rows), Duration::from_secs(5)));
        assert_eq!(job.matches.len(), 10);
        // 20 rows arrive; the old screen was rows 76..100, and row 90 changed in place.
        rows[90] = "row 90".into();
        rows[95] = "now a hit 95".into();
        rows.extend((100..120).map(|i| if i == 110 { "hit 110".to_string() } else { format!("row {i}") }));
        reads.set(0);
        assert!(job.extend(120, 76));
        assert!(!job.done());
        assert!(job.step(&mut rd(&rows), Duration::from_secs(5)));
        assert_eq!(reads.get(), 120 - 76, "only the screen and the new rows are read");
        let found: Vec<u32> = job.matches.iter().map(|m| m.row).collect();
        assert_eq!(found, vec![0, 10, 20, 30, 40, 50, 60, 70, 80, 95, 110]);
        // A running job that has not reached the screen keeps its place.
        let mut job = SearchJob::new("hit", 100).unwrap();
        job.step(&mut rd(&rows), Duration::ZERO);
        let at = job.progress().0;
        assert!(job.extend(120, 76));
        assert_eq!(job.progress().0, at.min(76));
        // A buffer that shrank cannot be continued.
        assert!(!job.extend(50, 20));
    }
}
