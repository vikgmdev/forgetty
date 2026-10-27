//! Where the local control pipe is, on Windows.
//!
//! The Unix server keeps a socket in a private directory. Windows has no such file: the control
//! socket is a named pipe, and the protection is in the pipe itself (see
//! `forgetty_socket::pipe`): a DACL and owner with the current user only, remote clients
//! rejected, the first instance claimed by the server, and a check of the peer on both ends.
//!
//! The name is `\\.\pipe\forgetty-<user SID>-<secret>`. `<secret>` is 128 random bits (hex) in
//! `pipe.secret` in the server's data folder, a file only this user can read.
//!
//! What the secret is for, and what it is not. Pipe names are global and any user can list
//! `\\.\pipe\`, so the name of a running server is not a secret from other users. The secret
//! stops one thing: another user preparing a squat. Without it the name `forgetty-<SID>` could
//! be created by anyone before the server starts. The server makes a new secret at every start,
//! under the state lock and before it binds, so a name learned (or a pipe made) before the
//! start is of no use. The real protection stays in the DACL, the first-instance flag and the
//! SID checks on both ends.
//!
//! A new secret at every start means the pipe name changes at every start: a client reads the
//! file each time it connects and must not cache the name.
//!
//! The server does not run elevated (it refuses to start, exit 78), so there is one name space
//! and no `-elevated` name.

use std::io;
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

/// Read the secret at `path`. `Ok(None)` when there is no file. A file that is empty, short or
/// not hex is read again a few times (something else may be replacing it) and is then an error;
/// it is never deleted or rewritten here, because that would turn a read into a write.
fn read_secret_at(path: &Path) -> io::Result<Option<String>> {
    let mut last = String::new();
    for attempt in 0..10 {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let secret = text.trim().to_string();
                if valid_secret(&secret) {
                    return Ok(Some(secret));
                }
                last = secret;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            // 5, 32, 33: access denied, sharing or lock violation. A replace is under way.
            Err(e) if matches!(e.raw_os_error(), Some(5 | 32 | 33)) => {
                last = String::new();
            }
            Err(e) => {
                return Err(io::Error::new(e.kind(), format!("cannot read {}: {e}", path.display())))
            }
        }
        if attempt < 9 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{} does not hold a valid secret ({} characters); a server start makes a new one",
            path.display(),
            last.len()
        ),
    ))
}

/// Make a new secret and put it in place at `path` in one step: write a temporary file next to
/// it (user-only from its first moment, flushed to disk), then rename it over the old file. A
/// reader sees the old secret or the new one, never an empty or half written file.
fn write_new_secret_at(path: &Path) -> io::Result<String> {
    let dir = path.parent().ok_or_else(|| io::Error::other("the secret file has no folder"))?;
    fsutil::ensure_private_dir(dir)?;
    let secret = new_secret();
    fsutil::write_private_atomic(path, secret.as_bytes())?;
    Ok(secret)
}

/// The pipe name for `sid` and `secret`.
pub fn pipe_path(sid: &str, secret: &str) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\forgetty-{sid}-{secret}"))
}

/// The pipe name with the secret cut out, for logs and error messages.
pub fn redact_pipe_name(name: &str) -> String {
    forgetty_socket::pipe::redact_pipe_name(name)
}

/// The control pipe of the running server, for the CLI and other clients: the SID of this user
/// and the secret in the data folder. `NotFound` when there is no secret file, that is when no
/// server has started here. Reads only; never makes the file.
pub fn default_socket_path() -> io::Result<PathBuf> {
    let sid = forgetty_core::fsutil::acl::current_user_sid()?;
    match read_secret_at(&secret_path())? {
        Some(secret) => Ok(pipe_path(&sid, &secret)),
        None => Err(io::Error::new(io::ErrorKind::NotFound, "no server has started here (no pipe.secret)")),
    }
}

/// The server's pipe name, with a new secret. Call it once per start, under the state lock and
/// before the bind.
pub fn new_server_socket_path() -> io::Result<PathBuf> {
    let sid = forgetty_core::fsutil::acl::current_user_sid()?;
    let secret = write_new_secret_at(&secret_path())?;
    Ok(pipe_path(&sid, &secret))
}

/// Make sure the runtime folder exists and only this user can use it.
pub fn prepare_socket_dir(dir: &Path) -> io::Result<()> {
    forgetty_core::fsutil::ensure_private_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_holds_the_sid_and_the_secret() {
        let secret = "0123456789abcdef0123456789abcdef";
        let n = pipe_path("S-1-5-21-1-2-3-1001", secret);
        assert_eq!(n, PathBuf::from(format!(r"\\.\pipe\forgetty-S-1-5-21-1-2-3-1001-{secret}")));
        assert_ne!(n, pipe_path("S-1-5-21-1-2-3-1001", "ffffffffffffffffffffffffffffffff"));
    }

    #[test]
    fn a_secret_is_new_each_time_and_the_old_file_is_replaced_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("pipe.secret");
        assert_eq!(read_secret_at(&path).unwrap(), None);
        let a = write_new_secret_at(&path).unwrap();
        assert_eq!(a.len(), 32);
        assert_eq!(read_secret_at(&path).unwrap().as_deref(), Some(a.as_str()));
        let b = write_new_secret_at(&path).unwrap();
        assert_ne!(a, b);
        assert_eq!(read_secret_at(&path).unwrap().as_deref(), Some(b.as_str()));
        // The file is for this user only, and no temporary file stays behind.
        assert!(fsutil::is_private(&path).unwrap());
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn a_damaged_secret_file_is_an_error_and_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe.secret");
        std::fs::write(&path, "short").unwrap();
        let err = read_secret_at(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short");
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_secret_at(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn an_empty_file_that_is_filled_during_the_read_is_a_retry_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe.secret");
        std::fs::write(&path, "").unwrap();
        let p = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            std::fs::write(&p, "0123456789abcdef0123456789abcdef").unwrap();
        });
        let got = read_secret_at(&path).unwrap();
        writer.join().unwrap();
        assert_eq!(got.as_deref(), Some("0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn readers_never_see_a_partial_secret_while_it_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe.secret");
        write_new_secret_at(&path).unwrap();
        let p = path.clone();
        let writer = std::thread::spawn(move || {
            for _ in 0..50 {
                write_new_secret_at(&p).unwrap();
            }
        });
        for _ in 0..200 {
            let s = read_secret_at(&path).unwrap().expect("the file is always there");
            assert!(valid_secret(&s));
        }
        writer.join().unwrap();
    }

    #[test]
    fn the_log_form_of_a_pipe_name_has_no_secret() {
        let secret = "0123456789abcdef0123456789abcdef";
        let n = pipe_path("S-1-5-21-1-2-3-1001", secret);
        let shown = redact_pipe_name(&n.to_string_lossy());
        assert!(!shown.contains(secret), "{shown}");
        assert!(shown.contains("S-1-5-21-1-2-3-1001"), "{shown}");
    }
}
