//! A limit on the VT instances that build at once, process wide.
//!
//! Capture and snapshot catch-up both feed a throwaway `forgetty-vt` terminal
//! from the byte-log ring. Each costs tens of MiB of CPU-bound work and a page
//! pool. Two of them at a time are enough, so a capture never stacks on the
//! builds a snapshot catch-up already runs (AD-019, AD-023).

use std::sync::{Condvar, Mutex};

/// VT builds that run at once, capture and snapshot together.
pub const MAX_VT_BUILDS: usize = 2;

/// A counting semaphore for blocking threads.
pub struct VtGate {
    free: Mutex<usize>,
    cv: Condvar,
    max: usize,
    /// Most permits ever held at once (for tests and the logs).
    peak: Mutex<usize>,
}

/// A permit. Gives itself back on drop.
pub struct VtPermit<'a>(&'a VtGate);

impl VtGate {
    pub const fn new(max: usize) -> Self {
        VtGate { free: Mutex::new(max), cv: Condvar::new(), max, peak: Mutex::new(0) }
    }

    /// Block until a permit is free.
    pub fn acquire(&self) -> VtPermit<'_> {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 {
            free = self.cv.wait(free).unwrap_or_else(|e| e.into_inner());
        }
        *free -= 1;
        let held = self.max - *free;
        let mut peak = self.peak.lock().unwrap_or_else(|e| e.into_inner());
        *peak = (*peak).max(held);
        VtPermit(self)
    }

    /// Most permits held at once so far.
    pub fn peak(&self) -> usize {
        *self.peak.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for VtPermit<'_> {
    fn drop(&mut self) {
        let mut free = self.0.free.lock().unwrap_or_else(|e| e.into_inner());
        *free += 1;
        self.0.cv.notify_one();
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
    use std::time::Duration;

    #[test]
    fn never_more_than_max_hold_a_permit() {
        let gate = Arc::new(VtGate::new(2));
        let now = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (gate, now, most) = (gate.clone(), now.clone(), most.clone());
                std::thread::spawn(move || {
                    let _p = gate.acquire();
                    let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    now.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(most.load(Ordering::SeqCst), 2);
        assert_eq!(gate.peak(), 2);
    }
}
