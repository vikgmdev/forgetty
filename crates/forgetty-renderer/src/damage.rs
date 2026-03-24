//! Damage tracking for efficient partial redraws.
//!
//! Tracks which regions of the terminal have changed since the last frame
//! so that only dirty cells need to be re-rendered.

// TODO: Phase 3 — implement DamageTracker
//
// pub struct DamageTracker {
//     dirty_rows: HashSet<usize>,
//     full_redraw: bool,
// }
//
// impl DamageTracker {
//     pub fn mark_dirty(&mut self, row: usize) { ... }
//     pub fn mark_all_dirty(&mut self) { ... }
//     pub fn take_damage(&mut self) -> Vec<usize> { ... }
// }
