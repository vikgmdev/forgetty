//! Ed25519 identity key — load or generate, persist at `~/.local/share/forgetty/identity.key`.
//!
//! The identity key is stored as raw 32 bytes with file permissions 0600 (user
//! read/write only) per AC-3 (file permissions) and R-3 (security risk).

use std::path::PathBuf;

use iroh::SecretKey;

/// Load the identity key from disk, or generate and persist a new one.
///
/// The key is stored at `~/.local/share/forgetty/identity.key` as 32 raw bytes.
/// File permissions are set to 0600 on creation (Linux only; NOP on other OSes).
pub fn load_or_generate() -> anyhow::Result<SecretKey> {
    let path = identity_path();
    if path.exists() {
        let bytes = std::fs::read(&path)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            anyhow::anyhow!("identity.key is not 32 bytes; delete it to regenerate")
        })?;
        Ok(SecretKey::from_bytes(&arr))
    } else {
        let key = SecretKey::generate(&mut rand::rng());
        std::fs::create_dir_all(path.parent().expect("identity path has parent"))?;
        std::fs::write(&path, key.to_bytes())?;
        set_file_permissions_600(&path)?;
        Ok(key)
    }
}

/// Returns the canonical path for the identity key file.
pub fn identity_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("forgetty")
        .join("identity.key")
}

/// Set file permissions to 0600 (user read/write only).
///
/// This is mandatory per R-3: a world-readable identity.key lets any local
/// user extract the private key and impersonate the daemon.
#[cfg(unix)]
fn set_file_permissions_600(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_permissions_600(_path: &std::path::Path) -> anyhow::Result<()> {
    // No-op on non-Unix platforms — Windows/Android use OS-level ACLs.
    Ok(())
}
