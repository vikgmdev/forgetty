//! Where the local control pipe is, on Windows.
//!
//! The Unix server keeps a socket in a private directory. Windows has no such file: the control
//! socket is a named pipe, and the protection is in the pipe itself (see
//! `forgetty_socket::pipe`): a DACL with the current user only, remote clients rejected, the
//! first instance claimed by the server, and a check of the peer's SID on both ends.
//!
//! The name is `\\.\pipe\forgetty-<user SID>-<secret>[-elevated]`:
//!
//! - `<secret>` is 128 random bits (hex) kept in `pipe.secret` in the server's data folder, a
//!   file only this user can read. Pipe names are global and a name that can be guessed can be
//!   taken first by another user, which would block the server. Nobody else can know this one.
//!   The server creates the file when it is missing and the CLI reads it. A server started with
//!   `--state-dir` has another data folder and so another secret and another pipe.
//! - `-elevated` is added in a process that runs elevated (high integrity). An elevated server
//!   must not be reachable from a normal process of the same user, and a normal CLI must not
//!   find it: the two name spaces are apart. The elevated pipe also carries a mandatory label,
//!   and the server refuses a client of a lower level.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use forgetty_core::fsutil;

/// The folder the server keeps its runtime files in (the same one `platform::runtime_dir` names,
/// else the data folder: Windows has no runtime directory).
pub fn socket_dir() -> PathBuf {
    forgetty_core::platform::runtime_dir().unwrap_or_else(forgetty_core::platform::data_dir)
}

/// Where the pipe secret is kept.
fn secret_path() -> PathBuf {
    forgetty_core::platform::data_dir().join("pipe.secret")
}

/// 128 random bits as 32 hex digits.
fn new_secret() -> String {
    rand::random::<[u8; 16]>().iter().map(|b| format!("{b:02x}")).collect()
}

fn valid_secret(s: &str) -> bool {
    s.len() >= 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read the secret at `path`, or make it. Two processes that both find the file missing cannot
/// both create it (`create_new`): the loser reads the winner's.
fn read_or_create_secret(path: &Path) -> io::Result<String> {
    for _ in 0..5 {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let secret = text.trim().to_string();
                return if valid_secret(&secret) {
                    Ok(secret)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} does not hold a valid secret; delete it and start the server again", path.display()),
                    ))
                };
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io::Error::new(e.kind(), format!("cannot read {}: {e}", path.display()))),
        }
        if let Some(dir) = path.parent() {
            fsutil::ensure_private_dir(dir)?;
        }
        match fsutil::create_private_new(path) {
            Ok(mut f) => {
                // A reader that sees the empty file before the write lands finds an invalid
                // secret and retries below (the loop reads again).
                f.write_all(new_secret().as_bytes())?;
                f.sync_all()?;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // Let the creator finish writing.
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                return Err(io::Error::new(e.kind(), format!("cannot create {}: {e}", path.display())))
            }
        }
    }
    Err(io::Error::other(format!("cannot read the pipe secret in {}", path.display())))
}

/// The pipe name for `sid` and `secret`; `elevated` adds the suffix.
pub fn pipe_path(sid: &str, secret: &str, elevated: bool) -> PathBuf {
    let suffix = if elevated { "-elevated" } else { "" };
    PathBuf::from(format!(r"\\.\pipe\forgetty-{sid}-{secret}{suffix}"))
}

/// The control pipe of the server of this user, in this data folder, for a process of this
/// elevation. The server and the CLI both call this. It creates the secret file when it is not
/// there, which is harmless for a CLI that finds no server (the connect then fails with "not
/// found"), and an error when the file cannot be read or made.
pub fn default_socket_path() -> io::Result<PathBuf> {
    let sid = forgetty_core::fsutil::acl::current_user_sid()?;
    let secret = read_or_create_secret(&secret_path())?;
    Ok(pipe_path(&sid, &secret, forgetty_core::fsutil::acl::is_elevated()))
}

/// Make sure the runtime folder exists and only this user can use it.
pub fn prepare_socket_dir(dir: &Path) -> io::Result<()> {
    forgetty_core::fsutil::ensure_private_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_holds_the_sid_the_secret_and_the_elevation() {
        let secret = "0123456789abcdef0123456789abcdef";
        let n = pipe_path("S-1-5-21-1-2-3-1001", secret, false);
        assert_eq!(n, PathBuf::from(format!(r"\\.\pipe\forgetty-S-1-5-21-1-2-3-1001-{secret}")));
        let e = pipe_path("S-1-5-21-1-2-3-1001", secret, true);
        assert_eq!(e, PathBuf::from(format!(r"\\.\pipe\forgetty-S-1-5-21-1-2-3-1001-{secret}-elevated")));
        assert_ne!(n, e);
        assert_ne!(n, pipe_path("S-1-5-21-1-2-3-1001", "ffffffffffffffffffffffffffffffff", false));
    }

    #[test]
    fn the_secret_is_made_once_and_read_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("pipe.secret");
        let a = read_or_create_secret(&path).unwrap();
        assert_eq!(a.len(), 32);
        assert!(valid_secret(&a));
        assert_eq!(read_or_create_secret(&path).unwrap(), a);
        // The file is for this user only.
        assert!(fsutil::is_private(&path).unwrap());
        // Two secrets differ.
        assert_ne!(new_secret(), new_secret());
    }

    #[test]
    fn a_damaged_secret_file_is_an_error_not_a_new_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe.secret");
        std::fs::write(&path, "short").unwrap();
        let err = read_or_create_secret(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short");
    }

    #[test]
    fn racing_creators_agree_on_one_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe.secret");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let p = path.clone();
                std::thread::spawn(move || read_or_create_secret(&p).unwrap())
            })
            .collect();
        let got: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(got.iter().all(|s| *s == got[0]), "{got:?}");
    }
}
