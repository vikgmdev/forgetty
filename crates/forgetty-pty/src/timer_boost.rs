//! A 1 ms system timer while a pane is in use, none while the server is idle (Windows).
//!
//! A shell on a pane waits on timers (PSReadLine paints a key's echo from a timer driven path;
//! the console host batches its output the same way). With the default tick of 15.6 ms an echo
//! from PowerShell takes 14 to 16 ms, and with the server asking for 1 ms it takes 9 to 10 ms
//! (`crates/forgetty-server/tests/timer_probe.rs`, CI job `timer-probe`, PowerShell 5.1 and 7).
//! `cmd.exe` does not wait on a timer and does not change.
//!
//! A 1 ms tick for as long as the server runs costs power, so the request is held only while
//! there is activity: [`note_activity`] (a pane started, a client attached, a key arrived)
//! starts it, and it ends [`IDLE`] after the last activity.
//!
//! One thread, started by [`init`] at server start, makes both calls (`timeBeginPeriod` and
//! `timeEndPeriod`). [`note_activity`] only stores the time and, when the tick is not requested,
//! wakes that thread: no system call to a timer API and no thread spawn on the key path.
//! Without [`init`] (a test process, a program that embeds the pty crate) it does nothing.
//!
//! `FORGETTY_TIMER_RES=off` turns it off. `period` and `full` hold a 1 ms tick for the life of
//! the server (`forgetty-server`'s `timerres`), so there is nothing for this module to do.
//! Not Windows: no-op.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::thread::Thread;
    use std::time::{Duration, Instant};

    use tracing::{debug, warn};

    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
        fn timeEndPeriod(period: u32) -> u32;
    }

    /// How long after the last activity the 1 ms request ends.
    const IDLE_MS: u64 = 10_000;

    static BASE: OnceLock<Instant> = OnceLock::new();
    /// The watcher thread, once [`init`] ran.
    static WATCHER: OnceLock<Option<Thread>> = OnceLock::new();
    /// Milliseconds since `BASE` of the last activity, plus one. 0: none yet.
    static LAST: AtomicU64 = AtomicU64::new(0);
    /// The watcher has the 1 ms request.
    static ACTIVE: AtomicBool = AtomicBool::new(false);

    fn now_ms() -> u64 {
        BASE.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
    }

    fn recent() -> bool {
        let last = LAST.load(Ordering::SeqCst);
        last != 0 && now_ms().saturating_sub(last) < IDLE_MS
    }

    /// Start the watcher thread. Call once at server start. Does nothing when
    /// `FORGETTY_TIMER_RES` is set (`off` disables, the other values hold the tick themselves).
    pub fn init() {
        if std::env::var_os("FORGETTY_TIMER_RES").is_some_and(|v| !v.is_empty()) {
            return;
        }
        WATCHER.get_or_init(|| {
            let spawned = std::thread::Builder::new().name("timer-boost".into()).spawn(watch);
            match spawned {
                Ok(handle) => Some(handle.thread().clone()),
                Err(e) => {
                    warn!("timer boost: cannot start the watcher thread: {e}");
                    None
                }
            }
        });
    }

    fn watch() {
        loop {
            if !recent() {
                std::thread::park();
                continue;
            }
            // SAFETY: plain winmm call; TIMERR_NOERROR is 0.
            if unsafe { timeBeginPeriod(1) } != 0 {
                warn!("timer boost: timeBeginPeriod(1) failed");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            ACTIVE.store(true, Ordering::SeqCst);
            debug!("timer boost: 1 ms requested");
            while recent() {
                std::thread::sleep(Duration::from_secs(1));
            }
            // Order matters: first say the tick is off, then look again for activity. A key that
            // stored its time before this store saw `ACTIVE` true and did not wake us, and the
            // look below sees its time. A key after this store sees `ACTIVE` false and wakes us.
            ACTIVE.store(false, Ordering::SeqCst);
            // SAFETY: pairs the successful `timeBeginPeriod(1)`.
            unsafe { timeEndPeriod(1) };
            debug!("timer boost: ended, idle");
        }
    }

    /// Something happened that a user waits on.
    pub fn note_activity() {
        let Some(Some(watcher)) = WATCHER.get() else { return };
        LAST.store(now_ms(), Ordering::SeqCst);
        if !ACTIVE.load(Ordering::SeqCst) {
            watcher.unpark();
        }
    }
}

#[cfg(windows)]
pub use imp::{init, note_activity};

/// Start the watcher thread. No-op off Windows.
#[cfg(not(windows))]
pub fn init() {}

/// Something happened that a user waits on. No-op off Windows.
#[cfg(not(windows))]
pub fn note_activity() {}
