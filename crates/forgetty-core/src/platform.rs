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

fn config_dir_resolved() -> PathBuf {
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

fn data_dir_resolved() -> PathBuf {
    let base = if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data)
    } else {
        dirs::data_dir().unwrap_or_default()
    };
    base.join("forgetty")
}

fn runtime_dir_resolved() -> Option<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        Some(PathBuf::from(runtime).join("forgetty"))
    } else {
        dirs::runtime_dir().map(|d| d.join("forgetty"))
    }
}

// ---------------------------------------------------------------------------
// Test isolation
// ---------------------------------------------------------------------------
//
// A real server may run on the same machine under the default directories
// (`~/.local/share/forgetty`, `~/.config/forgetty`, `$XDG_RUNTIME_DIR/forgetty`).
// A test that opens them writes pane logs, locks or a socket into the real
// state. So a test process never resolves the real default directories: the
// call returns a directory under a per-process temporary root instead (or
// panics with FORGETTY_TEST_STRICT=1, to find the test that needs isolating).

/// Directories a test process uses instead of the real ones, once
/// [`isolate_state_for_tests`] ran.
static TEST_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Set once a test asked for isolation, so the redirect notice stays quiet.
static ISOLATED_ON_PURPOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `true` for a test or bench binary: `cargo` puts those in a `deps`
/// directory, and a harness may set `FORGETTY_TEST=1` for any other process.
/// The installed server and `cargo run` binaries are neither.
fn is_test_process() -> bool {
    static IS_TEST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *IS_TEST.get_or_init(|| {
        std::env::var_os("FORGETTY_TEST").is_some_and(|v| v == "1")
            || std::env::current_exe()
                .ok()
                .and_then(|exe| {
                    exe.parent().and_then(|p| p.file_name()).map(|n| n == "deps")
                })
                .unwrap_or(false)
    })
}

/// Give this test process its own state, config and runtime directories under
/// a temporary directory, and return the root. Safe to call again and from
/// any thread (the first call decides). It sets no environment variable.
/// Directories of test processes that ended are removed on the way.
pub fn isolate_state_for_tests() -> PathBuf {
    ISOLATED_ON_PURPOSE.store(true, std::sync::atomic::Ordering::Relaxed);
    test_root()
}

fn test_root() -> PathBuf {
    TEST_ROOT
        .get_or_init(|| {
            let tmp = std::env::temp_dir();
            remove_stale_test_roots(&tmp);
            let root = tmp.join(format!("forgetty-test-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&root);
            root
        })
        .clone()
}

/// [`isolate_state_for_tests`] in a test process, nothing elsewhere. Call it
/// from code that tests reach before they touch any directory.
pub fn isolate_if_test_process() {
    if is_test_process() {
        isolate_state_for_tests();
    }
}

/// Remove `forgetty-test-<pid>` directories whose process is gone.
fn remove_stale_test_roots(tmp: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix("forgetty-test-")) else {
            continue;
        };
        if pid.parse::<u32>().is_ok() && !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// The directory a real install uses when no `XDG_*` variable says otherwise.
fn real_default(kind: &str) -> Option<PathBuf> {
    match kind {
        "data" => dirs::home_dir().map(|h| h.join(".local/share/forgetty")),
        "config" => dirs::home_dir().map(|h| h.join(".config/forgetty")),
        _ => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let uid = std::fs::metadata("/proc/self").ok()?.uid();
                Some(PathBuf::from(format!("/run/user/{uid}/forgetty")))
            }
            #[cfg(not(unix))]
            {
                None
            }
        }
    }
}

/// The directory for `kind` in a test process. The real default never comes
/// back: it becomes a directory under the test root, and the process says so
/// once on stderr. With `FORGETTY_TEST_STRICT=1` it panics instead, which
/// finds the test that did not isolate itself.
fn test_checked(kind: &str, resolved: PathBuf) -> PathBuf {
    let is_real = real_default(kind).is_some_and(|real| real == resolved);
    if !is_real {
        return resolved; // an explicit XDG_* override: the test chose its directory
    }
    if TEST_ROOT.get().is_none()
        && std::env::var_os("FORGETTY_TEST_STRICT").is_some_and(|v| v == "1")
    {
        panic!(
            "a test used the real Forgetty {kind} directory {}: call \
             forgetty_core::platform::isolate_state_for_tests() first, or set XDG_*_HOME \
             to a temporary directory",
            resolved.display()
        );
    }
    let root = test_root();
    if !ISOLATED_ON_PURPOSE.load(std::sync::atomic::Ordering::Relaxed) {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            eprintln!(
                "forgetty: a test process asked for the real {kind} directory {}; using {} instead",
                resolved.display(),
                root.display()
            );
        });
    }
    let dir = root.join(kind).join("forgetty");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Returns the platform-specific configuration directory for Forgetty.
///
/// - Linux: `$XDG_CONFIG_HOME/forgetty` or `~/.config/forgetty`
/// - macOS: `~/Library/Application Support/forgetty`
/// - Windows: `%APPDATA%/forgetty`
///
/// A test process never gets the real one (see [`isolate_state_for_tests`]).
pub fn config_dir() -> PathBuf {
    let dir = config_dir_resolved();
    if is_test_process() { test_checked("config", dir) } else { dir }
}

/// Returns the platform-specific data directory for Forgetty.
///
/// - Linux: `$XDG_DATA_HOME/forgetty` or `~/.local/share/forgetty`
/// - macOS: `~/Library/Application Support/forgetty`
/// - Windows: `%LOCALAPPDATA%/forgetty`
///
/// A test process never gets the real one (see [`isolate_state_for_tests`]).
pub fn data_dir() -> PathBuf {
    let dir = data_dir_resolved();
    if is_test_process() { test_checked("data", dir) } else { dir }
}

/// Returns the platform-specific runtime directory for Forgetty, if available.
///
/// On Linux this is typically `$XDG_RUNTIME_DIR/forgetty`.
/// Returns `None` on platforms that don't have a runtime directory concept.
///
/// A test process never gets the real one (see [`isolate_state_for_tests`]).
pub fn runtime_dir() -> Option<PathBuf> {
    let dir = runtime_dir_resolved()?;
    Some(if is_test_process() { test_checked("run", dir) } else { dir })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_process_never_gets_the_real_default_directories() {
        // This binary sits in `target/*/deps`, so it counts as a test process.
        assert!(is_test_process());
        let real_data = real_default("data").unwrap();
        let real_config = real_default("config").unwrap();
        // With no XDG override the resolver would return the real directory.
        // `test_checked` must turn it into a directory under the test root.
        isolate_state_for_tests(); // also keeps the strict mode quiet
        let data = test_checked("data", real_data.clone());
        let config = test_checked("config", real_config.clone());
        assert_ne!(data, real_data);
        assert_ne!(config, real_config);
        assert!(data.starts_with(isolate_state_for_tests()));
        assert!(config.starts_with(isolate_state_for_tests()));
        // An explicit override is the caller's choice and stays as it is.
        let own = PathBuf::from("/tmp/forgetty-own-choice/forgetty");
        assert_eq!(test_checked("data", own.clone()), own);
    }
}
