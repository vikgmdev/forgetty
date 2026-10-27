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
//! starts it, and it ends [`IDLE`] after the last activity. The call on the key path is one
//! relaxed atomic store once the request is running.
//!
//! `FORGETTY_TIMER_RES=off` turns it off. `period` and `full` hold a 1 ms tick for the life of
//! the server (`forgetty-server`'s `timerres`), so there is nothing for this module to do.
//! Not Windows: no-op.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use tracing::debug;

    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
        fn timeEndPeriod(period: u32) -> u32;
    }

    /// How long after the last activity the 1 ms request ends.
    const IDLE: Duration = Duration::from_secs(10);

    static ENABLED: OnceLock<bool> = OnceLock::new();
    static BASE: OnceLock<Instant> = OnceLock::new();
    /// Milliseconds since `BASE` of the last activity, plus one.
    static LAST: AtomicU64 = AtomicU64::new(0);
    static HOLDING: AtomicBool = AtomicBool::new(false);

    fn enabled() -> bool {
        *ENABLED.get_or_init(|| {
            !matches!(
                std::env::var("FORGETTY_TIMER_RES").unwrap_or_default().as_str(),
                "off" | "period" | "full" | "on"
            )
        })
    }

    fn now_ms() -> u64 {
        BASE.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
    }

    /// Something happened that a user waits on.
    pub fn note_activity() {
        if !enabled() {
            return;
        }
        LAST.store(now_ms(), Ordering::Relaxed);
        if HOLDING.load(Ordering::Relaxed) || HOLDING.swap(true, Ordering::AcqRel) {
            return;
        }
        begin_and_watch();
    }

    /// Requests 1 ms and starts the thread that ends it after the idle time. The caller flipped
    /// `HOLDING` from false to true, so exactly one request and one thread exist at a time.
    fn begin_and_watch() {
        // SAFETY: plain winmm call; TIMERR_NOERROR is 0.
        if unsafe { timeBeginPeriod(1) } != 0 {
            HOLDING.store(false, Ordering::Release);
            return;
        }
        debug!("timer boost: 1 ms requested");
        let spawned = std::thread::Builder::new().name("timer-boost".into()).spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            let idle = now_ms().saturating_sub(LAST.load(Ordering::Relaxed));
            if idle < IDLE.as_millis() as u64 {
                continue;
            }
            // SAFETY: pairs the successful `timeBeginPeriod(1)`.
            unsafe { timeEndPeriod(1) };
            HOLDING.store(false, Ordering::Release);
            debug!("timer boost: ended, idle");
            // Activity between the check and the release: take the request again.
            let idle = now_ms().saturating_sub(LAST.load(Ordering::Relaxed));
            if idle < IDLE.as_millis() as u64 && !HOLDING.swap(true, Ordering::AcqRel) {
                // SAFETY: plain winmm call.
                if unsafe { timeBeginPeriod(1) } == 0 {
                    continue;
                }
                HOLDING.store(false, Ordering::Release);
            }
            return;
        });
        if spawned.is_err() {
            // SAFETY: pairs the successful `timeBeginPeriod(1)`.
            unsafe { timeEndPeriod(1) };
            HOLDING.store(false, Ordering::Release);
        }
    }
}

#[cfg(windows)]
pub use imp::note_activity;

/// Something happened that a user waits on. No-op off Windows.
#[cfg(not(windows))]
pub fn note_activity() {}
