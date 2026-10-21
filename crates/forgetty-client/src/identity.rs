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

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
