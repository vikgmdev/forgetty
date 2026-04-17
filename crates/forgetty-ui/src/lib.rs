//! User interface layer for the Forgetty terminal emulator.
//!
//! This crate ties together the VT parser, PTY backend, and GPU renderer
//! into a windowed application using winit. It handles window management,
//! input processing, pane/tab layout, clipboard integration, and
//! system notifications.

// Crate slated for deletion in V2-012; suppressing clippy 1.94.0 nits rather
// than invest in cleanup for dead code.
#![allow(clippy::new_without_default, clippy::not_unsafe_ptr_arg_deref, clippy::too_many_arguments)]

pub mod app;
pub mod clipboard;
pub mod ghostty_input;
pub mod input;
pub mod keybindings;
pub mod layout;
pub mod notifications;
pub mod pane;
pub mod pane_tree;
pub mod tab;
pub mod tab_bar;
pub mod window;
