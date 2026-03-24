//! Platform detection and platform-specific directory resolution.
//!
//! Provides utilities for detecting the host operating system and
//! locating standard directories for configuration, data, and runtime files.

use std::path::PathBuf;

/// Returns `true` if the current platform is Linux.
pub fn is_linux() -> bool {
    cfg!(target_os = "linux")
}

/// Returns `true` if the current platform is macOS.
pub fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

/// Returns `true` if the current platform is Windows.
pub fn is_windows() -> bool {
    cfg!(target_os = "windows")
}

/// Returns the platform-specific configuration directory for Forgetty.
///
/// - Linux: `$XDG_CONFIG_HOME/forgetty` or `~/.config/forgetty`
/// - macOS: `~/Library/Application Support/forgetty`
/// - Windows: `%APPDATA%/forgetty`
pub fn config_dir() -> PathBuf {
    let base = if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config)
    } else if is_macos() {
        dirs::home_dir().unwrap_or_default().join("Library/Application Support")
    } else if is_windows() {
        dirs::config_dir().unwrap_or_default()
    } else {
        dirs::home_dir().unwrap_or_default().join(".config")
    };
    base.join("forgetty")
}

/// Returns the platform-specific data directory for Forgetty.
///
/// - Linux: `$XDG_DATA_HOME/forgetty` or `~/.local/share/forgetty`
/// - macOS: `~/Library/Application Support/forgetty`
/// - Windows: `%LOCALAPPDATA%/forgetty`
pub fn data_dir() -> PathBuf {
    let base = if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data)
    } else {
        dirs::data_dir().unwrap_or_default()
    };
    base.join("forgetty")
}

/// Returns the platform-specific runtime directory for Forgetty, if available.
///
/// On Linux this is typically `$XDG_RUNTIME_DIR/forgetty`.
/// Returns `None` on platforms that don't have a runtime directory concept.
pub fn runtime_dir() -> Option<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        Some(PathBuf::from(runtime).join("forgetty"))
    } else {
        dirs::runtime_dir().map(|d| d.join("forgetty"))
    }
}
