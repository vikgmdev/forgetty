//! Time stamps on the path of one key, for the smoke test only. Off by default: a stamp is one
//! atomic load then. The smoke turns it on to split the key-to-model time of a server pane.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

static ON: AtomicBool = AtomicBool::new(false);
static LOG: Mutex<Vec<(&'static str, Instant)>> = Mutex::new(Vec::new());

/// Turn the stamps on or off. Turning them on or off also empties the log.
pub fn enable(on: bool) {
    ON.store(on, Ordering::SeqCst);
    clear();
}

/// Forget the stamps taken so far.
pub fn clear() {
    LOG.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// The stamps taken since the last `clear`, oldest first.
pub fn take() -> Vec<(&'static str, Instant)> {
    std::mem::take(&mut *LOG.lock().unwrap_or_else(|e| e.into_inner()))
}

pub(crate) fn stamp(kind: &'static str) {
    if ON.load(Ordering::Relaxed) {
        let mut g = LOG.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() < 4096 {
            g.push((kind, Instant::now()));
        }
    }
}
