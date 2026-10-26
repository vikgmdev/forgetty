//! What the server knows about the shell in a pane (P7.1, shell integration).
//!
//! Three things, all read from standards and the kernel, none from a VT parser:
//!
//! - the prompt state, from the OSC 133 marks the shell integration script (or
//!   the shell itself) writes: prompt start (`A`), command line start (`B`),
//!   command start (`C`) and command end (`D`, with the exit code);
//! - the directory the shell reports with OSC 7 (display only, never a spawn
//!   directory);
//! - the tty mode, from the pty's termios: `ICANON` and `ECHO`.
//!
//! The termios is never read per byte or per chunk. It is read with every
//! prompt mark (the moment the terminal changes hands), when a caller asks
//! ([`ShellTracker::read_tty`]), and on a short chain of checks after output:
//! [`TTY_CHECK_DELAYS`] after the last output of the pane. A program that
//! masks input after it printed a prompt is seen at the first check. One that
//! masks it a little later, without printing, is seen at a later one. Output
//! that keeps coming restarts the chain, so a flood is checked about every
//! 60 ms, not once per chunk. A pane that is quiet costs nothing: there is no
//! timer and no check until it writes again.

use std::time::Duration;

use crate::osc::MarkKind;

/// Delays of the tty checks after the last output of a pane.
pub const TTY_CHECK_DELAYS: [Duration; 4] = [
    Duration::from_millis(60),
    Duration::from_millis(250),
    Duration::from_millis(1000),
    Duration::from_millis(3000),
];

/// Where the shell is in its prompt cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// No OSC 133 mark seen: the shell has no integration.
    #[default]
    Unknown,
    /// After `A`: the shell draws the prompt.
    Prompt,
    /// After `B`: the shell waits for a command line.
    Input,
    /// After `C`: a command runs.
    Running,
    /// After `D`: the command ended, the next prompt is due.
    Done,
}

impl Phase {
    /// The wire name: `unknown`, `prompt`, `input`, `running` or `done`.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Unknown => "unknown",
            Phase::Prompt => "prompt",
            Phase::Input => "input",
            Phase::Running => "running",
            Phase::Done => "done",
        }
    }
}

/// The line settings of the pty (`ICANON` and `ECHO`).
///
/// | `canonical` | `echo` | Who is typing into what |
/// |---|---|---|
/// | on | on | the kernel edits the line and echoes it (`cat`, `read`) |
/// | on | off | a password-style read |
/// | off | on | one key at a time, echoed (`read -n1`) |
/// | off | off | the program edits (a shell line editor, a full screen program) |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtyMode {
    pub canonical: bool,
    pub echo: bool,
}

/// The shell state of a pane at one moment, as sent to clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSnapshot {
    /// Grows with every change of the state of this pane. A client keeps the
    /// highest it has seen and ignores an older one.
    pub rev: u64,
    pub phase: Phase,
    /// Exit code of the last command that ended with a valid code.
    pub exit_code: Option<i32>,
    /// OSC 7 directory, `None` before the shell reported one.
    pub cwd: Option<String>,
    /// `None` until the termios was read once (or when it cannot be read).
    pub tty: Option<TtyMode>,
    /// The mark that caused this state (`A` to `D`), `None` for a change of
    /// the cwd or the tty mode.
    pub mark: Option<MarkKind>,
    /// Unix time in milliseconds of the change.
    pub at_ms: u64,
}

/// Per-pane tracker. Lives in `PaneOsc`.
#[derive(Debug, Default)]
pub struct ShellTracker {
    rev: u64,
    phase: Phase,
    exit_code: Option<i32>,
    tty: Option<TtyMode>,
    /// The chain of delayed tty checks that is current. A chain with another
    /// number ends at its next wake.
    pub chain_epoch: u64,
    /// The first check of the current chain has not run yet.
    pub first_check_pending: bool,
    /// Termios reads so far (for tests: it must stay far below the number of
    /// output chunks).
    pub tty_reads: u64,
}

impl ShellTracker {
    fn snapshot(&mut self, cwd: Option<&str>, mark: Option<MarkKind>, at_ms: u64) -> ShellSnapshot {
        self.rev += 1;
        ShellSnapshot {
            rev: self.rev,
            phase: self.phase,
            exit_code: self.exit_code,
            cwd: cwd.map(str::to_string),
            tty: self.tty,
            mark,
            at_ms,
        }
    }

    /// The state as it is now, without a change (the revision stays).
    pub fn current(&self, cwd: Option<&str>) -> ShellSnapshot {
        ShellSnapshot {
            rev: self.rev,
            phase: self.phase,
            exit_code: self.exit_code,
            cwd: cwd.map(str::to_string),
            tty: self.tty,
            mark: None,
            at_ms: 0,
        }
    }

    /// Output arrived. Returns the number of a new chain of tty checks to
    /// start, or `None` when the first check of the current chain is still to
    /// come (it will see this output too). So a flood is read about every
    /// [`TTY_CHECK_DELAYS`]`[0]`, and output that stops is read once more that
    /// long after its last chunk.
    pub fn output(&mut self) -> Option<u64> {
        if self.first_check_pending {
            return None;
        }
        self.first_check_pending = true;
        self.chain_epoch += 1;
        Some(self.chain_epoch)
    }

    /// The first check of chain `epoch` runs now. `false` when a newer chain
    /// took over.
    pub fn first_check(&mut self, epoch: u64) -> bool {
        if self.chain_epoch != epoch {
            return false;
        }
        self.first_check_pending = false;
        true
    }

    /// `true` when nothing is known yet: no mark, no termios read.
    pub fn is_blank(&self) -> bool {
        self.phase == Phase::Unknown && self.tty.is_none()
    }

    /// An OSC 133 mark. Returns the new state when it changed anything.
    ///
    /// `tty` is a fresh termios read taken with the mark: a mark is the moment
    /// the mode is most likely to have changed (the shell hands the terminal to
    /// a command or takes it back), and the state it reports should not lag.
    pub fn mark(
        &mut self,
        kind: MarkKind,
        exit_code: Option<i32>,
        tty: Option<(bool, bool)>,
        cwd: Option<&str>,
        at_ms: u64,
    ) -> Option<ShellSnapshot> {
        self.tty_reads += 1;
        let tty_changed = match tty.map(|(canonical, echo)| TtyMode { canonical, echo }) {
            Some(mode) if self.tty != Some(mode) => {
                self.tty = Some(mode);
                true
            }
            _ => false,
        };
        let phase = match kind {
            MarkKind::PromptStart => Phase::Prompt,
            MarkKind::InputStart => Phase::Input,
            MarkKind::CommandStart => Phase::Running,
            MarkKind::CommandEnd => Phase::Done,
        };
        let code = if kind == MarkKind::CommandEnd { exit_code } else { None };
        if phase == self.phase && code.is_none() && !tty_changed {
            // A prompt redraw (`A` again) says nothing new.
            return None;
        }
        self.phase = phase;
        if code.is_some() {
            self.exit_code = code;
        }
        Some(self.snapshot(cwd, Some(kind), at_ms))
    }

    /// A new OSC 7 directory.
    pub fn cwd_changed(&mut self, cwd: &str, at_ms: u64) -> ShellSnapshot {
        self.snapshot(Some(cwd), None, at_ms)
    }

    /// The result of a termios read. Returns the new state when the mode
    /// changed. A failed read (`None`) keeps what was known.
    pub fn read_tty(
        &mut self,
        flags: Option<(bool, bool)>,
        cwd: Option<&str>,
        at_ms: u64,
    ) -> Option<ShellSnapshot> {
        self.tty_reads += 1;
        let mode = flags.map(|(canonical, echo)| TtyMode { canonical, echo })?;
        if self.tty == Some(mode) {
            return None;
        }
        self.tty = Some(mode);
        Some(self.snapshot(cwd, None, at_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_walk_the_prompt_cycle() {
        let mut t = ShellTracker::default();
        assert!(t.is_blank());
        let a = t.mark(MarkKind::PromptStart, None, None, None, 1).unwrap();
        assert_eq!((a.phase, a.rev), (Phase::Prompt, 1));
        // A redraw of the prompt changes nothing.
        assert!(t.mark(MarkKind::PromptStart, None, None, None, 2).is_none());
        assert_eq!(t.mark(MarkKind::InputStart, None, None, None, 3).unwrap().phase, Phase::Input);
        assert_eq!(
            t.mark(MarkKind::CommandStart, None, None, None, 4).unwrap().phase,
            Phase::Running
        );
        let d = t.mark(MarkKind::CommandEnd, Some(2), None, None, 5).unwrap();
        assert_eq!(
            (d.phase, d.exit_code, d.mark),
            (Phase::Done, Some(2), Some(MarkKind::CommandEnd))
        );
        // The code of the last command stays until the next one ends.
        let a = t.mark(MarkKind::PromptStart, None, None, None, 6).unwrap();
        assert_eq!((a.phase, a.exit_code), (Phase::Prompt, Some(2)));
        assert!(!t.is_blank());
    }

    #[test]
    fn output_starts_a_chain_when_no_first_check_is_waiting() {
        let mut t = ShellTracker::default();
        assert_eq!(t.output(), Some(1));
        assert_eq!(t.output(), None, "the first check of chain 1 is still to come");
        assert!(t.first_check(1));
        assert_eq!(t.output(), Some(2), "chain 1 had its first check: chain 2 starts");
        assert!(!t.first_check(1), "chain 1 is stale");
        assert!(t.first_check(2));
    }

    #[test]
    fn a_mark_carries_a_fresh_tty_read() {
        let mut t = ShellTracker::default();
        let b = t.mark(MarkKind::InputStart, None, Some((false, false)), None, 1).unwrap();
        assert_eq!(b.tty, Some(TtyMode { canonical: false, echo: false }));
        // The phase is the same, the mode is not: still a change.
        let again = t.mark(MarkKind::InputStart, None, Some((true, true)), None, 2).unwrap();
        assert_eq!(again.tty, Some(TtyMode { canonical: true, echo: true }));
        assert!(t.mark(MarkKind::InputStart, None, Some((true, true)), None, 3).is_none());
        assert!(t.mark(MarkKind::InputStart, None, None, None, 4).is_none());
    }

    #[test]
    fn tty_reads_report_only_changes() {
        let mut t = ShellTracker::default();
        assert!(t.read_tty(None, None, 0).is_none(), "a failed read changes nothing");
        let s = t.read_tty(Some((true, true)), None, 1).unwrap();
        assert_eq!(s.tty, Some(TtyMode { canonical: true, echo: true }));
        assert!(t.read_tty(Some((true, true)), None, 2).is_none());
        let s = t.read_tty(Some((true, false)), Some("/tmp"), 3).unwrap();
        assert_eq!((s.tty.unwrap().echo, s.cwd.as_deref(), s.mark), (false, Some("/tmp"), None));
        assert!(t.read_tty(None, None, 4).is_none());
        assert_eq!(t.current(None).tty, s.tty, "a failed read keeps the last mode");
    }
}
