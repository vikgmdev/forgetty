//! Virtual terminal (VT) parser and terminal state management.
//!
//! This crate provides a safe Rust interface backed by libghostty-vt for
//! parsing escape sequences, maintaining terminal state, managing the screen
//! buffer, and handling text selection.

pub mod ffi;
pub mod screen;
pub mod selection;
pub mod terminal;

pub use screen::{Cell, CellAttributes, Color, Screen};
pub use selection::{Selection, SelectionMode};
pub use terminal::{Terminal, TerminalEvent};
