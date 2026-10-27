//! The command line of `forgetty-win.exe` (A13). Pure: the arguments in, the choices out. The
//! disk checks (does the folder exist, is the program found) are made by the caller.
//!
//! ```text
//! forgetty-win [--working-directory DIR] [--config-file FILE] [-e PROGRAM [ARGS...]]
//! ```
//!
//! `-e` takes the rest of the line. GTK's `--class`, `--no-restore`, `--session-id` and
//! `--restore-all` are Linux window and session concepts and are not accepted here.

/// What the command line asked for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// `--smoke`: the CI gate.
    pub smoke: bool,
    /// `--start-server`: the login Run value.
    pub start_server: bool,
    pub help: bool,
    pub version: bool,
    /// The folder the first tab starts in.
    pub working_directory: Option<String>,
    /// The program of the first tab (`-e`), with its arguments.
    pub command: Vec<String>,
    /// The configuration file to use instead of the default one.
    pub config_file: Option<String>,
}

impl Args {
    /// A launch that changes what the first tab is.
    pub fn has_launch(&self) -> bool {
        self.working_directory.is_some() || !self.command.is_empty()
    }
}

pub const USAGE: &str = "Usage: forgetty-win [OPTIONS] [-e PROGRAM [ARGS...]]\n\
\n\
  --working-directory DIR   start the first tab in DIR\n\
  -e PROGRAM [ARGS...]      run PROGRAM in the first tab (takes the rest of the line)\n\
  --config-file FILE        read FILE instead of the default config.toml\n\
  -h, --help                show this text\n\
  -V, --version             show the version\n";

/// `args` without the program name. An error is one line for the user.
pub fn parse(args: &[String]) -> Result<Args, String> {
    let mut out = Args::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        // `--name=value` and `--name value` are the same.
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (a, None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            let v = match inline.clone() {
                Some(v) => Some(v),
                None => {
                    let v = args.get(i).cloned();
                    if v.is_some() {
                        i += 1;
                    }
                    v
                }
            };
            match v.filter(|v| !v.is_empty()) {
                Some(v) if crate::osc::is_safe_local_path(&v) => Ok(v),
                Some(v) => Err(format!("{name}: {} is a network or device path", crate::notify::sanitize(&v, 60))),
                None => Err(format!("{name} needs {what}")),
            }
        };
        match name {
            "--smoke" => out.smoke = true,
            "--start-server" => out.start_server = true,
            "-h" | "--help" => out.help = true,
            "-V" | "--version" => out.version = true,
            "--working-directory" => out.working_directory = Some(value("a folder")?),
            "--config-file" => out.config_file = Some(value("a file")?),
            "-e" | "--command" if inline.is_none() => {
                if i >= args.len() {
                    return Err("-e needs a program".into());
                }
                out.command = args[i..].to_vec();
                if out.command[0].trim().is_empty() {
                    return Err("-e needs a program".into());
                }
                break;
            }
            other => {
                return Err(format!("unknown option {}\n\n{USAGE}", crate::notify::sanitize(other, 60)));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &[&str]) -> Result<Args, String> {
        parse(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn nothing_is_the_default() {
        assert_eq!(p(&[]).unwrap(), Args::default());
        assert!(!Args::default().has_launch());
    }

    #[test]
    fn the_modes() {
        assert!(p(&["--smoke"]).unwrap().smoke);
        assert!(p(&["--start-server"]).unwrap().start_server);
        assert!(p(&["-h"]).unwrap().help);
        assert!(p(&["--version"]).unwrap().version);
    }

    #[test]
    fn folder_and_file_in_both_forms() {
        let a = p(&["--working-directory", "C:\\src", "--config-file=D:\\c.toml"]).unwrap();
        assert_eq!(a.working_directory.as_deref(), Some("C:\\src"));
        assert_eq!(a.config_file.as_deref(), Some("D:\\c.toml"));
        assert!(a.has_launch());
        let a = p(&["--working-directory=C:\\a b"]).unwrap();
        assert_eq!(a.working_directory.as_deref(), Some("C:\\a b"));
    }

    #[test]
    fn e_takes_the_rest_of_the_line() {
        let a = p(&["--working-directory", "C:\\", "-e", "pwsh", "-NoLogo", "--working-directory", "x"]).unwrap();
        assert_eq!(a.command, vec!["pwsh", "-NoLogo", "--working-directory", "x"]);
        assert_eq!(a.working_directory.as_deref(), Some("C:\\"));
        // Options after -e belong to the program, so --smoke here is not the gate.
        assert!(!p(&["-e", "cmd", "--smoke"]).unwrap().smoke);
    }

    #[test]
    fn errors_are_one_clear_line() {
        assert_eq!(p(&["-e"]).unwrap_err(), "-e needs a program");
        assert_eq!(p(&["--working-directory"]).unwrap_err(), "--working-directory needs a folder");
        assert_eq!(p(&["--config-file="]).unwrap_err(), "--config-file needs a file");
        let e = p(&["--no-restore"]).unwrap_err();
        assert!(e.starts_with("unknown option --no-restore"), "{e}");
        assert!(e.contains("Usage:"));
        assert!(p(&["stray"]).is_err());
    }

    #[test]
    fn network_paths_are_refused() {
        let e = p(&["--config-file", "\\\\srv\\share\\c.toml"]).unwrap_err();
        assert!(e.contains("network or device path"), "{e}");
        assert!(p(&["--working-directory", "\\\\?\\C:\\x"]).is_err());
    }

    #[test]
    fn control_characters_do_not_reach_the_message() {
        let e = p(&["--x\u{1b}[31mred"]).unwrap_err();
        assert!(!e.contains('\u{1b}'), "{e:?}");
    }
}
