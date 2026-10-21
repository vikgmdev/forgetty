//! Identity helpers. Same file format as `forgetty-sync`: 32 raw bytes, mode
//! 0600 on Unix. The caller picks the path (Android has no `~/.local/share`).

use std::io;
use std::path::Path;

use iroh::SecretKey;
use uuid::Uuid;

/// Load the iroh secret key from `path`, or generate and persist one.
pub fn load_or_generate_identity(path: &Path) -> io::Result<SecretKey> {
    if path.exists() {
        let bytes = std::fs::read(path)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "identity.key is not 32 bytes")
        })?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let key = SecretKey::generate(&mut rand::rng());
    write_private(path, &key.to_bytes())?;
    Ok(key)
}

/// Load the stable `client_id` from `path`, or generate and persist one.
pub fn load_or_generate_client_id(path: &Path) -> io::Result<Uuid> {
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(id) = Uuid::parse_str(text.trim()) {
            return Ok(id);
        }
    }
    let id = Uuid::new_v4();
    write_private(path, id.hyphenated().to_string().as_bytes())?;
    Ok(id)
}

/// Write a private file. Unix: mode 0600 at creation, atomic rename, parent
/// directory created 0700. Elsewhere the app layer sets user-only ACLs, so this
/// is a plain write.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        forgetty_core::fsutil::write_private_atomic(path, bytes)
    }
    #[cfg(not(unix))]
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_fresh_key_file_is_0600_in_a_0700_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // The dir does not exist yet: it is created here, whatever the umask.
        let path = tmp.path().join("state/client/identity.key");
        let key = load_or_generate_identity(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        // Loading again returns the same key and leaves no temp file behind.
        assert_eq!(load_or_generate_identity(&path).unwrap().to_bytes(), key.to_bytes());
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn a_fresh_client_id_file_is_private_too() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cid/client_id");
        let id = load_or_generate_client_id(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(load_or_generate_client_id(&path).unwrap(), id);
    }
}
