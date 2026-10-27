//! Key-to-output stage timestamps, for finding where a keystroke's echo waits.
//!
//! Off unless `FORGETTY_TRACE_LATENCY=1` is set in the server's environment (one relaxed load
//! per stage when off). When on, the five stages of the first echo after an input are stamped:
//! input queued, PTY write done, first PTY read return, pane output event sent, output frame
//! written to the client stream. The last stage logs one line with the gaps in microseconds
//! (target `latency`). It follows one pane at a time and is for diagnosis, not for metrics.
//!
//! A PTY read that does not hold the first byte of the input (a ConPTY cursor move that comes
//! before the character is painted) is logged as a `chunk` line with its time after the input and
//! its first bytes; only the read that holds the byte counts as the echo.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

static ON: OnceLock<bool> = OnceLock::new();
static BASE: OnceLock<Instant> = OnceLock::new();
static BUSY: AtomicBool = AtomicBool::new(false);
static INPUT: AtomicU64 = AtomicU64::new(0);
static WRITTEN: AtomicU64 = AtomicU64::new(0);
static READ: AtomicU64 = AtomicU64::new(0);
static EVENT: AtomicU64 = AtomicU64::new(0);
/// The first byte of the traced input: the chunk that holds it is the echo.
static KEY: AtomicU8 = AtomicU8::new(0);
/// Chunks logged for the running trace.
static CHUNKS: AtomicU8 = AtomicU8::new(0);

fn on() -> bool {
    *ON.get_or_init(|| std::env::var("FORGETTY_TRACE_LATENCY").is_ok_and(|v| v == "1"))
}

/// Microseconds since the first call, never 0.
fn now_us() -> u64 {
    let base = BASE.get_or_init(Instant::now);
    base.elapsed().as_micros() as u64 + 1
}

/// Input bytes were queued for a pane. Starts a trace when none is running.
pub fn input(bytes: &[u8]) {
    if !on() || bytes.is_empty() || BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    KEY.store(bytes[0], Ordering::Relaxed);
    CHUNKS.store(0, Ordering::Relaxed);
    WRITTEN.store(0, Ordering::Relaxed);
    READ.store(0, Ordering::Relaxed);
    EVENT.store(0, Ordering::Relaxed);
    INPUT.store(now_us(), Ordering::Release);
}

fn stamp_once(slot: &AtomicU64) {
    if on() && INPUT.load(Ordering::Acquire) != 0 {
        let _ = slot.compare_exchange(0, now_us(), Ordering::AcqRel, Ordering::Relaxed);
    }
}

/// The PTY write of the input returned.
pub fn written() {
    stamp_once(&WRITTEN);
}

/// A read from the PTY returned `bytes`. The first read that holds the key byte of the input
/// counts as the echo; the reads before it are logged.
pub fn pty_read(bytes: &[u8]) {
    if !on() {
        return;
    }
    let input = INPUT.load(Ordering::Acquire);
    if input == 0 || READ.load(Ordering::Acquire) != 0 {
        return;
    }
    let key = KEY.load(Ordering::Relaxed);
    let has_key = printable(bytes).contains(&key);
    let at = now_us();
    if has_key {
        let _ = READ.compare_exchange(0, at, Ordering::AcqRel, Ordering::Relaxed);
    }
    if CHUNKS.fetch_add(1, Ordering::Relaxed) < 8 {
        let head: String = bytes.iter().take(40).flat_map(|b| std::ascii::escape_default(*b)).map(char::from).collect();
        tracing::info!(
            target: "latency",
            "chunk {} us after the input, {} bytes, key byte {}: {head}",
            at as i64 - input as i64,
            bytes.len(),
            if has_key { "in it" } else { "not in it" }
        );
    }
}

/// The bytes of a chunk that are not part of an escape sequence (CSI and OSC), so a letter that
/// ends `ESC [ ... h` does not pass for a typed key.
fn printable(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                b']' => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                        i += 1;
                    }
                    i += if i < bytes.len() && bytes[i] == 0x1b { 2 } else { 1 };
                }
                _ => i += 2,
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// The pane output event was sent to the subscribers.
pub fn event() {
    // Only output that follows the read of the traced echo.
    if on() && READ.load(Ordering::Acquire) != 0 {
        stamp_once(&EVENT);
    }
}

/// The output frame was written to a client stream. Logs the gaps and ends the trace.
pub fn sent() {
    if !on() || EVENT.load(Ordering::Acquire) == 0 {
        return;
    }
    let (i, w, r, e) = (
        INPUT.swap(0, Ordering::AcqRel),
        WRITTEN.load(Ordering::Relaxed),
        READ.load(Ordering::Relaxed),
        EVENT.load(Ordering::Relaxed),
    );
    let s = now_us();
    BUSY.store(false, Ordering::Release);
    if i == 0 {
        return;
    }
    // A stage that was stamped before the one it follows (the write returned after the echo was
    // already read) shows as a negative gap.
    let d = |a: u64, b: u64| b as i64 - a as i64;
    tracing::info!(
        target: "latency",
        "key echo: input to pty write done {} us, to first pty read {} us, to pane event {} us, to frame written {} us (total {} us)",
        d(i, w), d(w, r), d(r, e), d(e, s), d(i, s)
    );
}
