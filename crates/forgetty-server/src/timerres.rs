//! Timer resolution of the server process on Windows: the explicit settings.
//!
//! By default the server asks for a 1 ms timer only while a pane is in use
//! (`forgetty_pty::timer_boost`). A shell such as PowerShell waits on a timer, and its key echo
//! takes 14 to 16 ms at the default 15.6 ms tick and 9.4 ms at 1 ms (`tests/timer_probe.rs`, CI
//! job `timer-probe`). A tick of 1 ms for as long as the server runs costs power.
//!
//! `FORGETTY_TIMER_RES`: unset leaves it to `timer_boost`. `off` turns that off too. `period`
//! calls `timeBeginPeriod(1)` once for the life of the server. `full` (or `on`) does that and also
//! opts the process out of power throttling, because a process without a window can be classified
//! as background work, and then the system ignores its timer request.

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
    // Execution speed throttling (EcoQoS) for the server, on request (`FORGETTY_POWER_THROTTLING`
    // = `server` or `shells`, see `forgetty_pty::PowerThrottling`).
    if forgetty_pty::power_throttling_mode() != forgetty_pty::PowerThrottling::Off {
        // SAFETY: the pseudo handle of this process.
        match forgetty_pty::opt_out_of_power_throttling(unsafe { GetCurrentProcess() }) {
            Ok((control, state)) => info!(
                "server power throttling: ControlMask {control:#x}, StateMask {state:#x} read back"
            ),
            Err(e) => warn!("server power throttling opt-out failed: {e}"),
        }
    }
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
