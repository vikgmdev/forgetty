//! GTK4/libadwaita platform shell for the Forgetty terminal emulator.
//!
//! This crate provides the native Linux UI layer using GTK4 and libadwaita,
//! delivering GNOME-style client-side decorations and native text rendering
//! via Pango/FreeType. It is the thin platform shell that wraps the shared
//! Rust core crates.

// Pre-existing lints surfaced by clippy 1.94.0 in code outside V2-003 scope.
// Proper cleanup tracked as a follow-up P-xxx task.
#![allow(
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::if_same_then_else,
    clippy::implicit_saturating_sub,
    clippy::iter_cloned_collect,
    clippy::manual_is_multiple_of,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::redundant_closure,
    clippy::single_match,
    clippy::too_many_arguments,
    clippy::unnecessary_map_or
)]

pub mod app;
pub mod clipboard;
pub mod code_block;
pub mod daemon_client;
pub mod input;
pub mod osc_notification;
pub mod preferences;
pub mod settings_view;
pub mod terminal;
