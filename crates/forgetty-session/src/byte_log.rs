//! Per-pane byte log: in-memory ring + on-disk append-only file, with
//! absolute offsets.
//!
//! V2-007 / AD-013 / PROTOCOL_V2 section 8. Raw PTY bytes are logged to
//! `~/.local/share/forgetty/logs/{pane_uuid}.log` and held in a per-pane
//! in-memory ring. Every byte has an absolute offset in the life of the pane:
//!
//! - `total_written`: offset one past the newest byte.
//! - the ring holds `[total_written - ring.len(), total_written)`.
//! - the file holds `[disk_first, disk_first + file_len)`. `disk_first` is
//!   persisted in the sidecar `{pane_uuid}.log.off` (written when the file is
//!   rotated or restarted), so offsets survive a server restart: at start
//!   `total_written = disk_first + file_len`.
//!
//! Readers (the v2 pane output streams) pull by offset ([`ByteLog::plan`],
//! `SessionManager::read_log`). A slow reader therefore never loses bytes
//! while they are in the ring or the file, and never needs a broadcast
//! subscription.
//!
//! ## Hot-path contract (AD-009)
//!
//! `append` is called from `SessionManager::process_pane_bytes` while the
//! manager mutex is held. It never blocks:
//! - ring write: a slice copy,
//! - disk write: an unbounded queue send, counted in bytes. The drain task
//!   awaits `LogShared::wait_room` before it reads the next chunk from the
//!   PTY. Above `DISK_HIGH_WATER` queued bytes it stops reading, the PTY
//!   fills and the program blocks on write (flow control). A chunk is never
//!   dropped, so the file and the offsets have no holes.
//!
//! No timers, no polling: the appender blocks on `rx.recv().await`, and it
//! flushes at the end of every drain wake (event driven).

use std::collections::VecDeque;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, oneshot, Notify};
use tracing::{debug, warn};
use uuid::Uuid;

use forgetty_workspace::{logs_dir, pane_log_path};

/// Bytes that may wait for the disk appender before the PTY reader stops
/// (flow control). Above this, `LogShared::wait_room` blocks. A program that
/// writes faster than the disk blocks on its PTY write: nothing is dropped.
pub const DISK_HIGH_WATER: u64 = 4 * 1024 * 1024;

/// A pane stream that has not moved for this long no longer holds the PTY back.
const READER_STALL: Duration = Duration::from_secs(2);

/// The appender merges queued chunks up to about this size into one write.
const COALESCE_MAX: usize = 1024 * 1024;

/// The appender publishes the file extent at least this often during a burst,
/// so readers behind the ring can be served from the file while it runs.
const FLUSH_EVERY: u64 = 1024 * 1024;

/// Private message type: chunks to append + flush-signal with oneshot reply.
enum DiskMsg {
    /// `start` is the absolute pane offset of `data[0]`.
    Chunk {
        start: u64,
        data: Bytes,
    },
    Flush(oneshot::Sender<()>),
    /// Restart the file at absolute offset `first` holding `data` (test hook).
    Restart {
        first: u64,
        data: Bytes,
        reply: oneshot::Sender<()>,
    },
}

/// What the disk file holds right now: `[first, flushed_to)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskState {
    /// Absolute offset of the first byte of the file.
    pub first: u64,
    /// Absolute offset one past the last byte written to the file (flushed).
    pub flushed_to: u64,
}

/// State shared between the log, its disk appender and the stream readers.
pub struct LogShared {
    /// Path of the log file.
    pub path: PathBuf,
    /// Whether a disk appender runs for this log.
    pub disk_enabled: bool,
    disk: Mutex<DiskState>,
    /// Woken when bytes were appended.
    pub wake: Notify,
    /// Woken when the disk appender flushed more bytes (or moved the file).
    pub disk_progress: Notify,
    /// Bytes queued for the disk appender and not yet taken by it.
    queued: AtomicU64,
    /// Woken when the appender took a chunk off the queue.
    room: Notify,
    /// Set when the `ByteLog` is dropped (the pane is gone).
    closed: AtomicBool,
    /// Absolute offset one past the newest byte (mirror of the log's total).
    total: AtomicU64,
    /// Ring size in bytes. A live reader may lag at most half of it.
    ring_capacity: usize,
    /// Live pane streams and how far each has read.
    readers: Mutex<Vec<Arc<ReaderPos>>>,
    /// Woken when a reader moved, joined or left.
    reader_progress: Notify,
    /// Zero point of `ReaderPos::moved_ms`.
    started: Instant,
    /// Odd while the appender replaces the file (rotation, restart). A reader
    /// that saw the same even value before and after its read got one file.
    epoch: AtomicU64,
}

/// Held by the appender while it replaces the file. Drop it after the new
/// extent is published with `set_disk`.
struct FileChange<'a>(&'a LogShared);

impl<'a> FileChange<'a> {
    fn begin(shared: &'a LogShared) -> Self {
        shared.epoch.fetch_add(1, Ordering::AcqRel);
        FileChange(shared)
    }
}

impl Drop for FileChange<'_> {
    fn drop(&mut self) {
        self.0.epoch.fetch_add(1, Ordering::AcqRel);
        // Readers that found the file in flux wait for this.
        self.0.disk_progress.notify_waiters();
    }
}

impl LogShared {
    /// Current extent of the file.
    pub fn disk_state(&self) -> DiskState {
        *self.disk.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Register a pane stream that will send from `start`.
    pub fn register_reader(self: &Arc<Self>, start: u64) -> ReaderGuard {
        let pos = Arc::new(ReaderPos {
            next: AtomicU64::new(start),
            moved_ms: AtomicU64::new(self.now_ms()),
        });
        self.readers.lock().unwrap_or_else(|e| e.into_inner()).push(pos.clone());
        self.reader_progress.notify_waiters();
        ReaderGuard { shared: self.clone(), pos }
    }

    /// Is the PTY held back, and until when? `Some(None)`: held, no deadline.
    /// `Some(Some(t))`: held until `t` unless a reader moves first.
    /// `None`: free to read.
    fn held_back(&self) -> Option<Option<Instant>> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        if self.disk_enabled && self.queued.load(Ordering::Acquire) >= DISK_HIGH_WATER {
            return Some(None);
        }
        if self.ring_capacity == 0 {
            return None;
        }
        let total = self.total.load(Ordering::Acquire);
        let limit = (self.ring_capacity / 2) as u64;
        let now = self.now_ms();
        let stall = READER_STALL.as_millis() as u64;
        let mut deadline: Option<u64> = None;
        for r in self.readers.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            let lag = total.saturating_sub(r.next.load(Ordering::Acquire));
            let moved = r.moved_ms.load(Ordering::Acquire);
            if lag > limit && now.saturating_sub(moved) < stall {
                let d = moved + stall;
                deadline = Some(deadline.map_or(d, |x| x.min(d)));
            }
        }
        deadline.map(|d| Some(self.started + Duration::from_millis(d)))
    }

    /// Wait until the PTY may be read: the disk queue is below
    /// [`DISK_HIGH_WATER`] and no live pane stream is more than half a ring
    /// behind. A stream that has not moved for [`READER_STALL`] is not waited
    /// for. Event driven: the appender and the streams wake this, and the
    /// stall limit is one one-shot deadline.
    pub async fn wait_room(&self) {
        loop {
            let room = self.room.notified();
            let moved = self.reader_progress.notified();
            tokio::pin!(room, moved);
            room.as_mut().enable();
            moved.as_mut().enable();
            match self.held_back() {
                None => return,
                Some(None) => tokio::select! { _ = &mut room => {}, _ = &mut moved => {} },
                Some(Some(t)) => tokio::select! {
                    _ = &mut room => {},
                    _ = &mut moved => {},
                    _ = tokio::time::sleep_until(t.into()) => {},
                },
            }
        }
    }

    /// Bytes waiting for the disk appender.
    pub fn queued_bytes(&self) -> u64 {
        self.queued.load(Ordering::Acquire)
    }

    fn set_disk(&self, state: DiskState) {
        *self.disk.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }
}

/// Where one pane stream is in the log.
struct ReaderPos {
    /// Next offset the stream will send.
    next: AtomicU64,
    /// When it last moved, in ms since `LogShared::started`.
    moved_ms: AtomicU64,
}

/// A pane stream registered with its log. While registered, the PTY is not
/// read further than half a ring ahead of the stream, so a live reader is
/// always served from the ring and is not reset. Dropping it unregisters.
pub struct ReaderGuard {
    shared: Arc<LogShared>,
    pos: Arc<ReaderPos>,
}

impl ReaderGuard {
    /// The stream sent bytes up to (not including) `next`.
    pub fn advance(&self, next: u64) {
        self.pos.next.store(next, Ordering::Release);
        self.pos.moved_ms.store(self.shared.now_ms(), Ordering::Release);
        self.shared.reader_progress.notify_waiters();
    }
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.shared
            .readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|r| !Arc::ptr_eq(r, &self.pos));
        self.shared.reader_progress.notify_waiters();
    }
}

/// Where to read the next bytes of a pane log from ([`ByteLog::plan`]).
#[derive(Debug)]
pub enum LogPlan {
    /// Bytes copied out of the ring. `bytes` is never empty.
    Ring(Vec<u8>),
    /// `next` equals `total_written`: wait for [`LogShared::wake`].
    Caught,
    /// `next` is older than the ring: read `[next, next + len)` from the
    /// file, which holds `[disk.first, disk.flushed_to)` right now.
    Disk { disk: DiskState, len: usize },
    /// `next` is older than anything still logged. The oldest byte is `first`.
    Lost { first: u64 },
}

/// Per-pane in-memory ring + async disk appender handle.
///
/// Construction is blocking (synchronous file open + tail read); this matches
/// the blocking `create_pane` call site. Hot-path writes via `append` are
/// non-blocking.
pub struct ByteLog {
    ring: VecDeque<u8>,
    ring_capacity: usize,
    /// Absolute offset one past the newest byte. Includes bytes evicted from
    /// the ring. Serves as the replay cursor high-water mark.
    total_bytes_written: u64,
    /// Queue to the disk appender task. Unbounded, but the PTY reader waits
    /// on `LogShared::wait_room` first, so it holds about `DISK_HIGH_WATER`.
    disk_tx: mpsc::UnboundedSender<DiskMsg>,
    shared: Arc<LogShared>,
}

impl Drop for ByteLog {
    /// Wake the readers of a closed log, so their streams see the pane is gone.
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.room.notify_waiters();
        self.shared.wake.notify_waiters();
    }
}

/// Path of the sidecar that stores the absolute offset of the file's first byte.
pub fn offset_sidecar_path(log_path: &Path) -> PathBuf {
    let mut name = log_path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".off");
    log_path.with_file_name(name)
}

fn read_sidecar(log_path: &Path) -> u64 {
    std::fs::read_to_string(offset_sidecar_path(log_path))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn write_sidecar(log_path: &Path, first: u64) {
    let path = offset_sidecar_path(log_path);
    if let Err(e) = forgetty_core::fsutil::write_private_atomic(&path, first.to_string().as_bytes())
    {
        warn!(path = %path.display(), "byte log: could not write the offset sidecar: {e}");
    }
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
    ///   into the ring (AC-17 cold-start replay) and restores the absolute
    ///   offsets from the sidecar.
    /// - Spawns a tokio task as the disk appender.
    pub fn new(pane_id: Uuid, ring_capacity: usize, max_disk_bytes: u64) -> io::Result<Self> {
        let dir = logs_dir();
        forgetty_core::fsutil::ensure_private_dir(&dir)?;

        let log_path = pane_log_path(pane_id);
        let tmp_path = log_path.with_extension("log.tmp");

        // R-6: delete stale rotation tmp if present.
        if tmp_path.exists() {
            if let Err(e) = std::fs::remove_file(&tmp_path) {
                warn!(path = %tmp_path.display(), "ByteLog::new: failed to remove stale .tmp: {e}");
            }
        }

        // Pre-load the tail into the ring (AC-17).
        let mut ring: VecDeque<u8> = VecDeque::with_capacity(ring_capacity.min(1 << 20));
        let mut file_len: u64 = 0;
        let disk_first = if log_path.exists() { read_sidecar(&log_path) } else { 0 };
        if log_path.exists() {
            match std::fs::metadata(&log_path) {
                Ok(meta) => {
                    let size = meta.len();
                    file_len = size;
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
        let total_bytes_written = disk_first + file_len;

        // Open the log file for append, mode 0600 at creation.
        let std_file =
            std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&log_path)?;
        forgetty_core::fsutil::fix_file_mode(&log_path)?;
        let tokio_file = tokio::fs::File::from_std(std_file);

        let disk_enabled = max_disk_bytes > 0;
        let shared = Arc::new(LogShared {
            path: log_path.clone(),
            disk_enabled,
            disk: Mutex::new(DiskState { first: disk_first, flushed_to: total_bytes_written }),
            wake: Notify::new(),
            disk_progress: Notify::new(),
            queued: AtomicU64::new(0),
            room: Notify::new(),
            closed: AtomicBool::new(false),
            total: AtomicU64::new(total_bytes_written),
            ring_capacity,
            readers: Mutex::new(Vec::new()),
            reader_progress: Notify::new(),
            started: Instant::now(),
            epoch: AtomicU64::new(0),
        });

        // Disk queue. Chunks are never dropped: the caller of `append` waits
        // on `LogShared::wait_room` before it reads more from the PTY.
        let (disk_tx, disk_rx) = mpsc::unbounded_channel::<DiskMsg>();

        if disk_enabled {
            tokio::spawn(disk_appender_task(
                pane_id,
                tokio_file,
                disk_first,
                file_len,
                max_disk_bytes,
                shared.clone(),
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
            file_bytes = file_len,
            disk_first,
            total = total_bytes_written,
            "ByteLog::new"
        );

        Ok(Self { ring, ring_capacity, total_bytes_written, disk_tx, shared })
    }

    /// Append raw bytes to the ring and enqueue a disk write.
    ///
    /// Hot path, never blocks and never drops. The disk queue grows past
    /// `DISK_HIGH_WATER` only by what callers append without waiting.
    pub fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let start = self.total_bytes_written;
        if self.ring_capacity == 0 {
            self.ring.clear();
        } else if data.len() >= self.ring_capacity {
            self.ring.clear();
            self.ring.extend(&data[data.len() - self.ring_capacity..]);
        } else {
            self.ring.extend(data);
            let excess = self.ring.len().saturating_sub(self.ring_capacity);
            if excess > 0 {
                self.ring.drain(..excess);
            }
        }

        self.total_bytes_written = start.saturating_add(data.len() as u64);
        self.shared.total.store(self.total_bytes_written, Ordering::Release);

        if self.shared.disk_enabled {
            let len = data.len() as u64;
            self.shared.queued.fetch_add(len, Ordering::AcqRel);
            if self
                .disk_tx
                .send(DiskMsg::Chunk { start, data: Bytes::copy_from_slice(data) })
                .is_err()
            {
                self.shared.queued.fetch_sub(len, Ordering::AcqRel);
            }
        }
        self.shared.wake.notify_waiters();
    }

    /// Shared state for readers: wake-ups and the file extent.
    pub fn shared(&self) -> Arc<LogShared> {
        self.shared.clone()
    }

    /// Absolute offset one past the newest byte.
    pub fn total_written(&self) -> u64 {
        self.total_bytes_written
    }

    /// Absolute offset of the first byte in the ring.
    pub fn ring_first(&self) -> u64 {
        self.total_bytes_written - self.ring.len() as u64
    }

    /// Absolute offset of the oldest byte still readable (ring or file).
    pub fn first_available(&self) -> u64 {
        let ring_first = self.ring_first();
        if self.shared.disk_enabled {
            ring_first.min(self.shared.disk_state().first)
        } else {
            ring_first
        }
    }

    /// Decide how to read up to `max` bytes at absolute offset `next`.
    pub fn plan(&self, next: u64, max: usize) -> LogPlan {
        let total = self.total_bytes_written;
        if next >= total {
            return LogPlan::Caught;
        }
        let ring_first = self.ring_first();
        if next >= ring_first {
            let from = (next - ring_first) as usize;
            let take = (self.ring.len() - from).min(max);
            let (a, b) = self.ring.as_slices();
            let mut out = Vec::with_capacity(take);
            if from < a.len() {
                let end = (from + take).min(a.len());
                out.extend_from_slice(&a[from..end]);
                if out.len() < take {
                    out.extend_from_slice(&b[..take - out.len()]);
                }
            } else {
                let bfrom = from - a.len();
                out.extend_from_slice(&b[bfrom..bfrom + take]);
            }
            return LogPlan::Ring(out);
        }
        // Older than the ring: the file, if it still reaches back that far.
        if self.shared.disk_enabled {
            let disk = self.shared.disk_state();
            if next >= disk.first {
                let len = (ring_first - next).min(max as u64) as usize;
                return LogPlan::Disk { disk, len };
            }
            return LogPlan::Lost { first: disk.first.min(ring_first) };
        }
        LogPlan::Lost { first: ring_first }
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
    /// flushed and fsynced. Safe to call concurrently with `append`.
    pub async fn flush_signal(&self) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.disk_tx
            .send(DiskMsg::Flush(tx))
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
                .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
            reply_rx.await.map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
            Ok(())
        }
    }

    /// Test hook: drop log bytes before `offset` (ring and file). The log then
    /// starts at `offset`, rounded up to the ring start when the ring cannot
    /// go further back. The ring edit happens now. The returned future finishes
    /// the file edit and does not borrow `self`, so the caller can await it
    /// after releasing its lock.
    pub fn begin_truncate_before(
        &mut self,
        offset: u64,
    ) -> impl std::future::Future<Output = io::Result<()>> + 'static {
        let offset = offset.min(self.total_bytes_written);
        let drop_n = offset.saturating_sub(self.ring_first()).min(self.ring.len() as u64) as usize;
        self.ring.drain(..drop_n);
        let new_first = self.ring_first();
        let data = Bytes::from(self.ring.iter().copied().collect::<Vec<u8>>());
        let tx = self.disk_tx.clone();
        let enabled = self.shared.disk_enabled;
        let shared = self.shared.clone();
        async move {
            if enabled {
                let (reply, rx) = oneshot::channel();
                tx.send(DiskMsg::Restart { first: new_first, data, reply })
                    .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
                rx.await.map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
            }
            shared.wake.notify_waiters();
            Ok(())
        }
    }
}

/// Read `len` bytes at absolute offset `next` from the log file described by
/// `disk`. Returns `Ok(None)` when the file moved while reading (rotation or
/// restart): the caller plans again.
pub async fn read_disk(
    shared: &LogShared,
    disk: DiskState,
    next: u64,
    len: usize,
) -> io::Result<Option<Vec<u8>>> {
    use tokio::io::AsyncReadExt;
    let epoch = shared.epoch.load(Ordering::Acquire);
    if epoch & 1 == 1 {
        return Ok(None);
    }
    let avail = disk.flushed_to.saturating_sub(next);
    let want = (len as u64).min(avail) as usize;
    if want == 0 {
        return Ok(Some(Vec::new()));
    }
    let mut f = tokio::fs::File::open(&shared.path).await?;
    f.seek(tokio::io::SeekFrom::Start(next - disk.first)).await?;
    let mut buf = vec![0u8; want];
    f.read_exact(&mut buf).await?;
    // The file may have been rotated between the state read and the read.
    if shared.epoch.load(Ordering::Acquire) != epoch || shared.disk_state().first != disk.first {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// Per-pane disk appender. One tokio task per `ByteLog`.
///
/// Blocks on `rx.recv().await` — no timers. When the sender half of `disk_tx`
/// is dropped (because `ByteLog` is dropped), `recv` returns `None` and the
/// loop exits naturally. That is how `close_pane_byte_log` terminates this task.
async fn disk_appender_task(
    pane_id: Uuid,
    file: tokio::fs::File,
    mut file_first: u64,
    mut file_len: u64,
    max_disk_bytes: u64,
    shared: Arc<LogShared>,
    mut rx: mpsc::UnboundedReceiver<DiskMsg>,
) {
    // BufWriter amortises the cost of small PTY chunks.
    let mut writer = BufWriter::new(file);
    let log_path = shared.path.clone();
    let mut unflushed: u64 = 0;

    // A message read while coalescing chunks, handled on the next turn.
    let mut stash: Option<DiskMsg> = None;

    loop {
        let msg = match stash.take() {
            Some(m) => Some(m),
            None => rx.recv().await,
        };
        match msg {
            None => {
                // Sender dropped — ByteLog is being closed. Flush and exit.
                let _ = writer.flush().await;
                debug!(pane = %pane_id, "disk appender: channel closed, exiting");
                return;
            }
            Some(DiskMsg::Chunk { start, data }) => {
                shared.queued.fetch_sub(data.len() as u64, Ordering::AcqRel);
                // Take the chunks that are already queued and contiguous, and
                // write them with one call. One 64 KiB write per PTY read costs
                // a blocking-pool round trip each.
                let mut data = data;
                if data.len() < COALESCE_MAX {
                    let mut merged: Option<Vec<u8>> = None;
                    let mut end = start + data.len() as u64;
                    let mut size = data.len();
                    while size < COALESCE_MAX {
                        match rx.try_recv() {
                            Ok(DiskMsg::Chunk { start: s2, data: d2 }) if s2 == end => {
                                shared.queued.fetch_sub(d2.len() as u64, Ordering::AcqRel);
                                let m = merged.get_or_insert_with(|| {
                                    let mut v = Vec::with_capacity(COALESCE_MAX + 65536);
                                    v.extend_from_slice(&data);
                                    v
                                });
                                m.extend_from_slice(&d2);
                                end += d2.len() as u64;
                                size += d2.len();
                            }
                            Ok(other) => {
                                stash = Some(other);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    if let Some(m) = merged {
                        data = Bytes::from(m);
                    }
                }
                shared.room.notify_waiters();
                if start != file_first + file_len {
                    // The file was restarted or the log jumped: the file
                    // would have a hole. Start it over at
                    // this chunk so it stays one contiguous range.
                    let _ = writer.flush().await;
                    let _change = FileChange::begin(&shared);
                    match restart_file(&log_path).await {
                        Ok(f) => {
                            writer = BufWriter::new(f);
                            file_first = start;
                            file_len = 0;
                            write_sidecar(&log_path, start);
                            shared.set_disk(DiskState { first: start, flushed_to: start });
                            warn!(pane = %pane_id, start, "disk appender: gap in the log, file restarted");
                        }
                        Err(e) => {
                            warn!(pane = %pane_id, "disk appender: restart failed: {e}");
                            continue;
                        }
                    }
                }
                if let Err(e) = writer.write_all(&data).await {
                    warn!(pane = %pane_id, "disk appender: write_all failed: {e}");
                    continue;
                }
                file_len = file_len.saturating_add(data.len() as u64);
                unflushed += data.len() as u64;

                // End of this drain wake: nothing else is queued, so hand the
                // buffered bytes to the kernel now. A kill -9 or a crash then
                // loses nothing the pane already printed (the page cache
                // outlives the process). Event driven, no timer (AD-009). A
                // burst is written as one flush at its end.
                if shared.queued.load(Ordering::Acquire) == 0 || unflushed >= FLUSH_EVERY {
                    unflushed = 0;
                    if let Err(e) = writer.flush().await {
                        warn!(pane = %pane_id, "disk appender: flush failed: {e}");
                    } else {
                        shared.set_disk(DiskState {
                            first: file_first,
                            flushed_to: file_first + file_len,
                        });
                        shared.disk_progress.notify_waiters();
                    }
                }

                if file_len >= max_disk_bytes {
                    // Rotation: keep newest max_disk_bytes / 2 (SPEC §10.6).
                    let _change = FileChange::begin(&shared);
                    match rotate_log(&log_path, &mut writer, max_disk_bytes / 2).await {
                        Ok(kept) => {
                            let new_first = file_first + (file_len - kept);
                            file_first = new_first;
                            file_len = kept;
                            write_sidecar(&log_path, new_first);
                            shared.set_disk(DiskState {
                                first: new_first,
                                flushed_to: new_first + kept,
                            });
                            shared.disk_progress.notify_waiters();
                            debug!(pane = %pane_id, new_first, kept, "disk appender: rotated");
                        }
                        Err(e) => {
                            warn!(pane = %pane_id, "disk appender: rotation failed: {e}");
                            // Keep writing to the oversized log — better than a stall.
                        }
                    }
                }
            }
            Some(DiskMsg::Restart { first, data, reply }) => {
                let _ = writer.flush().await;
                let _change = FileChange::begin(&shared);
                match restart_file(&log_path).await {
                    Ok(f) => {
                        writer = BufWriter::new(f);
                        file_first = first;
                        file_len = 0;
                        write_sidecar(&log_path, first);
                        if !data.is_empty() && writer.write_all(&data).await.is_ok() {
                            file_len = data.len() as u64;
                        }
                        let _ = writer.flush().await;
                        shared.set_disk(DiskState { first, flushed_to: first + file_len });
                        shared.disk_progress.notify_waiters();
                    }
                    Err(e) => warn!(pane = %pane_id, "disk appender: restart failed: {e}"),
                }
                let _ = reply.send(());
            }
            Some(DiskMsg::Flush(reply)) => {
                // Explicit flush (shutdown, disconnect): also fsync, so the log
                // survives a power cut, not just a process crash.
                if let Err(e) = writer.flush().await {
                    warn!(pane = %pane_id, "disk appender: flush failed: {e}");
                }
                if let Err(e) = writer.get_ref().sync_data().await {
                    warn!(pane = %pane_id, "disk appender: fsync failed: {e}");
                }
                shared.set_disk(DiskState { first: file_first, flushed_to: file_first + file_len });
                shared.disk_progress.notify_waiters();
                let _ = reply.send(());
            }
        }
    }
}

/// Truncate the log file and reopen it for append. Used when the log restarts
/// at a new absolute offset.
async fn restart_file(log_path: &Path) -> io::Result<tokio::fs::File> {
    let std_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(log_path)?;
    drop(std_file);
    let std_file =
        std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(log_path)?;
    Ok(tokio::fs::File::from_std(std_file))
}

/// Rotate the log file: read the last `keep_bytes` bytes, write them to a
/// `.tmp` sibling, rename over the original. Returns the number of bytes kept.
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
        let mut buf = vec![0u8; 1024 * 1024];
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

    /// P1-QA item 4: bytes reach the file at the end of a drain wake, with no
    /// explicit flush and no close. Before the fix they sat in the BufWriter
    /// until a signal or a close, so a crash lost the scrollback.
    #[tokio::test]
    async fn bytes_reach_the_file_without_an_explicit_flush() {
        let pane_id = Uuid::new_v4();
        let mut log = ByteLog::new(pane_id, 1024, 1024 * 1024).unwrap();
        log.append(b"echo MARK_42\r\n");
        let path = pane_log_path(pane_id);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let got = std::fs::read(&path).unwrap_or_default();
            if got == b"echo MARK_42\r\n" {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "file holds {got:?}");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
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

    fn cleanup(pane_id: Uuid) {
        let path = pane_log_path(pane_id);
        let _ = std::fs::remove_file(offset_sidecar_path(&path));
        let _ = std::fs::remove_file(&path);
    }

    /// Offsets are absolute: after rotation the file starts at a later offset,
    /// the sidecar stores it, and a reopen restores `total_written`.
    #[tokio::test]
    async fn offsets_survive_rotation_and_reopen() {
        let pane_id = Uuid::new_v4();
        let max_disk = 32 * 1024u64;
        let mut log = ByteLog::new(pane_id, 1024, max_disk).unwrap();
        let chunk: Vec<u8> = (0..8 * 1024).map(|i| (i % 251) as u8).collect();
        for _ in 0..12 {
            log.append(&chunk);
        }
        log.flush_signal().await.unwrap();
        let total = log.total_written();
        assert_eq!(total, 12 * 8 * 1024);
        let disk = log.shared().disk_state();
        assert!(disk.first > 0, "rotation moved the first offset");
        assert_eq!(disk.flushed_to, total);
        let on_disk = std::fs::read(pane_log_path(pane_id)).unwrap();
        assert_eq!(disk.first + on_disk.len() as u64, total);
        let sidecar =
            std::fs::read_to_string(offset_sidecar_path(&pane_log_path(pane_id))).unwrap();
        assert_eq!(sidecar.trim().parse::<u64>().unwrap(), disk.first);
        drop(log);

        let log2 = ByteLog::new(pane_id, 1024, max_disk).unwrap();
        assert_eq!(log2.total_written(), total, "total survives a restart");
        assert_eq!(log2.ring_first(), total - 1024);
        assert_eq!(log2.first_available(), disk.first);
        cleanup(pane_id);
    }

    /// `plan` covers ring, disk, caught-up and lost.
    #[tokio::test]
    async fn plan_reads_ring_then_disk_then_lost() {
        let pane_id = Uuid::new_v4();
        let max_disk = 32 * 1024u64;
        let mut log = ByteLog::new(pane_id, 1024, max_disk).unwrap();
        let data: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
        for c in data.chunks(4000) {
            log.append(c);
        }
        log.flush_signal().await.unwrap();
        let total = log.total_written();
        assert_eq!(total, 40_000);
        assert!(matches!(log.plan(total, 100), LogPlan::Caught));
        match log.plan(total - 10, 100) {
            LogPlan::Ring(b) => assert_eq!(b, data[39_990..]),
            other => panic!("{other:?}"),
        }
        let disk = log.shared().disk_state();
        match log.plan(disk.first + 5, 100) {
            LogPlan::Disk { disk: d, len } => {
                assert_eq!(len, 100);
                let got = read_disk(&log.shared(), d, disk.first + 5, len).await.unwrap().unwrap();
                assert_eq!(got, data[(disk.first + 5) as usize..(disk.first + 105) as usize]);
            }
            other => panic!("{other:?}"),
        }
        match log.plan(0, 100) {
            LogPlan::Lost { first } => assert_eq!(first, disk.first),
            other => panic!("{other:?}"),
        }
        cleanup(pane_id);
    }

    /// A chunk that never reached the disk queue leaves a hole. The appender
    /// restarts the file at the next chunk, so the file stays contiguous.
    #[tokio::test]
    async fn dropped_chunk_restarts_the_file_at_the_new_offset() {
        let pane_id = Uuid::new_v4();
        let mut log = ByteLog::new(pane_id, 4096, 1024 * 1024).unwrap();
        log.append(b"aaaa");
        log.flush_signal().await.unwrap();
        // Simulate a dropped chunk: advance the offset without queueing.
        log.total_bytes_written += 6;
        log.append(b"bbbb");
        log.flush_signal().await.unwrap();
        let disk = log.shared().disk_state();
        assert_eq!(disk, DiskState { first: 10, flushed_to: 14 });
        assert_eq!(std::fs::read(pane_log_path(pane_id)).unwrap(), b"bbbb");
        cleanup(pane_id);
    }

    /// A burst larger than the high-water mark is written whole: `wait_room`
    /// holds the reader back, nothing is dropped, the file has no holes.
    #[tokio::test]
    async fn a_burst_over_the_high_water_mark_is_lossless() {
        let pane_id = Uuid::new_v4();
        let mut log = ByteLog::new(pane_id, 1024 * 1024, 64 * 1024 * 1024).unwrap();
        let shared = log.shared();
        let chunk: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
        let mut waited = false;
        for _ in 0..200 {
            // 12.5 MiB
            if shared.queued_bytes() >= DISK_HIGH_WATER {
                waited = true;
            }
            shared.wait_room().await;
            log.append(&chunk);
        }
        assert!(waited, "the queue never reached the high-water mark");
        log.flush_signal().await.unwrap();
        let total = 200 * chunk.len() as u64;
        assert_eq!(log.total_written(), total);
        assert_eq!(shared.disk_state(), DiskState { first: 0, flushed_to: total });
        let file = std::fs::read(pane_log_path(pane_id)).unwrap();
        assert_eq!(file.len() as u64, total);
        assert!(file.chunks(chunk.len()).all(|c| c == &chunk[..]));
        cleanup(pane_id);
    }

    /// A live reader more than half a ring behind holds the PTY back until it
    /// moves, leaves, or stops moving for READER_STALL.
    #[tokio::test]
    async fn a_lagging_reader_holds_the_pty_back() {
        use std::time::Duration;
        let pane_id = Uuid::new_v4();
        let mut log = ByteLog::new(pane_id, 64 * 1024, 64 * 1024 * 1024).unwrap();
        let shared = log.shared();
        let reader = shared.register_reader(0);
        log.append(&vec![1u8; 48 * 1024]); // 48 KiB ahead, limit is 32 KiB
        let held = |s: Arc<LogShared>| async move {
            tokio::time::timeout(Duration::from_millis(150), s.wait_room()).await.is_err()
        };
        assert!(held(shared.clone()).await, "a lagging reader must hold the PTY");
        reader.advance(24 * 1024); // now 24 KiB behind
        assert!(!held(shared.clone()).await, "a reader within the limit must not");
        reader.advance(0);
        assert!(held(shared.clone()).await);
        // A reader that stops moving is not waited for.
        let t0 = std::time::Instant::now();
        shared.wait_room().await;
        assert!(t0.elapsed() >= READER_STALL - Duration::from_millis(300));
        // A reader that leaves frees the PTY at once.
        reader.advance(0);
        let s2 = shared.clone();
        let waiter = tokio::spawn(async move { s2.wait_room().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(reader);
        tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
        cleanup(pane_id);
    }

    /// The test hook drops old bytes and the log then starts at the offset.
    #[tokio::test]
    async fn truncate_before_moves_the_first_offset() {
        let pane_id = Uuid::new_v4();
        let mut log = ByteLog::new(pane_id, 4096, 1024 * 1024).unwrap();
        log.append(b"0123456789");
        log.flush_signal().await.unwrap();
        log.begin_truncate_before(4).await.unwrap();
        assert_eq!(log.first_available(), 4);
        assert!(matches!(log.plan(2, 10), LogPlan::Lost { first: 4 }));
        match log.plan(4, 100) {
            LogPlan::Ring(b) => assert_eq!(b, b"456789"),
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(pane_log_path(pane_id)).unwrap(), b"456789");
        cleanup(pane_id);
    }
}
