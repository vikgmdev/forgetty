//! Timer resolution of the server process on Windows. Off by default.
//!
//! A process that never asks for a finer timer gets the default tick of 15.6 ms. The app asks for
//! 1 ms (`timeBeginPeriod(1)`). The server does not, because measuring showed no gain on the key
//! to output path (`tests/timer_probe.rs`, CI job `timer-probe`: the server adds about 0.35 ms
//! with or without it) and a 1 ms tick for as long as the server runs costs power. The code stays
//! as an opt-in for the next time a timer matters.
//!
//! `FORGETTY_TIMER_RES`: unset or `off` does nothing. `period` calls `timeBeginPeriod(1)` only.
//! `full` (or `on`) does that and also opts the process out of power throttling, because a
//! process without a window can be classified as background work, and then the system ignores
//! its timer request.

use tracing::{info, warn};
use windows::Win32::System::Threading::{
    GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
};

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

/// Ends the 1 ms request when dropped (the system also ends it when the process exits).
pub struct Guard {
    began: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.began {
            // SAFETY: pairs the successful `timeBeginPeriod(1)` of `begin`.
            unsafe { timeEndPeriod(1) };
        }
    }
}

/// Ask for a 1 ms timer for this process. Keep the guard for the life of the server.
pub fn begin() -> Guard {
    let mode = std::env::var("FORGETTY_TIMER_RES").unwrap_or_default();
    if !matches!(mode.as_str(), "period" | "full" | "on") {
        info!("timer resolution: left at the system default (FORGETTY_TIMER_RES={mode:?})");
        return Guard { began: false };
    }
    if mode != "period" {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            // A control bit with its state bit clear turns the throttling off.
            StateMask: 0,
        };
        // SAFETY: `state` is a valid PROCESS_POWER_THROTTLING_STATE of the size passed.
        let r = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if let Err(e) = r {
            // Older Windows 10 builds do not know the setting. The timer request still works.
            warn!("timer resolution: could not opt out of power throttling: {e}");
        }
    }
    // SAFETY: plain winmm call; the result is TIMERR_NOERROR (0) on success.
    let began = unsafe { timeBeginPeriod(1) } == 0;
    if began {
        info!("timer resolution: 1 ms requested ({})", if mode == "period" { "period only" } else { "with power throttling off" });
    } else {
        warn!("timer resolution: timeBeginPeriod(1) failed");
    }
    Guard { began }
}
