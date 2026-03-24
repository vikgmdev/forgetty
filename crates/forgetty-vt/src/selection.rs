//! Text selection handling for the terminal.
//!
//! Manages rectangular and linear text selections within the terminal grid,
//! supporting mouse-driven selection, word/line selection modes, and
//! conversion of selections to plain text for clipboard operations.

// TODO: Phase 3 — implement Selection types
//
// pub enum SelectionMode {
//     Normal,
//     Word,
//     Line,
//     Block,
// }
//
// pub struct Selection {
//     pub start: CellCoord,
//     pub end: CellCoord,
//     pub mode: SelectionMode,
// }
//
// impl Selection {
//     pub fn contains(&self, coord: CellCoord) -> bool { ... }
//     pub fn to_text(&self, screen: &Screen) -> String { ... }
// }
