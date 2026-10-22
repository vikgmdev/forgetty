//! Where the local control socket lives, and the checks that keep it private.
//!
//! The socket gives a shell (`new_tab`, `send_input`), so a squatted or
//! world-reachable socket is a local privilege problem. Rules:
//!
//! - The socket sits in a private directory: `$XDG_RUNTIME_DIR/forgetty/`, or
//!   `/tmp/forgetty-<uid>/` when `XDG_RUNTIME_DIR` is unset.
//! - The directory is created with mode 0700. The server refuses to start, and
//!   the CLI refuses to connect, if it is a symlink, is not owned by the
//!   current uid, or has any group/other permission bit.
//! - The CLI also refuses a socket file that is not a socket or is owned by
//!   another uid.
//!
//! The socket server adds `chmod 0600` after bind and an `SO_PEERCRED` uid
//! check on every accepted connection (in `forgetty-socket`).

use std::fs::Metadata;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

/// The real uid of this process.
pub fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Private directory that holds the socket.
pub fn socket_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        // `platform::runtime_dir` is the same path, and a test process never gets
        // the real one from it.
        Some(dir) if !dir.is_empty() => forgetty_core::platform::runtime_dir()
            .unwrap_or_else(|| PathBuf::from(dir).join("forgetty")),
        _ => PathBuf::from(format!("/tmp/forgetty-{}", current_uid())),
    }
}

/// Default socket path: `<socket_dir>/forgetty.sock`.
pub fn default_socket_path() -> PathBuf {
    socket_dir().join("forgetty.sock")
}

/// Judge the metadata of the socket directory. `meta` must come from
/// `symlink_metadata`, so a symlink is seen as a symlink.
pub fn check_dir_meta(dir: &Path, meta: &Metadata, uid: u32) -> Result<(), String> {
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", dir.display()));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    if meta.uid() != uid {
        return Err(format!("{} is owned by uid {}, not uid {uid}", dir.display(), meta.uid()));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} has mode {mode:03o}; group/other access is not allowed (need 700)",
            dir.display()
        ));
    }
    Ok(())
}

/// Judge the metadata of the socket file itself (`symlink_metadata`).
pub fn check_socket_meta(path: &Path, meta: &Metadata, uid: u32) -> Result<(), String> {
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", path.display()));
    }
    if !meta.file_type().is_socket() {
        return Err(format!("{} is not a socket", path.display()));
    }
    if meta.uid() != uid {
        return Err(format!("{} is owned by uid {}, not uid {uid}", path.display(), meta.uid()));
    }
    Ok(())
}

/// Create the socket directory (mode 0700) if missing, then verify it.
///
/// Server side. Fails if the directory exists and is unsafe.
pub fn prepare_socket_dir(dir: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new().mode(0o700).create(dir)?;
        }
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    check_dir_meta(dir, &meta, current_uid())
        .map_err(|m| io::Error::new(io::ErrorKind::PermissionDenied, m))
}

/// What the CLI found when it looked at the socket.
#[derive(Debug, PartialEq, Eq)]
pub enum SocketCheck {
    /// Directory or socket does not exist: no server.
    Missing,
    /// Safe to connect.
    Ok,
    /// Exists but must not be trusted. The string says why.
    Unsafe(String),
}

/// Check the directory and the socket file for `path`. Never creates anything.
pub fn verify_socket_path(path: &Path, uid: u32) -> SocketCheck {
    let Some(dir) = path.parent() else { return SocketCheck::Missing };
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if let Err(why) = check_dir_meta(dir, &meta, uid) {
                return SocketCheck::Unsafe(why);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return SocketCheck::Missing,
        Err(e) => return SocketCheck::Unsafe(format!("{}: {e}", dir.display())),
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => match check_socket_meta(path, &meta, uid) {
            Ok(()) => SocketCheck::Ok,
            Err(why) => SocketCheck::Unsafe(why),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => SocketCheck::Missing,
        Err(e) => SocketCheck::Unsafe(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn mkdir(path: &Path, mode: u32) {
        std::fs::DirBuilder::new().mode(mode).create(path).unwrap();
        // DirBuilder honours the umask; make the mode exact.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn prepare_creates_a_private_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("forgetty");
        prepare_socket_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // Idempotent.
        prepare_socket_dir(&dir).unwrap();
    }

    #[test]
    fn prepare_refuses_group_or_other_bits() {
        let tmp = tempfile::tempdir().unwrap();
        for mode in [0o750, 0o705, 0o755, 0o777, 0o770] {
            let dir = tmp.path().join(format!("d{mode:o}"));
            mkdir(&dir, mode);
            let err = prepare_socket_dir(&dir).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "mode {mode:o}");
            assert!(err.to_string().contains("group/other"), "{err}");
        }
    }

    #[test]
    fn prepare_refuses_a_symlink_even_to_a_good_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        mkdir(&real, 0o700);
        let link = tmp.path().join("link");
        symlink(&real, &link).unwrap();
        let err = prepare_socket_dir(&link).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn prepare_refuses_a_file_where_the_dir_should_be() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        let err = prepare_socket_dir(&file).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[test]
    fn dir_owned_by_another_uid_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        mkdir(&dir, 0o700);
        let meta = std::fs::symlink_metadata(&dir).unwrap();
        // Same dir, judged as if we were a different user.
        let err = check_dir_meta(&dir, &meta, current_uid() + 1).unwrap_err();
        assert!(err.contains("owned by uid"), "{err}");
        assert!(check_dir_meta(&dir, &meta, current_uid()).is_ok());
    }

    #[test]
    fn socket_file_owned_by_another_uid_or_not_a_socket_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        mkdir(&dir, 0o700);
        let sock = dir.join("forgetty.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let me = current_uid();
        assert_eq!(verify_socket_path(&sock, me), SocketCheck::Ok);
        assert!(matches!(verify_socket_path(&sock, me + 1), SocketCheck::Unsafe(_)));

        // A regular file with the socket's name.
        let fake = dir.join("fake.sock");
        std::fs::write(&fake, b"").unwrap();
        match verify_socket_path(&fake, me) {
            SocketCheck::Unsafe(why) => assert!(why.contains("not a socket"), "{why}"),
            other => panic!("expected Unsafe, got {other:?}"),
        }

        // A symlink to the real socket.
        let link = dir.join("link.sock");
        symlink(&sock, &link).unwrap();
        match verify_socket_path(&link, me) {
            SocketCheck::Unsafe(why) => assert!(why.contains("symlink"), "{why}"),
            other => panic!("expected Unsafe, got {other:?}"),
        }
    }

    #[test]
    fn missing_dir_or_socket_means_no_server() {
        let tmp = tempfile::tempdir().unwrap();
        let me = current_uid();
        assert_eq!(
            verify_socket_path(&tmp.path().join("nope/forgetty.sock"), me),
            SocketCheck::Missing
        );
        let dir = tmp.path().join("d");
        mkdir(&dir, 0o700);
        assert_eq!(verify_socket_path(&dir.join("forgetty.sock"), me), SocketCheck::Missing);
    }

    #[test]
    fn unsafe_dir_is_reported_before_the_socket_is_looked_at() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        mkdir(&dir, 0o755);
        match verify_socket_path(&dir.join("forgetty.sock"), current_uid()) {
            SocketCheck::Unsafe(why) => assert!(why.contains("group/other"), "{why}"),
            other => panic!("expected Unsafe, got {other:?}"),
        }
    }

    #[test]
    fn default_path_lives_in_a_forgetty_subdir() {
        let p = default_socket_path();
        assert!(
            p.ends_with("forgetty/forgetty.sock") || p.to_string_lossy().contains("/tmp/forgetty-")
        );
    }
}
