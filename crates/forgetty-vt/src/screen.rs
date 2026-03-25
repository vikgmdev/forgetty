//! Terminal screen buffer representation.
//!
//! Manages the grid of cells that makes up the visible terminal display,
//! including dirty-tracking via generation counters for efficient rendering.

/// A single cell in the terminal grid.
#[derive(Debug, Clone)]
pub struct Cell {
    pub character: char,
    pub attrs: CellAttributes,
}

impl Default for Cell {
    fn default() -> Self {
        Self { character: ' ', attrs: CellAttributes::default() }
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
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Color {
    /// Use the terminal's default foreground or background color.
    #[default]
    Default,
    /// A color from the 256-color palette (0-255).
    Indexed(u8),
    /// A 24-bit truecolor value.
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
}

impl Screen {
    /// Create a new screen with the given dimensions, filled with blank cells.
    pub fn new(rows: usize, cols: usize) -> Self {
        let cells = (0..rows).map(|_| (0..cols).map(|_| Cell::default()).collect()).collect();
        Self { cells, rows, cols, row_generations: vec![0; rows], generation: 0 }
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
