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

/// Revoked devices `revoked.pending` has room for without growing: 1024 ids
/// of 64 hex characters and a newline. A longer list makes the file grow.
const PENDING_IDS: usize = 1024;
/// Size reserved for `revoked.pending`.
const PENDING_BYTES: usize = PENDING_IDS * 65;

/// What [`DeviceRegistry::revoke`] did.
#[derive(Debug)]
pub struct RevokeResult {
    /// The device was in the list.
    pub found: bool,
    /// A restart cannot bring the device back: the record in
    /// `revoked.pending` or the registry file itself has the removal on disk.
    pub durable: bool,
    /// The registry file could not be saved.
    pub save_error: Option<RegistryError>,
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
    /// `revoked.pending` was written and not removed yet.
    pending_written: bool,
    /// The ids the record on disk names (or would name, after a failed write).
    pending_ids: std::collections::BTreeSet<String>,
    /// A save-retry thread runs for this registry.
    retry_running: bool,
}

impl DeviceRegistry {
    /// Load the registry from disk, creating an empty one if the file does not
    /// exist. Stale `.tmp` files (crash recovery) are removed on load.
    pub fn load() -> Result<Self, RegistryError> {
        Self::load_from(registry_path())
    }

    /// [`Self::load`] for a registry file at `path`.
    pub(crate) fn load_from(path: PathBuf) -> Result<Self, RegistryError> {
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

        let mut reg = Self {
            path,
            devices,
            revoked_unsaved: Default::default(),
            pending_written: false,
            pending_ids: Default::default(),
            retry_running: false,
        };
        // A revoke whose save failed before the last stop is applied again.
        reg.apply_pending_revokes();
        // Reserve the disk blocks the next revoke record needs now, so a full
        // disk cannot keep it from being written later.
        reg.preallocate_pending();
        Ok(reg)
    }

    /// File next to the registry that names revoked devices whose removal is
    /// not in the registry file yet.
    fn pending_path(&self) -> PathBuf {
        self.path.with_file_name("revoked.pending")
    }

    /// Read `revoked.pending`: take those devices out of the list and deny
    /// them, then try to save. The record is cleared once a save works.
    fn apply_pending_revokes(&mut self) {
        let Ok(text) = std::fs::read_to_string(self.pending_path()) else { return };
        // The file is preallocated and rewritten in place: padding is newlines
        // or NULs, not ids.
        let ids: Vec<String> = text
            .lines()
            .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c == '\0'))
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        if ids.is_empty() {
            return;
        }
        self.pending_written = true;
        self.pending_ids.extend(ids.iter().cloned());
        self.devices.retain(|d| !ids.contains(&d.device_id));
        self.revoked_unsaved.extend(ids);
        let list = self.devices.clone();
        if let Err(e) = self.commit(list) {
            tracing::warn!("registry: revoked devices are denied but the list is not saved: {e}");
        }
    }

    /// Make `revoked.pending` exist with [`PENDING_BYTES`] of blocks behind
    /// it, so recording a revoke later is a write in place.
    fn preallocate_pending(&self) {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.pending_path();
        if path.exists() {
            return;
        }
        let made = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut f =
                std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
            f.write_all(&[b'\n'; PENDING_BYTES])?;
            f.sync_all()?;
            if let Some(dir) = path.parent() {
                std::fs::File::open(dir)?.sync_all()?;
            }
            Ok(())
        })();
        if let Err(e) = made {
            tracing::warn!("registry: could not preallocate revoked.pending: {e}");
        }
    }

    /// Record the denylist in `revoked.pending` by writing over the file in
    /// place, then fsync. No new file and no rename: it needs no free inode
    /// and, with the preallocated blocks, no free space. A restart before the
    /// registry file is saved still denies these devices.
    fn write_pending(&mut self) -> std::io::Result<()> {
        use std::os::unix::fs::{FileExt, OpenOptionsExt};
        let path = self.pending_path();
        let existed = path.exists();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;
        let old_len = f.metadata()?.len() as usize;
        let mut buf: Vec<u8> =
            self.pending_ids.iter().map(|id| format!("{id}\n")).collect::<String>().into();
        if buf.len() < old_len {
            buf.resize(old_len, b'\n');
        }
        f.write_all_at(&buf, 0)?;
        f.sync_all()?;
        if !existed {
            if let Some(dir) = path.parent() {
                std::fs::File::open(dir)?.sync_all()?;
            }
        }
        self.pending_written = true;
        Ok(())
    }

    /// The registry file has the removals: blank the record, in place, and
    /// fsync it. When that fails the record stays (and `pending_ids` with it):
    /// a device that is paired again later is taken out of it by [`Self::add`],
    /// so an old record cannot revoke a new pairing.
    fn clear_pending(&mut self) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        let result = (|| {
            let f = match std::fs::OpenOptions::new().write(true).open(self.pending_path()) {
                Ok(f) => f,
                // No record on disk, nothing to clear.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            let len = f.metadata()?.len() as usize;
            f.write_all_at(&vec![b'\n'; len], 0)?;
            f.sync_all()
        })();
        match &result {
            Ok(()) => {
                self.pending_ids.clear();
                self.pending_written = false;
            }
            Err(e) => tracing::warn!("registry: could not clear revoked.pending: {e}"),
        }
        result
    }

    /// `true` while a revoke is denied in memory but not saved.
    pub fn has_unsaved_revokes(&self) -> bool {
        !self.revoked_unsaved.is_empty()
    }

    /// Try to save the current list again (the retry after a failed revoke).
    pub fn retry_save(&mut self) -> Result<(), RegistryError> {
        let list = self.devices.clone();
        self.commit(list)
    }

    /// An empty registry persisted at `path` (tests).
    #[cfg(test)]
    pub(crate) fn empty_at(path: PathBuf) -> Self {
        Self {
            path,
            devices: Vec::new(),
            revoked_unsaved: Default::default(),
            pending_written: false,
            pending_ids: Default::default(),
            retry_running: false,
        }
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
        // A device that was revoked and pairs again must not be in the revoke
        // record: a restart would revoke it a second time. Take it out first
        // (durably). If that fails the pairing fails, so the record stays true.
        let id = entry.device_id.clone();
        let in_record = self.pending_ids.remove(&id);
        if in_record {
            if let Err(e) = self.write_pending() {
                self.pending_ids.insert(id);
                return Err(e.into());
            }
        }
        let mut next = self.devices.clone();
        next.push(entry);
        let res = self.commit(next);
        if res.is_err() && in_record {
            // Not paired after all: the revoke stands, on disk as well.
            self.pending_ids.insert(id);
            let _ = self.write_pending();
        }
        res
    }

    /// Remove a device by its `device_id` string. Returns `true` if found.
    ///
    /// A revoke fails closed. The device leaves the in-memory list first. If
    /// the save then fails, the error is returned, the device id stays on an
    /// in-memory denylist that [`Self::is_authorized`] checks, and the next
    /// successful registry write (any `add`, `remove` or `update_last_seen`)
    /// saves the list without it.
    pub fn remove(&mut self, device_id: &str) -> Result<bool, RegistryError> {
        let out = self.revoke(device_id);
        match out.save_error {
            Some(e) => Err(e),
            None => Ok(out.found),
        }
    }

    /// [`Self::remove`] with the whole outcome. The record in
    /// `revoked.pending` is written and fsynced before the registry file is
    /// saved, so once this returns with `durable`, a crash or a restart cannot
    /// bring the device back. Without `durable` the device is still denied in
    /// memory, but a restart could allow it again.
    pub fn revoke(&mut self, device_id: &str) -> RevokeResult {
        let before = self.devices.len();
        self.devices.retain(|d| d.device_id != device_id);
        let found = self.devices.len() < before;
        if !found {
            return RevokeResult { found, durable: true, save_error: None };
        }
        self.revoked_unsaved.insert(device_id.to_string());
        self.pending_ids.insert(device_id.to_string());
        let recorded = match self.write_pending() {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("registry: could not record the revoke in revoked.pending: {e}");
                false
            }
        };
        let list = self.devices.clone();
        match self.commit(list) {
            Ok(()) => RevokeResult { found, durable: true, save_error: None },
            Err(e) => RevokeResult { found, durable: recorded, save_error: Some(e) },
        }
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
        if !self.revoked_unsaved.is_empty() {
            self.revoked_unsaved.clear();
        }
        // Whatever was pending is in the file now. A record that cannot be
        // blanked stays harmless (it only names devices the file lacks) and is
        // logged; `add` keeps it from revoking a device that pairs again.
        if self.pending_written {
            let _ = self.clear_pending();
        }
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
    iso8601_from_secs(secs)
}

/// An ISO 8601 UTC string for `secs` seconds since the Unix epoch.
pub fn iso8601_from_secs(secs: u64) -> String {
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

/// Keep trying to save `registry` every `every` until it works, on a thread of
/// its own. For a revoke whose save failed (see [`DeviceRegistry::remove`]):
/// the device is denied meanwhile. One retry thread per registry.
pub fn spawn_save_retry(
    registry: std::sync::Arc<std::sync::Mutex<DeviceRegistry>>,
    every: std::time::Duration,
) {
    {
        let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        if reg.retry_running {
            return;
        }
        reg.retry_running = true;
    }
    let reg2 = registry.clone();
    let spawned =
        std::thread::Builder::new().name("registry-save-retry".into()).spawn(move || loop {
            std::thread::sleep(every);
            let mut reg = reg2.lock().unwrap_or_else(|e| e.into_inner());
            if !reg.has_unsaved_revokes() {
                reg.retry_running = false;
                break;
            }
            match reg.retry_save() {
                Ok(()) => {
                    tracing::info!("registry: the pending revoke is saved");
                    reg.retry_running = false;
                    break;
                }
                Err(e) => tracing::warn!("registry: save still failing: {e}"),
            }
        });
    if spawned.is_err() {
        registry.lock().unwrap_or_else(|e| e.into_inner()).retry_running = false;
    }
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
        let mut names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        // The revoke record is a fixed file, written in place: no temp names.
        assert_eq!(
            names,
            vec!["authorized_devices.json".to_string(), "revoked.pending".to_string()],
            "{names:?}"
        );
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

    #[test]
    fn a_revoke_that_cannot_be_saved_survives_a_restart_and_is_retried() {
        let key = iroh::SecretKey::from_bytes(&[9u8; 32]);
        let id = key.public();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/authorized_devices.json");
        let mut reg = DeviceRegistry::empty_at(path.clone());
        reg.add(entry(&id.to_string())).unwrap();
        // The next save fails: a non-empty directory sits where the file goes.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        assert!(reg.remove(&id.to_string()).is_err());
        let pending = path.with_file_name("revoked.pending");
        assert!(std::fs::read_to_string(&pending).unwrap().contains(&id.to_string()));
        assert!(reg.has_unsaved_revokes());

        // The process dies and starts again with the old file content on disk.
        let old = serde_json::to_string(&[entry(&id.to_string())]).unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        std::fs::write(&path, old).unwrap();
        let again = DeviceRegistry::load_from(path.clone()).unwrap();
        assert!(!again.is_authorized(&id), "a revoke lost to a failed save came back");
        assert!(
            !std::fs::read_to_string(&pending).unwrap().contains(&id.to_string()),
            "the revoke record stays after a good save"
        );
        assert!(!std::fs::read_to_string(&path).unwrap().contains(&id.to_string()));
        assert!(!again.has_unsaved_revokes());

        // The retry thread saves once the disk works again.
        let mut reg = DeviceRegistry::empty_at(tmp.path().join("r2/authorized_devices.json"));
        reg.add(entry(&id.to_string())).unwrap();
        let p2 = tmp.path().join("r2/authorized_devices.json");
        std::fs::remove_file(&p2).unwrap();
        std::fs::create_dir(&p2).unwrap();
        std::fs::write(p2.join("keep"), b"x").unwrap();
        assert!(reg.remove(&id.to_string()).is_err());
        let reg = std::sync::Arc::new(std::sync::Mutex::new(reg));
        spawn_save_retry(reg.clone(), std::time::Duration::from_millis(50));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(reg.lock().unwrap().has_unsaved_revokes(), "still failing, still denied");
        std::fs::remove_dir_all(&p2).unwrap();
        let t0 = std::time::Instant::now();
        while reg.lock().unwrap().has_unsaved_revokes() {
            assert!(t0.elapsed() < std::time::Duration::from_secs(3), "the retry never saved");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!std::fs::read_to_string(&p2).unwrap().contains(&id.to_string()));
    }

    #[test]
    fn a_revoke_is_durable_only_when_the_record_or_the_registry_file_has_it() {
        let key = iroh::SecretKey::from_bytes(&[11u8; 32]);
        let id = key.public().to_string();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/authorized_devices.json");
        let mut reg = DeviceRegistry::empty_at(path.clone());
        reg.add(entry(&id)).unwrap();
        // The registry file cannot be saved, the record can: durable.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        let out = reg.revoke(&id);
        assert!(out.found && out.save_error.is_some() && out.durable);
        let pending = path.with_file_name("revoked.pending");
        assert!(std::fs::read_to_string(&pending).unwrap().contains(&id));

        // Neither can be written: the revoke holds in memory, not durable.
        reg.add(entry("x")).ok();
        let mut reg2 = DeviceRegistry::empty_at(path.clone());
        reg2.devices.push(entry(&id));
        std::fs::remove_file(&pending).unwrap();
        std::fs::create_dir(&pending).unwrap();
        let out = reg2.revoke(&id);
        assert!(out.found && out.save_error.is_some() && !out.durable);
        let other = iroh::SecretKey::from_bytes(&[11u8; 32]).public();
        assert!(!reg2.is_authorized(&other), "an unrecorded revoke must still deny the device");

        // Only the pending record fails but the registry file saves: durable.
        std::fs::remove_dir_all(&path).unwrap();
        let mut reg3 = DeviceRegistry::empty_at(path.clone());
        reg3.devices.push(entry(&id));
        let out = reg3.revoke(&id);
        assert!(out.found && out.save_error.is_none() && out.durable);
    }

    #[test]
    fn a_device_paired_again_is_not_revoked_by_an_old_record() {
        let id = iroh::SecretKey::from_bytes(&[12u8; 32]).public().to_string();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/authorized_devices.json");
        let mut reg = DeviceRegistry::empty_at(path.clone());
        reg.add(entry(&id)).unwrap();
        // The save fails, so the record keeps the id.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        assert!(reg.remove(&id).is_err());
        let pending = path.with_file_name("revoked.pending");
        assert!(std::fs::read_to_string(&pending).unwrap().contains(&id));
        // The disk works again and the device pairs again.
        std::fs::remove_dir_all(&path).unwrap();
        reg.add(entry(&id)).unwrap();
        assert!(
            !std::fs::read_to_string(&pending).unwrap().contains(&id),
            "the record still names a device that paired again"
        );
        let again = DeviceRegistry::load_from(path).unwrap();
        let key: iroh::EndpointId = id.parse().unwrap();
        assert!(again.is_authorized(&key), "a restart revoked the new pairing");
    }

    #[test]
    fn a_failed_pairing_leaves_the_revoke_in_the_record() {
        let id = iroh::SecretKey::from_bytes(&[13u8; 32]).public().to_string();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/authorized_devices.json");
        let mut reg = DeviceRegistry::empty_at(path.clone());
        reg.add(entry(&id)).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        assert!(reg.remove(&id).is_err());
        // The pairing cannot be saved either.
        assert!(reg.add(entry(&id)).is_err());
        let pending = path.with_file_name("revoked.pending");
        assert!(std::fs::read_to_string(&pending).unwrap().contains(&id));
    }

    #[test]
    fn the_revoke_record_has_room_for_a_thousand_devices_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/authorized_devices.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut reg = DeviceRegistry::load_from(path.clone()).unwrap();
        let ids: Vec<String> = (0..PENDING_IDS)
            .map(|i| {
                let mut b = [0u8; 32];
                b[..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
                iroh::SecretKey::from_bytes(&b).public().to_string()
            })
            .collect();
        for id in &ids {
            reg.pending_ids.insert(id.clone());
        }
        reg.write_pending().unwrap();
        let len = std::fs::metadata(path.with_file_name("revoked.pending")).unwrap().len();
        assert_eq!(len as usize, PENDING_BYTES, "1024 ids must fit without growing the file");
    }

    #[test]
    fn a_loaded_registry_reserves_its_revoke_record() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/authorized_devices.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let _ = DeviceRegistry::load_from(path.clone()).unwrap();
        let len = std::fs::metadata(path.with_file_name("revoked.pending")).unwrap().len();
        assert_eq!(len as usize, PENDING_BYTES);
    }
}
