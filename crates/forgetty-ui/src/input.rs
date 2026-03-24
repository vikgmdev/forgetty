//! Input event processing.
//!
//! Translates winit keyboard and mouse events into terminal input sequences
//! or UI actions (e.g., scrolling, selection, pane focus changes).

// TODO: Phase 4 — implement input handling
//
// pub fn handle_key_event(event: &KeyEvent, modifiers: ModifiersState) -> InputAction { ... }
// pub fn handle_mouse_event(event: &MouseEvent) -> InputAction { ... }
//
// pub enum InputAction {
//     SendBytes(Vec<u8>),
//     Scroll(i32),
//     StartSelection(CellCoord),
//     UpdateSelection(CellCoord),
//     EndSelection,
//     UiCommand(UiCommand),
// }
