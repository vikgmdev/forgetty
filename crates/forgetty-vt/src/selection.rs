//! Text selection handling for the terminal.
//!
//! Manages rectangular and linear text selections within the terminal grid,
//! supporting mouse-driven selection and conversion of selected regions
//! to coordinate ranges.

use crate::screen::Screen;

/// The mode of a text selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Character-level selection (normal click-drag).
    Normal,
    /// Rectangular (block) selection.
    Block,
    /// Full-line selection.
    Line,
    /// Word-level selection (double-click).
    Word,
}

/// A text selection within the terminal grid.
#[derive(Debug, Clone)]
pub struct Selection {
    /// The anchor point where selection started (row, col).
    pub start: (usize, usize),
    /// The current end point of the selection (row, col).
    pub end: (usize, usize),
    /// The selection mode.
    pub mode: SelectionMode,
}

impl Selection {
    /// Create a new selection starting at the given position.
    pub fn new(row: usize, col: usize, mode: SelectionMode) -> Self {
        Self { start: (row, col), end: (row, col), mode }
    }

    /// Update the end point of the selection.
    pub fn update(&mut self, row: usize, col: usize) {
        self.end = (row, col);
    }

    /// Returns true if the given cell is within the selection.
    pub fn contains(&self, row: usize, col: usize) -> bool {
        let ((sr, sc), (er, ec)) = self.ordered();

        match self.mode {
            SelectionMode::Normal | SelectionMode::Word => {
                if sr == er {
                    row == sr && col >= sc && col <= ec
                } else if row == sr {
                    col >= sc
                } else if row == er {
                    col <= ec
                } else {
                    row > sr && row < er
                }
            }
            SelectionMode::Block => {
                let min_col = sc.min(ec);
                let max_col = sc.max(ec);
                row >= sr && row <= er && col >= min_col && col <= max_col
            }
            SelectionMode::Line => row >= sr && row <= er,
        }
    }

    /// Returns the selection with start <= end (ordered by position).
    pub fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.start.0 < self.end.0 || (self.start.0 == self.end.0 && self.start.1 <= self.end.1) {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    /// Returns true if the selection is empty (start == end with no drag).
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Extract the selected text from a screen buffer.
    ///
    /// Collects graphemes from the selected cells and joins rows with `\n`.
    /// For Line mode, selects entire rows.
    /// For Normal/Word mode, follows the standard linear selection.
    pub fn extract_text(&self, screen: &Screen) -> String {
        let ((sr, sc), (er, ec)) = self.ordered();
        let num_rows = screen.rows();
        let num_cols = screen.cols();

        if sr >= num_rows {
            return String::new();
        }

        let mut lines: Vec<String> = Vec::new();

        match self.mode {
            SelectionMode::Normal | SelectionMode::Word => {
                for row in sr..=er.min(num_rows - 1) {
                    let cells = screen.row(row);
                    let col_start = if row == sr { sc } else { 0 };
                    let col_end = if row == er { ec.min(num_cols - 1) } else { num_cols - 1 };

                    let mut line = String::new();
                    for col in col_start..=col_end.min(cells.len().saturating_sub(1)) {
                        line.push_str(&cells[col].grapheme);
                    }
                    lines.push(line);
                }
            }
            SelectionMode::Line => {
                for row in sr..=er.min(num_rows - 1) {
                    let cells = screen.row(row);
                    let mut line = String::new();
                    for col in 0..num_cols.min(cells.len()) {
                        line.push_str(&cells[col].grapheme);
                    }
                    lines.push(line);
                }
            }
            SelectionMode::Block => {
                let min_col = sc.min(ec);
                let max_col = sc.max(ec).min(num_cols - 1);
                for row in sr..=er.min(num_rows - 1) {
                    let cells = screen.row(row);
                    let mut line = String::new();
                    for col in min_col..=max_col.min(cells.len().saturating_sub(1)) {
                        line.push_str(&cells[col].grapheme);
                    }
                    lines.push(line);
                }
            }
        }

        lines.join("\n")
    }
}
