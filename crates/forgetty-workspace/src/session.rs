//! Terminal session state.
//!
//! Captures the state of a terminal session including window geometry,
//! tab layout, pane configuration, and working directories for
//! save/restore functionality.

// TODO: Phase 7 — implement Session
//
// use serde::{Deserialize, Serialize};
//
// #[derive(Debug, Clone, Serialize, Deserialize)]
// pub struct Session {
//     pub windows: Vec<WindowState>,
// }
//
// #[derive(Debug, Clone, Serialize, Deserialize)]
// pub struct WindowState {
//     pub x: i32,
//     pub y: i32,
//     pub width: u32,
//     pub height: u32,
//     pub tabs: Vec<TabState>,
// }
