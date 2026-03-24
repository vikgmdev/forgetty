//! Virtual terminal (VT) parser and terminal state management.
//!
//! This crate wraps the Zig-based VT parser (derived from Ghostty) and provides
//! a safe Rust interface for parsing escape sequences, maintaining terminal state,
//! managing the screen buffer, and handling text selection.

pub mod ffi;
pub mod screen;
pub mod selection;
pub mod terminal;

// TODO: Phase 2 — re-export key types once the VT parser is integrated
