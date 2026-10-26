//! Where the local control pipe is, on Windows.
//!
//! The Unix server keeps a socket in a private directory. Windows has no such file: the control
//! socket is a named pipe, `\\.\pipe\forgetty-<user SID>`, and the protection is in the pipe
//! itself (see `forgetty_socket::pipe`): a DACL with the current user only, remote clients
//! rejected, the first instance claimed by the server, and a SID check on both ends.
//!
//! A server started with `--state-dir` (tests, a second instance) must not share the pipe of
//! the real one, so a hash of the runtime directory is added to the name when
//! `XDG_RUNTIME_DIR` is set, which `--state-dir` does.

use std::io;
use std::path::{Path, PathBuf};

/// The folder the server keeps its runtime files in (the same one `platform::runtime_dir` names,
/// else the data folder: Windows has no runtime directory).
pub fn socket_dir() -> PathBuf {
    forgetty_core::platform::runtime_dir().unwrap_or_else(forgetty_core::platform::data_dir)
}

/// FNV-1a over the lowercase text of `s`: a name that is the same in every process and every
/// run (the std hasher is not).
fn stable_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.to_lowercase().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The pipe name for `sid`, and for the runtime folder `runtime` when there is one.
pub fn pipe_path(sid: &str, runtime: Option<&Path>) -> PathBuf {
    match runtime {
        Some(dir) => PathBuf::from(format!(
            r"\\.\pipe\forgetty-{sid}-{:016x}",
            stable_hash(&dir.to_string_lossy())
        )),
        None => PathBuf::from(format!(r"\\.\pipe\forgetty-{sid}")),
    }
}

/// Default control pipe: `\\.\pipe\forgetty-<SID>`, with a hash of the runtime folder after the
/// SID when `XDG_RUNTIME_DIR` is set (`--state-dir`).
pub fn default_socket_path() -> PathBuf {
    // Without a SID there is no safe name: use a name nobody else would have (the connect
    // fails and says why).
    let sid = forgetty_core::fsutil::acl::current_user_sid().unwrap_or_else(|_| "unknown".into());
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(|_| socket_dir());
    pipe_path(&sid, runtime.as_deref())
}

/// Make sure the runtime folder exists and only this user can use it.
pub fn prepare_socket_dir(dir: &Path) -> io::Result<()> {
    forgetty_core::fsutil::ensure_private_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_holds_the_sid_and_a_state_dir_changes_it() {
        let a = pipe_path("S-1-5-21-1-2-3-1001", None);
        assert_eq!(a, PathBuf::from(r"\\.\pipe\forgetty-S-1-5-21-1-2-3-1001"));
        let b = pipe_path("S-1-5-21-1-2-3-1001", Some(Path::new(r"C:\x\run")));
        let c = pipe_path("S-1-5-21-1-2-3-1001", Some(Path::new(r"C:\y\run")));
        assert_ne!(a, b);
        assert_ne!(b, c);
        // Case does not matter on Windows paths; the name is stable.
        assert_eq!(b, pipe_path("S-1-5-21-1-2-3-1001", Some(Path::new(r"c:\X\RUN"))));
    }

    #[test]
    fn the_hash_is_fixed() {
        // A change here would split a CLI and a server of different builds.
        assert_eq!(stable_hash("abc"), 0xe71fa2190541574b);
    }
}
