//! Configuration loading and management for Forgetty.
//!
//! Handles reading, parsing, and validating the TOML configuration file,
//! provides default values, and manages the theme system.

pub mod defaults;
pub mod loader;
pub mod schema;
pub mod theme;

pub use loader::load_config;
pub use schema::{Config, CursorStyle};
pub use theme::Theme;
