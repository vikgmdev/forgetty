//! Where a program or a start directory is, decided before anything runs.
//!
//! Two rules from the security review:
//! - A bare program name (`pwsh.exe`) is looked up in the PATH folders only. It is never run
//!   from the app's own working directory, which is where `CreateProcessW` would look next.
//! - A network path (`\\server\share`, `//server`, `\\?\...`) is refused before any stat or
//!   spawn. Touching one sends the user's credentials to the named server.
//!
//! The probes are in [`Host`], so the rules are tested on Linux with a fake disk.

use std::path::PathBuf;

/// What the resolver may ask of the machine.
pub struct Host {
    pub is_file: Box<dyn Fn(&str) -> bool + Send + Sync>,
    pub is_dir: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// The PATH folders, in order.
    pub path_dirs: Vec<String>,
    /// PATHEXT, with the dots (`.EXE`).
    pub pathext: Vec<String>,
    /// The home directory, for `~`.
    pub home: Option<String>,
    /// Where the custom themes are. `None`: the default config folder's `themes`.
    pub themes_dir: Option<PathBuf>,
}

impl Host {
    /// This machine. `themes_dir` is the folder next to the config file.
    pub fn real(themes_dir: Option<PathBuf>) -> Self {
        // A PATH entry such as `%USERPROFILE%\bin` (a REG_EXPAND_SZ value copied as it is) is
        // expanded first, then judged by the same rules as any other entry.
        let path_dirs = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .map(|d| expand_env(&d.to_string_lossy(), &|name| std::env::var(name).ok()))
                    .collect()
            })
            .unwrap_or_default();
        let pathext = std::env::var("PATHEXT")
            .ok()
            .map(|v| v.split(';').filter(|e| e.starts_with('.')).map(str::to_string).collect::<Vec<_>>())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()]);
        let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok().filter(|h| !h.is_empty());
        Host {
            // The resolver checks `is_safe_local_path` first, so these never see a network path.
            is_file: Box::new(|p| std::path::Path::new(p).is_file()),
            is_dir: Box::new(|p| std::path::Path::new(p).is_dir()),
            path_dirs,
            pathext,
            home,
            themes_dir,
        }
    }

    /// A machine with the given files, for tests. PATH is `C:\tools` and `C:\Windows\System32`.
    pub fn fake(files: &[&str], dirs: &[&str]) -> Self {
        let files: Vec<String> = files.iter().map(|s| s.to_ascii_lowercase()).collect();
        let dirs: Vec<String> = dirs.iter().map(|s| s.to_ascii_lowercase()).collect();
        Host {
            is_file: Box::new(move |p| files.contains(&p.to_ascii_lowercase())),
            is_dir: Box::new(move |p| dirs.contains(&p.to_ascii_lowercase())),
            path_dirs: vec!["C:\\tools".into(), "C:\\Windows\\System32".into()],
            pathext: vec![".EXE".into(), ".CMD".into()],
            home: Some("C:\\Users\\v".into()),
            themes_dir: None,
        }
    }
}

/// `%NAME%` replaced by the variable's value, as `ExpandEnvironmentStringsW` does. A name that is
/// not set, or a lone `%`, stays as written. One pass: a value is not expanded again.
pub fn expand_env(s: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 && !after[..end].contains(['\\', '/', ' ']) => match lookup(&after[..end]) {
                Some(v) => {
                    out.push_str(&v);
                    rest = &after[end + 1..];
                }
                None => {
                    out.push('%');
                    rest = after;
                }
            },
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

/// `C:\x` or `C:/x`: a drive and a separator. `\x`, `C:x` and `x\y` depend on the current
/// directory or drive, so they are not absolute here.
pub fn is_absolute(p: &str) -> bool {
    let mut it = p.chars();
    matches!((it.next(), it.next(), it.next()), (Some(d), Some(':'), Some(s)) if d.is_ascii_alphabetic() && is_sep(s))
}

fn candidates(name: &str, pathext: &[String]) -> Vec<String> {
    let lower = name.to_ascii_lowercase();
    let mut v = Vec::new();
    if pathext.iter().any(|e| lower.ends_with(&e.to_ascii_lowercase())) {
        v.push(name.to_string());
    }
    v.extend(pathext.iter().map(|e| format!("{name}{e}")));
    v
}

/// The full path of the program `argv0` names.
///
/// - A bare name is searched in the PATH folders (an empty, relative or network entry of PATH
///   is skipped), with each PATHEXT ending.
/// - A path must be a full path (`C:\...`) of a file that exists.
/// - Anything else is an error that says what to do.
pub fn resolve_program(argv0: &str, host: &Host) -> Result<String, String> {
    let name = argv0.trim();
    if name.is_empty() {
        return Err("the command is empty".into());
    }
    if !crate::osc::is_safe_local_path(name) {
        return Err(format!("`{}`: network and device paths are not allowed", crate::notify::notice_text(name)));
    }
    if name.contains(is_sep) || name.contains(':') {
        if !is_absolute(name) {
            return Err(format!(
                "`{}` is not a full path (write C:\\...\\program.exe, or only the program name to search PATH)",
                crate::notify::notice_text(name)
            ));
        }
        return candidates(name, &host.pathext)
            .into_iter()
            .chain(std::iter::once(name.to_string()))
            .find(|c| (host.is_file)(c))
            .ok_or_else(|| format!("`{}` was not found", crate::notify::notice_text(name)));
    }
    for dir in &host.path_dirs {
        let dir = dir.trim();
        if !is_absolute(dir) || !crate::osc::is_safe_local_path(dir) {
            continue;
        }
        let dir = dir.trim_end_matches(is_sep);
        for c in candidates(name, &host.pathext) {
            let full = format!("{dir}\\{c}");
            if (host.is_file)(&full) {
                return Ok(full);
            }
        }
    }
    Err(format!("`{}` is not on PATH", crate::notify::notice_text(name)))
}

/// A start directory: `~` expanded, a network path refused (before any stat), and the folder
/// must exist. `Ok(None)` for an empty text.
pub fn resolve_dir(text: &str, host: &Host) -> Result<Option<String>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let shown = crate::notify::notice_text(t);
    let Some(path) = crate::config::expand_dir(t, host.home.as_deref()) else {
        return Err(format!("`{shown}` needs a home directory, and there is none"));
    };
    if !crate::osc::is_safe_local_path(&path) {
        return Err(format!("`{shown}`: network and device paths are not allowed"));
    }
    // `\x` (the current drive) and `C:x` (the current folder of a drive) depend on where the app
    // was started.
    if !is_absolute(&path) {
        return Err(format!("`{shown}` is not a full path (write C:\\...)"));
    }
    if !(host.is_dir)(&path) {
        return Err(format!("`{shown}` does not exist"));
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host::fake(
            &["C:\\tools\\pwsh.exe", "C:\\Windows\\System32\\cmd.exe", "C:\\Program Files\\App\\app.exe", "C:\\tools\\run.cmd"],
            &["C:\\Users\\v", "C:\\Users\\v\\src", "D:\\work"],
        )
    }

    fn same(a: String, b: &str) {
        assert_eq!(a.to_ascii_lowercase(), b.to_ascii_lowercase());
    }

    #[test]
    fn a_bare_name_is_found_on_path_with_the_pathext_ending() {
        let h = host();
        same(resolve_program("pwsh.exe", &h).unwrap(), "C:\\tools\\pwsh.exe");
        same(resolve_program("pwsh", &h).unwrap(), "C:\\tools\\pwsh.exe");
        same(resolve_program("cmd", &h).unwrap(), "C:\\Windows\\System32\\cmd.exe");
        same(resolve_program("run", &h).unwrap(), "C:\\tools\\run.cmd");
    }

    #[test]
    fn a_name_that_is_not_on_path_is_refused_not_run_from_the_working_directory() {
        let h = host();
        let e = resolve_program("evil.exe", &h).unwrap_err();
        assert!(e.contains("is not on PATH"), "{e}");
        // The working directory is never searched, whatever the disk says.
        let h2 = Host { is_file: Box::new(|p| p == "evil.exe" || p == ".\\evil.exe"), ..host() };
        assert!(resolve_program("evil.exe", &h2).is_err());
    }

    #[test]
    fn path_entries_that_are_relative_empty_or_network_are_skipped() {
        let mut h = Host::fake(&["\\\\srv\\share\\x.exe", ".\\x.exe", "x.exe", "C:\\ok\\x.exe"], &[]);
        h.path_dirs = vec!["".into(), ".".into(), "\\\\srv\\share".into(), "bin".into(), "C:\\ok\\".into()];
        same(resolve_program("x.exe", &h).unwrap(), "C:\\ok\\x.exe");
        h.path_dirs.pop();
        assert!(resolve_program("x.exe", &h).is_err());
    }

    #[test]
    fn a_full_path_must_exist_and_a_relative_one_is_refused() {
        let h = host();
        assert_eq!(resolve_program("C:\\Program Files\\App\\app.exe", &h).unwrap(), "C:\\Program Files\\App\\app.exe");
        same(resolve_program("C:\\Program Files\\App\\app", &h).unwrap(), "C:\\Program Files\\App\\app.exe");
        assert!(resolve_program("C:\\nope\\x.exe", &h).unwrap_err().contains("was not found"));
        for rel in [".\\x.exe", "bin\\x.exe", "\\x.exe", "C:x.exe", "../x.exe"] {
            assert!(resolve_program(rel, &h).unwrap_err().contains("not a full path"), "{rel}");
        }
    }

    #[test]
    fn a_network_path_is_refused_before_the_disk_is_asked() {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let a = asked.clone();
        let a2 = asked.clone();
        let h = Host {
            is_file: Box::new(move |_| {
                a.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }),
            is_dir: Box::new(move |_| {
                a2.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }),
            ..host()
        };
        for p in [
            "\\\\srv\\share\\x.exe",
            "//srv/share/x.exe",
            "\\\\?\\C:\\x.exe",
            "\\\\.\\pipe\\x",
            "\\??\\UNC\\srv\\share\\x.exe",
            "/??/UNC/srv/share/x.exe",
            "\\??/UNC\\srv\\share",
        ] {
            assert!(resolve_program(p, &h).unwrap_err().contains("network"), "{p}");
            assert!(resolve_dir(p, &h).unwrap_err().contains("network"), "{p}");
        }
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn a_start_directory_is_expanded_and_must_exist() {
        let h = host();
        assert_eq!(resolve_dir("~", &h).unwrap().as_deref(), Some("C:\\Users\\v"));
        assert_eq!(resolve_dir("~\\src", &h).unwrap().as_deref(), Some("C:\\Users\\v\\src"));
        assert_eq!(resolve_dir("D:\\work", &h).unwrap().as_deref(), Some("D:\\work"));
        assert_eq!(resolve_dir("  ", &h).unwrap(), None);
        assert!(resolve_dir("~\\gone", &h).unwrap_err().contains("does not exist"));
        let mut no_home = host();
        no_home.home = None;
        assert!(resolve_dir("~", &no_home).unwrap_err().contains("home directory"));
        // A home that is itself a network path is refused too.
        let mut unc_home = host();
        unc_home.home = Some("\\\\srv\\home".into());
        assert!(resolve_dir("~", &unc_home).unwrap_err().contains("network"));
    }

    #[test]
    fn a_directory_that_depends_on_the_working_drive_is_refused_without_asking_the_disk() {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let a = asked.clone();
        let h = Host {
            is_dir: Box::new(move |_| {
                a.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }),
            ..host()
        };
        for p in ["\\x", "/x", "C:x", "src", ".\\src", "..\\src", "~x"] {
            let e = resolve_dir(p, &h).unwrap_err();
            assert!(e.contains("not a full path"), "{p}: {e}");
        }
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(resolve_dir("D:\\work", &h).unwrap().as_deref(), Some("D:\\work"));
    }

    #[test]
    fn percent_variables_in_a_path_entry_are_expanded_then_judged() {
        let env = |n: &str| match n.to_ascii_uppercase().as_str() {
            "USERPROFILE" => Some("C:\\Users\\v".to_string()),
            "SHARE" => Some("\\\\srv\\share".to_string()),
            "REL" => Some("bin".to_string()),
            _ => None,
        };
        assert_eq!(expand_env("%USERPROFILE%\\bin", &env), "C:\\Users\\v\\bin");
        assert_eq!(expand_env("%userprofile%\\%REL%", &env), "C:\\Users\\v\\bin");
        assert_eq!(expand_env("%NOPE%\\x", &env), "%NOPE%\\x");
        assert_eq!(expand_env("100%", &env), "100%");
        assert_eq!(expand_env("a%b c%d", &env), "a%b c%d");
        assert_eq!(expand_env("%%", &env), "%%");
        // The rules apply to what it expands to.
        let mut h = Host::fake(&["C:\\Users\\v\\bin\\tool.exe", "\\\\srv\\share\\tool.exe", "bin\\tool.exe"], &[]);
        h.path_dirs = ["%SHARE%", "%REL%", "%USERPROFILE%\\bin", "%NOPE%"].iter().map(|d| expand_env(d, &env)).collect();
        same(resolve_program("tool.exe", &h).unwrap(), "C:\\Users\\v\\bin\\tool.exe");
        h.path_dirs.truncate(2);
        assert!(resolve_program("tool.exe", &h).is_err(), "a network or relative entry is skipped after expansion");
    }

    #[test]
    fn is_absolute_wants_a_drive_and_a_separator() {
        assert!(is_absolute("C:\\x") && is_absolute("d:/x"));
        assert!(!is_absolute("C:x") && !is_absolute("\\x") && !is_absolute("x\\y") && !is_absolute("C:") && !is_absolute(""));
    }
}
