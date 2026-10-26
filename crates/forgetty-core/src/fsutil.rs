//! Private, durable file helpers for the state directory.
//!
//! The state directory holds the identity key, the paired-device list and the
//! session layout. These helpers make sure that
//!
//! - directories are created `0700` and files `0600` *at creation time*
//!   (never created with the umask and fixed up afterwards),
//! - a file is replaced atomically: unique temp name in the same directory,
//!   `fsync` of the file, `rename`, `fsync` of the directory,
//! - two processes writing the same file cannot share a temp file.
//!
//! On Windows the same promises hold with a protected DACL that has one entry (the current
//! user, full access) in place of the Unix modes. See [`acl`].

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub mod acl;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

/// Mode of private directories.
pub const DIR_MODE: u32 = 0o700;
/// Mode of private files.
pub const FILE_MODE: u32 = 0o600;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temp path next to `path` that no other writer uses: pid, a process-wide
/// counter and the clock are in the name.
pub fn unique_tmp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    path.with_file_name(format!(".{name}.tmp.{}.{n}.{nanos}", std::process::id()))
}

/// Create `dir` (and parents) with mode `0700`, and tighten it if it exists
/// with a wider mode.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new().recursive(true).mode(DIR_MODE).create(dir)?;
        let mode = fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode != DIR_MODE {
            fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))?;
        }
    }
    #[cfg(windows)]
    {
        use std::collections::HashSet;
        use std::sync::Mutex;
        // The DACL of a folder this process already restricted is not set again on every
        // call: the callers write state files often.
        static DONE: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
        fs::create_dir_all(dir)?;
        let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
        let done = done.get_or_insert_with(HashSet::new);
        if !done.contains(dir) {
            acl::restrict_dir_to_user(dir)?;
            done.insert(dir.to_path_buf());
        }
    }
    #[cfg(not(any(unix, windows)))]
    fs::create_dir_all(dir)?;
    Ok(())
}

/// Open a new file for writing with mode `0600` set at creation. Fails if the
/// file exists.
pub fn create_private_new(path: &Path) -> io::Result<fs::File> {
    #[cfg(windows)]
    {
        acl::create_private_file(path)
    }
    #[cfg(not(windows))]
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(FILE_MODE);
        opts.open(path)
    }
}

/// Open `path` with `opts`, and when `opts` creates the file, create it private (`0600` on Unix,
/// a user-only DACL on Windows) from its first moment.
pub fn open_private(opts: &fs::OpenOptions, path: &Path) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        let mut opts = opts.clone();
        opts.mode(FILE_MODE);
        opts.open(path)
    }
    #[cfg(not(unix))]
    {
        // `opts` may create the file. Make a missing one private first. Two racing callers: the
        // loser gets `AlreadyExists`, which is fine, the file is there and private.
        if !path.exists() {
            match create_private_new(path) {
                Ok(f) => drop(f),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        opts.open(path)
    }
}

/// Make an existing file `0600`. A missing file is not an error. Returns
/// `true` if the mode had to be changed.
pub fn fix_file_mode(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        match fs::metadata(path) {
            Ok(meta) => {
                if meta.permissions().mode() & 0o777 != FILE_MODE {
                    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
                    return Ok(true);
                }
                Ok(false)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    #[cfg(windows)]
    {
        if !path.exists() {
            return Ok(false);
        }
        if matches!(acl::is_user_only(path), Ok(true)) {
            return Ok(false);
        }
        acl::restrict_to_user(path)?;
        Ok(true)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(false)
    }
}

/// `true` when `path` (a file or a folder) can be used by the current user only: mode `0600`
/// (`0700` for a folder) on Unix, a protected DACL with one entry for the user on Windows.
pub fn is_private(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        let meta = fs::metadata(path)?;
        let want = if meta.is_dir() { DIR_MODE } else { FILE_MODE };
        Ok(meta.permissions().mode() & 0o777 == want)
    }
    #[cfg(windows)]
    {
        acl::is_user_only(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(true)
    }
}

/// Flush the directory entry change (a rename or a new file) to disk.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Rename `tmp` over `path`. On Windows the move is written through to disk before it returns.
fn replace_file(tmp: &Path, path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        acl::replace_file(tmp, path)
    }
    #[cfg(not(windows))]
    {
        fs::rename(tmp, path)
    }
}

fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Write `bytes` to `path` atomically and durably, as a private (`0600`) file.
///
/// Unique temp file, `fsync`, `rename`, `fsync` of the directory. There is no
/// non-atomic fallback: an error is returned and the old file stays.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = parent_of(path);
    ensure_private_dir(dir)?;
    let tmp = unique_tmp_path(path);
    let result = (|| {
        let mut f = create_private_new(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        replace_file(&tmp, path)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Create `path` with `bytes` only if it does not exist yet. Atomic: other
/// processes see either no file or the complete file.
///
/// Returns `true` if this call created it, `false` if it already existed (the
/// existing file is left untouched).
pub fn create_private_if_absent(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    let dir = parent_of(path);
    ensure_private_dir(dir)?;
    let tmp = unique_tmp_path(path);
    let result = (|| {
        let mut f = create_private_new(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        // hard_link fails with AlreadyExists if another writer got there first.
        match fs::hard_link(&tmp, path) {
            Ok(()) => {
                sync_dir(dir)?;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    })();
    let _ = fs::remove_file(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn private_dir_is_0700_even_under_a_loose_umask_and_existing_loose_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("a/b");
        ensure_private_dir(&d).unwrap();
        assert_eq!(mode(&d), 0o700);
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&d).unwrap();
        assert_eq!(mode(&d), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn files_are_0600_at_creation_not_chmodded_later() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        // The file never exists with a wider mode: open with a 0 umask would
        // give 0600 & ~0 = 0600 only if the mode is passed at open time.
        let f = create_private_new(&p).unwrap();
        assert_eq!(f.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        drop(f);
        let q = tmp.path().join("g");
        write_private_atomic(&q, b"x").unwrap();
        assert_eq!(mode(&q), 0o600);
        let r = tmp.path().join("h");
        assert!(create_private_if_absent(&r, b"x").unwrap());
        assert_eq!(mode(&r), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn fix_file_mode_tightens_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        fs::write(&p, b"x").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(fix_file_mode(&p).unwrap());
        assert_eq!(mode(&p), 0o600);
        assert!(!fix_file_mode(&p).unwrap());
        assert!(!fix_file_mode(&tmp.path().join("missing")).unwrap());
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f.json");
        write_private_atomic(&p, b"one").unwrap();
        write_private_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        let left: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["f.json".to_string()], "no temp file is left: {left:?}");
    }

    #[test]
    fn unique_tmp_names_differ() {
        let p = Path::new("/x/y.json");
        assert_ne!(unique_tmp_path(p), unique_tmp_path(p));
        assert_eq!(unique_tmp_path(p).parent(), p.parent());
    }

    #[test]
    fn concurrent_writers_never_share_a_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for j in 0..50 {
                        write_private_atomic(&p, format!("writer {i} round {j}").as_bytes())
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("writer "), "complete content: {text:?}");
    }

    #[test]
    fn create_if_absent_has_exactly_one_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("key");
        let wins: usize = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || create_private_if_absent(&p, &[i as u8; 32]).unwrap())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum();
        assert_eq!(wins, 1);
        let content = fs::read(&p).unwrap();
        assert_eq!(content.len(), 32);
        assert!(content.iter().all(|b| *b == content[0]), "one writer's whole content");
    }

    #[cfg(windows)]
    #[test]
    fn windows_files_and_dirs_are_user_only_from_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("state/server");
        ensure_private_dir(&d).unwrap();
        // Calling it again is fine (the second call sets nothing).
        ensure_private_dir(&d).unwrap();
        let sddl = acl::describe(&d).unwrap();
        assert!(sddl.starts_with("D:P") && sddl.contains("OICI"), "{sddl}");

        let p = d.join("f");
        let f = create_private_new(&p).unwrap();
        drop(f);
        assert!(acl::is_user_only(&p).unwrap(), "{:?}", acl::describe(&p));
        assert_eq!(
            create_private_new(&p).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists,
            "a second create fails"
        );

        let q = d.join("g");
        write_private_atomic(&q, b"x").unwrap();
        assert!(acl::is_user_only(&q).unwrap(), "{:?}", acl::describe(&q));
        assert_eq!(fs::read(&q).unwrap(), b"x");

        // A file made by plain std inherits the folder's one entry. `fix_file_mode` makes
        // it protected, like the Unix 0600 fix.
        let r = d.join("h");
        fs::write(&r, b"x").unwrap();
        assert!(fix_file_mode(&r).unwrap());
        assert!(acl::is_user_only(&r).unwrap());
        assert!(!fix_file_mode(&r).unwrap());
        assert!(!fix_file_mode(&d.join("missing")).unwrap());

        // open_private makes a missing file private.
        let t = d.join("i");
        let mut o = fs::OpenOptions::new();
        o.write(true).create(true).truncate(false);
        drop(open_private(&o, &t).unwrap());
        assert!(acl::is_user_only(&t).unwrap());
    }
}
