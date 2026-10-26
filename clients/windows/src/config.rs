//! The config is `forgetty_shell_core::config` (pure, tested on Linux). This adds the file
//! location and the first read at start.

use std::path::PathBuf;

pub use forgetty_shell_core::config::*;

fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from)?;
    Some(base.join("forgetty").join("config.toml"))
}

/// `FORGETTY_CONFIG` when it is a local path, else `%APPDATA%\forgetty\config.toml`. A network
/// path in the variable is not used (see [`env_refusal`]).
pub fn config_path() -> Option<PathBuf> {
    pick_config_path(std::env::var("FORGETTY_CONFIG").ok(), default_path()).0
}

/// Why `FORGETTY_CONFIG` is not used, when it is set to a network or device path.
pub fn env_refusal() -> Option<String> {
    pick_config_path(std::env::var("FORGETTY_CONFIG").ok(), default_path()).1
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
    let mut s = load_file();
    // A network FORGETTY_CONFIG was not used: say so first.
    if let Some(why) = env_refusal() {
        s.warnings.insert(0, why);
    }
    s
}

fn load_file() -> Startup {
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
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    // ShellExecuteW wants COM on the calling thread, and this runs on a background thread that has
    // none. A thread that already has COM in another mode (RPC_E_CHANGED_MODE) is left alone.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok();
    let h = unsafe { ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    if com {
        unsafe { CoUninitialize() };
    }
    if h.0 as isize > 32 {
        return Ok(());
    }
    // No program is set for .toml: Notepad is always there.
    std::process::Command::new("notepad.exe").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}
