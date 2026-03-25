//! Virtual terminal (VT) parser and terminal state management.
//!
//! This crate provides a safe Rust interface for parsing escape sequences,
//! maintaining terminal state, managing the screen buffer, and handling
//! text selection.
//!
//! Currently backed by the `vte` crate as an interim pure-Rust VT parser.
//! The public API is designed so that swapping in libghostty-vt later
//! only changes the internals, not the API surface.

pub mod ffi;
pub mod screen;
pub mod selection;
pub mod terminal;

pub use screen::{Cell, CellAttributes, Color, Screen};
pub use selection::{Selection, SelectionMode};
pub use terminal::{Terminal, TerminalEvent};
