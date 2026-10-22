//! A limit on the VT instances that build at once, process wide.
//!
//! Capture and snapshot catch-up both feed a throwaway `forgetty-vt` terminal
//! from the byte-log ring. Each is CPU-bound work and a page pool. Two at a
//! time are enough, so a capture never stacks on the builds a snapshot
//! catch-up already runs (AD-019, AD-023).
//!
//! - first come, first served inside each kind (a ticket queue)
//! - one of the [`MAX_VT_BUILDS`] permits is kept for snapshots: captures hold
//!   at most `MAX_VT_BUILDS - 1`, so a capture can never delay a catch-up
//! - when a permit frees, the oldest waiter that is allowed to take it gets it

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};

/// VT builds that run at once, capture and snapshot together.
pub const MAX_VT_BUILDS: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Capture,
    Snapshot,
}

#[derive(Default)]
struct State {
    held: usize,
    capture_held: usize,
    next_ticket: u64,
    capture_q: VecDeque<u64>,
    snapshot_q: VecDeque<u64>,
    peak: usize,
}

/// A counting semaphore for blocking threads. See the module docs.
pub struct VtGate {
    state: Mutex<State>,
    cv: Condvar,
    max: usize,
}

/// A permit. Gives itself back on drop.
pub struct VtPermit<'a> {
    gate: &'a VtGate,
    kind: Kind,
}

impl State {
    fn queue(&mut self, kind: Kind) -> &mut VecDeque<u64> {
        match kind {
            Kind::Capture => &mut self.capture_q,
            Kind::Snapshot => &mut self.snapshot_q,
        }
    }

    /// May a waiter of `kind` take a permit now (ignoring who waits longer)?
    fn allowed(&self, kind: Kind, max: usize) -> bool {
        self.held < max
            && (kind == Kind::Snapshot || self.capture_held < max.saturating_sub(1).max(1))
    }

    /// Is `ticket` the one to go: head of its queue, allowed, and no older
    /// head that is allowed too.
    fn is_turn(&self, kind: Kind, ticket: u64, max: usize) -> bool {
        let head = |q: &VecDeque<u64>| q.front().copied();
        let (mine, other_kind, other) = match kind {
            Kind::Capture => (head(&self.capture_q), Kind::Snapshot, head(&self.snapshot_q)),
            Kind::Snapshot => (head(&self.snapshot_q), Kind::Capture, head(&self.capture_q)),
        };
        if mine != Some(ticket) || !self.allowed(kind, max) {
            return false;
        }
        !matches!(other, Some(t) if t < ticket && self.allowed(other_kind, max))
    }
}

impl VtGate {
    pub const fn new(max: usize) -> Self {
        VtGate {
            state: Mutex::new(State {
                held: 0,
                capture_held: 0,
                next_ticket: 0,
                capture_q: VecDeque::new(),
                snapshot_q: VecDeque::new(),
                peak: 0,
            }),
            cv: Condvar::new(),
            max,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn acquire(&self, kind: Kind) -> VtPermit<'_> {
        let mut st = self.lock();
        let ticket = st.next_ticket;
        st.next_ticket += 1;
        st.queue(kind).push_back(ticket);
        while !st.is_turn(kind, ticket, self.max) {
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.queue(kind).pop_front();
        st.held += 1;
        if kind == Kind::Capture {
            st.capture_held += 1;
        }
        st.peak = st.peak.max(st.held);
        // The next waiter in a queue may be allowed too.
        self.cv.notify_all();
        VtPermit { gate: self, kind }
    }

    /// Block until a capture may build.
    pub fn acquire_capture(&self) -> VtPermit<'_> {
        self.acquire(Kind::Capture)
    }

    /// Block until a snapshot build may run.
    pub fn acquire_snapshot(&self) -> VtPermit<'_> {
        self.acquire(Kind::Snapshot)
    }

    /// Most permits held at once so far.
    pub fn peak(&self) -> usize {
        self.lock().peak
    }
}

impl Drop for VtPermit<'_> {
    fn drop(&mut self) {
        let mut st = self.gate.lock();
        st.held -= 1;
        if self.kind == Kind::Capture {
            st.capture_held -= 1;
        }
        self.gate.cv.notify_all();
    }
}

static GATE: VtGate = VtGate::new(MAX_VT_BUILDS);

/// The process wide gate.
pub fn vt_gate() -> &'static VtGate {
    &GATE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn never_more_than_max_hold_a_permit_and_captures_leave_one_free() {
        let gate = Arc::new(VtGate::new(2));
        let now = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let caps_now = Arc::new(AtomicUsize::new(0));
        let caps_most = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..12)
            .map(|i| {
                let (gate, now, most) = (gate.clone(), now.clone(), most.clone());
                let (caps_now, caps_most) = (caps_now.clone(), caps_most.clone());
                std::thread::spawn(move || {
                    let capture = i % 3 != 0;
                    let _p = if capture { gate.acquire_capture() } else { gate.acquire_snapshot() };
                    let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(n, Ordering::SeqCst);
                    if capture {
                        let c = caps_now.fetch_add(1, Ordering::SeqCst) + 1;
                        caps_most.fetch_max(c, Ordering::SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(15));
                    if capture {
                        caps_now.fetch_sub(1, Ordering::SeqCst);
                    }
                    now.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(most.load(Ordering::SeqCst) <= 2);
        assert_eq!(caps_most.load(Ordering::SeqCst), 1, "captures hold one permit at most");
        assert_eq!(gate.peak(), 2);
    }

    #[test]
    fn a_waiting_capture_does_not_delay_a_snapshot() {
        let gate = Arc::new(VtGate::new(2));
        let cap1 = gate.acquire_capture(); // the one capture slot
                                           // A second capture waits (blocked by the capture limit).
        let g2 = gate.clone();
        let waiting = std::thread::spawn(move || {
            let _p = g2.acquire_capture();
        });
        std::thread::sleep(Duration::from_millis(50));
        // A snapshot arrives after it and still goes at once.
        let t0 = Instant::now();
        let snap = gate.acquire_snapshot();
        assert!(t0.elapsed() < Duration::from_millis(40), "snapshot waited {:?}", t0.elapsed());
        drop(snap);
        drop(cap1);
        waiting.join().unwrap();
    }

    #[test]
    fn waiters_of_one_kind_go_in_arrival_order() {
        let gate = Arc::new(VtGate::new(2));
        let blocker = gate.acquire_capture();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut threads = Vec::new();
        for i in 0..4 {
            let (gate, order) = (gate.clone(), order.clone());
            threads.push(std::thread::spawn(move || {
                let _p = gate.acquire_capture();
                order.lock().unwrap().push(i);
                std::thread::sleep(Duration::from_millis(5));
            }));
            std::thread::sleep(Duration::from_millis(30)); // arrive in order
        }
        drop(blocker);
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3]);
    }
}
