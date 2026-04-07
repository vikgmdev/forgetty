//! Terminal screen buffer representation.
//!
//! Manages the grid of cells that makes up the visible terminal display,
//! including dirty-tracking via generation counters for efficient rendering.
//! The cell grid is populated from the libghostty-vt render state.

/// A single cell in the terminal grid.
#[derive(Debug, Clone)]
pub struct Cell {
    pub grapheme: String,
    pub attrs: CellAttributes,
}

impl Default for Cell {
    fn default() -> Self {
        Self { grapheme: " ".to_string(), attrs: CellAttributes::default() }
    }
}

impl Cell {
    /// Update this cell's grapheme in-place, reusing the existing `String`
    /// heap allocation when the new value fits within the current capacity.
    /// Returns `true` if the grapheme actually changed.
    #[inline]
    pub fn update_grapheme(&mut self, new_grapheme: &str) -> bool {
        if self.grapheme == new_grapheme {
            return false;
        }
        self.grapheme.clear();
        self.grapheme.push_str(new_grapheme);
        true
    }

    /// Update this cell in-place from new grapheme + attrs, reusing the
    /// existing `String` allocation when possible. Returns `true` if
    /// anything changed.
    #[inline]
    pub fn update_in_place(&mut self, new_grapheme: &str, new_attrs: CellAttributes) -> bool {
        let grapheme_changed = self.update_grapheme(new_grapheme);
        let attrs_changed = self.attrs != new_attrs;
        if attrs_changed {
            self.attrs = new_attrs;
        }
        grapheme_changed || attrs_changed
    }
}

/// Visual attributes for a terminal cell.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CellAttributes {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub inverse: bool,
    pub dim: bool,
}

/// A color value for terminal cells.
///
/// All palette lookups are resolved by libghostty-vt before reaching Rust.
/// Only `Default` (use theme color) and `Rgb` (pre-resolved) remain.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Color {
    /// Use the terminal's default foreground or background color.
    #[default]
    Default,
    /// A 24-bit truecolor value (pre-resolved from palette by libghostty-vt).
    Rgb(u8, u8, u8),
}

/// The visible screen buffer.
pub struct Screen {
    cells: Vec<Vec<Cell>>,
    rows: usize,
    cols: usize,
    /// Generation counter per row (for dirty tracking).
    row_generations: Vec<u64>,
    /// Global generation counter.
    generation: u64,
    /// Per-row soft-wrap flag. `true` means the row's content continues on
    /// the next row (no hard newline at the end).
    row_wraps: Vec<bool>,
}

impl Screen {
    /// Create a new screen with the given dimensions, filled with blank cells.
    pub fn new(rows: usize, cols: usize) -> Self {
        let cells = (0..rows).map(|_| (0..cols).map(|_| Cell::default()).collect()).collect();
        Self {
            cells,
            rows,
            cols,
            row_generations: vec![0; rows],
            generation: 0,
            row_wraps: vec![false; rows],
        }
    }

    /// Get a cell at (row, col).
    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.cells[row][col]
    }

    /// Set a cell at (row, col), marking the row as dirty.
    pub fn set_cell(&mut self, row: usize, col: usize, cell: Cell) {
        self.generation += 1;
        self.cells[row][col] = cell;
        self.row_generations[row] = self.generation;
    }

    /// Get a full row as a slice.
    pub fn row(&self, row: usize) -> &[Cell] {
        &self.cells[row]
    }

    /// Number of rows.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Number of columns.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Resize the screen. New cells are filled with blanks; excess is truncated.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        // Resize columns in existing rows
        for row in &mut self.cells {
            row.resize_with(cols, Cell::default);
        }
        // Add or remove rows
        self.cells.resize_with(rows, || (0..cols).map(|_| Cell::default()).collect());
        self.row_generations.resize(rows, 0);
        self.row_wraps.resize(rows, false);
        self.rows = rows;
        self.cols = cols;
        self.generation += 1;
    }

    /// Check if a row has been modified since the given generation.
    pub fn row_dirty_since(&self, row: usize, since: u64) -> bool {
        self.row_generations[row] > since
    }

    /// Get the current generation counter.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns `true` if the given row is soft-wrapped (its content
    /// continues on the next row without a hard newline).
    pub fn is_row_wrapped(&self, row: usize) -> bool {
        self.row_wraps.get(row).copied().unwrap_or(false)
    }

    /// Set the soft-wrap flag for a row.
    pub(crate) fn set_row_wrap(&mut self, row: usize, wrapped: bool) {
        if row < self.row_wraps.len() {
            self.row_wraps[row] = wrapped;
        }
    }

    /// Replace the entire grid from an externally-built cell matrix.
    /// Also marks all replaced rows dirty and bumps the generation.
    #[allow(dead_code)]
    pub(crate) fn replace_from_grid(&mut self, grid: Vec<Vec<Cell>>, dirty_rows: &[bool]) {
        let new_rows = grid.len();
        let new_cols = grid.first().map(|r| r.len()).unwrap_or(0);
        self.cells = grid;
        self.rows = new_rows;
        self.cols = new_cols;
        self.row_generations.resize(new_rows, 0);

        self.generation += 1;
        for (i, is_dirty) in dirty_rows.iter().enumerate() {
            if *is_dirty && i < new_rows {
                self.row_generations[i] = self.generation;
            }
        }
    }

    /// Update a cell in-place, reusing existing heap allocations.
    /// Only bumps the row generation if the cell actually changed.
    #[inline]
    pub(crate) fn update_cell_in_place(
        &mut self,
        row: usize,
        col: usize,
        grapheme: &str,
        attrs: CellAttributes,
    ) -> bool {
        let changed = self.cells[row][col].update_in_place(grapheme, attrs);
        if changed {
            self.generation += 1;
            self.row_generations[row] = self.generation;
        }
        changed
    }

    /// Ensure the grid has exactly `rows` x `cols` dimensions.
    /// Reuses existing row/cell allocations where possible.
    pub(crate) fn ensure_capacity(&mut self, rows: usize, cols: usize) {
        // Adjust column count in existing rows
        for row in &mut self.cells {
            if row.len() < cols {
                row.reserve(cols - row.len());
                while row.len() < cols {
                    row.push(Cell::default());
                }
            } else if row.len() > cols {
                row.truncate(cols);
            }
        }
        // Adjust row count
        if self.cells.len() < rows {
            self.cells.reserve(rows - self.cells.len());
            while self.cells.len() < rows {
                let mut new_row = Vec::with_capacity(cols);
                new_row.resize_with(cols, Cell::default);
                self.cells.push(new_row);
            }
        } else if self.cells.len() > rows {
            self.cells.truncate(rows);
        }
        self.row_generations.resize(rows, 0);
        self.row_wraps.resize(rows, false);
        self.rows = rows;
        self.cols = cols;
    }

    /// Get mutable access to a cell (for direct in-place updates).
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        &mut self.cells[row][col]
    }

    /// Mark a specific row dirty (bump generation) without changing cell contents.
    /// Used when the caller knows a row changed but already updated cells directly.
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn mark_row_dirty(&mut self, row: usize) {
        self.generation += 1;
        self.row_generations[row] = self.generation;
    }

    /// Clear the entire screen with blank cells.
    pub fn clear(&mut self) {
        self.generation += 1;
        for row_idx in 0..self.rows {
            for col_idx in 0..self.cols {
                self.cells[row_idx][col_idx] = Cell::default();
            }
            self.row_generations[row_idx] = self.generation;
        }
    }

    /// Scroll the region [top..=bottom] up by `n` lines.
    /// Returns the lines that scrolled off the top (for scrollback).
    pub fn scroll_up(&mut self, n: usize, top: usize, bottom: usize) -> Vec<Vec<Cell>> {
        let n = n.min(bottom - top + 1);
        self.generation += 1;

        // Collect lines that scroll off
        let scrolled_off: Vec<Vec<Cell>> = self.cells[top..top + n].to_vec();

        // Shift rows up within the region
        for i in top..=bottom {
            if i + n <= bottom {
                let src = self.cells[i + n].clone();
                self.cells[i] = src;
            } else {
                self.cells[i] = (0..self.cols).map(|_| Cell::default()).collect();
            }
            self.row_generations[i] = self.generation;
        }

        scrolled_off
    }

    /// Scroll the region [top..=bottom] down by `n` lines.
    pub fn scroll_down(&mut self, n: usize, top: usize, bottom: usize) {
        let n = n.min(bottom - top + 1);
        self.generation += 1;

        // Shift rows down within the region
        for i in (top..=bottom).rev() {
            if i >= top + n {
                let src = self.cells[i - n].clone();
                self.cells[i] = src;
            } else {
                self.cells[i] = (0..self.cols).map(|_| Cell::default()).collect();
            }
            self.row_generations[i] = self.generation;
        }
    }

    /// Mark a row as dirty (bump its generation).
    pub fn mark_dirty(&mut self, row: usize) {
        self.generation += 1;
        self.row_generations[row] = self.generation;
    }
}
