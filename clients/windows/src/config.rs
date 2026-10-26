//! The config is `forgetty_shell_core::config` (pure, tested on Linux). This adds the file
//! location and the first read at start.

use std::path::PathBuf;

pub use forgetty_shell_core::config::*;

/// `FORGETTY_CONFIG`, else `%APPDATA%\forgetty\config.toml`.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FORGETTY_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("APPDATA").map(PathBuf::from)?;
    Some(base.join("forgetty").join("config.toml"))
}

/// What the first read found.
pub struct Startup {
    pub config: Config,
    /// Unknown keys and the like. The file was used.
    pub warnings: Vec<String>,
    /// The file did not load: the defaults are in `config`.
    pub error: Option<String>,
}

pub fn load() -> Startup {
    let Some(path) = config_path() else {
        return Startup { config: Config::default(), warnings: Vec::new(), error: None };
    };
    match forgetty_shell_core::confwatch::read(&path) {
        forgetty_shell_core::confwatch::Reload::Loaded(l) => {
            Startup { config: l.config, warnings: l.warnings, error: None }
        }
        // No file at start is the defaults.
        forgetty_shell_core::confwatch::Reload::Missing => {
            Startup { config: Config::default(), warnings: Vec::new(), error: None }
        }
        forgetty_shell_core::confwatch::Reload::Failed(e) => {
            Startup { config: Config::default(), warnings: Vec::new(), error: Some(e) }
        }
    }
}

const TEMPLATE: &str = "# Forgetty configuration. Changes apply when you save this file.\n\
# Keys: https://forgetty.dev/docs/config\n\
\n\
# font_family = \"Cascadia Mono\"\n\
# font_size = 11.0\n\
# theme = \"Dracula\"\n\
# shell = \"pwsh.exe -NoLogo\"\n\
# bell_mode = \"visual\"          # visual | audio | both | none\n\
# notification_mode = \"all\"     # all | ringonly | none\n\
\n\
# default_profile = \"PowerShell\"\n\
# [[profiles]]\n\
# name = \"PowerShell\"\n\
# command = \"pwsh.exe -NoLogo\"\n\
# directory = \"~\"\n";

/// Open the config file in the editor Windows has for it, or Notepad. The file is created first
/// (with a commented template) when it is missing. Only a local `.toml` path is touched
/// (`FORGETTY_CONFIG` can name anything); see [`open_refusal`]. It reads and writes the disk:
/// call it from a worker thread.
pub fn open_in_editor(path: &std::path::Path) -> Result<(), String> {
    if let Some(why) = open_refusal(path) {
        return Err(why);
    }
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, TEMPLATE).map_err(|e| e.to_string())?;
    }
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe { ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    if h.0 as isize > 32 {
        return Ok(());
    }
    // No program is set for .toml: Notepad is always there.
    std::process::Command::new("notepad.exe").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}
