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
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(data).join("forgetty");
    }
    if is_windows() {
        // The server keeps its state in the local (not roaming) folder, and in a `server`
        // subfolder: `%APPDATA%\forgetty` is the Windows app's folder (its identity key and
        // machine list), and the two must never share a file.
        return dirs::data_local_dir().unwrap_or_default().join("forgetty").join("server");
    }
    dirs::data_dir().unwrap_or_default().join("forgetty")
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
// (`~/.local/share/forgetty`, `~/.config/forgetty`, `$XDG_RUNTIME_DIR/forgetty`,
// `%APPDATA%`, `~/Library/...`). A test that opens them writes pane logs, locks
// or a socket into the real state. One rule covers every platform: in a test
// process a data, config or runtime directory is accepted only when it lies
// under `std::env::temp_dir()`. Anything else becomes a directory under a
// per-process test root (or panics with `FORGETTY_TEST_STRICT=1`, to find the
// test that needs isolating). An `XDG_*` variable that points outside the
// temporary directory does not count as a choice by the test.
//
// A test process is a binary that `cargo` puts in a `deps` directory, or a
// process that called [`isolate_state_for_tests`]. No environment variable
// turns the guard on, so an inherited variable cannot move a live server's
// files.

/// Directory a test process uses instead of any non-temporary one, once it
/// exists. Set by [`isolate_state_for_tests`] or on the first redirect.
static TEST_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Set once a test asked for isolation, so the redirect notice stays quiet.
static ISOLATED_ON_PURPOSE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `true` for a test or bench binary (`cargo` puts those in a `deps`
/// directory) and for any process that called [`isolate_state_for_tests`].
/// The installed server and `cargo run` binaries are neither.
fn is_test_process() -> bool {
    static IN_DEPS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *IN_DEPS.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().and_then(|p| p.file_name()).map(|n| n == "deps"))
            .unwrap_or(false)
    }) || ISOLATED_ON_PURPOSE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Give this process its own state, config and runtime directories under a
/// temporary directory, and return the root. A harness that is not a `cargo`
/// test binary calls this before it opens any state. From then on the guard
/// is on for this process. Safe to call again and from any thread. It sets no
/// environment variable. Roots of test processes that ended are removed.
pub fn isolate_state_for_tests() -> PathBuf {
    ISOLATED_ON_PURPOSE.store(true, std::sync::atomic::Ordering::Relaxed);
    test_root()
}

/// Name prefix of a test root: short, because a socket path must fit in the
/// 104 bytes of `sun_path` on macOS (`<root>/run/forgetty/forgetty.sock`).
const TEST_ROOT_PREFIX: &str = "ft-";

/// File that marks a directory as a test root. Only directories that hold it
/// are ever removed as stale. It holds the start time of the owning process
/// (field 22 of `/proc/<pid>/stat`), so a reused pid is told from the owner.
const TEST_ROOT_MARKER: &str = ".forgetty-test-root";

fn test_root() -> PathBuf {
    TEST_ROOT
        .get_or_init(|| {
            let tmp = trusted_tmp();
            remove_stale_test_roots(&tmp);
            let root = tmp.join(format!("{TEST_ROOT_PREFIX}{}", std::process::id()));
            let _ = std::fs::create_dir_all(&root);
            // An empty marker when the start time is unknown: it keeps the root
            // of a live pid and still lets a dead pid's root go.
            let started =
                process_start_time(std::process::id()).map(|t| t.to_string()).unwrap_or_default();
            let _ = std::fs::write(root.join(TEST_ROOT_MARKER), started);
            root
        })
        .clone()
}

/// Start time of process `pid` in clock ticks since boot (field 22 of
/// `/proc/<pid>/stat`). `None` when `/proc` has no such process.
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid ...": the fields follow the last ')', and field 22
    // is the 20th of them (the state is field 3).
    stat.rsplit(')').next()?.split_whitespace().nth(19)?.parse().ok()
}

/// Resolve symlinks in the longest existing ancestor of `p` and put the rest
/// back, so `/var` and `/private/var` agree on macOS.
fn resolve_path(p: &std::path::Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(real) = std::fs::canonicalize(cur) {
            return rest.iter().rev().fold(real, |acc, c| acc.join(c));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// The directories a live install uses, from the platform and from the
/// `XDG_*` variables, plus the home directory.
fn live_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = [
        dirs::home_dir(),
        dirs::data_dir(),
        dirs::config_dir(),
        dirs::runtime_dir(),
        dirs::data_local_dir(),
        dirs::cache_dir(),
    ]
    .into_iter()
    .flatten()
    .collect();
    for key in ["XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_RUNTIME_DIR"] {
        if let Some(d) = std::env::var_os(key).filter(|d| !d.is_empty()) {
            v.push(PathBuf::from(d));
        }
    }
    v
}

/// `temp` if it can be trusted as a place for throwaway state, else a fixed
/// one. A temporary directory that is `/`, the home directory or an ancestor
/// of any live directory would make the live state count as temporary, so it
/// is untrusted. The fixed place is `/tmp` on unix. Elsewhere there is none,
/// and `temp` stays.
fn trusted_tmp_for(temp: &std::path::Path, live: &[PathBuf]) -> PathBuf {
    let t = resolve_path(temp);
    let swallows_live = t.parent().is_none()
        || t.as_os_str().is_empty()
        || live.iter().any(|d| resolve_path(d).starts_with(&t));
    if swallows_live && cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        temp.to_path_buf()
    }
}

/// The temporary directory this process trusts for test state.
fn trusted_tmp() -> PathBuf {
    trusted_tmp_for(&std::env::temp_dir(), &live_dirs())
}

/// [`isolate_state_for_tests`] in a test process, nothing elsewhere. Call it
/// from code that tests reach before they touch any directory.
pub fn isolate_if_test_process() {
    if is_test_process() {
        isolate_state_for_tests();
    }
}

/// Remove `ft-<pid>` test roots whose owner is gone. A directory counts only
/// if it holds the test-root marker. With `/proc`, the owner is gone when the
/// pid is dead, or alive with another start time than the marker records (the
/// pid was reused). A marker without a start time keeps a live pid's root.
/// Without `/proc` only roots whose mtime is older than 24 hours go.
fn remove_stale_test_roots(tmp: &std::path::Path) {
    let has_proc = std::path::Path::new("/proc/self").exists();
    let Ok(entries) = std::fs::read_dir(tmp) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix(TEST_ROOT_PREFIX)) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else { continue };
        let Ok(marker) = std::fs::read_to_string(e.path().join(TEST_ROOT_MARKER)) else {
            continue;
        };
        let stale = if has_proc {
            match process_start_time(pid) {
                None => true,
                Some(now) => marker.trim().parse::<u64>().is_ok_and(|then| then != now),
            }
        } else {
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(24 * 3600))
        };
        if stale {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// `true` if `path` lies under the trusted temporary directory or under the
/// test root. A path with a `..` component never counts. Symlinks in the
/// existing part of both paths are resolved.
fn under_temp_dir(path: &std::path::Path) -> bool {
    use std::path::Component;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let tmp = trusted_tmp();
    if path.starts_with(&tmp) {
        return true;
    }
    let real = resolve_path(path);
    real.starts_with(resolve_path(&tmp))
        || TEST_ROOT.get().is_some_and(|root| real.starts_with(resolve_path(root)))
}

/// The directory for `kind` in a test process. A directory under the
/// temporary directory (or the test root) is the test's own choice and stays.
/// Any other becomes a directory under the test root, and the process says so
/// once on stderr. With `FORGETTY_TEST_STRICT=1`, and no
/// [`isolate_state_for_tests`] call yet, it panics instead.
fn test_checked(kind: &str, resolved: PathBuf) -> PathBuf {
    if under_temp_dir(&resolved) {
        return resolved;
    }
    if TEST_ROOT.get().is_none()
        && std::env::var_os("FORGETTY_TEST_STRICT").is_some_and(|v| v == "1")
    {
        panic!(
            "a test used the non-temporary Forgetty {kind} directory {}: call \
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
                "forgetty: a test process asked for the {kind} directory {}, which is not under {}; using {} instead",
                resolved.display(),
                trusted_tmp().display(),
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
/// - Windows: `%LOCALAPPDATA%\forgetty\server`. Not `%APPDATA%\forgetty`: that folder is the
///   Windows app's (the app and the server are two programs with their own identity keys).
///
/// `$XDG_DATA_HOME` wins on every platform (it is how `--state-dir` and the tests isolate).
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
    use std::sync::Mutex;

    /// Tests that change `XDG_*` variables run one at a time.
    static ENV: Mutex<()> = Mutex::new(());

    fn set_env(key: &str, value: Option<&std::ffi::OsStr>) -> Option<std::ffi::OsString> {
        let prior = std::env::var_os(key);
        // SAFETY: the ENV mutex serializes every test that touches these variables.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        prior
    }

    #[test]
    fn this_binary_counts_as_a_test_process_without_any_variable() {
        // It sits in `target/*/deps`.
        assert!(is_test_process());
    }

    #[test]
    fn a_test_with_the_platform_default_gets_redirected() {
        let root = isolate_state_for_tests();
        let default_data = dirs::data_dir().unwrap().join("forgetty");
        let default_config = dirs::config_dir().unwrap().join("forgetty");
        // A HOME under the temporary directory is its own choice; skip then.
        if !under_temp_dir(&default_data) && !under_temp_dir(&default_config) {
            let data = test_checked("data", default_data.clone());
            let config = test_checked("config", default_config.clone());
            assert_ne!(data, default_data);
            assert_ne!(config, default_config);
            assert!(data.starts_with(&root) && config.starts_with(&root));
        }
        // The same through the public resolver with no XDG override.
        let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prior = set_env("XDG_DATA_HOME", None);
        let got = data_dir();
        set_env("XDG_DATA_HOME", prior.as_deref());
        if !under_temp_dir(&default_data) {
            assert!(got.starts_with(&root), "{got:?} is not under {root:?}");
        }
    }

    #[test]
    fn a_non_temp_xdg_data_home_is_redirected() {
        let root = isolate_state_for_tests();
        let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let outside = std::ffi::OsString::from("/forgetty-guard-test-outside-tmp/data");
        let prior = set_env("XDG_DATA_HOME", Some(&outside));
        let got = data_dir();
        set_env("XDG_DATA_HOME", prior.as_deref());
        assert!(got.starts_with(&root), "{got:?} is not under {root:?}");
        assert!(!std::path::Path::new("/forgetty-guard-test-outside-tmp").exists());
    }

    #[test]
    fn a_temp_xdg_data_home_is_the_tests_own_choice() {
        isolate_state_for_tests();
        let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let own = trusted_tmp().join("forgetty-guard-own-choice");
        let prior = set_env("XDG_DATA_HOME", Some(own.as_os_str()));
        let got = data_dir();
        set_env("XDG_DATA_HOME", prior.as_deref());
        assert_eq!(got, own.join("forgetty"));
    }

    #[test]
    fn a_path_that_climbs_out_of_temp_is_not_temp() {
        let climb = trusted_tmp().join("x/../../forgetty-guard-test");
        assert!(!under_temp_dir(&climb));
    }

    #[test]
    fn a_temp_dir_that_holds_the_live_dirs_is_not_trusted() {
        let live = vec![
            PathBuf::from("/home/u"),
            PathBuf::from("/home/u/.local/share"),
            PathBuf::from("/run/user/1000"),
        ];
        let fixed = PathBuf::from("/tmp");
        if cfg!(unix) {
            // TMPDIR is $HOME, an ancestor of $HOME, a live dir, or /.
            for bad in ["/home/u", "/home", "/", "/home/u/.local/share", "/run/user/1000", "/run"] {
                assert_eq!(trusted_tmp_for(std::path::Path::new(bad), &live), fixed, "{bad}");
            }
        }
        // A directory next to the live ones is fine.
        let ok = PathBuf::from("/var/tmp/forgetty-guard-trusted-check");
        assert_eq!(trusted_tmp_for(&ok, &live), ok);
        // A temp dir inside HOME is fine too: it does not hold HOME.
        let inside = PathBuf::from("/home/u/tmp");
        assert_eq!(trusted_tmp_for(&inside, &live), inside);
    }

    #[test]
    fn the_test_root_keeps_a_socket_path_under_the_macos_limit() {
        let root = isolate_state_for_tests();
        let sock = root.join("run").join("forgetty").join("forgetty.sock");
        let len = sock.as_os_str().len();
        assert!(len < 100, "{} is {len} bytes; sun_path holds 103 on macOS", sock.display());
    }

    // Reads /proc, which Windows does not have.
    #[cfg(not(windows))]
    #[test]
    fn only_a_root_whose_owner_is_gone_is_removed_as_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let my_start = process_start_time(me).expect("this process has a start time");
        let root = |pid: u32| tmp.path().join(format!("{TEST_ROOT_PREFIX}{pid}"));
        let make = |pid: u32, marker: Option<String>| {
            std::fs::create_dir_all(root(pid)).unwrap();
            if let Some(m) = marker {
                std::fs::write(root(pid).join(TEST_ROOT_MARKER), m).unwrap();
            }
        };
        // pid 4294967 is not running.
        make(4294967, Some("123".into())); // dead owner: removed
        make(4294966, None); // no marker: never removed
        make(me, Some(my_start.to_string())); // the live owner: kept
        // Alive pid with another start time (a reused pid): removed. Use pid 1.
        make(1, Some((process_start_time(1).unwrap() + 1).to_string()));
        remove_stale_test_roots(tmp.path());
        assert!(!root(4294967).exists());
        assert!(root(4294966).exists(), "a directory without the marker is never removed");
        assert!(root(me).exists(), "the live owner keeps its root");
        assert!(!root(1).exists(), "a reused pid does not keep the root");
    }

    #[test]
    fn the_live_dirs_include_the_cache_dir() {
        if let Some(cache) = dirs::cache_dir() {
            assert!(live_dirs().contains(&cache));
        }
    }
}
