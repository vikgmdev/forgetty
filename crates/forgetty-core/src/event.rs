//! Terminal events emitted by the VT parser and PTY subsystem.
//!
//! These events are used to communicate state changes from the terminal
//! backend to the UI layer.

use std::path::PathBuf;

/// Events emitted by the terminal subsystem.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// The terminal bell was triggered.
    Bell,

    /// The terminal title changed (via OSC escape sequence).
    TitleChanged(String),

    /// The current working directory changed (via OSC 7).
    CwdChanged(PathBuf),

    /// A desktop notification was requested (via OSC 9 or OSC 777).
    Notification {
        /// Notification title.
        title: String,
        /// Notification body text.
        body: String,
    },

    /// The shell process exited with the given status code.
    Exit(i32),
}
