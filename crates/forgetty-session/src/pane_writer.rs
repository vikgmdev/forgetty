//! Per-pane input queue and writer thread.
//!
//! A write to a PTY blocks when the program does not read (`sleep infinity`,
//! or a program stuck on its own output). Such a write must never hold a lock
//! another pane, client or control message needs. Each pane therefore owns a
//! bounded byte queue and one thread that does the real `write`. Callers put
//! bytes in the queue and either wait for the real write
//! ([`PaneWriter::write`], used where an ack follows) or only for room
//! ([`PaneWriter::queue`]). A full queue costs the caller's own task time and
//! nothing else.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, Notify};

/// Bytes a pane queues (the job being written counts) before a writer has to
/// wait. One job larger than this is accepted when nothing is queued, so a big
/// paste is never refused.
pub const INPUT_QUEUE_BYTES: usize = 256 * 1024;

/// Why bytes could not be queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    /// The queue is over its byte cap right now.
    Full,
    /// The pane is gone or its PTY refused a write.
    Closed,
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::Full => write!(f, "pane input queue full"),
            QueueError::Closed => write!(f, "pane input closed"),
        }
    }
}

/// Runs when a job leaves the queue for good: written, discarded, or given up
/// because the pane is gone. A job still queued when the process exits never
/// runs it.
pub type OnEnd = Box<dyn FnOnce() + Send>;

struct Job {
    data: Vec<u8>,
    done: Option<oneshot::Sender<Result<(), String>>>,
    on_end: Option<OnEnd>,
}

impl Job {
    fn new(data: Vec<u8>) -> Self {
        Job { data, done: None, on_end: None }
    }

    /// The job is over. Called outside the state lock.
    fn end(mut self) {
        if let Some(f) = self.on_end.take() {
            f();
        }
    }
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    bytes: usize,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    work: Condvar,
    space: Notify,
    /// Pulsed after every finished job and on close. Blocking callers wait
    /// on it for room ([`PaneWriter::queue_blocking`]) or for the drain
    /// ([`PaneWriter::close_and_drain`]).
    progress: Condvar,
    /// Set when the pane is gone: an interrupted write gives up.
    abort: AtomicBool,
    /// Bumped by [`PaneWriter::discard_pending`]. A job under way when it
    /// changes stops writing what is left of it.
    generation: AtomicU64,
    /// The writer thread has returned.
    exited: AtomicBool,
    /// Clock for the two stamps below (milliseconds since this instant).
    base: Instant,
    /// When the write under way began, `IDLE` while the thread is not
    /// inside a job.
    write_since_ms: AtomicU64,
    /// Last sign of life: bytes taken by the PTY, a job finished, or the
    /// queue went from empty to not empty.
    progress_ms: AtomicU64,
    /// The writer thread, while it runs, so a pane that closes can interrupt a
    /// write that is blocked on a program that never reads.
    #[cfg(unix)]
    thread: Mutex<Option<libc::pthread_t>>,
    #[cfg(windows)]
    thread: Mutex<Option<winthread::ThreadHandle>>,
}

/// `Shared::write_since_ms` while no write is under way.
const IDLE: u64 = u64::MAX;

/// What a pane's input queue and writer thread are doing (watchdog, `qa_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterStats {
    /// Bytes queued, the write under way included.
    pub queued_bytes: usize,
    /// Frames queued, the write under way included.
    pub frames: usize,
    /// How long the thread has been inside `write()` for the current job.
    /// `None`: it is not writing.
    pub in_write: Option<Duration>,
    /// Time since the PTY last took bytes, a job ended, or the queue got its
    /// first frame.
    pub since_progress: Duration,
    /// The writer thread has returned.
    pub exited: bool,
}

impl WriterStats {
    /// Bytes wait and nothing moved for `limit`.
    pub fn stalled(&self, limit: Duration) -> bool {
        self.queued_bytes > 0 && self.since_progress >= limit
    }

    /// The writer, in words.
    pub fn writer_state(&self) -> String {
        if self.exited {
            "writer thread exited".to_string()
        } else if let Some(d) = self.in_write {
            format!("writer blocked in write() for {:.1} s", d.as_secs_f64())
        } else if self.queued_bytes > 0 {
            "writer idle with the queue not empty (wakeup bug)".to_string()
        } else {
            "writer idle".to_string()
        }
    }
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.base.elapsed().as_millis() as u64
    }

    fn touch(&self) {
        self.progress_ms.store(self.now_ms(), Ordering::SeqCst);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn try_push(&self, job: Job) -> Result<(), (QueueError, Job)> {
        let mut st = self.lock();
        if st.closed {
            return Err((QueueError::Closed, job));
        }
        if st.bytes > 0 && st.bytes + job.data.len() > INPUT_QUEUE_BYTES {
            return Err((QueueError::Full, job));
        }
        if st.bytes == 0 {
            self.touch();
        }
        st.bytes += job.data.len();
        st.queue.push_back(job);
        drop(st);
        self.work.notify_one();
        Ok(())
    }
}

impl Shared {
    /// The writer thread is done.
    fn finish(&self) {
        #[cfg(any(unix, windows))]
        {
            *self.thread.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        self.exited.store(true, Ordering::SeqCst);
        self.progress.notify_all();
    }
}

/// Write all of `data` and flush. Unlike `write_all`, an interrupted write
/// ends the job when the pane is gone (`abort`), so a thread blocked on a
/// program that never reads can be woken and stopped.
fn write_job(
    out: &mut dyn Write,
    data: &[u8],
    abort: &AtomicBool,
    generation: &AtomicU64,
    started_at: u64,
    progress: &dyn Fn(),
) -> Result<(), String> {
    let mut off = 0;
    while off < data.len() {
        // The input was discarded (an interrupt): what is left of this job is
        // stale and must not reach the program.
        if generation.load(Ordering::SeqCst) != started_at {
            return Ok(());
        }
        match out.write(&data[off..]) {
            Ok(0) => return Err("write returned 0".to_string()),
            Ok(n) => {
                off += n;
                progress();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                if abort.load(Ordering::SeqCst) {
                    return Err("pane closed".to_string());
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    loop {
        match out.flush() {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                if abort.load(Ordering::SeqCst) {
                    return Err("pane closed".to_string());
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// A signal handler that does nothing, installed without `SA_RESTART`: the
/// signal only makes a blocked `write` return `EINTR`.
#[cfg(unix)]
fn install_wake_handler() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    extern "C" fn nop(_: libc::c_int) {}
    ONCE.call_once(|| {
        // SAFETY: a zeroed sigaction with a valid handler and an empty mask.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = nop as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            libc::sigaction(WAKE_SIGNAL, &sa, std::ptr::null_mut());
        }
    });
}

#[cfg(unix)]
const WAKE_SIGNAL: libc::c_int = libc::SIGUSR1;

/// Make a `write` that is blocked in `t` return as interrupted.
#[cfg(unix)]
fn wake_thread(t: &libc::pthread_t) {
    // SAFETY: `t` is the id of the live writer thread (the caller holds the lock that
    // clears it before the thread returns).
    unsafe { libc::pthread_kill(*t, WAKE_SIGNAL) };
}

#[cfg(windows)]
fn wake_thread(t: &winthread::ThreadHandle) {
    t.interrupt();
}

/// The writer thread on Windows: a real handle (the pseudo handle of `GetCurrentThread` means
/// "the calling thread" for whoever uses it) so another thread can end its blocked `WriteFile`
/// with `CancelSynchronousIo`. The write then fails with `ERROR_OPERATION_ABORTED`, which the PTY
/// writer reports as `Interrupted`.
#[cfg(windows)]
mod winthread {
    use windows::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
    use windows::Win32::System::IO::CancelSynchronousIo;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};

    pub struct ThreadHandle(HANDLE);

    // SAFETY: a kernel handle value; the calls made with it are thread safe.
    unsafe impl Send for ThreadHandle {}

    impl ThreadHandle {
        /// A handle to the calling thread, or `None` if Windows refuses to duplicate it.
        pub fn current() -> Option<Self> {
            let mut h = HANDLE::default();
            // SAFETY: the pseudo handles are valid for the calling thread and process.
            unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    GetCurrentThread(),
                    GetCurrentProcess(),
                    &mut h,
                    0,
                    false,
                    DUPLICATE_SAME_ACCESS,
                )
            }
            .ok()
            .map(|()| Self(h))
        }

        /// End the synchronous I/O the thread is blocked in, if any.
        pub fn interrupt(&self) {
            // SAFETY: `self.0` is a live thread handle. "No I/O to cancel" is an error we ignore.
            unsafe {
                let _ = CancelSynchronousIo(self.0);
            }
        }
    }

    impl Drop for ThreadHandle {
        fn drop(&mut self) {
            // SAFETY: closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Owner of a pane's writer thread. Dropping it closes the queue and ends the
/// thread (once the write it may be stuck in returns).
pub struct PaneWriterOwner {
    shared: Arc<Shared>,
}

/// A cheap handle to a pane's input queue.
#[derive(Clone)]
pub struct PaneWriter {
    shared: Arc<Shared>,
}

impl PaneWriterOwner {
    /// Start the writer thread for `out`.
    pub fn spawn(mut out: Box<dyn Write + Send>) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            work: Condvar::new(),
            space: Notify::new(),
            progress: Condvar::new(),
            abort: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            exited: AtomicBool::new(false),
            base: Instant::now(),
            write_since_ms: AtomicU64::new(IDLE),
            progress_ms: AtomicU64::new(0),
            #[cfg(any(unix, windows))]
            thread: Mutex::new(None),
        });
        let s = shared.clone();
        std::thread::Builder::new().name("session-pty-writer".to_string()).spawn(move || {
            #[cfg(unix)]
            {
                install_wake_handler();
                // SAFETY: pthread_self has no preconditions.
                *s.thread.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(unsafe { libc::pthread_self() });
            }
            #[cfg(windows)]
            {
                *s.thread.lock().unwrap_or_else(|e| e.into_inner()) = winthread::ThreadHandle::current();
            }
            loop {
                let job = {
                    let mut st = s.lock();
                    loop {
                        if let Some(job) = st.queue.pop_front() {
                            break job;
                        }
                        if st.closed {
                            drop(st);
                            s.finish();
                            return;
                        }
                        st = s.work.wait(st).unwrap_or_else(|e| e.into_inner());
                    }
                };
                // The real write. It may block for as long as the program
                // does not read; nothing else waits on it. A pane that closes
                // interrupts it (see `PaneWriterOwner::drop`).
                let started_at = s.generation.load(Ordering::SeqCst);
                s.write_since_ms.store(s.now_ms(), Ordering::SeqCst);
                let res = write_job(
                    &mut *out,
                    &job.data,
                    &s.abort,
                    &s.generation,
                    started_at,
                    &|| s.touch(),
                );
                s.write_since_ms.store(IDLE, Ordering::SeqCst);
                crate::latency_trace::written();
                s.touch();
                let failed = res.is_err();
                let mut ended = Vec::new();
                {
                    let mut st = s.lock();
                    st.bytes = st.bytes.saturating_sub(job.data.len());
                    if failed {
                        // The PTY is gone: refuse new bytes, fail the rest.
                        st.closed = true;
                        for mut rest in st.queue.drain(..) {
                            if let Some(d) = rest.done.take() {
                                let _ = d.send(Err("pane input closed".to_string()));
                            }
                            ended.push(rest);
                        }
                        st.bytes = 0;
                    }
                }
                let Job { done, on_end, .. } = job;
                if let Some(d) = done {
                    let _ = d.send(res);
                }
                if let Some(f) = on_end {
                    f();
                }
                for rest in ended {
                    rest.end();
                }
                s.space.notify_waiters();
                s.progress.notify_all();
                if failed {
                    s.finish();
                    return;
                }
            }
        })?;
        Ok(Self { shared })
    }

    /// A handle for callers.
    pub fn handle(&self) -> PaneWriter {
        PaneWriter { shared: self.shared.clone() }
    }
}

impl Drop for PaneWriterOwner {
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        st.closed = true;
        let mut ended = Vec::new();
        for mut rest in st.queue.drain(..) {
            if let Some(d) = rest.done.take() {
                let _ = d.send(Err("pane closed".to_string()));
            }
            ended.push(rest);
        }
        st.bytes = 0;
        drop(st);
        for rest in ended {
            rest.end();
        }
        self.shared.abort.store(true, Ordering::SeqCst);
        self.shared.work.notify_all();
        self.shared.space.notify_waiters();
        self.shared.progress.notify_all();
        // A write blocked on a program that does not read never returns by
        // itself, and the thread would hold the PTY fd for good. Interrupt it
        // until the thread is gone. A signal that lands just before the write
        // starts is lost, hence the repeats.
        #[cfg(any(unix, windows))]
        {
            let shared = self.shared.clone();
            let _ = std::thread::Builder::new().name("pty-writer-kick".into()).spawn(move || {
                for _ in 0..100 {
                    if shared.exited.load(Ordering::SeqCst) {
                        return;
                    }
                    {
                        let guard = shared.thread.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(t) = guard.as_ref() {
                            // `t` is the live writer thread (cleared under this lock before
                            // it returns).
                            wake_thread(t);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
        }
    }
}

impl PaneWriter {
    /// Queue `data` without waiting. `Err(Full)` when the queue is over its cap.
    pub fn try_queue(&self, data: Vec<u8>) -> Result<(), QueueError> {
        self.shared.try_push(Job::new(data)).map_err(|(e, _)| e)
    }

    /// [`Self::try_queue`] that runs `on_end` when the bytes leave the queue
    /// for good (written, discarded or given up). Not run when refused.
    pub fn try_queue_tracked(&self, data: Vec<u8>, on_end: OnEnd) -> Result<(), QueueError> {
        let job = Job { data, done: None, on_end: Some(on_end) };
        self.shared.try_push(job).map_err(|(e, _)| e)
    }

    /// Queue `data`, waiting for room. Returns once the bytes are queued, not
    /// once they are written.
    pub async fn queue(&self, data: Vec<u8>) -> Result<(), QueueError> {
        self.push_waiting(Job::new(data)).await
    }

    /// Queue `data` and wait until the PTY has taken all of it.
    pub async fn write(&self, data: Vec<u8>) -> Result<(), QueueError> {
        let (tx, rx) = oneshot::channel();
        self.push_waiting(Job { data, done: Some(tx), on_end: None }).await?;
        match rx.await {
            Ok(Ok(())) => Ok(()),
            _ => Err(QueueError::Closed),
        }
    }

    /// Queue `data`, blocking the calling thread until there is room, up to
    /// `timeout`. For synchronous callers (the local socket). `Err(Full)` when
    /// the time ran out.
    pub fn queue_blocking(&self, data: Vec<u8>, timeout: Duration) -> Result<(), QueueError> {
        let deadline = Instant::now() + timeout;
        let job = Job::new(data);
        let mut st = self.shared.lock();
        loop {
            if st.closed {
                return Err(QueueError::Closed);
            }
            if st.bytes == 0 || st.bytes + job.data.len() <= INPUT_QUEUE_BYTES {
                if st.bytes == 0 {
                    self.shared.touch();
                }
                st.bytes += job.data.len();
                st.queue.push_back(job);
                drop(st);
                self.shared.work.notify_one();
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(QueueError::Full);
            }
            st = self.shared.progress.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Stop taking input. What is queued is still written.
    pub fn close(&self) {
        self.shared.lock().closed = true;
        self.shared.work.notify_all();
    }

    /// Stop taking input and wait until everything queued has been written,
    /// up to `deadline`. Returns `true` when the queue drained. The write
    /// that is under way is not interrupted.
    pub fn close_and_drain(&self, deadline: Instant) -> bool {
        let mut st = self.shared.lock();
        st.closed = true;
        self.shared.work.notify_all();
        loop {
            if st.bytes == 0 && st.queue.is_empty() {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self.shared.progress.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Throw away everything queued and what is left of the write under way.
    /// For an interrupt: input typed or pasted before Ctrl+C must not reach
    /// the shell after the program it was meant for is gone. Returns the
    /// bytes that were waiting.
    pub fn discard_pending(&self) -> usize {
        let mut ended = Vec::new();
        let mut freed = 0;
        {
            let mut st = self.shared.lock();
            self.shared.generation.fetch_add(1, Ordering::SeqCst);
            for mut job in st.queue.drain(..) {
                if let Some(d) = job.done.take() {
                    let _ = d.send(Err("input discarded".to_string()));
                }
                freed += job.data.len();
                ended.push(job);
            }
            // The job in the thread, if any, still counts until it returns.
            st.bytes = st.bytes.saturating_sub(freed);
        }
        for job in ended {
            job.end();
        }
        self.shared.space.notify_waiters();
        self.shared.progress.notify_all();
        // A write blocked on a program that does not read is woken, so it can
        // see the change and stop.
        #[cfg(any(unix, windows))]
        {
            let shared = self.shared.clone();
            let _ = std::thread::Builder::new().name("pty-writer-kick".into()).spawn(move || {
                for pause in [0u64, 25, 50, 100, 200] {
                    std::thread::sleep(Duration::from_millis(pause));
                    if shared.exited.load(Ordering::SeqCst) {
                        return;
                    }
                    {
                        let guard = shared.thread.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(t) = guard.as_ref() {
                            // `t` is the live writer thread (cleared under this lock before
                            // it returns).
                            wake_thread(t);
                        }
                    }
                }
            });
        }
        freed
    }

    /// Wait until nothing is queued and no write is under way, up to
    /// `timeout`. Returns `true` when idle.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            if st.bytes == 0 && st.queue.is_empty() {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self.shared.progress.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// What the queue and the writer thread are doing.
    pub fn stats(&self) -> WriterStats {
        let (queued_bytes, frames) = {
            let st = self.shared.lock();
            (st.bytes, st.queue.len())
        };
        let now = self.shared.now_ms();
        let since = self.shared.write_since_ms.load(Ordering::SeqCst);
        let writing = since != IDLE;
        WriterStats {
            queued_bytes,
            // The job in the thread is not in `queue` any more.
            frames: frames + usize::from(writing),
            in_write: writing.then(|| Duration::from_millis(now.saturating_sub(since))),
            since_progress: Duration::from_millis(
                now.saturating_sub(self.shared.progress_ms.load(Ordering::SeqCst)),
            ),
            exited: self.shared.exited.load(Ordering::SeqCst),
        }
    }

    /// Bytes waiting in the queue (test and QA hook).
    pub fn queued_bytes(&self) -> usize {
        self.shared.lock().bytes
    }

    async fn push_waiting(&self, mut job: Job) -> Result<(), QueueError> {
        loop {
            // Arm the wake-up before the test, so a pop in between is not lost.
            let space = self.shared.space.notified();
            tokio::pin!(space);
            space.as_mut().enable();
            match self.shared.try_push(job) {
                Ok(()) => return Ok(()),
                Err((QueueError::Closed, _)) => return Err(QueueError::Closed),
                Err((QueueError::Full, back)) => job = back,
            }
            space.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A writer that blocks until released, like a PTY nobody reads.
    struct Gate {
        open: Arc<AtomicBool>,
        out: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for Gate {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            while !self.open.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            self.out.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_blocked_pty_blocks_only_its_own_callers() {
        let open = Arc::new(AtomicBool::new(false));
        let out = Arc::new(Mutex::new(Vec::new()));
        let owner = PaneWriterOwner::spawn(Box::new(Gate { open: open.clone(), out: out.clone() }))
            .unwrap();
        let w = owner.handle();
        // The first job is taken by the thread, which then blocks.
        w.try_queue(vec![1; 10]).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Fill the queue (the job in the thread still counts): the next is refused.
        w.try_queue(vec![2; INPUT_QUEUE_BYTES - 10]).unwrap();
        assert_eq!(w.try_queue(vec![3; 1]), Err(QueueError::Full));
        // A waiting writer is held, other tasks keep running.
        let w2 = w.clone();
        let waiting = tokio::spawn(async move { w2.write(vec![4; 5]).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiting.is_finished());
        open.store(true, Ordering::SeqCst);
        waiting.await.unwrap().unwrap();
        let got = out.lock().unwrap().clone();
        assert_eq!(got.len(), INPUT_QUEUE_BYTES + 5);
        assert_eq!(&got[..10], &[1; 10]);
        assert_eq!(&got[got.len() - 5..], &[4; 5]);
        drop(owner);
        assert_eq!(w.try_queue(vec![0]), Err(QueueError::Closed));
    }

    #[tokio::test]
    async fn stats_say_where_the_bytes_wait() {
        let open = Arc::new(AtomicBool::new(false));
        let out = Arc::new(Mutex::new(Vec::new()));
        let owner = PaneWriterOwner::spawn(Box::new(Gate { open: open.clone(), out })).unwrap();
        let w = owner.handle();
        let idle = w.stats();
        assert_eq!((idle.queued_bytes, idle.frames, idle.in_write), (0, 0, None));
        assert!(!idle.stalled(Duration::ZERO), "an empty queue is never stalled");
        w.try_queue(vec![1; 10]).unwrap();
        w.try_queue(vec![2; 20]).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let blocked = w.stats();
        assert_eq!((blocked.queued_bytes, blocked.frames), (30, 2));
        assert!(blocked.in_write.is_some_and(|d| d >= Duration::from_millis(50)), "{blocked:?}");
        assert!(blocked.since_progress >= Duration::from_millis(50));
        assert!(blocked.stalled(Duration::from_millis(50)));
        assert!(blocked.writer_state().contains("blocked in write()"));
        open.store(true, Ordering::SeqCst);
        assert!(w.wait_idle(Duration::from_secs(5)));
        let done = w.stats();
        assert_eq!((done.queued_bytes, done.frames, done.in_write), (0, 0, None));
        assert!(!done.stalled(Duration::ZERO));
    }

    #[tokio::test]
    async fn close_and_drain_waits_for_the_queue_and_refuses_new_input() {
        let open = Arc::new(AtomicBool::new(false));
        let out = Arc::new(Mutex::new(Vec::new()));
        let owner = PaneWriterOwner::spawn(Box::new(Gate { open: open.clone(), out: out.clone() }))
            .unwrap();
        let w = owner.handle();
        w.try_queue(vec![1; 100]).unwrap();
        w.try_queue(vec![2; 100]).unwrap();
        // Blocked: the drain runs out of time.
        let w2 = w.clone();
        let late = tokio::task::spawn_blocking(move || {
            w2.close_and_drain(Instant::now() + Duration::from_millis(100))
        })
        .await
        .unwrap();
        assert!(!late);
        assert_eq!(w.try_queue(vec![3]), Err(QueueError::Closed));
        // Released: everything queued before the close is written.
        open.store(true, Ordering::SeqCst);
        let w2 = w.clone();
        let ok = tokio::task::spawn_blocking(move || {
            w2.close_and_drain(Instant::now() + Duration::from_secs(5))
        })
        .await
        .unwrap();
        assert!(ok);
        assert_eq!(out.lock().unwrap().len(), 200);
        drop(owner);
    }

    #[tokio::test]
    async fn queue_blocking_waits_for_room_then_times_out() {
        let open = Arc::new(AtomicBool::new(false));
        let out = Arc::new(Mutex::new(Vec::new()));
        let owner = PaneWriterOwner::spawn(Box::new(Gate { open: open.clone(), out: out.clone() }))
            .unwrap();
        let w = owner.handle();
        w.try_queue(vec![1; 10]).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        w.try_queue(vec![2; INPUT_QUEUE_BYTES - 10]).unwrap();
        let w2 = w.clone();
        let r = tokio::task::spawn_blocking(move || {
            w2.queue_blocking(vec![3; 5], Duration::from_millis(100))
        })
        .await
        .unwrap();
        assert_eq!(r, Err(QueueError::Full));
        let w2 = w.clone();
        let waiting = tokio::task::spawn_blocking(move || {
            w2.queue_blocking(vec![4; 5], Duration::from_secs(10))
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        open.store(true, Ordering::SeqCst);
        waiting.await.unwrap().unwrap();
        drop(owner);
    }

    #[tokio::test]
    async fn discarding_drops_the_queue_and_the_rest_of_the_job_under_way() {
        /// Takes 4 bytes per `write`, and only when told to.
        struct Slow {
            tokens: Arc<std::sync::atomic::AtomicUsize>,
            out: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for Slow {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                loop {
                    let t = self.tokens.load(Ordering::SeqCst);
                    if t > 0
                        && self
                            .tokens
                            .compare_exchange(t, t - 1, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                let n = buf.len().min(4);
                self.out.lock().unwrap().extend_from_slice(&buf[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let tokens = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let out = Arc::new(Mutex::new(Vec::new()));
        let owner =
            PaneWriterOwner::spawn(Box::new(Slow { tokens: tokens.clone(), out: out.clone() }))
                .unwrap();
        let w = owner.handle();
        let ended = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let track = |ended: &Arc<std::sync::atomic::AtomicUsize>| -> OnEnd {
            let e = ended.clone();
            Box::new(move || {
                e.fetch_add(1, Ordering::SeqCst);
            })
        };
        // A paste: the first 4 bytes go out, the thread then waits to write
        // the rest. Two more jobs wait behind it.
        w.try_queue_tracked(b"PASTEPASTEPASTE".to_vec(), track(&ended)).unwrap();
        w.try_queue_tracked(b"second".to_vec(), track(&ended)).unwrap();
        w.try_queue_tracked(b"third".to_vec(), track(&ended)).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(out.lock().unwrap().len(), 4);
        // The interrupt: what is queued is thrown away at once.
        let freed = w.discard_pending();
        assert_eq!(freed, "second".len() + "third".len());
        assert_eq!(ended.load(Ordering::SeqCst), 2, "the discarded jobs report their end");
        // The write under way finishes its current call and stops.
        tokens.store(100, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(out.lock().unwrap().len(), 8, "the rest of the paste was still written");
        assert_eq!(ended.load(Ordering::SeqCst), 3);
        assert_eq!(w.queued_bytes(), 0);
        // New input goes through.
        w.try_queue(b"fresh".to_vec()).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(out.lock().unwrap().ends_with(b"fresh"));
        drop(owner);
    }

    /// A pane that closes while its writer is blocked on a program that never
    /// reads: the thread must end (N8). A raw-mode pty with no reader blocks
    /// a large write for good.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn closing_a_pane_frees_a_writer_blocked_on_a_hung_program() {
        use std::os::fd::FromRawFd;
        let (mut master, mut slave) = (0, 0);
        // SAFETY: plain libc calls on fds we own.
        let dup = unsafe {
            assert_eq!(
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null()
                ),
                0
            );
            let mut t: libc::termios = std::mem::zeroed();
            libc::tcgetattr(slave, &mut t);
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(slave, libc::TCSANOW, &t);
            libc::dup(master)
        };
        let out = unsafe { std::fs::File::from_raw_fd(dup) };
        let owner = PaneWriterOwner::spawn(Box::new(out)).unwrap();
        let w = owner.handle();
        w.try_queue(vec![b'x'; 200 * 1024]).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(w.queued_bytes() > 0, "the write should be stuck");
        let shared = owner.shared.clone();
        assert!(!shared.exited.load(Ordering::SeqCst));
        drop(owner);
        let t0 = Instant::now();
        while !shared.exited.load(Ordering::SeqCst) {
            assert!(t0.elapsed() < Duration::from_secs(3), "the writer thread is still blocked");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // SAFETY: closing the fds this test opened.
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }
}
