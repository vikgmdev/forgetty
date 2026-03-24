//! Error types for the Forgetty terminal emulator.
//!
//! Provides a unified error type used across all Forgetty crates,
//! with variants for each subsystem.

use thiserror::Error;

/// The top-level error type for Forgetty.
#[derive(Error, Debug)]
pub enum ForgettyError {
    /// An error from the virtual terminal (VT) parser.
    #[error("VT error: {0}")]
    Vt(String),

    /// An error from the PTY subsystem.
    #[error("PTY error: {0}")]
    Pty(String),

    /// An error from the GPU renderer.
    #[error("Renderer error: {0}")]
    Renderer(String),

    /// An error from configuration loading or parsing.
    #[error("Config error: {0}")]
    Config(String),

    /// An I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A convenience Result type that uses [`ForgettyError`].
pub type Result<T> = std::result::Result<T, ForgettyError>;
