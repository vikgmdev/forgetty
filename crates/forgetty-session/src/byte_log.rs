//! Per-pane byte log — in-memory ring + on-disk append-only file.
//!
//! V2-007 / AD-013: raw PTY bytes are durably logged to
//! `~/.local/share/forgetty/logs/{pane_uuid}.log` and held in a per-pane
//! in-memory ring buffer. When a client subscribes to a pane's output, the
//! daemon emits the ring contents as binary frames before switching to live
//! output. The client's own VT parser reconstructs the screen.
//!
//! ## Hot-path contract (AD-009)
//!
//! `append(&mut self, data: &[u8])` is called from `SessionManager::process_pane_bytes`
//! while the `SessionManagerInner` mutex is held. It must never block.
//! - Ring write: `VecDeque<u8>::extend` + `pop_front` while over cap. O(data_len).
//! - Disk write: `mpsc::Sender::try_send`. Non-blocking. Drops silently under
//!   sustained burst (SPEC §6 R-3).
//!
//! No timers, no `tokio::time::sleep`, no polling — the disk appender task
//! blocks on `rx.recv().await` until a chunk arrives.

use std::collections::VecDeque;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use bytes::Bytes;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};
use uuid::Uuid;

use forgetty_workspace::{logs_dir, pane_log_path};

/// Bounded channel capacity from `append` → disk appender task (SPEC §4.1).
const DISK_CHANNEL_CAP: usize = 64;

/// Private message type: chunks to append + flush-signal with oneshot reply.
enum DiskMsg {
    Chunk(Bytes),
    Flush(oneshot::Sender<()>),
}

/// Per-pane in-memory ring + async disk appender handle.
///
/// Construction is blocking (synchronous file open + tail read); this matches
/// the blocking `create_pane` call site. Hot-path writes via `append` are
/// non-blocking.
pub struct ByteLog {
    ring: VecDeque<u8>,
    ring_capacity: usize,
    /// Monotonic count of bytes appended since construction, including those
    /// evicted from the ring. Serves as the replay cursor high-water mark.
    total_bytes_written: u64,
    /// Bounded channel to the disk appender task.
    disk_tx: mpsc::Sender<DiskMsg>,
}

impl ByteLog {
    /// Create a new byte log for a pane.
    ///
    /// - `pane_id` — pane UUID, used as the filename.
    /// - `ring_capacity` — in-memory ring size in bytes.
    /// - `max_disk_bytes` — on-disk cap. Rotation keeps newest `max_disk_bytes / 2`.
    ///   `0` disables disk appending (ring-only mode for testing).
    ///
    /// Side effects:
    /// - Creates `~/.local/share/forgetty/logs/` with mode 0700 if absent.
    /// - Opens `~/.local/share/forgetty/logs/{pane_uuid}.log` for append (O_APPEND)
    ///   with mode 0600.
    /// - Deletes any stale `{pane_uuid}.log.tmp` from a crashed rotation (R-6).
    /// - Pre-loads last `min(file_size, ring_capacity)` bytes of the existing log
    ///   into the ring (AC-17 cold-start replay).
    /// - Spawns a tokio task as the disk appender.
    pub fn new(pane_id: Uuid, ring_capacity: usize, max_disk_bytes: u64) -> io::Result<Self> {
        let dir = logs_dir();
        std::fs::create_dir_all(&dir)?;
        // Mode 0700 on the directory (AC-2). Re-applied even if dir existed
        // because `create_dir_all` preserves whatever mode was there.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            // Best-effort: log (but do not fail) if setting perms fails — the
            // ByteLog still functions and the daemon stays alive.
            if let Err(e) = std::fs::set_permissions(&dir, perms) {
                warn!(dir = %dir.display(), "ByteLog::new: failed to chmod 0700: {e}");
            }
        }

        let log_path = pane_log_path(pane_id);
        let tmp_path = log_path.with_extension("log.tmp");

        // R-6: delete stale rotation tmp if present.
        if tmp_path.exists() {
            if let Err(e) = std::fs::remove_file(&tmp_path) {
                warn!(path = %tmp_path.display(), "ByteLog::new: failed to remove stale .tmp: {e}");
            }
        }

        // Pre-load the tail into the ring (AC-17). Must happen before we open
        // in append mode (the seek end gives the right size either way, but
        // reading before open avoids interleaving concerns).
        let mut ring: VecDeque<u8> = VecDeque::with_capacity(ring_capacity);
        let mut total_bytes_written: u64 = 0;
        if log_path.exists() {
            match std::fs::metadata(&log_path) {
                Ok(meta) => {
                    let size = meta.len();
                    total_bytes_written = size;
                    if ring_capacity > 0 && size > 0 {
                        let to_read = std::cmp::min(size as usize, ring_capacity);
                        let mut buf = vec![0u8; to_read];
                        use std::io::{Read, Seek, SeekFrom};
                        let mut f = std::fs::File::open(&log_path)?;
                        f.seek(SeekFrom::End(-(to_read as i64)))?;
                        f.read_exact(&mut buf)?;
                        ring.extend(buf);
                    }
                }
                Err(e) => warn!(path = %log_path.display(), "ByteLog::new: stat failed: {e}"),
            }
        }

        // Open the log file for append. Mode 0600 set at open time via
        // OpenOptionsExt; if the file already existed with different perms,
        // chmod below fixes it. Uses std::fs::OpenOptions so we can apply the
        // mode atomically at creation.
        let std_file =
            std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&log_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = std_file.set_permissions(perms) {
                warn!(path = %log_path.display(), "ByteLog::new: failed to chmod 0600: {e}");
            }
        }
        let tokio_file = tokio::fs::File::from_std(std_file);

        // Bounded disk channel. `try_send` drops chunks when full (R-3).
        let (disk_tx, disk_rx) = mpsc::channel::<DiskMsg>(DISK_CHANNEL_CAP);

        if max_disk_bytes > 0 {
            // Spawn the per-pane disk appender task.
            tokio::spawn(disk_appender_task(
                pane_id,
                log_path.clone(),
                tokio_file,
                total_bytes_written,
                max_disk_bytes,
                disk_rx,
            ));
        } else {
            // Ring-only mode: drop the receiver immediately. Any Chunk sent will
            // fail try_send silently (channel is closed).
            drop(disk_rx);
        }

        debug!(
            pane = %pane_id,
            ring_bytes = ring.len(),
            file_bytes = total_bytes_written,
            "ByteLog::new"
        );

        Ok(Self { ring, ring_capacity, total_bytes_written, disk_tx })
    }

    /// Append raw bytes to the ring and enqueue a disk write.
    ///
    /// Hot path — must never block. Disk queue full → chunk dropped silently.
    pub fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        // Ring write. `VecDeque::extend` is O(data_len); when over cap we
        // drop the oldest bytes.
        self.ring.extend(data.iter().copied());
        if self.ring_capacity == 0 {
            // Edge case: ring disabled. Still count bytes for cursor arithmetic.
            self.ring.clear();
        } else {
            while self.ring.len() > self.ring_capacity {
                self.ring.pop_front();
            }
        }

        self.total_bytes_written = self.total_bytes_written.saturating_add(data.len() as u64);

        // Disk write: non-blocking enqueue. Under burst, try_send drops chunks.
        let _ = self.disk_tx.try_send(DiskMsg::Chunk(Bytes::copy_from_slice(data)));
    }

    /// Return a contiguous copy of the ring plus the cursor high-water mark.
    ///
    /// `&mut self` is required because `VecDeque::make_contiguous` mutates the
    /// deque's internal layout.
    pub fn ring_snapshot(&mut self) -> (Bytes, u64) {
        let slice = self.ring.make_contiguous();
        (Bytes::copy_from_slice(slice), self.total_bytes_written)
    }

    /// Send a flush signal to the disk appender and await its completion.
    ///
    /// Called on clean shutdown / disconnect to ensure the file buffer is
    /// fsync-free-flushed to disk. Safe to call concurrently with `append`.
    pub async fn flush_signal(&self) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.disk_tx
            .send(DiskMsg::Flush(tx))
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        rx.await.map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        Ok(())
    }

    /// Return an owned future that flushes this log's disk buffer.
    ///
    /// Unlike `flush_signal`, the returned future does NOT borrow from `self`,
    /// so it can be collected while holding a synchronous mutex and awaited
    /// later. Used by `SessionManager::flush_all_byte_logs`.
    pub fn make_flush_future(&self) -> impl std::future::Future<Output = io::Result<()>> + 'static {
        let tx_clone = self.disk_tx.clone();
        async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            tx_clone
                .send(DiskMsg::Flush(reply_tx))
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
            reply_rx.await.map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
            Ok(())
        }
    }
}

/// Per-pane disk appender. One tokio task per `ByteLog`.
///
/// Blocks on `rx.recv().await` — no timers. When the sender half of `disk_tx`
/// is dropped (because `ByteLog` is dropped), `recv` returns `None` and the
/// loop exits naturally. That is how `close_pane_byte_log` terminates this task.
async fn disk_appender_task(
    pane_id: Uuid,
    log_path: PathBuf,
    file: tokio::fs::File,
    mut file_size_approx: u64,
    max_disk_bytes: u64,
    mut rx: mpsc::Receiver<DiskMsg>,
) {
    // BufWriter amortises the cost of small PTY chunks — typical chunk is
    // ~65,536 bytes but output can arrive in smaller bursts.
    let mut writer = BufWriter::new(file);

    loop {
        match rx.recv().await {
            None => {
                // Sender dropped — ByteLog is being closed. Flush and exit.
                let _ = writer.flush().await;
                debug!(pane = %pane_id, "disk appender: channel closed, exiting");
                return;
            }
            Some(DiskMsg::Chunk(data)) => {
                if let Err(e) = writer.write_all(&data).await {
                    warn!(pane = %pane_id, "disk appender: write_all failed: {e}");
                    continue;
                }
                file_size_approx = file_size_approx.saturating_add(data.len() as u64);

                if file_size_approx >= max_disk_bytes {
                    // Rotation: keep newest max_disk_bytes / 2 (SPEC §10.6).
                    // Rationale: halving the size avoids immediate re-rotation
                    // on the next write at steady-state.
                    match rotate_log(&log_path, &mut writer, max_disk_bytes / 2).await {
                        Ok(new_size) => {
                            file_size_approx = new_size;
                            debug!(pane = %pane_id, new_size, "disk appender: rotated");
                        }
                        Err(e) => {
                            warn!(pane = %pane_id, "disk appender: rotation failed: {e}");
                            // Keep writing to the oversized log — better than a stall.
                        }
                    }
                }
            }
            Some(DiskMsg::Flush(reply)) => {
                if let Err(e) = writer.flush().await {
                    warn!(pane = %pane_id, "disk appender: flush failed: {e}");
                }
                let _ = reply.send(());
            }
        }
    }
}

/// Rotate the log file: read the last `keep_bytes` bytes, write them to a
/// `.tmp` sibling, rename over the original.
///
/// The appender replaces its own `BufWriter` to point at the freshly-rotated
/// file so subsequent writes hit the truncated file.
async fn rotate_log(
    log_path: &PathBuf,
    writer: &mut BufWriter<tokio::fs::File>,
    keep_bytes: u64,
) -> io::Result<u64> {
    // Flush and close (via drop) the current writer before reading the file.
    writer.flush().await?;

    // Read the last keep_bytes of the existing file.
    let current_size = tokio::fs::metadata(log_path).await?.len();
    let to_keep = std::cmp::min(current_size, keep_bytes);
    let start_offset = current_size.saturating_sub(to_keep);

    let tmp_path = log_path.with_extension("log.tmp");

    // Write newest tail to tmp file.
    {
        let mut src = tokio::fs::File::open(log_path).await?;
        if start_offset > 0 {
            src.seek(tokio::io::SeekFrom::Start(start_offset)).await?;
        }
        // Open tmp with mode 0600 via std then convert to tokio (same pattern
        // as the main log open).
        let std_tmp = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp_path)?;
        let mut dst = tokio::fs::File::from_std(std_tmp);
        // Copy in chunks — avoids loading entire log into memory.
        let mut buf = vec![0u8; 64 * 1024];
        let mut remaining = to_keep;
        use tokio::io::AsyncReadExt;
        while remaining > 0 {
            let want = std::cmp::min(remaining as usize, buf.len());
            let n = src.read(&mut buf[..want]).await?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n]).await?;
            remaining = remaining.saturating_sub(n as u64);
        }
        dst.flush().await?;
        dst.sync_all().await?; // rotation cross-flush: durability on the new tail
    }

    // Rename tmp over original (atomic on same filesystem).
    tokio::fs::rename(&tmp_path, log_path).await?;

    // Re-open the log for append — same flags as ByteLog::new.
    let std_reopened =
        std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(log_path)?;
    let tokio_reopened = tokio::fs::File::from_std(std_reopened);
    *writer = BufWriter::new(tokio_reopened);

    Ok(to_keep)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-5: after 2 × ring_capacity bytes written, ring_snapshot returns
    /// exactly ring_capacity bytes and they are the *newest* bytes (tail).
    #[tokio::test]
    async fn ring_overflow_keeps_newest() {
        let pane_id = Uuid::new_v4();
        // max_disk_bytes = 0 disables disk appender — pure ring test.
        let mut log = ByteLog::new(pane_id, 1024, 0).unwrap();

        let total = 2 * 1024;
        let data: Vec<u8> = (0..total).map(|i| (i % 256) as u8).collect();
        log.append(&data);

        let (snapshot, hwm) = log.ring_snapshot();
        assert_eq!(snapshot.len(), 1024, "ring should be exactly ring_capacity after overflow");
        assert_eq!(hwm, total as u64, "cursor high-water mark = total bytes written");

        // Newest 1024 bytes = bytes[1024..2048].
        let expected_tail: Vec<u8> = data[1024..].to_vec();
        assert_eq!(
            &snapshot[..],
            &expected_tail[..],
            "ring snapshot should equal newest 1024 bytes"
        );

        // Best-effort cleanup of the test log file (no disk writes happened).
        let _ = std::fs::remove_file(pane_log_path(pane_id));
    }

    /// AC-14: single-pass write 100 KiB → ring_snapshot covers all of it (ring
    /// capacity ≥ 100 KiB), and the cursor high-water mark matches total bytes.
    #[tokio::test]
    async fn ring_covers_single_write_under_capacity() {
        let pane_id = Uuid::new_v4();
        let mut log = ByteLog::new(pane_id, 256 * 1024, 0).unwrap();

        let data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        log.append(&data);

        let (snapshot, hwm) = log.ring_snapshot();
        assert_eq!(snapshot.len(), 100_000, "ring holds all bytes when under capacity");
        assert_eq!(hwm, 100_000);
        assert_eq!(&snapshot[..], &data[..]);

        let _ = std::fs::remove_file(pane_log_path(pane_id));
    }

    /// AC-13 replay-cursor invariant: after multiple appends, a subscriber
    /// that calls ring_snapshot gets ALL bytes written up to that instant
    /// (i.e. hwm == ring.len() when under capacity). This is the zero-gap
    /// correctness property the server.rs cursor-skip logic depends on.
    #[tokio::test]
    async fn ring_cursor_matches_snapshot_when_under_capacity() {
        let pane_id = Uuid::new_v4();
        let mut log = ByteLog::new(pane_id, 256 * 1024, 0).unwrap();

        log.append(b"hello ");
        log.append(b"world");
        let (snap, hwm) = log.ring_snapshot();
        assert_eq!(snap.as_ref(), b"hello world");
        assert_eq!(hwm, 11);

        let _ = std::fs::remove_file(pane_log_path(pane_id));
    }

    /// AC-6: after writing > max_disk_bytes, the rotation must trim the file
    /// to `max_disk_bytes / 2`. Writing >= 2x should trigger at least one
    /// rotation; the file must stay ≤ max_disk_bytes + a tolerance band.
    ///
    /// We use small max_disk_bytes (32 KiB) to keep the test fast.
    #[tokio::test]
    async fn disk_rotation_trims_file() {
        let pane_id = Uuid::new_v4();
        let max_disk = 32 * 1024u64;
        let mut log = ByteLog::new(pane_id, 1024, max_disk).unwrap();

        // Write 3x max_disk — guaranteed to trigger rotation at least once.
        let chunk = vec![b'A'; 8 * 1024];
        for _ in 0..12 {
            log.append(&chunk);
        }

        // Flush ensures every queued chunk has been written + rotations complete.
        log.flush_signal().await.unwrap();

        let path = pane_log_path(pane_id);
        let meta = std::fs::metadata(&path).unwrap();
        // Tolerance: one chunk (8 KiB) above max_disk_bytes is acceptable
        // because rotation fires after a write pushes size over the cap.
        assert!(
            meta.len() <= max_disk + 8 * 1024,
            "log should be ≤ max + 1 chunk, got {} (max {})",
            meta.len(),
            max_disk
        );

        let _ = std::fs::remove_file(&path);
    }

    /// AC-17: after ByteLog::new opens an existing log file, the ring is
    /// pre-loaded with the file's tail (up to ring_capacity). This is how
    /// cold-start replay works after daemon restart.
    #[tokio::test]
    async fn cold_start_preloads_disk_tail() {
        let pane_id = Uuid::new_v4();
        let path = pane_log_path(pane_id);

        // Manually create the log file with known contents.
        std::fs::create_dir_all(logs_dir()).unwrap();
        let payload: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        std::fs::write(&path, &payload).unwrap();

        // Open a fresh ByteLog (ring cap 4096 < file size 5000 — tail only).
        let mut log = ByteLog::new(pane_id, 4096, 0).unwrap();

        let (snap, hwm) = log.ring_snapshot();
        assert_eq!(snap.len(), 4096, "ring should be pre-loaded to capacity");
        assert_eq!(hwm, 5000, "cursor should equal file size after preload");
        // Snapshot must equal the last 4096 bytes of payload.
        assert_eq!(&snap[..], &payload[payload.len() - 4096..]);

        let _ = std::fs::remove_file(&path);
    }

    /// R-6: stale `.log.tmp` from a crashed rotation is deleted during new().
    #[tokio::test]
    async fn new_deletes_stale_tmp() {
        let pane_id = Uuid::new_v4();
        let log_path = pane_log_path(pane_id);
        let tmp_path = log_path.with_extension("log.tmp");

        std::fs::create_dir_all(logs_dir()).unwrap();
        std::fs::write(&tmp_path, b"leftover from crash").unwrap();
        assert!(tmp_path.exists());

        let _log = ByteLog::new(pane_id, 1024, 0).unwrap();

        assert!(!tmp_path.exists(), "ByteLog::new must delete stale .log.tmp");

        let _ = std::fs::remove_file(&log_path);
    }
}
