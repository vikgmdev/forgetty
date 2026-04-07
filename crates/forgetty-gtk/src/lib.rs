//! GTK4/libadwaita platform shell for the Forgetty terminal emulator.
//!
//! This crate provides the native Linux UI layer using GTK4 and libadwaita,
//! delivering GNOME-style client-side decorations and native text rendering
//! via Pango/FreeType. It is the thin platform shell that wraps the shared
//! Rust core crates.

pub mod app;
pub mod clipboard;
pub mod code_block;
pub mod daemon_client;
pub mod input;
pub mod preferences;
pub mod pty_bridge;
pub mod settings_view;
pub mod terminal;
