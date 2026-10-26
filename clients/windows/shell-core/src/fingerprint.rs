//! Did the rows of the scrollback move?
//!
//! A scan or a copy that runs over many ticks names rows by absolute number. When the
//! scrollback is full, old rows are dropped as new output comes in and every number moves up.
//! libghostty-vt has no counter of dropped rows, so the rows are recognised by what they say:
//! a fingerprint takes a hash of rows spread over a range, plus one hash of a window at each
//! end of it, and the same fingerprint taken again must be equal.
//!
//! Limit: output that repeats with a period that divides the shift (the same line over and
//! over) looks the same after the move. Nothing is lost then, because the rows that moved into
//! place say the same thing as the rows that left.

use std::hash::{Hash, Hasher};

/// Rows hashed one by one, spread over the range (both ends included).
const SPREAD: usize = 32;
/// Rows hashed together at the start of the range, and again at its end.
const WINDOW: usize = 16;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fingerprint {
    pub lo: usize,
    pub hi: usize,
    hashes: Vec<u64>,
}

impl Fingerprint {
    /// Fingerprint of rows `lo..hi`. `row_hash(r)` is any hash of row `r`'s text (a fixed value
    /// for a row that cannot be read). An empty range gives an empty fingerprint that equals
    /// every other empty one with the same bounds.
    pub fn take(lo: usize, hi: usize, mut row_hash: impl FnMut(usize) -> u64) -> Self {
        let mut hashes = Vec::new();
        if hi > lo {
            let n = hi - lo;
            for k in 0..=SPREAD {
                hashes.push(row_hash(lo + (n - 1) * k / SPREAD));
            }
            let mut window = |from: usize, to: usize| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                for r in from..to {
                    row_hash(r).hash(&mut h);
                }
                hashes.push(h.finish());
            };
            window(lo, (lo + WINDOW).min(hi));
            window(hi.saturating_sub(WINDOW).max(lo), hi);
        }
        Fingerprint { lo, hi, hashes }
    }
}

/// A hash of a row's text, for `Fingerprint::take`.
pub fn hash_text(text: &str, wrapped: bool) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    wrapped.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A buffer of rows with no repeats: row `i` says `(i * 7919) % 10007`.
    fn row(i: usize) -> u64 {
        hash_text(&format!("line {}", (i * 7919) % 10_007), false)
    }

    #[test]
    fn the_same_rows_give_the_same_fingerprint() {
        assert_eq!(Fingerprint::take(0, 5000, row), Fingerprint::take(0, 5000, row));
        assert_ne!(Fingerprint::take(0, 5000, row), Fingerprint::take(1, 5000, row));
        assert_eq!(Fingerprint::take(7, 7, row), Fingerprint::take(7, 7, |_| 1));
    }

    #[test]
    fn dropped_rows_are_seen_for_every_shift() {
        let before = Fingerprint::take(0, 3000, row);
        // The oldest `n` rows are gone: row `i` now holds what row `i + n` held.
        for n in 1..300 {
            let after = Fingerprint::take(0, 3000, |i| row(i + n));
            assert_ne!(before, after, "shift {n}");
        }
    }

    #[test]
    fn short_ranges_and_single_rows_work() {
        for len in 1..40 {
            let before = Fingerprint::take(10, 10 + len, row);
            assert_ne!(before, Fingerprint::take(10, 10 + len, |i| row(i + 1)), "len {len}");
        }
    }

    #[test]
    fn a_row_that_changed_in_the_middle_of_the_window_is_seen() {
        let before = Fingerprint::take(0, 100, row);
        // Row 3 is inside the first window but is not one of the spread samples.
        let after = Fingerprint::take(0, 100, |i| if i == 3 { 1 } else { row(i) });
        assert_ne!(before, after);
    }

    #[test]
    fn repeating_output_moves_unseen_only_by_whole_periods() {
        // A line that repeats with period 5: a shift of 5 looks the same (and means the
        // same), a shift of 1 to 4 is seen.
        let rep = |i: usize| hash_text(&format!("tick {}", i % 5), false);
        let before = Fingerprint::take(0, 3000, rep);
        for n in 1..5 {
            assert_ne!(before, Fingerprint::take(0, 3000, |i| rep(i + n)), "shift {n}");
        }
        assert_eq!(before, Fingerprint::take(0, 3000, |i| rep(i + 5)));
    }
}
