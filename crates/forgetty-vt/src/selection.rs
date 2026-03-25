//! Text selection handling for the terminal.
//!
//! Manages rectangular and linear text selections within the terminal grid,
//! supporting mouse-driven selection and conversion of selected regions
//! to coordinate ranges.

/// The mode of a text selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Character-level selection (normal click-drag).
    Normal,
    /// Rectangular (block) selection.
    Block,
    /// Full-line selection.
    Line,
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
            SelectionMode::Normal => {
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
}
