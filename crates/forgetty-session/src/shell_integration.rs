//! Shell integration scripts and how the server hands them to a shell (P7.1).
//!
//! The scripts make zsh, bash and fish write the standard prompt marks
//! (OSC 133 A, B, C, D with the exit code) and the working directory (OSC 7).
//! They are part of the server binary. [`ShellIntegration::install`] writes
//! them to a directory of the server (`<data dir>/shell-integration`), and
//! [`ShellIntegration::env_for`] gives the environment that makes a shell the
//! server starts load them. The user's rc files are never edited:
//!
//! - zsh: `ZDOTDIR` points at a directory with a `.zshenv` that gives the
//!   startup files back to the user's own `ZDOTDIR` and loads the script at the
//!   first prompt.
//! - bash: `PROMPT_COMMAND` starts the script at the first prompt. A startup
//!   file that replaces `PROMPT_COMMAND` instead of adding to it turns the
//!   integration off for that shell.
//! - fish: the server's data directory is put in front of `XDG_DATA_DIRS`, and
//!   fish loads the script from `vendor_conf.d`.

use std::io;
use std::path::{Path, PathBuf};

const ZSH_SCRIPT: &str = include_str!("../shell-integration/forgetty.zsh");
const ZSH_ENV: &str = include_str!("../shell-integration/zsh/.zshenv");
const BASH_SCRIPT: &str = include_str!("../shell-integration/forgetty.bash");
const FISH_SCRIPT: &str =
    include_str!("../shell-integration/fish-data/fish/vendor_conf.d/forgetty-integration.fish");

/// Files of the installed directory, relative to it.
const FILES: [(&str, &str); 4] = [
    ("forgetty.zsh", ZSH_SCRIPT),
    ("zsh/.zshenv", ZSH_ENV),
    ("forgetty.bash", BASH_SCRIPT),
    ("fish-data/fish/vendor_conf.d/forgetty-integration.fish", FISH_SCRIPT),
];

/// The shells the integration knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    /// The shell a program path is: `/usr/bin/zsh`, `-zsh` and `zsh` are zsh.
    /// Anything else (sh, dash, nu, a program that is no shell) is `None`.
    pub fn from_program(program: &str) -> Option<Shell> {
        let name = Path::new(program).file_name()?.to_str()?;
        match name.trim_start_matches('-') {
            "zsh" => Some(Shell::Zsh),
            "bash" => Some(Shell::Bash),
            "fish" => Some(Shell::Fish),
            _ => None,
        }
    }
}

/// The installed scripts.
#[derive(Debug, Clone)]
pub struct ShellIntegration {
    dir: PathBuf,
}

impl ShellIntegration {
    /// Write the scripts to `dir` (created, mode 0700; files 0644). A file that
    /// already has the same content is left alone, so a restart does not touch
    /// the disk and a running shell never sees a half written file (the others
    /// are written to a temporary name and renamed).
    pub fn install(dir: &Path) -> io::Result<Self> {
        forgetty_core::fsutil::ensure_private_dir(dir)?;
        for (rel, content) in FILES {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if std::fs::read_to_string(&path).is_ok_and(|have| have == content) {
                continue;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, content)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(Self { dir: dir.to_path_buf() })
    }

    /// The directory the scripts are in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The environment for a new pane that runs `program` as its shell, on top
    /// of the environment of the server. `ambient` reads a variable of the
    /// environment the shell would inherit. Empty when `program` is no known
    /// shell.
    pub fn env_for(
        &self,
        program: &str,
        ambient: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<(String, String)> {
        let Some(shell) = Shell::from_program(program) else { return Vec::new() };
        let dir = self.dir.to_string_lossy().into_owned();
        let mut env = vec![("FORGETTY_SHELL_INTEGRATION_DIR".to_string(), dir.clone())];
        match shell {
            Shell::Zsh => {
                if let Some(user) = ambient("ZDOTDIR") {
                    env.push(("FORGETTY_USER_ZDOTDIR".into(), user));
                }
                env.push(("ZDOTDIR".into(), format!("{dir}/zsh")));
            }
            Shell::Bash => {
                let boot = r#". "$FORGETTY_SHELL_INTEGRATION_DIR/forgetty.bash""#;
                let value = match ambient("PROMPT_COMMAND") {
                    Some(own) if !own.trim().is_empty() => format!("{boot};{own}"),
                    _ => boot.to_string(),
                };
                env.push(("PROMPT_COMMAND".into(), value));
            }
            Shell::Fish => {
                let data = format!("{dir}/fish-data");
                let value = match ambient("XDG_DATA_DIRS") {
                    Some(own) if !own.is_empty() => format!("{data}:{own}"),
                    // The XDG default, so the system's fish data stays found.
                    _ => format!("{data}:/usr/local/share:/usr/share"),
                };
                env.push(("XDG_DATA_DIRS".into(), value));
            }
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_are_known_by_program_name() {
        assert_eq!(Shell::from_program("/usr/bin/zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::from_program("-bash"), Some(Shell::Bash));
        assert_eq!(Shell::from_program("fish"), Some(Shell::Fish));
        assert_eq!(Shell::from_program("/bin/sh"), None);
        assert_eq!(Shell::from_program("/usr/bin/cat"), None);
        assert_eq!(Shell::from_program("zsh-wrapper"), None);
    }

    #[test]
    fn install_writes_the_scripts_and_keeps_them_on_a_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("shell-integration");
        let si = ShellIntegration::install(&dir).unwrap();
        for (rel, content) in FILES {
            assert_eq!(std::fs::read_to_string(dir.join(rel)).unwrap(), content, "{rel}");
        }
        let before = std::fs::metadata(dir.join("forgetty.zsh")).unwrap().modified().unwrap();
        ShellIntegration::install(&dir).unwrap();
        let after = std::fs::metadata(dir.join("forgetty.zsh")).unwrap().modified().unwrap();
        assert_eq!(before, after, "unchanged content is not rewritten");
        // A stale copy is replaced.
        std::fs::write(dir.join("forgetty.bash"), "old").unwrap();
        ShellIntegration::install(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("forgetty.bash")).unwrap(), BASH_SCRIPT);
        assert_eq!(si.dir(), dir);
    }

    #[test]
    fn env_for_each_shell_never_names_an_rc_file() {
        let si = ShellIntegration { dir: PathBuf::from("/d") };
        let none = |_: &str| None;
        let get = |env: &[(String, String)], k: &str| {
            env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        let z = si.env_for("/usr/bin/zsh", &none);
        assert_eq!(get(&z, "ZDOTDIR").as_deref(), Some("/d/zsh"));
        assert_eq!(get(&z, "FORGETTY_USER_ZDOTDIR"), None);
        let z = si.env_for("zsh", &|k| (k == "ZDOTDIR").then(|| "/home/u/.zsh".to_string()));
        assert_eq!(get(&z, "FORGETTY_USER_ZDOTDIR").as_deref(), Some("/home/u/.zsh"));
        let b = si.env_for("bash", &|k| (k == "PROMPT_COMMAND").then(|| "echo hi".to_string()));
        assert!(get(&b, "PROMPT_COMMAND").unwrap().ends_with(";echo hi"));
        let f = si.env_for("fish", &|k| (k == "XDG_DATA_DIRS").then(|| "/x".to_string()));
        assert_eq!(get(&f, "XDG_DATA_DIRS").as_deref(), Some("/d/fish-data:/x"));
        assert!(si.env_for("/bin/sh", &none).is_empty());
    }
}
