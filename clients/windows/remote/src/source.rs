//! `PaneSource` over one `forgetty_client::PaneStream`.
//!
//! Three tokio tasks per pane: the reader (stream to `PaneEvent`), the input
//! writer (ordered, never blocks the UI) and the size sender (latest size
//! wins). Dropping or shutting down the source aborts them, which drops the
//! `PaneStream` and detaches the pane.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use forgetty_client::{ClientHandle, PaneItem, PaneStream};
use forgetty_shell_core::{PaneEvent, PaneSource};
use tokio::runtime::Handle;
use tokio::sync::{watch, Notify};
use tokio::task::AbortHandle;
use uuid::Uuid;

/// Output events buffered between the reader and the view. A full channel
/// back-pressures the QUIC stream (the client buffers `pane_buffer` items).
const EVENT_BUFFER: usize = 256;
/// Input waiting for the writer task. A full queue (the server or the link is stuck) refuses
/// whole buffers: user input is counted as dropped, `auto` replies just go.
const INPUT_QUEUE_CAP: usize = 256 * 1024;
/// Input bytes joined into one `send_input` call.
const INPUT_COALESCE: usize = 16 * 1024;

/// Text of the scrollback line above a snapshot.
pub const SNAPSHOT_MARKER: &str = "-- earlier output not loaded --";

/// VT bytes that put one dim line, [`SNAPSHOT_MARKER`], into the scrollback of
/// a freshly reset terminal of `rows` rows: write it on the top row, then
/// scroll the screen up by one line from the bottom row.
pub fn snapshot_marker(rows: u16) -> Vec<u8> {
    let rows = rows.max(1);
    format!("\x1b[0m\x1b[1;1H\x1b[2;3m{SNAPSHOT_MARKER}\x1b[0m\x1b[{rows};1H\n\x1b[H").into_bytes()
}

/// Bounded input queue between the UI thread (never blocks) and the writer task.
#[derive(Default)]
struct InputQueue {
    state: Mutex<QueueState>,
    ready: Notify,
    /// Bytes refused or dropped for good. Only grows.
    dropped: AtomicU64,
}

#[derive(Default)]
struct QueueState {
    /// `(auto, bytes)` in arrival order.
    items: VecDeque<(bool, Vec<u8>)>,
    bytes: usize,
}

impl InputQueue {
    /// Queue `bytes` whole, or refuse them whole when the queue is full.
    fn push(&self, auto: bool, bytes: &[u8]) -> bool {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.bytes + bytes.len() > INPUT_QUEUE_CAP {
            drop(st);
            self.dropped.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            return false;
        }
        st.bytes += bytes.len();
        match st.items.back_mut() {
            Some((a, b)) if *a == auto && b.len() < INPUT_COALESCE => b.extend_from_slice(bytes),
            _ => st.items.push_back((auto, bytes.to_vec())),
        }
        drop(st);
        self.ready.notify_one();
        true
    }

    /// The oldest item, joined with the items after it that have the same kind.
    fn pop(&self) -> Option<(bool, Vec<u8>)> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (auto, mut buf) = st.items.pop_front()?;
        while buf.len() < INPUT_COALESCE && st.items.front().is_some_and(|(a, _)| *a == auto) {
            let (_, more) = st.items.pop_front().expect("front checked");
            buf.extend_from_slice(&more);
        }
        st.bytes -= buf.len();
        Some((auto, buf))
    }
}

pub struct RemotePaneSource {
    events: async_channel::Receiver<PaneEvent>,
    input: Arc<InputQueue>,
    size: watch::Sender<(u64, u16, u16)>,
    last_size: Mutex<Option<(u16, u16)>>,
    tasks: Vec<AbortHandle>,
}

impl RemotePaneSource {
    pub(crate) fn spawn(rt: &Handle, client: ClientHandle, pane: Uuid, mut stream: PaneStream) -> Self {
        let (ev_tx, ev_rx) = async_channel::bounded::<PaneEvent>(EVENT_BUFFER);
        let reader = rt.spawn(async move {
            let mut rows = 24u16;
            let mut marker_due = false;
            // Where the replay after the last reset ends, while it is still going on.
            let mut replay_end: Option<u64> = None;
            let mut replay_open = false;
            loop {
                let mut evs: Vec<PaneEvent> = Vec::with_capacity(2);
                let mut last = false;
                match stream.next().await {
                    Some(PaneItem::Reset { rows: r, cols }) => {
                        rows = r;
                        marker_due = true;
                        replay_end = stream.replay_end();
                        replay_open = replay_end.is_some();
                        evs.push(PaneEvent::Reset { rows: r, cols, exact_replay: replay_open });
                    }
                    // AD-023: the client fell behind. The VT was just reset. A one line
                    // marker goes into scrollback, then the snapshot paints the screen.
                    Some(PaneItem::Snapshot { bytes, .. }) => {
                        // A snapshot can arrive in several frames: one marker per reset.
                        let mut out = if std::mem::take(&mut marker_due) { snapshot_marker(rows) } else { Vec::new() };
                        out.extend_from_slice(&bytes);
                        evs.push(PaneEvent::Output(out));
                    }
                    Some(PaneItem::Data { offset, bytes }) => {
                        crate::trace::stamp("recv");
                        // Bytes from `replay_end` on are live: end the replay before them.
                        if replay_open && replay_end.is_some_and(|e| offset >= e) {
                            replay_open = false;
                            evs.push(PaneEvent::ReplayEnd);
                        }
                        let end = offset + bytes.len() as u64;
                        evs.push(PaneEvent::Output(bytes));
                        if replay_open && replay_end.is_some_and(|e| end >= e) {
                            replay_open = false;
                            evs.push(PaneEvent::ReplayEnd);
                        }
                    }
                    Some(PaneItem::Ended) => {
                        evs.push(PaneEvent::Closed("pane ended".into()));
                        last = true;
                    }
                    None => {
                        evs.push(PaneEvent::Closed("connection closed".into()));
                        last = true;
                    }
                }
                for ev in evs {
                    if ev_tx.send(ev).await.is_err() {
                        return; // the view is gone
                    }
                }
                if last {
                    break;
                }
            }
        });

        let queue = Arc::new(InputQueue::default());
        let q = queue.clone();
        let c = client.clone();
        let writer = rt.spawn(async move {
            loop {
                let Some((auto, buf)) = q.pop() else {
                    q.ready.notified().await;
                    continue;
                };
                crate::trace::stamp("popped");
                // While disconnected the client buffers up to 64 KiB and sends the bytes in
                // order after the connection resumes. It takes a buffer whole or refuses it.
                match c.send_input_flagged(pane, &buf, auto).await {
                    Ok(()) => crate::trace::stamp("handed"),
                    Err(e) => {
                        q.dropped.fetch_add(buf.len() as u64, Ordering::Relaxed);
                        eprintln!("forgetty-win: remote input dropped ({} bytes): {e}", buf.len());
                    }
                }
            }
        });

        let (size_tx, mut size_rx) = watch::channel((0u64, 0u16, 0u16));
        let sizer = rt.spawn(async move {
            while size_rx.changed().await.is_ok() {
                let (_, rows, cols) = *size_rx.borrow_and_update();
                if let Err(e) = client.resize(pane, rows, cols).await {
                    eprintln!("forgetty-win: remote resize failed: {e}");
                }
            }
        });

        RemotePaneSource {
            events: ev_rx,
            input: queue,
            size: size_tx,
            last_size: Mutex::new(None),
            tasks: vec![reader.abort_handle(), writer.abort_handle(), sizer.abort_handle()],
        }
    }

    fn send_size(&self, rows: u16, cols: u16) {
        self.size.send_modify(|s| *s = (s.0 + 1, rows, cols));
    }
}

impl PaneSource for RemotePaneSource {
    fn events(&self) -> async_channel::Receiver<PaneEvent> {
        self.events.clone()
    }

    fn write_input(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            crate::trace::stamp("queued");
            self.input.push(false, bytes);
        }
    }

    fn write_auto(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.input.push(true, bytes);
        }
    }

    fn letterbox(&self) -> bool {
        true
    }

    fn dropped_input_bytes(&self) -> u64 {
        self.input.dropped.load(Ordering::Relaxed)
    }

    fn resize(&self, rows: u16, cols: u16) {
        let mut last = self.last_size.lock().unwrap_or_else(|e| e.into_inner());
        if *last == Some((rows, cols)) {
            return;
        }
        *last = Some((rows, cols));
        drop(last);
        self.send_size(rows, cols);
    }

    fn reassert_size(&self, rows: u16, cols: u16) {
        *self.last_size.lock().unwrap_or_else(|e| e.into_inner()) = Some((rows, cols));
        self.send_size(rows, cols);
    }

    fn shutdown(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Drop for RemotePaneSource {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_input_queue_is_bounded_and_counts_what_it_refuses() {
        let q = InputQueue::default();
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..4 {
            assert!(q.push(false, &chunk));
        }
        // Full: the next buffer is refused whole, user or auto.
        assert!(!q.push(false, b"typed"));
        assert!(!q.push(true, b"\x1b[?6c"));
        assert_eq!(q.dropped.load(Ordering::Relaxed), (b"typed".len() + b"\x1b[?6c".len()) as u64);
        // Draining makes room again.
        while q.pop().is_some() {}
        assert!(q.push(false, b"typed"));
    }

    #[test]
    fn user_and_auto_bytes_are_not_mixed_in_one_frame() {
        let q = InputQueue::default();
        q.push(false, b"ab");
        q.push(false, b"cd");
        q.push(true, b"\x1b[I");
        q.push(false, b"ef");
        assert_eq!(q.pop(), Some((false, b"abcd".to_vec())));
        assert_eq!(q.pop(), Some((true, b"\x1b[I".to_vec())));
        assert_eq!(q.pop(), Some((false, b"ef".to_vec())));
        assert_eq!(q.pop(), None);
    }
}
