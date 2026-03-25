//! Damage tracking for efficient partial redraws.
//!
//! Tracks which regions of the terminal have changed since the last frame
//! so that only dirty cells need to be re-rendered.

use forgetty_vt::Screen;

/// Tracks which rows need re-rendering by comparing generation counters
/// with the terminal screen's dirty tracking.
pub struct DamageTracker {
    /// The generation counter at the time we last marked clean.
    last_generation: u64,
    /// Per-row generation snapshots from the last clean mark.
    row_generations: Vec<u64>,
}

impl DamageTracker {
    /// Create a new tracker for a screen with the given number of rows.
    pub fn new(rows: usize) -> Self {
        Self { last_generation: 0, row_generations: vec![0; rows] }
    }

    /// Returns true if the screen has changed since we last marked it clean,
    /// or if our row count doesn't match (meaning a resize happened).
    pub fn needs_full_redraw(&self, screen: &Screen) -> bool {
        self.row_generations.len() != screen.rows() || screen.generation() != self.last_generation
    }

    /// Returns the indices of rows that have been modified since we last marked clean.
    pub fn dirty_rows(&self, screen: &Screen) -> Vec<usize> {
        (0..screen.rows())
            .filter(|&row| {
                if row >= self.row_generations.len() {
                    return true;
                }
                screen.row_dirty_since(row, self.row_generations[row])
            })
            .collect()
    }

    /// Snapshot the current screen generation so future queries compare against now.
    pub fn mark_clean(&mut self, screen: &Screen) {
        self.last_generation = screen.generation();
        let gen = screen.generation();
        self.row_generations.resize(screen.rows(), 0);
        for g in &mut self.row_generations {
            *g = gen;
        }
    }

    /// Resize internal tracking to match a new row count.
    pub fn resize(&mut self, rows: usize) {
        self.row_generations.resize(rows, 0);
    }
}
