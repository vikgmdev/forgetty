//! The outgoing Control queue of one v2 connection.
//!
//! Memory is capped by bytes, not by message count. Answers (`Response`,
//! `Failure`, `Pong`) are never dropped and never wait: their number is bounded
//! upstream, because the connection stops reading requests while the queue is
//! over its byte budget and caps the requests in flight. A `LayoutChanged` is a
//! full snapshot, so a connection holds at most one: a newer one replaces the
//! pending one in its place. A `ShellState` is a whole state too: a connection
//! holds at most one per pane, so a pane that writes prompt marks in a loop
//! cannot grow the queue or make a client read old states.
//!
//! Nothing here polls. Waiters use `Notify` with the arm-then-check pattern.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::Notify;

/// Fixed cost counted per queued frame on top of its payload (the `Vec`
/// header, the deque slot and allocator slack).
const ITEM_OVERHEAD: usize = 64;

/// What a pending frame is replaced by: the kind of event and the pane.
pub type KeyedSlot = (u8, uuid::Uuid);

struct State {
    items: VecDeque<Vec<u8>>,
    /// When each item was queued (same order as `items`).
    stamps: VecDeque<std::time::Instant>,
    /// Payload bytes plus [`ITEM_OVERHEAD`] per item.
    bytes: usize,
    /// Items taken off the front so far. The item pushed as number `n` sits at
    /// deque index `n - popped`.
    popped: u64,
    pushed: u64,
    /// Push number of the pending `LayoutChanged`, if any.
    layout: Option<u64>,
    /// Push number of the pending keyed frame of each key (`ShellState`: the
    /// pane), and the key of each pending push number.
    keyed: HashMap<KeyedSlot, u64>,
    keyed_by_push: HashMap<u64, KeyedSlot>,
    /// Frames the writer finished writing.
    written: u64,
    /// Payload bytes of those frames.
    written_bytes: u64,
    /// Bytes set aside for answers still being computed (see [`ControlQueue::reserve`]).
    reserved: usize,
}

/// A byte-capped FIFO of encoded Control frames with one replaceable slot.
pub struct ControlQueue {
    state: Mutex<State>,
    /// Pulsed on every change (push, pop, write done, close).
    changed: Notify,
    closed: AtomicBool,
}

impl Default for ControlQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlQueue {
    /// An empty queue.
    pub fn new() -> Self {
        ControlQueue {
            state: Mutex::new(State {
                items: VecDeque::new(),
                stamps: VecDeque::new(),
                bytes: 0,
                popped: 0,
                pushed: 0,
                layout: None,
                keyed: HashMap::new(),
                keyed_by_push: HashMap::new(),
                written: 0,
                written_bytes: 0,
                reserved: 0,
            }),
            changed: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue a frame that must not be dropped. Never waits.
    pub fn push(&self, frame: Vec<u8>) {
        {
            let mut st = self.lock();
            st.bytes += frame.len() + ITEM_OVERHEAD;
            st.items.push_back(frame);
            st.stamps.push_back(std::time::Instant::now());
            st.pushed += 1;
        }
        self.changed.notify_waiters();
    }

    /// Queue a `LayoutChanged` frame. A pending one is replaced in place.
    pub fn put_layout(&self, frame: Vec<u8>) {
        {
            let mut st = self.lock();
            let slot = st
                .layout
                .and_then(|n| n.checked_sub(st.popped))
                .and_then(|i| usize::try_from(i).ok())
                .filter(|i| *i < st.items.len());
            match slot {
                Some(i) => {
                    let old = std::mem::replace(&mut st.items[i], frame);
                    let new_len = st.items[i].len();
                    st.bytes = st.bytes - old.len() + new_len;
                }
                None => {
                    st.bytes += frame.len() + ITEM_OVERHEAD;
                    st.items.push_back(frame);
                    st.stamps.push_back(std::time::Instant::now());
                    st.layout = Some(st.pushed);
                    st.pushed += 1;
                }
            }
        }
        self.changed.notify_waiters();
    }

    /// Queue a frame that is a whole state for `key`. A pending frame of the
    /// same key is replaced in place.
    pub fn put_keyed(&self, key: KeyedSlot, frame: Vec<u8>) {
        {
            let mut st = self.lock();
            let slot = st
                .keyed
                .get(&key)
                .and_then(|n| n.checked_sub(st.popped))
                .and_then(|i| usize::try_from(i).ok())
                .filter(|i| *i < st.items.len());
            match slot {
                Some(i) => {
                    let old = std::mem::replace(&mut st.items[i], frame);
                    let new_len = st.items[i].len();
                    st.bytes = st.bytes - old.len() + new_len;
                }
                None => {
                    st.bytes += frame.len() + ITEM_OVERHEAD;
                    st.items.push_back(frame);
                    st.stamps.push_back(std::time::Instant::now());
                    let n = st.pushed;
                    st.keyed.insert(key, n);
                    st.keyed_by_push.insert(n, key);
                    st.pushed += 1;
                }
            }
        }
        self.changed.notify_waiters();
    }

    /// Take the next frame, waiting for one. `None` once the queue is closed.
    pub async fn pop(&self) -> Option<Vec<u8>> {
        self.pop_timed().await.map(|(frame, _)| frame)
    }

    /// [`Self::pop`], with how long the frame sat in the queue.
    pub async fn pop_timed(&self) -> Option<(Vec<u8>, std::time::Duration)> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut st = self.lock();
                if let Some(frame) = st.items.pop_front() {
                    let waited = st.stamps.pop_front().map_or(std::time::Duration::ZERO, |t| t.elapsed());
                    st.bytes -= frame.len() + ITEM_OVERHEAD;
                    if st.layout.is_some_and(|n| n == st.popped) {
                        st.layout = None;
                    }
                    let popped = st.popped;
                    if let Some(key) = st.keyed_by_push.remove(&popped) {
                        st.keyed.remove(&key);
                    }
                    st.popped += 1;
                    drop(st);
                    self.changed.notify_waiters();
                    return Some((frame, waited));
                }
                if self.closed.load(Ordering::SeqCst) {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// The writer calls this after a frame of `len` bytes is fully written.
    pub fn wrote(&self, len: usize) {
        {
            let mut st = self.lock();
            st.written += 1;
            st.written_bytes += len as u64;
        }
        self.changed.notify_waiters();
    }

    /// Payload bytes written so far. A stall is also a queue at pressure that
    /// drains slower than a minimum rate.
    pub fn written_bytes(&self) -> u64 {
        self.lock().written_bytes
    }

    /// Set `n` bytes aside for an answer that is not computed yet. They count
    /// against the budget the request reader checks ([`Self::wait_below_committed`]),
    /// so a connection cannot have more answer bytes owed and queued than its
    /// budget allows, however many requests it has in flight. Pair with
    /// [`Self::release`].
    pub fn reserve(&self, n: usize) {
        self.lock().reserved += n;
    }

    /// Give back a [`Self::reserve`].
    pub fn release(&self, n: usize) {
        {
            let mut st = self.lock();
            st.reserved = st.reserved.saturating_sub(n);
        }
        self.changed.notify_waiters();
    }

    /// Queued bytes plus reserved bytes.
    pub fn committed(&self) -> usize {
        let st = self.lock();
        st.bytes + st.reserved
    }

    /// Wait until fewer than `limit` bytes are queued or reserved.
    pub async fn wait_below_committed(&self, limit: usize) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.committed() < limit {
                return;
            }
            notified.await;
        }
    }

    /// Frames written so far. A stall is a queue at pressure whose count does
    /// not move.
    pub fn written(&self) -> u64 {
        self.lock().written
    }

    /// Bytes queued, with the per-item overhead.
    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }

    /// Frames queued.
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// `true` when nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stop the queue: `pop` returns `None` once it is empty.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    /// Wait until fewer than `limit` bytes are queued.
    pub async fn wait_below(&self, limit: usize) {
        self.wait_for(|b| b < limit).await;
    }

    /// Wait until at least `limit` bytes are queued.
    pub async fn wait_at_least(&self, limit: usize) {
        self.wait_for(|b| b >= limit).await;
    }

    async fn wait_for(&self, ok: impl Fn(usize) -> bool) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if ok(self.bytes()) {
                return;
            }
            notified.await;
        }
    }

    /// Wait for any change (push, pop, write done, close).
    pub async fn changed(&self) {
        self.changed.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: u8, len: usize) -> Vec<u8> {
        vec![n; len]
    }

    #[tokio::test]
    async fn fifo_order_and_byte_accounting() {
        let q = ControlQueue::new();
        q.push(frame(1, 10));
        q.push(frame(2, 20));
        assert_eq!(q.bytes(), 30 + 2 * ITEM_OVERHEAD);
        assert_eq!(q.pop().await.unwrap()[0], 1);
        assert_eq!(q.bytes(), 20 + ITEM_OVERHEAD);
        assert_eq!(q.pop().await.unwrap()[0], 2);
        assert_eq!(q.bytes(), 0);
    }

    #[tokio::test]
    async fn a_pending_layout_is_replaced_in_place() {
        let q = ControlQueue::new();
        q.push(frame(1, 5));
        q.put_layout(frame(10, 100));
        q.push(frame(2, 5));
        q.put_layout(frame(11, 50));
        q.put_layout(frame(12, 70));
        assert_eq!(q.len(), 3, "one layout slot, however many layouts arrive");
        assert_eq!(q.bytes(), 5 + 70 + 5 + 3 * ITEM_OVERHEAD);
        assert_eq!(q.pop().await.unwrap()[0], 1);
        let l = q.pop().await.unwrap();
        assert_eq!((l[0], l.len()), (12, 70), "newest layout, in the first layout's place");
        assert_eq!(q.pop().await.unwrap()[0], 2);
        // The slot is free again once the layout left the queue.
        q.put_layout(frame(13, 10));
        assert_eq!(q.len(), 1);
    }

    #[tokio::test]
    async fn a_pending_keyed_frame_is_replaced_in_place_per_key() {
        let q = ControlQueue::new();
        let (a, b) = ((0, uuid::Uuid::from_u128(1)), (0, uuid::Uuid::from_u128(2)));
        q.put_keyed(a, frame(1, 5));
        q.put_keyed(b, frame(2, 5));
        q.push(frame(3, 5));
        for i in 0..1000u32 {
            q.put_keyed(a, frame(10 + (i % 100) as u8, 20));
        }
        q.put_keyed(b, frame(99, 7));
        assert_eq!(q.len(), 3, "one slot per key, however many states arrive");
        assert_eq!(q.bytes(), 20 + 7 + 5 + 3 * ITEM_OVERHEAD);
        let first = q.pop().await.unwrap();
        assert_eq!(first.len(), 20, "the newest state of a, in a's place");
        assert_eq!(q.pop().await.unwrap()[0], 99);
        assert_eq!(q.pop().await.unwrap()[0], 3);
        // The slots are free again once the frames left the queue.
        q.put_keyed(a, frame(4, 10));
        q.put_keyed(a, frame(5, 10));
        assert_eq!(q.len(), 1);
        assert_eq!(q.pop().await.unwrap()[0], 5);
        assert_eq!(q.bytes(), 0);
    }

    #[tokio::test]
    async fn waiters_wake_on_pop_and_push() {
        let q = std::sync::Arc::new(ControlQueue::new());
        for _ in 0..4 {
            q.push(frame(0, 100));
        }
        let limit = 2 * (100 + ITEM_OVERHEAD);
        let w = {
            let q = q.clone();
            tokio::spawn(async move { q.wait_below(limit).await })
        };
        tokio::task::yield_now().await;
        assert!(!w.is_finished());
        q.pop().await;
        q.pop().await;
        q.pop().await;
        tokio::time::timeout(std::time::Duration::from_secs(1), w).await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), q.wait_at_least(100))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn close_ends_pop_after_the_queue_drains() {
        let q = ControlQueue::new();
        q.push(frame(1, 1));
        q.close();
        assert!(q.pop().await.is_some());
        assert!(q.pop().await.is_none());
    }

    #[tokio::test]
    async fn reserved_bytes_count_against_the_request_budget_only() {
        let q = std::sync::Arc::new(ControlQueue::new());
        q.reserve(1000);
        assert_eq!(q.bytes(), 0, "a reservation is not queued data");
        assert_eq!(q.committed(), 1000);
        let w = {
            let q = q.clone();
            tokio::spawn(async move { q.wait_below_committed(500).await })
        };
        tokio::task::yield_now().await;
        assert!(!w.is_finished());
        q.release(600);
        tokio::time::timeout(std::time::Duration::from_secs(1), w).await.unwrap().unwrap();
        q.release(10_000);
        assert_eq!(q.committed(), 0, "release never goes below zero");
    }

    #[test]
    fn written_bytes_add_up() {
        let q = ControlQueue::new();
        q.wrote(10);
        q.wrote(5);
        assert_eq!((q.written(), q.written_bytes()), (2, 15));
    }
}
