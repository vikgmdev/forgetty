//! Selection state and the math around it. Rows are absolute (scrollback offset plus viewport
//! row), so a selection survives scrolling. No terminal types.

use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelMode {
    Normal,
    Word,
    Line,
}

/// A selection: the unit the user first clicked (the anchor, `a0` to `a1`) and the unit under
/// the pointer now (the head, `h0` to `h1`). Units are inclusive `(row, col)` ends: a single
/// cell, a word or a line. The selected range runs from the earlier start to the later end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sel {
    pub mode: SelMode,
    pub a0: (usize, usize),
    pub a1: (usize, usize),
    pub h0: (usize, usize),
    pub h1: (usize, usize),
}

impl Sel {
    pub fn new(mode: SelMode, u0: (usize, usize), u1: (usize, usize)) -> Self {
        Sel { mode, a0: u0, a1: u1, h0: u0, h1: u1 }
    }

    pub fn range(&self) -> ((usize, usize), (usize, usize)) {
        (self.a0.min(self.h0), self.a1.max(self.h1))
    }

    pub fn is_empty(&self) -> bool {
        self.mode == SelMode::Normal && self.a0 == self.h0 && self.a1 == self.h1
    }

    /// Move the head to another unit (a drag, or a shift-click that extends the selection).
    /// The anchor stays. Returns true when the selection changed.
    pub fn extend_to(&mut self, u0: (usize, usize), u1: (usize, usize)) -> bool {
        if self.h0 == u0 && self.h1 == u1 {
            return false;
        }
        self.h0 = u0;
        self.h1 = u1;
        true
    }

    /// Select everything from `(first_row, 0)` to `(last_row, last_col)`.
    pub fn all(first_row: usize, last_row: usize, last_col: usize) -> Self {
        Sel::new(SelMode::Normal, (first_row, 0), (first_row, 0)).with_head((last_row, last_col))
    }

    fn with_head(mut self, end: (usize, usize)) -> Self {
        self.h0 = end;
        self.h1 = end;
        self
    }
}

/// How often the viewport scrolls while the pointer is held outside the pane.
pub const AUTOSCROLL_TICK: Duration = Duration::from_millis(50);

/// Rows to scroll on one tick while the pointer is at height `y` (px from the top of the text
/// area) over a text area `height` px tall. Negative scrolls into history. Zero inside. One
/// row at the edge, one more for every extra line height the pointer is away, at most 12.
/// (GTK scrolls 3 rows per mouse move event and stops when the mouse stops.)
pub fn autoscroll_rows(y: f32, height: f32, line_h: f32) -> isize {
    let line_h = line_h.max(1.0);
    let (dir, dist) = if y < 0.0 {
        (-1, -y)
    } else if y > height {
        (1, y - height)
    } else {
        return 0;
    };
    dir * (1 + (dist / line_h) as isize).min(12)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_runs_from_the_earlier_start_to_the_later_end() {
        let mut s = Sel::new(SelMode::Normal, (10, 5), (10, 5));
        assert!(s.is_empty());
        assert!(s.extend_to((12, 3), (12, 3)));
        assert_eq!(s.range(), ((10, 5), (12, 3)));
        // Dragging back above the anchor flips the range; the anchor cell stays inside it.
        assert!(s.extend_to((8, 2), (8, 2)));
        assert_eq!(s.range(), ((8, 2), (10, 5)));
        assert!(!s.extend_to((8, 2), (8, 2)), "no change");
        assert!(!s.is_empty());
    }

    #[test]
    fn a_word_anchor_is_kept_whole_when_extending() {
        // Double click on a word at cols 4..8, then shift-click further on in the same row.
        let mut s = Sel::new(SelMode::Word, (3, 4), (3, 8));
        s.extend_to((3, 20), (3, 25));
        assert_eq!(s.range(), ((3, 4), (3, 25)));
        // And before it.
        s.extend_to((3, 0), (3, 1));
        assert_eq!(s.range(), ((3, 0), (3, 8)));
        assert!(!s.is_empty());
    }

    #[test]
    fn select_all_covers_the_given_rows() {
        let s = Sel::all(100, 129, 119);
        assert_eq!(s.range(), ((100, 0), (129, 119)));
        assert!(!s.is_empty());
    }

    #[test]
    fn autoscroll_speeds_up_with_distance_and_stops_inside() {
        assert_eq!(autoscroll_rows(10.0, 500.0, 20.0), 0);
        assert_eq!(autoscroll_rows(0.0, 500.0, 20.0), 0);
        assert_eq!(autoscroll_rows(500.0, 500.0, 20.0), 0);
        assert_eq!(autoscroll_rows(-1.0, 500.0, 20.0), -1);
        assert_eq!(autoscroll_rows(-45.0, 500.0, 20.0), -3);
        assert_eq!(autoscroll_rows(501.0, 500.0, 20.0), 1);
        assert_eq!(autoscroll_rows(560.0, 500.0, 20.0), 4);
        assert_eq!(autoscroll_rows(-100_000.0, 500.0, 20.0), -12);
        assert_eq!(autoscroll_rows(100_000.0, 500.0, 20.0), 12);
    }
}
