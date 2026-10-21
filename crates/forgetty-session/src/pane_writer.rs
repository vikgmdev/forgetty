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
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

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

struct Job {
    data: Vec<u8>,
    done: Option<oneshot::Sender<Result<(), String>>>,
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
}

impl Shared {
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
        st.bytes += job.data.len();
        st.queue.push_back(job);
        drop(st);
        self.work.notify_one();
        Ok(())
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
        });
        let s = shared.clone();
        std::thread::Builder::new().name("session-pty-writer".to_string()).spawn(move || {
            loop {
                let job = {
                    let mut st = s.lock();
                    loop {
                        if let Some(job) = st.queue.pop_front() {
                            break job;
                        }
                        if st.closed {
                            return;
                        }
                        st = s.work.wait(st).unwrap_or_else(|e| e.into_inner());
                    }
                };
                // The real write. It may block for as long as the program
                // does not read; nothing else waits on it.
                let res =
                    out.write_all(&job.data).and_then(|()| out.flush()).map_err(|e| e.to_string());
                let failed = res.is_err();
                {
                    let mut st = s.lock();
                    st.bytes = st.bytes.saturating_sub(job.data.len());
                    if failed {
                        // The PTY is gone: refuse new bytes, fail the rest.
                        st.closed = true;
                        for rest in st.queue.drain(..) {
                            if let Some(d) = rest.done {
                                let _ = d.send(Err("pane input closed".to_string()));
                            }
                        }
                        st.bytes = 0;
                    }
                }
                if let Some(d) = job.done {
                    let _ = d.send(res);
                }
                s.space.notify_waiters();
                if failed {
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
        for rest in st.queue.drain(..) {
            if let Some(d) = rest.done {
                let _ = d.send(Err("pane closed".to_string()));
            }
        }
        st.bytes = 0;
        drop(st);
        self.shared.work.notify_all();
        self.shared.space.notify_waiters();
    }
}

impl PaneWriter {
    /// Queue `data` without waiting. `Err(Full)` when the queue is over its cap.
    pub fn try_queue(&self, data: Vec<u8>) -> Result<(), QueueError> {
        self.shared.try_push(Job { data, done: None }).map_err(|(e, _)| e)
    }

    /// Queue `data`, waiting for room. Returns once the bytes are queued, not
    /// once they are written.
    pub async fn queue(&self, data: Vec<u8>) -> Result<(), QueueError> {
        self.push_waiting(Job { data, done: None }).await
    }

    /// Queue `data` and wait until the PTY has taken all of it.
    pub async fn write(&self, data: Vec<u8>) -> Result<(), QueueError> {
        let (tx, rx) = oneshot::channel();
        self.push_waiting(Job { data, done: Some(tx) }).await?;
        match rx.await {
            Ok(Ok(())) => Ok(()),
            _ => Err(QueueError::Closed),
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
}
