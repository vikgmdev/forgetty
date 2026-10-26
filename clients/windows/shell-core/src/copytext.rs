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

/// The part of a row that lies in columns `first..=last`. `cols` is `forgetty_vt::RowText::cols`:
/// one column per char of `text`, then the row width. A char is kept when any of its cells is
/// inside the range, so a wide glyph is kept whole when either half is selected.
pub fn slice_cols(text: &str, cols: &[u16], first: usize, last: usize) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        let Some(&start) = cols.get(i) else { break };
        // The char ends where the next cell starts: combining chars share their base column.
        let end = cols[i + 1..].iter().copied().find(|&c| c > start).unwrap_or(start + 1);
        if (start as usize) <= last && (end as usize) > first {
            out.push(ch);
        }
    }
    out
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

/// Copy of a selection over many rows, read in slices so a selection over the whole scrollback
/// never holds the UI thread (about 100 ms for 100 000 rows in one go). The caller gives it the
/// row reader and a time budget per slice, as for `search::SearchJob`.
pub struct CopyJob {
    r0: usize,
    c0: usize,
    r1: usize,
    c1: usize,
    next: usize,
    rows: Vec<(String, bool)>,
    text: String,
    cols: Vec<u16>,
}

impl CopyJob {
    /// Rows `r0..=r1`, from column `c0` of the first row to column `c1` of the last.
    pub fn new(r0: usize, c0: usize, r1: usize, c1: usize) -> Self {
        Self {
            r0,
            c0,
            r1,
            c1,
            next: r0,
            rows: Vec::with_capacity((r1.saturating_sub(r0) + 1).min(200_000)),
            text: String::new(),
            cols: Vec::new(),
        }
    }

    pub fn done(&self) -> bool {
        self.next > self.r1
    }

    /// Rows to read in all.
    pub fn total_rows(&self) -> usize {
        self.r1.saturating_sub(self.r0) + 1
    }

    /// Read rows until the budget is used. `reader` fills the text and the column list of a row
    /// and returns whether the row soft-wraps, or `None` when the row is gone. At least one row
    /// is read per call. Returns true when done.
    pub fn step(
        &mut self,
        reader: &mut impl FnMut(usize, &mut String, &mut Vec<u16>) -> Option<bool>,
        budget: std::time::Duration,
    ) -> bool {
        let t0 = std::time::Instant::now();
        let mut n = 0usize;
        while self.next <= self.r1 {
            let ar = self.next;
            self.next += 1;
            if let Some(wrapped) = reader(ar, &mut self.text, &mut self.cols) {
                let first = if ar == self.r0 { self.c0 } else { 0 };
                let last = if ar == self.r1 { self.c1 } else { usize::MAX };
                self.rows.push((slice_cols(&self.text, &self.cols, first, last), wrapped));
            }
            n += 1;
            if n % 32 == 0 && t0.elapsed() >= budget {
                break;
            }
        }
        self.done()
    }

    /// The text of the rows read so far, cleaned as `smart` says.
    pub fn finish(&self, smart: bool) -> String {
        let joined = join_rows(&self.rows);
        if smart {
            smart_copy(&joined)
        } else {
            raw_copy(&joined)
        }
    }
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
    fn slice_keeps_wide_glyphs_whole_and_combining_marks_with_their_base() {
        // "a", wide "\u{4e2d}" (cols 1-2), "e" + combining acute (col 3), "z" (col 4); width 5.
        let text = "a\u{4e2d}e\u{301}z";
        let cols = [0u16, 1, 3, 3, 4, 5];
        assert_eq!(slice_cols(text, &cols, 0, 0), "a");
        assert_eq!(slice_cols(text, &cols, 2, 2), "\u{4e2d}", "the tail half selects the glyph");
        assert_eq!(slice_cols(text, &cols, 1, 1), "\u{4e2d}");
        assert_eq!(slice_cols(text, &cols, 3, 3), "e\u{301}");
        assert_eq!(slice_cols(text, &cols, 3, 9), "e\u{301}z");
        assert_eq!(slice_cols(text, &cols, 5, 9), "");
    }

    #[test]
    fn blank_edges_go_but_inner_blank_lines_stay() {
        assert_eq!(trim_blank_lines("  \n  \nhello\nworld\n  \n  "), "hello\nworld");
        assert_eq!(trim_blank_lines("hello\n\nworld"), "hello\n\nworld");
        assert_eq!(trim_blank_lines("  \n  "), "");
        assert_eq!(trim_blank_lines(""), "");
    }

    #[test]
    fn a_copy_job_in_slices_equals_one_pass() {
        let rows: Vec<String> = (0..500).map(|i| format!("line {i}   ")).collect();
        let read = |ar: usize, text: &mut String, cols: &mut Vec<u16>| -> Option<bool> {
            let r = rows.get(ar)?;
            text.clear();
            text.push_str(r);
            cols.clear();
            cols.extend(0..=(r.chars().count() as u16));
            Some(ar % 100 == 99)
        };
        let mut one = CopyJob::new(3, 2, 497, 4);
        assert!(one.step(&mut { read }, std::time::Duration::from_secs(60)));
        // A zero budget still reads 32 rows per call: many calls, same text.
        let mut sliced = CopyJob::new(3, 2, 497, 4);
        let mut calls = 0;
        while !sliced.step(&mut { read }, std::time::Duration::ZERO) {
            calls += 1;
            assert!(calls < 100);
        }
        assert!(calls > 3);
        assert_eq!(sliced.finish(true), one.finish(true));
        assert_eq!(one.total_rows(), 495);
        let t = one.finish(true);
        assert!(t.starts_with("ne 3\n") && t.ends_with("\nline"), "{}", &t[t.len() - 20..]);
        // A row that is gone is skipped.
        let mut gone = CopyJob::new(498, 0, 505, 9);
        assert!(gone.step(&mut { read }, std::time::Duration::from_secs(1)));
        assert_eq!(gone.finish(false), "line 498\nline 499");
    }
}
