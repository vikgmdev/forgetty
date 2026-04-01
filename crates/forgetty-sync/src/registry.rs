//! Authorized device registry: `~/.local/share/forgetty/authorized_devices.json`.
//!
//! All writes are atomic (write to `.tmp` + rename) per R-4 to prevent
//! corruption on daemon crash mid-write.

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
}

impl DeviceRegistry {
    /// Load the registry from disk, creating an empty one if the file does not
    /// exist. Stale `.tmp` files (crash recovery) are removed on load.
    pub fn load() -> Result<Self, RegistryError> {
        let path = registry_path();

        // Remove stale temp file from a previous crash.
        let tmp = path.with_extension("json.tmp");
        if tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
        }

        let devices = if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            serde_json::from_str(&data)?
        } else {
            Vec::new()
        };

        Ok(Self { path, devices })
    }

    /// Check whether an iroh `EndpointId` is in the authorized list.
    pub fn is_authorized(&self, endpoint_id: &EndpointId) -> bool {
        let id_str = endpoint_id.to_string();
        self.devices.iter().any(|d| d.device_id == id_str)
    }

    /// Add a new device entry and persist atomically.
    pub fn add(&mut self, entry: DeviceEntry) -> Result<(), RegistryError> {
        self.devices.push(entry);
        self.save()
    }

    /// Remove a device by its `device_id` string. Returns `true` if found.
    pub fn remove(&mut self, device_id: &str) -> Result<bool, RegistryError> {
        let before = self.devices.len();
        self.devices.retain(|d| d.device_id != device_id);
        let removed = self.devices.len() < before;
        if removed {
            self.save()?;
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
        for d in &mut self.devices {
            if d.device_id == device_id {
                d.last_seen = Some(now);
                break;
            }
        }
        self.save()
    }

    /// Atomically write the registry to disk (write tmp, then rename).
    fn save(&self) -> Result<(), RegistryError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&self.devices)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)?;
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
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
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
        days -= dy;
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
        days -= dm;
        m += 1;
    }
    (y, m, days + 1)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}
