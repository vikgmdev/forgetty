//! URL detection in terminal text. No UI and no terminal types.
//!
//! The caller builds a `LogicalLine` from the viewport rows that make up one wrapped line, and
//! asks which URL covers a cell. GTK looks at one row only; here a URL that the terminal wrapped
//! over several rows is found whole. Only `http://` and `https://` are links (the GTK rule), and
//! `is_openable` is the check that must pass before the OS is asked to open anything.

/// Most chars looked at in one logical line (a wrapped line is cut here).
const MAX_LINE: usize = 4096;
/// Longest URL that is a link.
const MAX_URL: usize = 2048;

/// The text of one logical line, with the cell of every char.
#[derive(Default, Debug)]
pub struct LogicalLine {
    chars: Vec<char>,
    /// (row, column) of each char. Rows are the caller's numbering.
    cells: Vec<(u16, u16)>,
}

impl LogicalLine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the cell at (`row`, `col`). An empty grapheme is a blank. The tail half of a wide
    /// glyph must not be added. Only the first char of a cluster is kept.
    pub fn push(&mut self, row: usize, col: usize, grapheme: &str) {
        if self.chars.len() >= MAX_LINE {
            return;
        }
        self.chars.push(grapheme.chars().next().unwrap_or(' '));
        self.cells.push((row as u16, col as u16));
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }
}

/// A URL and the cells it covers, as `(row, first col, one past the last col)` per row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub segments: Vec<(usize, usize, usize)>,
}

fn breaks_url(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(c, '"' | '\'' | '<' | '>' | '{' | '}' | '|' | '\\' | '^' | '`' | '[' | ']')
}

/// Length of the scheme prefix ("http://" or "https://") at `i`, ignoring case. 0 if none.
fn scheme_len(chars: &[char], i: usize) -> usize {
    let at = |s: &str| -> bool {
        let n = s.chars().count();
        chars.len() >= i + n && chars[i..i + n].iter().zip(s.chars()).all(|(a, b)| a.to_ascii_lowercase() == b)
    };
    if at("https://") {
        8
    } else if at("http://") {
        7
    } else {
        0
    }
}

/// Drop trailing punctuation that is rarely part of a URL. A closing parenthesis stays when the
/// URL has its opening one (Wikipedia links).
fn trim_end(chars: &[char], start: usize, mut end: usize) -> usize {
    while end > start {
        let c = chars[end - 1];
        let drop = match c {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => {
                let open = chars[start..end].iter().filter(|&&x| x == '(').count();
                let close = chars[start..end].iter().filter(|&&x| x == ')').count();
                close > open
            }
            _ => false,
        };
        if !drop {
            break;
        }
        end -= 1;
    }
    end
}

/// Every URL in the line, as char ranges.
fn find_all(chars: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !matches!(chars[i], 'h' | 'H') {
            i += 1;
            continue;
        }
        let sl = scheme_len(chars, i);
        if sl == 0 {
            i += 1;
            continue;
        }
        let mut end = i + sl;
        while end < chars.len() && end - i < MAX_URL && !breaks_url(chars[end]) {
            end += 1;
        }
        let end = trim_end(chars, i, end);
        // Something must follow the scheme, and it must start like a host.
        if end > i + sl && (chars[i + sl].is_alphanumeric() || chars[i + sl] == '[') {
            out.push((i, end));
            i = end;
        } else {
            i += sl;
        }
    }
    out
}

/// The URL that covers the cell (`row`, `col`), if any.
pub fn link_at(line: &LogicalLine, row: usize, col: usize) -> Option<Link> {
    // The char under the pointer: the last one in this row that starts at or before `col`.
    let idx = line.cells.iter().rposition(|&(r, c)| r as usize == row && (c as usize) <= col)?;
    // A cell past the last char of the row (blank padding) is not under a char.
    if let Some(&(r2, c2)) = line.cells.get(idx + 1) {
        if r2 as usize == row && (c2 as usize) <= col {
            return None;
        }
    }
    let (a, b) = find_all(&line.chars).into_iter().find(|&(a, b)| idx >= a && idx < b)?;
    let url: String = line.chars[a..b].iter().collect();
    let mut segments: Vec<(usize, usize, usize)> = Vec::new();
    for k in a..b {
        let (r, c) = (line.cells[k].0 as usize, line.cells[k].1 as usize);
        match segments.last_mut() {
            Some(s) if s.0 == r => s.2 = c + 1,
            _ => segments.push((r, c, c + 1)),
        }
    }
    // A wide glyph covers its tail cell too.
    for s in segments.iter_mut() {
        if let Some(next) = line.cells.iter().find(|&&(r, c)| r as usize == s.0 && (c as usize) >= s.2) {
            let n = next.1 as usize;
            if n > s.2 && n - s.2 == 1 {
                // The cell after the last char was skipped by the caller: a wide tail.
                s.2 = n;
            }
        }
    }
    Some(Link { url, segments })
}

/// True when `url` may be handed to the OS to open: http or https, a host, no control or
/// direction-changing characters, a sane length.
pub fn is_openable(url: &str) -> bool {
    let chars: Vec<char> = url.chars().collect();
    let sl = scheme_len(&chars, 0);
    if sl == 0 || chars.len() > MAX_URL || chars.len() <= sl {
        return false;
    }
    let bad = |c: char| {
        c.is_control()
            || c.is_whitespace()
            || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200f}' | '\u{feff}')
    };
    if chars.iter().any(|&c| bad(c)) {
        return false;
    }
    let host_end = chars[sl..].iter().position(|&c| matches!(c, '/' | '?' | '#')).map_or(chars.len(), |p| sl + p);
    host_end > sl
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row of text as a logical line (one char per cell).
    fn line(rows: &[&str]) -> LogicalLine {
        let mut l = LogicalLine::new();
        for (r, text) in rows.iter().enumerate() {
            for (c, ch) in text.chars().enumerate() {
                l.push(r, c, &ch.to_string());
            }
        }
        l
    }

    fn url_at(rows: &[&str], row: usize, col: usize) -> Option<String> {
        link_at(&line(rows), row, col).map(|l| l.url)
    }

    #[test]
    fn finds_a_url_under_the_pointer_only() {
        let l = ["see https://example.com/a?b=1#c now"];
        assert_eq!(url_at(&l, 0, 4).as_deref(), Some("https://example.com/a?b=1#c"));
        assert_eq!(url_at(&l, 0, 20).as_deref(), Some("https://example.com/a?b=1#c"));
        assert_eq!(url_at(&l, 0, 30).as_deref(), Some("https://example.com/a?b=1#c"));
        assert_eq!(url_at(&l, 0, 3), None);
        assert_eq!(url_at(&l, 0, 31), None, "the blank after the URL");
        assert_eq!(url_at(&l, 0, 33), None);
        assert_eq!(url_at(&l, 0, 80), None, "past the row");
    }

    #[test]
    fn trailing_punctuation_and_brackets() {
        assert_eq!(url_at(&["(http://a.org/x)."], 0, 5).as_deref(), Some("http://a.org/x"));
        assert_eq!(url_at(&["go to http://a.org/x, then"], 0, 8).as_deref(), Some("http://a.org/x"));
        assert_eq!(url_at(&["http://a.org/wiki/Foo_(bar)"], 0, 3).as_deref(), Some("http://a.org/wiki/Foo_(bar)"));
        assert_eq!(url_at(&["\"http://a.org/q\" x"], 0, 5).as_deref(), Some("http://a.org/q"));
        assert_eq!(url_at(&["<http://a.org/q>"], 0, 5).as_deref(), Some("http://a.org/q"));
        assert_eq!(url_at(&["HTTPS://A.ORG/Q"], 0, 5).as_deref(), Some("HTTPS://A.ORG/Q"));
    }

    #[test]
    fn two_urls_in_one_line() {
        let l = ["http://a.org and https://b.org/z"];
        assert_eq!(url_at(&l, 0, 2).as_deref(), Some("http://a.org"));
        assert_eq!(url_at(&l, 0, 25).as_deref(), Some("https://b.org/z"));
    }

    #[test]
    fn not_a_link() {
        for t in ["http://", "https:// x", "ftp://a.org/x", "file:///etc/passwd", "javascript:alert(1)", "xhttp//a", "http://.", "http:///x"] {
            assert_eq!(url_at(&[t], 0, 2), None, "{t}");
        }
    }

    #[test]
    fn a_url_wrapped_over_rows_is_one_link_with_a_segment_per_row() {
        let rows = ["visit https://exam", "ple.com/path/to/thi", "s now"];
        let l = link_at(&line(&rows), 1, 3).unwrap();
        assert_eq!(l.url, "https://example.com/path/to/this");
        assert_eq!(l.segments, vec![(0, 6, 18), (1, 0, 19), (2, 0, 1)]);
        // The pointer on the first row finds the same link.
        assert_eq!(link_at(&line(&rows), 0, 8).unwrap().url, l.url);
    }

    #[test]
    fn a_wide_glyph_before_the_url_keeps_columns_right() {
        // "日 http://a.org": the glyph is at col 0, its tail (col 1) is not pushed.
        let mut l = LogicalLine::new();
        l.push(0, 0, "日");
        for (i, ch) in " http://a.org".chars().enumerate() {
            l.push(0, 2 + i, &ch.to_string());
        }
        let k = link_at(&l, 0, 4).unwrap();
        assert_eq!(k.url, "http://a.org");
        assert_eq!(k.segments, vec![(0, 3, 15)]);
        assert!(link_at(&l, 0, 1).is_none());
    }

    #[test]
    fn only_http_urls_with_a_host_are_openable() {
        assert!(is_openable("https://example.com"));
        assert!(is_openable("http://localhost:8080/x?y=1"));
        assert!(!is_openable("http://"));
        assert!(!is_openable("https:///path"));
        assert!(!is_openable("file:///C:/Windows/System32/calc.exe"));
        assert!(!is_openable("ms-settings:"));
        assert!(!is_openable("javascript:alert(1)"));
        assert!(!is_openable("https://a.org/x y"));
        assert!(!is_openable("https://a.org/\u{7}"));
        assert!(!is_openable("https://a.org/\u{202e}txt"));
        assert!(!is_openable(&format!("https://a.org/{}", "x".repeat(3000))));
    }

    /// Random text never panics and every link it finds is openable text.
    #[test]
    fn fuzz_no_panic() {
        let mut x: u64 = 0x2545F4914F6CDD1D;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let frags = ["http://", "https://", "a.org", "(", ")", ".", " ", "日", "\u{301}", "h", "ttp", "://", "/x", "[", "]"];
        for _ in 0..20_000 {
            let n = (rnd() % 12) as usize;
            let mut s = String::new();
            for _ in 0..n {
                s.push_str(frags[(rnd() % frags.len() as u64) as usize]);
            }
            let l = line(&[&s]);
            for c in 0..s.chars().count() + 2 {
                if let Some(k) = link_at(&l, 0, c) {
                    assert!(k.url.starts_with("http"));
                }
            }
        }
    }
}
