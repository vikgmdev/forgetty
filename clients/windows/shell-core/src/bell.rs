//! Bell policy. GTK: `bell_mode` visual (default), audio, both or none; a visual bell is a
//! 150 ms white wash over the pane; bells closer than 200 ms are one bell; the bell that the
//! shell echoes after Ctrl+C is dropped.

use std::time::{Duration, Instant};

pub const FLASH: Duration = Duration::from_millis(150);
pub const MIN_GAP: Duration = Duration::from_millis(200);
pub const CTRL_C_QUIET: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BellMode {
    #[default]
    Visual,
    Audio,
    Both,
    None,
}

impl BellMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "visual" => Some(Self::Visual),
            "audio" => Some(Self::Audio),
            "both" => Some(Self::Both),
            "none" => Some(Self::None),
            _ => None,
        }
    }
    pub fn visual(self) -> bool {
        matches!(self, Self::Visual | Self::Both)
    }
    pub fn audio(self) -> bool {
        matches!(self, Self::Audio | Self::Both)
    }
}

/// Decides which bells count.
#[derive(Default, Debug)]
pub struct BellGate {
    last: Option<Instant>,
    quiet_until: Option<Instant>,
}

impl BellGate {
    /// A bell arrived. True when it should be shown and heard.
    pub fn ring(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|t| now.saturating_duration_since(t) < MIN_GAP) {
            return false;
        }
        if self.quiet_until.is_some_and(|t| now < t) {
            // Only the first bell after Ctrl+C is dropped.
            self.quiet_until = None;
            return false;
        }
        self.last = Some(now);
        true
    }

    /// The user pressed Ctrl+C: the next bell within `CTRL_C_QUIET` is the program's reply.
    pub fn quiet_after_ctrl_c(&mut self, now: Instant) {
        self.quiet_until = Some(now + CTRL_C_QUIET);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(BellMode::parse(" Both "), Some(BellMode::Both));
        assert_eq!(BellMode::parse("loud"), None);
        assert!(BellMode::default().visual() && !BellMode::default().audio());
        assert!(BellMode::Both.visual() && BellMode::Both.audio());
        assert!(!BellMode::None.visual() && !BellMode::None.audio());
        assert!(!BellMode::Audio.visual() && BellMode::Audio.audio());
    }

    #[test]
    fn bells_closer_than_200ms_are_one() {
        let mut g = BellGate::default();
        let t = Instant::now();
        assert!(g.ring(t));
        assert!(!g.ring(t + Duration::from_millis(50)));
        assert!(!g.ring(t + Duration::from_millis(199)));
        assert!(g.ring(t + Duration::from_millis(250)));
    }

    #[test]
    fn the_bell_after_ctrl_c_is_dropped_once() {
        let mut g = BellGate::default();
        let t = Instant::now();
        g.quiet_after_ctrl_c(t);
        assert!(!g.ring(t + Duration::from_millis(100)));
        assert!(g.ring(t + Duration::from_millis(400)), "the quiet period is used up");
        let mut g = BellGate::default();
        g.quiet_after_ctrl_c(t);
        assert!(g.ring(t + Duration::from_millis(400)), "it also ends by time");
    }
}
