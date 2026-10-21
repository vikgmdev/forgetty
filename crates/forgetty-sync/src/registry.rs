//! Authorized device registry: `~/.local/share/forgetty/authorized_devices.json`.
//!
//! All writes are atomic (unique temp file, fsync, rename) per R-4 to prevent
//! corruption on daemon crash mid-write. The file is created mode 0600.

use std::path::PathBuf;

use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A single entry in the authorized-devices registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceEntry {
    /// iroh `EndpointId` as a base32 string (spec called this "NodeId").
    pub device_id: String,
    /// Human-readable label; auto-generated if the client doesn't provide one.
    pub name: String,
    /// ISO 8601 timestamp of when the device was first paired.
    pub paired_at: String,
    /// ISO 8601 timestamp of the most recent connection; `None` if never seen
    /// after the initial pairing.
    pub last_seen: Option<String>,
}

/// Errors from the device registry.
#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("I/O error reading/writing registry: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error in registry: {0}")]
    Json(#[from] serde_json::Error),
}

/// In-memory view of `authorized_devices.json`.
///
/// Wrap in `Arc<Mutex<DeviceRegistry>>` in the daemon so the pairing handler
/// and socket RPC handlers can share it.
pub struct DeviceRegistry {
    path: PathBuf,
    devices: Vec<DeviceEntry>,
    /// Devices removed in memory whose removal is not on disk yet (the save
    /// failed). Never authorized. Cleared by the next successful save.
    revoked_unsaved: std::collections::HashSet<String>,
}

impl DeviceRegistry {
    /// Load the registry from disk, creating an empty one if the file does not
    /// exist. Stale `.tmp` files (crash recovery) are removed on load.
    pub fn load() -> Result<Self, RegistryError> {
        let path = registry_path();

        // Remove the shared temp file an older build could leave behind.
        let tmp = path.with_extension("json.tmp");
        if tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
        }

        // The device list is a credential store: keep it 0600 even if an older
        // build or a loose umask created it wider.
        forgetty_core::fsutil::fix_file_mode(&path)?;

        let devices = if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            serde_json::from_str(&data)?
        } else {
            Vec::new()
        };

        // Entries written before names were sanitised may hold anything.
        let mut devices: Vec<DeviceEntry> = devices;
        for d in &mut devices {
            d.name = crate::sanitize::untrusted(&d.name);
        }

        Ok(Self { path, devices, revoked_unsaved: Default::default() })
    }

    /// An empty registry persisted at `path` (tests).
    #[cfg(test)]
    pub(crate) fn empty_at(path: PathBuf) -> Self {
        Self { path, devices: Vec::new(), revoked_unsaved: Default::default() }
    }

    /// Check whether an iroh `EndpointId` is in the authorized list. A device
    /// whose revoke could not be saved is not authorized (fail closed).
    pub fn is_authorized(&self, endpoint_id: &EndpointId) -> bool {
        let id_str = endpoint_id.to_string();
        !self.revoked_unsaved.contains(&id_str)
            && self.devices.iter().any(|d| d.device_id == id_str)
    }

    /// Add a new device entry and persist atomically.
    ///
    /// The name is sanitised here too ([`crate::sanitize::untrusted`]), so no
    /// caller can store a hostile name. A failed save leaves the registry as
    /// it was (the list is saved first and swapped in after), so a device whose
    /// pairing could not be saved is not authorized.
    pub fn add(&mut self, mut entry: DeviceEntry) -> Result<(), RegistryError> {
        entry.name = crate::sanitize::untrusted(&entry.name);
        let mut next = self.devices.clone();
        next.push(entry);
        self.commit(next)
    }

    /// Remove a device by its `device_id` string. Returns `true` if found.
    ///
    /// A revoke fails closed. The device leaves the in-memory list first. If
    /// the save then fails, the error is returned, the device id stays on an
    /// in-memory denylist that [`Self::is_authorized`] checks, and the next
    /// successful registry write (any `add`, `remove` or `update_last_seen`)
    /// saves the list without it.
    pub fn remove(&mut self, device_id: &str) -> Result<bool, RegistryError> {
        let before = self.devices.len();
        self.devices.retain(|d| d.device_id != device_id);
        let removed = self.devices.len() < before;
        if removed {
            self.revoked_unsaved.insert(device_id.to_string());
            let list = self.devices.clone();
            self.commit(list)?;
        }
        Ok(removed)
    }

    /// Return all current entries (read-only).
    pub fn list(&self) -> &[DeviceEntry] {
        &self.devices
    }

    /// Update the `last_seen` timestamp for a device by its `device_id`.
    pub fn update_last_seen(&mut self, device_id: &str) -> Result<(), RegistryError> {
        let now = iso8601_now();
        let mut next = self.devices.clone();
        for d in &mut next {
            if d.device_id == device_id {
                d.last_seen = Some(now);
                break;
            }
        }
        self.commit(next)
    }

    /// Save `next`, then make it the in-memory list. A failed save changes
    /// nothing in `devices`. A save that works also persists every removal
    /// that was waiting, so the denylist is empty after it.
    fn commit(&mut self, next: Vec<DeviceEntry>) -> Result<(), RegistryError> {
        Self::save(&self.path, &next)?;
        self.devices = next;
        self.revoked_unsaved.clear();
        Ok(())
    }

    /// Atomically write `devices` to `path`: unique temp file created 0600,
    /// fsync, rename, fsync of the directory. No shared temp name, no
    /// non-atomic fallback.
    fn save(path: &std::path::Path, devices: &[DeviceEntry]) -> Result<(), RegistryError> {
        let json = serde_json::to_string_pretty(devices)?;
        forgetty_core::fsutil::write_private_atomic(path, json.as_bytes())?;
        Ok(())
    }
}

/// Returns the canonical path for the authorized-devices file.
pub fn registry_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("forgetty")
        .join("authorized_devices.json")
}

/// Returns the current UTC time as an ISO 8601 string.
pub fn iso8601_now() -> String {
    // Use std::time for minimal deps — no chrono needed for basic ISO 8601.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    // Format as YYYY-MM-DDTHH:MM:SSZ
    let s = secs;
    let secs_in_day = s % 86400;
    let days = s / 86400;
    let (y, m, d) = days_to_ymd(days);
    let hh = secs_in_day / 3600;
    let mm = (secs_in_day % 3600) / 60;
    let ss = secs_in_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Convert days-since-epoch to (year, month, day).
fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Simple Gregorian calendar algorithm.
    let mut y = 1970u64;
    loop {
        let dy = if is_leap(y) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days = days.saturating_sub(dy);
        y += 1;
    }
    let months = if is_leap(y) {
        [31u64, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31u64, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1u64;
    for dm in &months {
        if days < *dm {
            break;
        }
        days = days.saturating_sub(*dm);
        m += 1;
    }
    (y, m, days + 1)
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn entry(id: &str) -> DeviceEntry {
        DeviceEntry {
            device_id: id.into(),
            name: "n".into(),
            paired_at: iso8601_now(),
            last_seen: None,
        }
    }

    #[test]
    fn registry_file_is_created_0600_and_leaves_no_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/authorized_devices.json");
        let mut reg = DeviceRegistry::empty_at(path.clone());
        reg.add(entry("a")).unwrap();
        reg.add(entry("b")).unwrap();
        assert!(reg.remove("a").unwrap());
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["authorized_devices.json".to_string()], "{names:?}");
    }

    #[test]
    fn a_failed_save_leaves_memory_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = DeviceRegistry::empty_at(tmp.path().join("state/authorized_devices.json"));
        reg.add(entry("a")).unwrap();
        // Make saving fail: the directory path becomes a regular file.
        let dir = tmp.path().join("state");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, b"not a directory").unwrap();

        assert!(reg.add(entry("b")).is_err());
        assert_eq!(reg.list().len(), 1, "a device that was not saved is in memory");
        assert!(reg.update_last_seen("a").is_err());
        assert!(reg.list()[0].last_seen.is_none(), "last_seen changed without a save");
    }

    #[test]
    fn a_revoke_that_cannot_be_saved_still_denies_the_device() {
        let key = iroh::SecretKey::from_bytes(&[7u8; 32]);
        let id = key.public();
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = DeviceRegistry::empty_at(tmp.path().join("state/authorized_devices.json"));
        reg.add(entry(&id.to_string())).unwrap();
        assert!(reg.is_authorized(&id));

        // Make saving fail: the state directory becomes a regular file.
        let dir = tmp.path().join("state");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, b"not a directory").unwrap();
        assert!(reg.remove(&id.to_string()).is_err(), "the save should fail");
        assert!(!reg.is_authorized(&id), "a revoked device must not stay authorized");
        assert!(reg.list().is_empty());

        // A pairing that cannot be saved does not authorize the device.
        let other = iroh::SecretKey::from_bytes(&[8u8; 32]).public();
        assert!(reg.add(entry(&other.to_string())).is_err());
        assert!(!reg.is_authorized(&other), "an unsaved pairing must not authorize");

        // The next successful write persists the removal and clears the denial.
        std::fs::remove_file(&dir).unwrap();
        reg.add(entry(&other.to_string())).unwrap();
        assert!(reg.is_authorized(&other));
        assert!(!reg.is_authorized(&id));
        let on_disk = std::fs::read_to_string(dir.join("authorized_devices.json")).unwrap();
        assert!(!on_disk.contains(&id.to_string()), "the revoked device is back on disk");
    }
}
