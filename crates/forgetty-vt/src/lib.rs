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

/// A placeholder ANSI palette entry: fully opaque black.
///
/// Used by callers that have no theme access yet (standalone UI panes,
/// boot-time smoke tests) to fill the required `[Rgba; 16]` argument to
/// `Terminal::new()`. The GTK renderer calls `set_ansi_palette()` with the
/// real theme palette before the first `sync_screen`, so these black entries
/// are never actually used for rendering.
pub const ANSI_PALETTE_BLACK: forgetty_core::Rgba =
    forgetty_core::Rgba { r: 0, g: 0, b: 0, a: 255 };
