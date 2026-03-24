//! Core types, errors, and utilities for the Forgetty terminal emulator.
//!
//! This crate provides the foundational types shared across all Forgetty crates,
//! including coordinate types, color representations, error definitions, and
//! platform-specific utilities.

pub mod error;
pub mod event;
pub mod platform;
pub mod types;

pub use error::{ForgettyError, Result};
pub use event::TerminalEvent;
pub use types::{CellCoord, PaneId, Rgba, Size};
