//! A small multi-line text editor for the TOML page of Settings: lines, a caret, scrolling,
//! undo. No selection (paste, copy-all and select-all are on the page's buttons and keys).
//! Pure; the window feeds it keys and draws the lines.

/// A key as the editor needs it.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyIn<'a> {
    pub key: &'a str,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// The Windows key.
    pub win: bool,
    /// The characters the key types, if any.
    pub text: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    /// The text changed.
    Edited,
    /// Only the caret moved.
    Moved,
    /// Not a key of the editor.
    Ignored,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snap {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

#[derive(Clone, Debug)]
pub struct TextArea {
    lines: Vec<String>,
    /// Caret row and column (in chars).
    row: usize,
    col: usize,
    /// The column the caret goes back to on a vertical move.
    want: usize,
    /// First row on screen.
    pub top: usize,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    saved: Vec<String>,
}

const UNDO_MAX: usize = 200;
/// Longest text the editor takes (a config file is a few KB).
pub const MAX_BYTES: usize = 512 * 1024;

fn split(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text.replace("\r\n", "\n").replace('\r', "\n").split('\n').map(str::to_string).collect();
    if v.is_empty() {
        v.push(String::new());
    }
    v
}

fn byte_at(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(s.len())
}

fn width(s: &str) -> usize {
    s.chars().count()
}

impl TextArea {
    pub fn new(text: &str) -> Self {
        let lines = split(text);
        TextArea { saved: lines.clone(), lines, row: 0, col: 0, want: 0, top: 0, undo: Vec::new(), redo: Vec::new() }
    }

    /// The text, lines joined with `\n` (a file that had `\r\n` is written with `\n`).
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// Changed since `new` or the last `mark_saved`.
    pub fn is_dirty(&self) -> bool {
        self.lines != self.saved
    }

    pub fn mark_saved(&mut self) {
        self.saved = self.lines.clone();
    }

    /// Replace everything (undo keeps the old text).
    pub fn set_text(&mut self, text: &str) {
        self.push_undo();
        self.lines = split(text);
        self.row = 0;
        self.col = 0;
        self.want = 0;
        self.top = 0;
    }

    pub fn go_to_line(&mut self, line1: usize) {
        self.row = line1.saturating_sub(1).min(self.lines.len() - 1);
        self.col = 0;
        self.want = 0;
    }

    fn snap(&self) -> Snap {
        Snap { lines: self.lines.clone(), row: self.row, col: self.col }
    }

    fn push_undo(&mut self) {
        if self.undo.len() >= UNDO_MAX {
            self.undo.remove(0);
        }
        self.undo.push(self.snap());
        self.redo.clear();
    }

    pub fn undo(&mut self) -> bool {
        let Some(s) = self.undo.pop() else { return false };
        self.redo.push(self.snap());
        self.restore(s);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(s) = self.redo.pop() else { return false };
        self.undo.push(self.snap());
        self.restore(s);
        true
    }

    fn restore(&mut self, s: Snap) {
        self.lines = s.lines;
        self.row = s.row;
        self.col = s.col;
        self.want = s.col;
    }

    /// Type or paste. Control characters other than line breaks and tabs are dropped. Returns
    /// false when the result would be over the size limit.
    pub fn insert(&mut self, s: &str) -> bool {
        let clean: String = s.chars().filter(|c| *c == '\n' || *c == '\r' || *c == '\t' || !c.is_control()).collect();
        if clean.is_empty() {
            return true;
        }
        if self.text().len() + clean.len() > MAX_BYTES {
            return false;
        }
        self.push_undo();
        let parts = split(&clean);
        let line = std::mem::take(&mut self.lines[self.row]);
        let at = byte_at(&line, self.col);
        let (head, tail) = line.split_at(at);
        if parts.len() == 1 {
            self.lines[self.row] = format!("{head}{}{tail}", parts[0]);
            self.col += width(&parts[0]);
        } else {
            let last = parts.len() - 1;
            let mut rows: Vec<String> = Vec::with_capacity(parts.len());
            rows.push(format!("{head}{}", parts[0]));
            rows.extend(parts[1..last].iter().cloned());
            rows.push(format!("{}{tail}", parts[last]));
            self.lines.splice(self.row..=self.row, rows);
            self.row += last;
            self.col = width(&parts[last]);
        }
        self.want = self.col;
        true
    }

    /// Enter: a new line that starts with the indentation of this one.
    pub fn newline(&mut self) -> bool {
        let indent: String = self.lines[self.row].chars().take_while(|c| *c == ' ' || *c == '\t').take(self.col).collect();
        self.insert(&format!("\n{indent}"))
    }

    pub fn backspace(&mut self) -> bool {
        if self.row == 0 && self.col == 0 {
            return false;
        }
        self.push_undo();
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let a = byte_at(line, self.col - 1);
            let b = byte_at(line, self.col);
            line.replace_range(a..b, "");
            self.col -= 1;
        } else {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = width(&self.lines[self.row]);
            self.lines[self.row].push_str(&cur);
        }
        self.want = self.col;
        true
    }

    pub fn delete(&mut self) -> bool {
        let n = width(&self.lines[self.row]);
        if self.col >= n && self.row + 1 >= self.lines.len() {
            return false;
        }
        self.push_undo();
        if self.col < n {
            let line = &mut self.lines[self.row];
            let a = byte_at(line, self.col);
            let b = byte_at(line, self.col + 1);
            line.replace_range(a..b, "");
        } else {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        true
    }

    /// Delete to the end of the line (Ctrl+K), or the line break when the line is at its end.
    pub fn kill_line(&mut self) -> bool {
        let n = width(&self.lines[self.row]);
        if self.col >= n {
            return self.delete();
        }
        self.push_undo();
        let line = &mut self.lines[self.row];
        let a = byte_at(line, self.col);
        line.truncate(a);
        true
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = width(&self.lines[self.row]);
        }
        self.want = self.col;
    }

    pub fn right(&mut self) {
        if self.col < width(&self.lines[self.row]) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
        self.want = self.col;
    }

    pub fn up(&mut self, n: usize) {
        self.row = self.row.saturating_sub(n);
        self.col = self.want.min(width(&self.lines[self.row]));
    }

    pub fn down(&mut self, n: usize) {
        self.row = (self.row + n).min(self.lines.len() - 1);
        self.col = self.want.min(width(&self.lines[self.row]));
    }

    pub fn home(&mut self) {
        // The first press goes after the indentation, the second to the start.
        let ind = self.lines[self.row].chars().take_while(|c| *c == ' ' || *c == '\t').count();
        self.col = if self.col == ind || self.col == 0 && ind == 0 { 0 } else { ind };
        self.want = self.col;
    }

    pub fn end(&mut self) {
        self.col = width(&self.lines[self.row]);
        self.want = self.col;
    }

    pub fn doc_start(&mut self) {
        self.row = 0;
        self.col = 0;
        self.want = 0;
    }

    pub fn doc_end(&mut self) {
        self.row = self.lines.len() - 1;
        self.end();
    }

    pub fn word_left(&mut self) {
        let is_w = |c: char| c.is_alphanumeric() || c == '_';
        loop {
            if self.col == 0 {
                if self.row == 0 {
                    break;
                }
                self.left();
                break;
            }
            let ch: Vec<char> = self.lines[self.row].chars().collect();
            if is_w(ch[self.col - 1]) {
                while self.col > 0 && is_w(ch[self.col - 1]) {
                    self.col -= 1;
                }
                break;
            }
            self.col -= 1;
        }
        self.want = self.col;
    }

    pub fn word_right(&mut self) {
        let is_w = |c: char| c.is_alphanumeric() || c == '_';
        loop {
            let ch: Vec<char> = self.lines[self.row].chars().collect();
            if self.col >= ch.len() {
                self.right();
                break;
            }
            if is_w(ch[self.col]) {
                while self.col < ch.len() && is_w(ch[self.col]) {
                    self.col += 1;
                }
                break;
            }
            self.col += 1;
        }
        self.want = self.col;
    }

    /// Scroll so the caret row is inside `rows` visible rows.
    pub fn ensure_visible(&mut self, rows: usize) {
        let rows = rows.max(1);
        if self.row < self.top {
            self.top = self.row;
        } else if self.row >= self.top + rows {
            self.top = self.row + 1 - rows;
        }
        self.top = self.top.min(self.lines.len().saturating_sub(1));
    }

    pub fn scroll(&mut self, delta: isize, rows: usize) {
        let max = self.lines.len().saturating_sub(rows.max(1));
        self.top = (self.top as isize + delta).clamp(0, max as isize) as usize;
    }

    /// Put the caret at a row and column (a click), clamped.
    pub fn click(&mut self, row: usize, col: usize) {
        self.row = row.min(self.lines.len() - 1);
        self.col = col.min(width(&self.lines[self.row]));
        self.want = self.col;
    }

    /// Apply one key. `page` is the number of rows a PageUp/PageDown moves.
    pub fn key(&mut self, k: &KeyIn, page: usize) -> Out {
        let moved = Out::Moved;
        let edited = |ok: bool| if ok { Out::Edited } else { Out::Moved };
        if k.alt {
            return Out::Ignored;
        }
        if k.ctrl {
            return match k.key {
                "z" if k.shift => {
                    if self.redo() { Out::Edited } else { Out::Moved }
                }
                "z" => {
                    if self.undo() { Out::Edited } else { Out::Moved }
                }
                "y" => {
                    if self.redo() { Out::Edited } else { Out::Moved }
                }
                "k" => edited(self.kill_line()),
                "left" => {
                    self.word_left();
                    moved
                }
                "right" => {
                    self.word_right();
                    moved
                }
                "home" => {
                    self.doc_start();
                    moved
                }
                "end" => {
                    self.doc_end();
                    moved
                }
                "backspace" => {
                    // Delete the word before the caret.
                    let (r, c) = (self.row, self.col);
                    self.word_left();
                    if (self.row, self.col) == (r, c) {
                        return Out::Moved;
                    }
                    let (r2, c2) = (self.row, self.col);
                    self.row = r;
                    self.col = c;
                    let mut any = false;
                    while (self.row, self.col) != (r2, c2) {
                        any |= self.backspace();
                    }
                    edited(any)
                }
                _ => Out::Ignored,
            };
        }
        match k.key {
            "enter" => edited(self.newline()),
            "tab" => edited(self.insert("  ")),
            "backspace" => edited(self.backspace()),
            "delete" => edited(self.delete()),
            "left" => {
                self.left();
                moved
            }
            "right" => {
                self.right();
                moved
            }
            "up" => {
                self.up(1);
                moved
            }
            "down" => {
                self.down(1);
                moved
            }
            "pageup" => {
                self.up(page.max(1));
                moved
            }
            "pagedown" => {
                self.down(page.max(1));
                moved
            }
            "home" => {
                self.home();
                moved
            }
            "end" => {
                self.end();
                moved
            }
            "space" => edited(self.insert(" ")),
            _ => match k.text {
                Some(t) if !t.is_empty() && t.chars().all(|c| !c.is_control()) => edited(self.insert(t)),
                _ => Out::Ignored,
            },
        }
    }
}

/// The line number a TOML error names ("... at line 3, column 5" or "TOML parse error at line 3").
pub fn error_line(msg: &str) -> Option<usize> {
    let i = msg.find("line ")?;
    let digits: String = msg[i + 5..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|n| *n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(key: &str) -> KeyIn<'_> {
        KeyIn { key, ..Default::default() }
    }
    fn t(text: &str) -> KeyIn<'_> {
        KeyIn { key: "x", text: Some(text), ..Default::default() }
    }

    #[test]
    fn typing_enter_backspace_and_the_text_that_comes_out() {
        let mut a = TextArea::new("");
        for c in ["a", "b"] {
            assert_eq!(a.key(&t(c), 10), Out::Edited);
        }
        assert_eq!(a.key(&k("enter"), 10), Out::Edited);
        a.key(&t("c"), 10);
        assert_eq!(a.text(), "ab\nc");
        assert_eq!(a.cursor(), (1, 1));
        a.key(&k("backspace"), 10);
        a.key(&k("backspace"), 10);
        assert_eq!(a.text(), "ab");
        assert_eq!(a.cursor(), (0, 2));
        assert!(a.is_dirty());
        a.mark_saved();
        assert!(!a.is_dirty());
    }

    #[test]
    fn enter_keeps_the_indent_and_a_paste_of_many_lines_lands_in_place() {
        let mut a = TextArea::new("  key = 1");
        a.end();
        a.key(&k("enter"), 10);
        assert_eq!(a.text(), "  key = 1\n  ");
        let mut b = TextArea::new("start end");
        b.click(0, 6);
        assert!(b.insert("one\r\ntwo\nthree"));
        assert_eq!(b.text(), "start one\ntwo\nthreeend");
        assert_eq!(b.cursor(), (2, 5));
    }

    #[test]
    fn moves_keep_the_wanted_column_and_wrap_at_line_ends() {
        let mut a = TextArea::new("long line here\nab\nlong line here");
        a.click(0, 10);
        a.down(1);
        assert_eq!(a.cursor(), (1, 2));
        a.down(1);
        assert_eq!(a.cursor(), (2, 10));
        a.click(1, 2);
        a.right();
        assert_eq!(a.cursor(), (2, 0));
        a.left();
        assert_eq!(a.cursor(), (1, 2));
        a.doc_end();
        assert_eq!(a.cursor(), (2, 14));
        a.key(&KeyIn { key: "left", ctrl: true, ..Default::default() }, 5);
        assert_eq!(a.cursor(), (2, 10));
    }

    #[test]
    fn delete_joins_lines_and_undo_redo_walk_back() {
        let mut a = TextArea::new("a\nb\nc");
        a.click(0, 1);
        assert!(a.delete());
        assert_eq!(a.text(), "ab\nc");
        a.key(&KeyIn { key: "z", ctrl: true, ..Default::default() }, 5);
        assert_eq!(a.text(), "a\nb\nc");
        a.key(&KeyIn { key: "z", ctrl: true, shift: true, ..Default::default() }, 5);
        assert_eq!(a.text(), "ab\nc");
        a.doc_end();
        assert!(!a.delete());
        a.click(0, 0);
        assert!(a.kill_line());
        assert_eq!(a.text(), "\nc");
    }

    #[test]
    fn unicode_columns_are_chars_not_bytes() {
        let mut a = TextArea::new("héllo wörld");
        a.click(0, 2);
        a.insert("X");
        assert_eq!(a.text(), "héXllo wörld");
        a.end();
        a.backspace();
        assert_eq!(a.text(), "héXllo wörl");
    }

    #[test]
    fn the_view_follows_the_caret_and_the_size_is_capped() {
        let text = (0..100).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let mut a = TextArea::new(&text);
        a.doc_end();
        a.ensure_visible(20);
        assert_eq!(a.top, 80);
        a.doc_start();
        a.ensure_visible(20);
        assert_eq!(a.top, 0);
        a.scroll(500, 20);
        assert_eq!(a.top, 80);
        let mut b = TextArea::new("");
        assert!(!b.insert(&"x".repeat(MAX_BYTES + 1)));
        assert_eq!(b.text(), "");
    }

    #[test]
    fn error_lines_come_out_of_the_toml_message() {
        assert_eq!(error_line("TOML parse error at line 3, column 5"), Some(3));
        assert_eq!(error_line("bad value"), None);
    }

    #[test]
    fn crlf_in_a_file_does_not_leave_carriage_returns() {
        let a = TextArea::new("a = 1\r\nb = 2\r\n");
        assert_eq!(a.lines(), &["a = 1".to_string(), "b = 2".to_string(), String::new()]);
        assert_eq!(a.text(), "a = 1\nb = 2\n");
    }
}
