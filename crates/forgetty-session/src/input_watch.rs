//! Input stall watchdog.
//!
//! A pane whose input queue is not empty and whose writer has taken no byte
//! for [`STALL_AFTER`] is stalled. [`InputWatchdog::tick`] turns the per-pane
//! reports into events: one when a stall starts, one more every
//! [`REPEAT_AFTER`] while it lasts, and one when it clears. The report says
//! where the bytes wait: in the writer's `write()` (the program does not
//! read), in an idle writer (a wakeup bug), or behind a PTY reader that
//! stopped (the program cannot write, so it cannot read).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use forgetty_core::PaneId;

use crate::byte_log::ReaderState;
use crate::pane_writer::WriterStats;

/// No progress for this long, with bytes queued, is a stall.
pub const STALL_AFTER: Duration = Duration::from_secs(5);
/// A stall that goes on is logged again after this long.
pub const REPEAT_AFTER: Duration = Duration::from_secs(30);

/// Where one pane's input stands.
#[derive(Debug, Clone, Copy)]
pub struct InputReport {
    pub pane_id: PaneId,
    pub writer: WriterStats,
    pub reader: ReaderState,
    pub child_pid: Option<u32>,
    pub child_alive: bool,
}

impl InputReport {
    /// Bytes wait and nothing moved for [`STALL_AFTER`].
    pub fn stalled(&self) -> bool {
        self.writer.stalled(STALL_AFTER)
    }

    /// The program in the pane does not read its input: the writer is inside
    /// `write()`, the child lives and the PTY reader runs. That is the
    /// program's doing (`sleep`, a stuck job), so it is news, not a fault.
    /// Anything else (an idle writer with bytes queued, a PTY reader held in
    /// `wait_room`, a writer that exited, a dead child) points at the server.
    pub fn program_not_reading(&self) -> bool {
        self.writer.in_write.is_some()
            && !self.writer.exited
            && self.child_alive
            && self.reader == ReaderState::Reading
    }
}

/// Is the child process still running? Looks without reaping and without
/// needing the PTY object, so it can run outside any server lock. A child
/// that was reaped, or is not ours, counts as gone.
pub fn child_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: `info` is zeroed plain data that `waitid` fills in; the
        // flags make it return at once and leave the child waitable.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let rc = libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            // 0 with no pid in the answer: still running. 0 with the pid:
            // exited, not yet reaped. An error (ECHILD): reaped or not ours.
            rc == 0 && info.si_pid() == 0
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        // SAFETY: plain calls; the handle is closed before returning. A process that cannot be
        // opened is gone (or not ours).
        unsafe {
            match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                Ok(h) => {
                    let running = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
                    let _ = CloseHandle(h);
                    running
                }
                Err(_) => false,
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

impl std::fmt::Display for InputReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let w = &self.writer;
        write!(
            f,
            "pane {}: {} bytes queued in {} frames, no progress for {:.1} s, {}, \
             pty reader {}, child pid {} {}",
            self.pane_id,
            w.queued_bytes,
            w.frames,
            w.since_progress.as_secs_f64(),
            w.writer_state(),
            self.reader,
            self.child_pid.map_or_else(|| "unknown".to_string(), |p| p.to_string()),
            if self.child_alive { "alive" } else { "dead" },
        )
    }
}

/// What the watchdog found.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// A stall began, or went on for [`REPEAT_AFTER`] since the last event.
    Stalled(InputReport),
    /// A pane that was reported as stalled moves again (or is gone).
    Cleared { pane_id: PaneId, stalled_for: Duration },
}

struct Flagged {
    since: Instant,
    last_logged: Instant,
}

/// Keeps what was reported, so each stall is logged once, again after
/// [`REPEAT_AFTER`], and once when it clears.
#[derive(Default)]
pub struct InputWatchdog {
    flagged: HashMap<PaneId, Flagged>,
}

impl InputWatchdog {
    pub fn tick(&mut self, now: Instant, reports: &[InputReport]) -> Vec<WatchEvent> {
        let mut events = Vec::new();
        for r in reports {
            if r.stalled() {
                match self.flagged.get_mut(&r.pane_id) {
                    None => {
                        self.flagged.insert(r.pane_id, Flagged { since: now, last_logged: now });
                        events.push(WatchEvent::Stalled(*r));
                    }
                    Some(f) if now.duration_since(f.last_logged) >= REPEAT_AFTER => {
                        f.last_logged = now;
                        events.push(WatchEvent::Stalled(*r));
                    }
                    Some(_) => {}
                }
            } else if let Some(f) = self.flagged.remove(&r.pane_id) {
                events.push(WatchEvent::Cleared {
                    pane_id: r.pane_id,
                    stalled_for: now.duration_since(f.since),
                });
            }
        }
        // Panes that left the list.
        let gone: Vec<PaneId> = self
            .flagged
            .keys()
            .filter(|id| !reports.iter().any(|r| r.pane_id == **id))
            .copied()
            .collect();
        for id in gone {
            if let Some(f) = self.flagged.remove(&id) {
                events.push(WatchEvent::Cleared {
                    pane_id: id,
                    stalled_for: now.duration_since(f.since),
                });
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(id: PaneId, queued: usize, since_progress: Duration) -> InputReport {
        InputReport {
            pane_id: id,
            writer: WriterStats {
                queued_bytes: queued,
                frames: usize::from(queued > 0),
                in_write: (queued > 0).then_some(since_progress),
                since_progress,
                exited: false,
            },
            reader: ReaderState::Reading,
            child_pid: Some(1),
            child_alive: true,
        }
    }

    #[test]
    fn a_stall_is_logged_once_again_after_30_s_and_once_when_it_clears() {
        let id = PaneId::new();
        let mut dog = InputWatchdog::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        // Moving, or idle with an empty queue: nothing.
        assert!(dog.tick(at(0), &[report(id, 10, Duration::from_secs(1))]).is_empty());
        assert!(dog.tick(at(1), &[report(id, 0, Duration::from_secs(60))]).is_empty());
        // 5 s without progress: one event.
        let ev = dog.tick(at(6), &[report(id, 10, Duration::from_secs(5))]);
        assert!(matches!(ev.as_slice(), [WatchEvent::Stalled(_)]));
        // Still stalled: quiet for 30 s.
        assert!(dog.tick(at(20), &[report(id, 10, Duration::from_secs(19))]).is_empty());
        assert!(dog.tick(at(35), &[report(id, 10, Duration::from_secs(34))]).is_empty());
        let ev = dog.tick(at(36), &[report(id, 10, Duration::from_secs(35))]);
        assert!(matches!(ev.as_slice(), [WatchEvent::Stalled(_)]));
        // Clears: one event, then nothing.
        let ev = dog.tick(at(40), &[report(id, 0, Duration::from_secs(0))]);
        assert!(matches!(ev.as_slice(), [WatchEvent::Cleared { .. }]));
        assert!(dog.tick(at(41), &[report(id, 0, Duration::from_secs(1))]).is_empty());
        // A pane that disappears while stalled clears too.
        dog.tick(at(50), &[report(id, 10, Duration::from_secs(9))]);
        let ev = dog.tick(at(51), &[]);
        assert!(matches!(ev.as_slice(), [WatchEvent::Cleared { .. }]));
    }

    #[test]
    fn only_a_program_that_does_not_read_is_news() {
        let id = PaneId::new();
        let r = report(id, 5, Duration::from_secs(7));
        assert!(r.program_not_reading(), "blocked write, child alive, reader running");
        let mut idle = r;
        idle.writer.in_write = None;
        assert!(!idle.program_not_reading(), "idle writer with a queue is a wakeup bug");
        let mut full = r;
        full.reader = ReaderState::WaitingForRoom(Duration::from_secs(6));
        assert!(!full.program_not_reading(), "a held PTY reader is the server's");
        let mut gone = r;
        gone.writer.exited = true;
        assert!(!gone.program_not_reading(), "an exited writer is a fault");
        let mut dead = r;
        dead.child_alive = false;
        assert!(!dead.program_not_reading(), "a dead child is not just a quiet program");
    }

    #[cfg(unix)]
    #[test]
    fn child_alive_sees_a_running_and_an_exited_child() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        assert!(child_alive(pid));
        child.kill().unwrap();
        // Exited but not reaped yet: already gone for the check.
        let t0 = std::time::Instant::now();
        while child_alive(pid) {
            assert!(t0.elapsed() < Duration::from_secs(5), "still alive after the kill");
            std::thread::sleep(Duration::from_millis(10));
        }
        child.wait().unwrap();
        assert!(!child_alive(pid), "reaped");
    }

    #[test]
    fn the_report_names_the_writer_state() {
        let id = PaneId::new();
        let r = report(id, 5, Duration::from_secs(7));
        assert!(r.to_string().contains("writer blocked in write()"), "{r}");
        let mut idle = r;
        idle.writer.in_write = None;
        assert!(idle.to_string().contains("wakeup bug"), "{idle}");
    }
}
