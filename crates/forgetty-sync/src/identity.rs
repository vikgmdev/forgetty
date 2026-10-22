//! Ed25519 identity key — load or generate, persist at `~/.local/share/forgetty/identity.key`.
//!
//! The identity key is stored as raw 32 bytes with file permissions 0600 (user
//! read/write only) per AC-3 (file permissions) and R-3 (security risk).

use std::path::PathBuf;

use iroh::SecretKey;

/// Load the identity key from disk, or generate and persist a new one.
///
/// The key is stored at `~/.local/share/forgetty/identity.key` as 32 raw bytes.
/// The file is created with mode 0600 at open time (never world-readable, not
/// even for an instant), and the state directory is 0700. A file left with a
/// wider mode by an older build is tightened here.
pub fn load_or_generate() -> anyhow::Result<SecretKey> {
    load_or_generate_at(&identity_path())
}

/// [`load_or_generate`] for an explicit path.
///
/// Safe against two processes starting at once: the key is written to a unique
/// temp file and linked into place, so exactly one writer wins and the loser
/// reads the winner's key. Nobody ever sees a partial key.
pub fn load_or_generate_at(path: &std::path::Path) -> anyhow::Result<SecretKey> {
    if let Some(parent) = path.parent() {
        forgetty_core::fsutil::ensure_private_dir(parent)?;
    }
    if !path.exists() {
        let key = SecretKey::generate(&mut rand::rng());
        if forgetty_core::fsutil::create_private_if_absent(path, &key.to_bytes())? {
            return Ok(key);
        }
        // Lost the race: fall through and read the winner's key.
    }
    forgetty_core::fsutil::fix_file_mode(path)?;
    let bytes = std::fs::read(path)?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("identity.key is not 32 bytes; delete it to regenerate"))?;
    Ok(SecretKey::from_bytes(&arr))
}

/// Returns the canonical path for the identity key file.
pub fn identity_path() -> PathBuf {
    forgetty_core::platform::data_dir().join("identity.key")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn key_is_created_0600_in_a_0700_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/identity.key");
        let key = load_or_generate_at(&path).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        // Reload gives the same key.
        assert_eq!(load_or_generate_at(&path).unwrap().to_bytes(), key.to_bytes());
    }

    #[test]
    fn a_wide_existing_key_is_tightened() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("identity.key");
        std::fs::write(&path, [7u8; 32]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        load_or_generate_at(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn parallel_first_start_yields_one_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("identity.key");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let p = path.clone();
                std::thread::spawn(move || load_or_generate_at(&p).unwrap().to_bytes())
            })
            .collect();
        let keys: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(keys.iter().all(|k| *k == keys[0]), "every starter sees the same key");
    }

    #[test]
    fn a_short_key_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("identity.key");
        std::fs::write(&path, b"short").unwrap();
        assert!(load_or_generate_at(&path).is_err());
    }
}
