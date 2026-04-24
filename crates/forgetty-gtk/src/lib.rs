//! GTK4/libadwaita platform shell for the Forgetty terminal emulator.
//!
//! This crate provides the native Linux UI layer using GTK4 and libadwaita,
//! delivering GNOME-style client-side decorations and native text rendering
//! via Pango/FreeType. It is the thin platform shell that wraps the shared
//! Rust core crates.

// Why: GTK4 signal callbacks (button click handlers, keybinding callbacks,
// layout-event handlers) cannot capture `self`, so they receive every UI
// dependency (`wm`, `sidebar_lb`, `main_area`, `tab_bar`, `window`,
// `daemon_client`, `shared_config`) as individual parameters. The natural
// fix is a `UiContext` bundle struct, which is P-012 sidebar refactor
// scope — bundling it into P-005 would double scope and risk regressions
// in the daily-driver tab/workspace system.
#![allow(clippy::too_many_arguments)]

pub mod app;
pub mod clipboard;
pub mod code_block;
pub mod daemon_client;
pub mod input;
pub mod osc_notification;
pub mod preferences;
pub mod settings_view;
pub mod terminal;
