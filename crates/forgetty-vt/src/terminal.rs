//! High-level terminal state machine.
//!
//! Wraps the `vte` crate parser and provides a safe, ergonomic Rust interface
//! for feeding input data and querying terminal state. When libghostty-vt is
//! integrated, this module's internals will change but the public API will remain.

use crate::screen::{Cell, CellAttributes, Color, Screen};

/// Events emitted by the terminal during parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    /// The terminal title changed (via OSC 0 or OSC 2).
    TitleChanged(String),
    /// The terminal bell was triggered.
    Bell,
    /// The terminal requested a mode change (e.g., alt screen).
    ModeChanged,
}

/// A virtual terminal that processes VT escape sequences and maintains screen state.
pub struct Terminal {
    screen: Screen,
    /// The VT parser state machine (persists across `feed` calls).
    parser: vte::Parser,
    /// Current cursor row.
    cursor_row: usize,
    /// Current cursor column.
    cursor_col: usize,
    /// Cursor visibility.
    cursor_visible: bool,
    /// Terminal size (rows, cols).
    rows: usize,
    cols: usize,
    /// Saved cursor position (for save/restore).
    saved_cursor: (usize, usize),
    /// Current text attributes for new characters.
    current_attrs: CellAttributes,
    /// Terminal title set by OSC sequences.
    title: String,
    /// Scrollback buffer (lines that scrolled off the top).
    scrollback: Vec<Vec<Cell>>,
    /// Maximum number of scrollback lines to retain.
    max_scrollback: usize,
    /// Alternate screen buffer.
    alt_screen: Option<Screen>,
    /// Whether the alternate screen is active.
    using_alt_screen: bool,
    /// Pending events from parsing.
    events: Vec<TerminalEvent>,
    /// Scroll region top (inclusive).
    scroll_top: usize,
    /// Scroll region bottom (inclusive).
    scroll_bottom: usize,
}

impl Terminal {
    /// Create a new terminal with the given dimensions.
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            screen: Screen::new(rows, cols),
            parser: vte::Parser::new(),
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            rows,
            cols,
            saved_cursor: (0, 0),
            current_attrs: CellAttributes::default(),
            title: String::new(),
            scrollback: Vec::new(),
            max_scrollback: 10_000,
            alt_screen: None,
            using_alt_screen: false,
            events: Vec::new(),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
        }
    }

    /// Feed raw bytes from the PTY into the terminal parser.
    ///
    /// The parser state is preserved across calls, so incomplete escape
    /// sequences at buffer boundaries are handled correctly.
    pub fn feed(&mut self, bytes: &[u8]) {
        // We must temporarily take the parser out to avoid a double
        // mutable borrow (`self.parser` + `self` as Perform).
        let mut parser = std::mem::replace(&mut self.parser, vte::Parser::new());
        for &byte in bytes {
            parser.advance(self, byte);
        }
        self.parser = parser;
    }

    /// Get the current screen state.
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Get cursor position as (row, col).
    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    /// Is the cursor visible?
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Resize the terminal to new dimensions.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;
        self.screen.resize(rows, cols);
        if let Some(ref mut alt) = self.alt_screen {
            alt.resize(rows, cols);
        }
        // Clamp cursor
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
        // Reset scroll region
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
    }

    /// Get the terminal title (set via OSC).
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Drain pending events.
    pub fn drain_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.events)
    }

    /// Get scrollback lines.
    pub fn scrollback(&self) -> &[Vec<Cell>] {
        &self.scrollback
    }

    /// Total lines: scrollback + visible rows.
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    // --- Private helpers ---

    fn scroll_up(&mut self, n: usize) {
        let scrolled = self.screen.scroll_up(n, self.scroll_top, self.scroll_bottom);
        if self.scroll_top == 0 && !self.using_alt_screen {
            for line in scrolled {
                self.scrollback.push(line);
                if self.scrollback.len() > self.max_scrollback {
                    self.scrollback.remove(0);
                }
            }
        }
    }

    fn scroll_down(&mut self, n: usize) {
        self.screen.scroll_down(n, self.scroll_top, self.scroll_bottom);
    }

    fn put_char(&mut self, c: char) {
        if self.cursor_col >= self.cols {
            // Wrap to next line
            self.cursor_col = 0;
            self.cursor_row += 1;
            if self.cursor_row > self.scroll_bottom {
                self.cursor_row = self.scroll_bottom;
                self.scroll_up(1);
            }
        }

        let cell = Cell { character: c, attrs: self.current_attrs.clone() };
        self.screen.set_cell(self.cursor_row, self.cursor_col, cell);

        // Advance cursor, accounting for wide characters
        let width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
        self.cursor_col += width;
    }

    fn handle_sgr(&mut self, params: &vte::Params) {
        let mut iter = ParamsIter::new(params);

        if iter.is_empty() {
            // ESC[m with no params means reset
            self.current_attrs = CellAttributes::default();
            return;
        }

        while let Some(param) = iter.next() {
            match param {
                0 => self.current_attrs = CellAttributes::default(),
                1 => self.current_attrs.bold = true,
                2 => self.current_attrs.dim = true,
                3 => self.current_attrs.italic = true,
                4 => self.current_attrs.underline = true,
                7 => self.current_attrs.inverse = true,
                9 => self.current_attrs.strikethrough = true,
                22 => {
                    self.current_attrs.bold = false;
                    self.current_attrs.dim = false;
                }
                23 => self.current_attrs.italic = false,
                24 => self.current_attrs.underline = false,
                27 => self.current_attrs.inverse = false,
                29 => self.current_attrs.strikethrough = false,
                // Standard foreground colors
                30..=37 => self.current_attrs.fg = Color::Indexed(param as u8 - 30),
                38 => {
                    if let Some(color) = parse_extended_color(&mut iter) {
                        self.current_attrs.fg = color;
                    }
                }
                39 => self.current_attrs.fg = Color::Default,
                // Standard background colors
                40..=47 => self.current_attrs.bg = Color::Indexed(param as u8 - 40),
                48 => {
                    if let Some(color) = parse_extended_color(&mut iter) {
                        self.current_attrs.bg = color;
                    }
                }
                49 => self.current_attrs.bg = Color::Default,
                // Bright foreground colors
                90..=97 => self.current_attrs.fg = Color::Indexed(param as u8 - 90 + 8),
                // Bright background colors
                100..=107 => self.current_attrs.bg = Color::Indexed(param as u8 - 100 + 8),
                _ => {} // Ignore unknown SGR params
            }
        }
    }

    fn enter_alt_screen(&mut self) {
        if !self.using_alt_screen {
            let alt = Screen::new(self.rows, self.cols);
            self.alt_screen = Some(std::mem::replace(&mut self.screen, alt));
            self.using_alt_screen = true;
            self.events.push(TerminalEvent::ModeChanged);
        }
    }

    fn exit_alt_screen(&mut self) {
        if self.using_alt_screen {
            if let Some(main) = self.alt_screen.take() {
                self.screen = main;
            }
            self.using_alt_screen = false;
            self.events.push(TerminalEvent::ModeChanged);
        }
    }
}

impl vte::Perform for Terminal {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // Newline (LF)
            b'\n' | 0x0b | 0x0c => {
                self.cursor_row += 1;
                if self.cursor_row > self.scroll_bottom {
                    self.cursor_row = self.scroll_bottom;
                    self.scroll_up(1);
                }
            }
            // Carriage return
            b'\r' => {
                self.cursor_col = 0;
            }
            // Tab
            b'\t' => {
                let next_tab = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next_tab.min(self.cols.saturating_sub(1));
            }
            // Backspace
            0x08 => {
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            // Bell
            0x07 => {
                self.events.push(TerminalEvent::Bell);
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let first = first_param(params, 1) as usize;

        match action {
            // SGR — set graphics rendition
            'm' => self.handle_sgr(params),

            // Cursor Up
            'A' => {
                self.cursor_row = self.cursor_row.saturating_sub(first);
            }
            // Cursor Down
            'B' => {
                self.cursor_row = (self.cursor_row + first).min(self.rows - 1);
            }
            // Cursor Forward
            'C' => {
                self.cursor_col = (self.cursor_col + first).min(self.cols - 1);
            }
            // Cursor Back
            'D' => {
                self.cursor_col = self.cursor_col.saturating_sub(first);
            }

            // Cursor Position (CUP)
            'H' | 'f' => {
                let row = first_param(params, 1) as usize;
                let col = second_param(params, 1) as usize;
                // CSI params are 1-based
                self.cursor_row = (row.saturating_sub(1)).min(self.rows.saturating_sub(1));
                self.cursor_col = (col.saturating_sub(1)).min(self.cols.saturating_sub(1));
            }

            // Erase in Display
            'J' => {
                let mode = first_param(params, 0);
                match mode {
                    0 => {
                        // Erase from cursor to end of display
                        for col in self.cursor_col..self.cols {
                            self.screen.set_cell(self.cursor_row, col, Cell::default());
                        }
                        for row in (self.cursor_row + 1)..self.rows {
                            for col in 0..self.cols {
                                self.screen.set_cell(row, col, Cell::default());
                            }
                        }
                    }
                    1 => {
                        // Erase from start to cursor
                        for row in 0..self.cursor_row {
                            for col in 0..self.cols {
                                self.screen.set_cell(row, col, Cell::default());
                            }
                        }
                        for col in 0..=self.cursor_col.min(self.cols.saturating_sub(1)) {
                            self.screen.set_cell(self.cursor_row, col, Cell::default());
                        }
                    }
                    2 | 3 => {
                        // Erase entire display (3 also clears scrollback)
                        self.screen.clear();
                        if mode == 3 {
                            self.scrollback.clear();
                        }
                    }
                    _ => {}
                }
            }

            // Erase in Line
            'K' => {
                let mode = first_param(params, 0);
                match mode {
                    0 => {
                        // Erase from cursor to end of line
                        for col in self.cursor_col..self.cols {
                            self.screen.set_cell(self.cursor_row, col, Cell::default());
                        }
                    }
                    1 => {
                        // Erase from start to cursor
                        for col in 0..=self.cursor_col.min(self.cols.saturating_sub(1)) {
                            self.screen.set_cell(self.cursor_row, col, Cell::default());
                        }
                    }
                    2 => {
                        // Erase entire line
                        for col in 0..self.cols {
                            self.screen.set_cell(self.cursor_row, col, Cell::default());
                        }
                    }
                    _ => {}
                }
            }

            // Insert Lines
            'L' => {
                if self.cursor_row >= self.scroll_top && self.cursor_row <= self.scroll_bottom {
                    self.screen.scroll_down(first, self.cursor_row, self.scroll_bottom);
                }
            }

            // Delete Lines
            'M' => {
                if self.cursor_row >= self.scroll_top && self.cursor_row <= self.scroll_bottom {
                    self.screen.scroll_up(first, self.cursor_row, self.scroll_bottom);
                }
            }

            // Delete Characters
            'P' => {
                let count = first.min(self.cols - self.cursor_col);
                let row = self.cursor_row;
                for col in self.cursor_col..self.cols {
                    if col + count < self.cols {
                        let src = self.screen.cell(row, col + count).clone();
                        self.screen.set_cell(row, col, src);
                    } else {
                        self.screen.set_cell(row, col, Cell::default());
                    }
                }
            }

            // Insert Characters
            '@' => {
                let count = first.min(self.cols - self.cursor_col);
                let row = self.cursor_row;
                // Shift right
                for col in (self.cursor_col..self.cols).rev() {
                    if col >= self.cursor_col + count {
                        let src = self.screen.cell(row, col - count).clone();
                        self.screen.set_cell(row, col, src);
                    } else {
                        self.screen.set_cell(row, col, Cell::default());
                    }
                }
            }

            // Scroll Up
            'S' => {
                self.scroll_up(first);
            }

            // Scroll Down
            'T' => {
                self.scroll_down(first);
            }

            // Set scrolling region
            'r' => {
                let top = first_param(params, 1) as usize;
                // Default bottom to the number of rows (not 1)
                let bot = params
                    .iter()
                    .nth(1)
                    .and_then(|sub| sub.first().copied())
                    .map(|v| if v == 0 { self.rows as u16 } else { v })
                    .unwrap_or(self.rows as u16) as usize;
                self.scroll_top = top.saturating_sub(1).min(self.rows.saturating_sub(1));
                self.scroll_bottom = bot.saturating_sub(1).min(self.rows.saturating_sub(1));
                if self.scroll_top > self.scroll_bottom {
                    self.scroll_top = 0;
                    self.scroll_bottom = self.rows.saturating_sub(1);
                }
                // Move cursor to home
                self.cursor_row = 0;
                self.cursor_col = 0;
            }

            // Set Mode / Reset Mode
            'h' | 'l' => {
                let set = action == 'h';
                let is_private = intermediates.contains(&b'?');

                if is_private {
                    match first_param(params, 0) {
                        // Cursor visibility (DECTCEM)
                        25 => self.cursor_visible = set,
                        // Alt screen buffer (various modes)
                        1049 => {
                            if set {
                                self.saved_cursor = (self.cursor_row, self.cursor_col);
                                self.enter_alt_screen();
                                self.screen.clear();
                                self.cursor_row = 0;
                                self.cursor_col = 0;
                            } else {
                                self.exit_alt_screen();
                                let (r, c) = self.saved_cursor;
                                self.cursor_row = r.min(self.rows.saturating_sub(1));
                                self.cursor_col = c.min(self.cols.saturating_sub(1));
                            }
                        }
                        1047 | 47 => {
                            if set {
                                self.enter_alt_screen();
                            } else {
                                self.exit_alt_screen();
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Erase Characters
            'X' => {
                let count = first.min(self.cols - self.cursor_col);
                for i in 0..count {
                    self.screen.set_cell(self.cursor_row, self.cursor_col + i, Cell::default());
                }
            }

            // Cursor Next Line
            'E' => {
                self.cursor_col = 0;
                self.cursor_row = (self.cursor_row + first).min(self.rows - 1);
            }

            // Cursor Previous Line
            'F' => {
                self.cursor_col = 0;
                self.cursor_row = self.cursor_row.saturating_sub(first);
            }

            // Cursor Horizontal Absolute
            'G' => {
                self.cursor_col = (first.saturating_sub(1)).min(self.cols.saturating_sub(1));
            }

            // Cursor Vertical Absolute
            'd' => {
                self.cursor_row = (first.saturating_sub(1)).min(self.rows.saturating_sub(1));
            }

            _ => {
                tracing::trace!("unhandled CSI: {:?} {:?}", params_to_vec(params), action);
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }

        match params[0] {
            // Set window title
            b"0" | b"2" => {
                if params.len() >= 2 {
                    let title = String::from_utf8_lossy(params[1]).to_string();
                    self.title = title.clone();
                    self.events.push(TerminalEvent::TitleChanged(title));
                }
            }
            _ => {
                tracing::trace!("unhandled OSC: {:?}", params[0]);
            }
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            // Save cursor (DECSC)
            b'7' => {
                self.saved_cursor = (self.cursor_row, self.cursor_col);
            }
            // Restore cursor (DECRC)
            b'8' => {
                let (r, c) = self.saved_cursor;
                self.cursor_row = r.min(self.rows.saturating_sub(1));
                self.cursor_col = c.min(self.cols.saturating_sub(1));
            }
            // Reverse Index — move cursor up, scroll down if at top
            b'M' => {
                if self.cursor_row == self.scroll_top {
                    self.scroll_down(1);
                } else {
                    self.cursor_row = self.cursor_row.saturating_sub(1);
                }
            }
            // Index — move cursor down, scroll up if at bottom
            b'D' => {
                if self.cursor_row == self.scroll_bottom {
                    self.scroll_up(1);
                } else {
                    self.cursor_row += 1;
                }
            }
            // Next Line
            b'E' => {
                self.cursor_col = 0;
                if self.cursor_row == self.scroll_bottom {
                    self.scroll_up(1);
                } else {
                    self.cursor_row += 1;
                }
            }
            _ => {
                tracing::trace!("unhandled ESC: 0x{:02x}", byte);
            }
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        // DCS sequences — not yet handled
    }

    fn unhook(&mut self) {}

    fn put(&mut self, _byte: u8) {
        // DCS data — not yet handled
    }
}

// --- Helper functions for parameter parsing ---

/// A helper to iterate over vte::Params as u16 values,
/// handling subparameters (e.g., `38;2;R;G;B` or `38;5;N`).
struct ParamsIter {
    values: Vec<u16>,
    pos: usize,
}

impl ParamsIter {
    fn new(params: &vte::Params) -> Self {
        let values: Vec<u16> = params.iter().flat_map(|sub| sub.iter().copied()).collect();
        Self { values, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn next(&mut self) -> Option<u16> {
        if self.pos < self.values.len() {
            let val = self.values[self.pos];
            self.pos += 1;
            Some(val)
        } else {
            None
        }
    }
}

fn parse_extended_color(iter: &mut ParamsIter) -> Option<Color> {
    match iter.next()? {
        5 => {
            // 256-color: 38;5;N or 48;5;N
            let idx = iter.next()?;
            Some(Color::Indexed(idx as u8))
        }
        2 => {
            // Truecolor: 38;2;R;G;B or 48;2;R;G;B
            let r = iter.next()?;
            let g = iter.next()?;
            let b = iter.next()?;
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

fn first_param(params: &vte::Params, default: u16) -> u16 {
    params
        .iter()
        .next()
        .and_then(|sub| sub.first().copied())
        .map(|v| if v == 0 { default } else { v })
        .unwrap_or(default)
}

fn second_param(params: &vte::Params, default: u16) -> u16 {
    params
        .iter()
        .nth(1)
        .and_then(|sub| sub.first().copied())
        .map(|v| if v == 0 { default } else { v })
        .unwrap_or(default)
}

fn params_to_vec(params: &vte::Params) -> Vec<Vec<u16>> {
    params.iter().map(|sub| sub.to_vec()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_print_hello_world() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"Hello, World!");
        assert_eq!(term.screen().cell(0, 0).character, 'H');
        assert_eq!(term.screen().cell(0, 1).character, 'e');
        assert_eq!(term.screen().cell(0, 4).character, 'o');
        assert_eq!(term.screen().cell(0, 7).character, 'W');
        assert_eq!(term.screen().cell(0, 12).character, '!');
        assert_eq!(term.cursor(), (0, 13));
    }

    #[test]
    fn test_ansi_color() {
        let mut term = Terminal::new(24, 80);
        // ESC[31m = set foreground to red (color index 1), then print "Red"
        term.feed(b"\x1b[31mRed");
        assert_eq!(term.screen().cell(0, 0).character, 'R');
        assert_eq!(term.screen().cell(0, 0).attrs.fg, Color::Indexed(1));
        assert_eq!(term.screen().cell(0, 1).character, 'e');
        assert_eq!(term.screen().cell(0, 1).attrs.fg, Color::Indexed(1));
    }

    #[test]
    fn test_truecolor() {
        let mut term = Terminal::new(24, 80);
        // ESC[38;2;255;128;0m = set fg to RGB(255, 128, 0)
        term.feed(b"\x1b[38;2;255;128;0mOrange");
        assert_eq!(term.screen().cell(0, 0).character, 'O');
        assert_eq!(term.screen().cell(0, 0).attrs.fg, Color::Rgb(255, 128, 0));
    }

    #[test]
    fn test_256_color() {
        let mut term = Terminal::new(24, 80);
        // ESC[38;5;200m = set fg to palette color 200
        term.feed(b"\x1b[38;5;200mPink");
        assert_eq!(term.screen().cell(0, 0).attrs.fg, Color::Indexed(200));
    }

    #[test]
    fn test_bold_italic() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"\x1b[1;3mBoldItalic");
        assert!(term.screen().cell(0, 0).attrs.bold);
        assert!(term.screen().cell(0, 0).attrs.italic);
    }

    #[test]
    fn test_sgr_reset() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"\x1b[1;31mR\x1b[0mN");
        assert!(term.screen().cell(0, 0).attrs.bold);
        assert_eq!(term.screen().cell(0, 0).attrs.fg, Color::Indexed(1));
        // After reset
        assert!(!term.screen().cell(0, 1).attrs.bold);
        assert_eq!(term.screen().cell(0, 1).attrs.fg, Color::Default);
    }

    #[test]
    fn test_cursor_movement() {
        let mut term = Terminal::new(24, 80);
        // Move cursor to row 5, col 10 (1-based: 5,10)
        term.feed(b"\x1b[5;10H");
        assert_eq!(term.cursor(), (4, 9)); // 0-based

        // Move cursor up 2
        term.feed(b"\x1b[2A");
        assert_eq!(term.cursor(), (2, 9));

        // Move cursor down 1
        term.feed(b"\x1b[1B");
        assert_eq!(term.cursor(), (3, 9));

        // Move cursor forward 3
        term.feed(b"\x1b[3C");
        assert_eq!(term.cursor(), (3, 12));

        // Move cursor back 5
        term.feed(b"\x1b[5D");
        assert_eq!(term.cursor(), (3, 7));
    }

    #[test]
    fn test_newline_scrolling() {
        let mut term = Terminal::new(3, 10);
        term.feed(b"Line1\r\nLine2\r\nLine3\r\nLine4");

        // After 4 lines in a 3-row terminal, Line1 should have scrolled off.
        // Visible screen should show Line2, Line3, Line4
        assert_eq!(term.screen().cell(0, 0).character, 'L');
        assert_eq!(term.screen().cell(0, 4).character, '2');
        assert_eq!(term.screen().cell(1, 4).character, '3');
        assert_eq!(term.screen().cell(2, 4).character, '4');

        // Scrollback should have Line1
        assert_eq!(term.scrollback().len(), 1);
    }

    #[test]
    fn test_erase_display() {
        let mut term = Terminal::new(3, 10);
        term.feed(b"AAAAAAAAAA");
        // Erase entire display
        term.feed(b"\x1b[2J");
        for col in 0..10 {
            assert_eq!(term.screen().cell(0, col).character, ' ');
        }
    }

    #[test]
    fn test_erase_line() {
        let mut term = Terminal::new(3, 10);
        term.feed(b"ABCDEFGHIJ");
        // Move cursor to col 5 (1-based: 6)
        term.feed(b"\x1b[1;6H");
        // Erase from cursor to end of line
        term.feed(b"\x1b[0K");
        // First 5 characters should remain
        assert_eq!(term.screen().cell(0, 0).character, 'A');
        assert_eq!(term.screen().cell(0, 4).character, 'E');
        // Position 5 onward should be blank
        assert_eq!(term.screen().cell(0, 5).character, ' ');
        assert_eq!(term.screen().cell(0, 9).character, ' ');
    }

    #[test]
    fn test_osc_title() {
        let mut term = Terminal::new(24, 80);
        // OSC 0 ; title ST  (ST = ESC \)
        term.feed(b"\x1b]0;My Terminal Title\x1b\\");
        assert_eq!(term.title(), "My Terminal Title");

        let events = term.drain_events();
        assert!(events
            .iter()
            .any(|e| *e == TerminalEvent::TitleChanged("My Terminal Title".to_string())));
    }

    #[test]
    fn test_resize() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"Hello");
        term.resize(10, 40);
        assert_eq!(term.screen().rows(), 10);
        assert_eq!(term.screen().cols(), 40);
        // Text should still be there
        assert_eq!(term.screen().cell(0, 0).character, 'H');
    }

    #[test]
    fn test_alt_screen() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"Main screen text");

        // Switch to alt screen
        term.feed(b"\x1b[?1049h");
        assert!(term.using_alt_screen);
        // Alt screen should be clear
        assert_eq!(term.screen().cell(0, 0).character, ' ');

        term.feed(b"Alt text");

        // Switch back to main screen
        term.feed(b"\x1b[?1049l");
        assert!(!term.using_alt_screen);
        // Main screen text should be restored
        assert_eq!(term.screen().cell(0, 0).character, 'M');
    }

    #[test]
    fn test_cursor_visibility() {
        let mut term = Terminal::new(24, 80);
        assert!(term.cursor_visible());

        // Hide cursor
        term.feed(b"\x1b[?25l");
        assert!(!term.cursor_visible());

        // Show cursor
        term.feed(b"\x1b[?25h");
        assert!(term.cursor_visible());
    }

    #[test]
    fn test_tab() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"A\tB");
        assert_eq!(term.screen().cell(0, 0).character, 'A');
        assert_eq!(term.screen().cell(0, 8).character, 'B');
    }

    #[test]
    fn test_backspace() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"AB\x08C");
        // Backspace moves cursor back, then 'C' overwrites 'B'
        assert_eq!(term.screen().cell(0, 0).character, 'A');
        assert_eq!(term.screen().cell(0, 1).character, 'C');
    }

    #[test]
    fn test_bell() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"\x07");
        let events = term.drain_events();
        assert!(events.iter().any(|e| *e == TerminalEvent::Bell));
    }

    #[test]
    fn test_line_wrap() {
        let mut term = Terminal::new(3, 5);
        term.feed(b"ABCDEFGH");
        // First row: ABCDE
        assert_eq!(term.screen().cell(0, 0).character, 'A');
        assert_eq!(term.screen().cell(0, 4).character, 'E');
        // Second row: FGH (wrapped)
        assert_eq!(term.screen().cell(1, 0).character, 'F');
        assert_eq!(term.screen().cell(1, 2).character, 'H');
    }

    #[test]
    fn test_save_restore_cursor() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"\x1b[5;10H"); // Move to (4, 9)
        term.feed(b"\x1b7"); // Save cursor
        term.feed(b"\x1b[1;1H"); // Move to (0, 0)
        assert_eq!(term.cursor(), (0, 0));
        term.feed(b"\x1b8"); // Restore cursor
        assert_eq!(term.cursor(), (4, 9));
    }

    #[test]
    fn test_delete_characters() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"ABCDEF");
        term.feed(b"\x1b[1;3H"); // Move to col 2 (0-based)
        term.feed(b"\x1b[2P"); // Delete 2 chars
        assert_eq!(term.screen().cell(0, 0).character, 'A');
        assert_eq!(term.screen().cell(0, 1).character, 'B');
        assert_eq!(term.screen().cell(0, 2).character, 'E');
        assert_eq!(term.screen().cell(0, 3).character, 'F');
        assert_eq!(term.screen().cell(0, 4).character, ' ');
    }

    #[test]
    fn test_insert_characters() {
        let mut term = Terminal::new(24, 80);
        term.feed(b"ABCD");
        term.feed(b"\x1b[1;3H"); // Move to col 2 (0-based)
        term.feed(b"\x1b[2@"); // Insert 2 blank chars
        assert_eq!(term.screen().cell(0, 0).character, 'A');
        assert_eq!(term.screen().cell(0, 1).character, 'B');
        assert_eq!(term.screen().cell(0, 2).character, ' ');
        assert_eq!(term.screen().cell(0, 3).character, ' ');
        assert_eq!(term.screen().cell(0, 4).character, 'C');
        assert_eq!(term.screen().cell(0, 5).character, 'D');
    }

    #[test]
    fn test_bright_colors() {
        let mut term = Terminal::new(24, 80);
        // ESC[91m = bright red foreground
        term.feed(b"\x1b[91mBright");
        assert_eq!(term.screen().cell(0, 0).attrs.fg, Color::Indexed(9));
    }

    #[test]
    fn test_background_color() {
        let mut term = Terminal::new(24, 80);
        // ESC[44m = blue background
        term.feed(b"\x1b[44mBlue");
        assert_eq!(term.screen().cell(0, 0).attrs.bg, Color::Indexed(4));
    }

    #[test]
    fn test_total_lines() {
        let mut term = Terminal::new(3, 10);
        assert_eq!(term.total_lines(), 3);
        // Cause scrollback
        term.feed(b"L1\r\nL2\r\nL3\r\nL4\r\nL5");
        assert_eq!(term.total_lines(), 3 + term.scrollback().len());
    }
}
