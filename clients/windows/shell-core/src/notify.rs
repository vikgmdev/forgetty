//! Toast notification policy. No OS code: the decision (who gets a toast),
//! the text (pane output is untrusted), the rate limit and the activation
//! argument that carries a click back to its pane.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use forgetty_core::PaneId;
use uuid::Uuid;

use crate::backend::PaneStatus;
use crate::config::NotificationMode;

/// What the user can see right now.
#[derive(Clone, Copy, Debug)]
pub struct Visibility {
    /// The Forgetty window is the foreground window.
    pub window_active: bool,
    /// The pane's machine is the one on screen.
    pub machine_active: bool,
    /// The pane is the focused pane of the visible tab.
    pub pane_focused: bool,
}

impl Visibility {
    pub fn user_is_looking(&self) -> bool {
        self.window_active && self.machine_active && self.pane_focused
    }
}

/// A status change raises a toast when it asks for the user (waiting, done,
/// failed), it is a change, and the user is not looking at the pane.
pub fn status_wants_toast(prev: PaneStatus, now: PaneStatus, vis: Visibility) -> bool {
    now.wants_attention() && now != prev && !vis.user_is_looking()
}

/// A program notification (OSC 9/99/777) raises a toast unless the user is
/// looking at the pane.
pub fn notification_wants_toast(vis: Visibility) -> bool {
    !vis.user_is_looking()
}

/// `notification_mode` as GTK reads it: `all` is ring, tab badge and desktop toast,
/// `ringonly` drops the toast, `none` drops all three.
pub fn mode_allows_ring(mode: NotificationMode) -> bool {
    mode != NotificationMode::None
}

pub fn mode_allows_toast(mode: NotificationMode) -> bool {
    mode == NotificationMode::All
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastText {
    pub title: String,
    pub lines: Vec<String>,
}

/// The longest notice shown at the bottom of a pane.
pub const NOTICE_MAX: usize = 160;

/// The words of a pane notice. Error text can come from the server or the wire, so every
/// notice goes through this: no control characters, no bidi overrides, one line, bounded.
pub fn notice_text(s: &str) -> String {
    sanitize(s, NOTICE_MAX)
}

/// Text from a pane is untrusted: no control characters, no bidi overrides,
/// bounded length.
pub fn sanitize(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for c in s.chars() {
        let bad = c.is_control() && !c.is_whitespace()
            || matches!(
                c,
                '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{061c}'
                    | '\u{2060}'..='\u{2064}'
                    | '\u{feff}'
            );
        let c = if bad {
            continue;
        } else if c.is_whitespace() {
            ' '
        } else {
            c
        };
        if c == ' ' && last_space {
            continue;
        }
        last_space = c == ' ';
        out.push(c);
    }
    let out = out.trim().to_string();
    if out.chars().count() > max {
        let cut: String = out.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}\u{2026}")
    } else {
        out
    }
}

pub fn status_text(machine: &str, tab: &str, status: PaneStatus, message: &str) -> ToastText {
    let what = match status {
        PaneStatus::Waiting => "Waiting for input",
        PaneStatus::Done => "Finished",
        PaneStatus::Failed => "Failed",
        _ => "Status changed",
    };
    let mut lines = vec![what.to_string()];
    let m = sanitize(message, 160);
    if !m.is_empty() {
        lines[0] = format!("{what}: {m}");
    }
    lines.push(format!("on {}", sanitize(machine, 48)));
    ToastText { title: nonempty(sanitize(tab, 60), "Terminal"), lines }
}

pub fn note_text(machine: &str, tab: &str, title: &str, body: &str) -> ToastText {
    let t = sanitize(title, 60);
    let b = sanitize(body, 200);
    let (title, mut lines) = match (t.is_empty(), b.is_empty()) {
        (false, false) => (t, vec![b]),
        (false, true) => (t, Vec::new()),
        (true, false) => (nonempty(sanitize(tab, 60), "Terminal"), vec![b]),
        (true, true) => (nonempty(sanitize(tab, 60), "Terminal"), vec!["Notification".to_string()]),
    };
    lines.push(format!("on {}", sanitize(machine, 48)));
    ToastText { title, lines }
}

fn nonempty(s: String, fallback: &str) -> String {
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

/// Limits toasts per pane and overall, so a noisy pane cannot flood the desktop.
#[derive(Default)]
pub struct ToastLimiter {
    per_pane: HashMap<(Uuid, PaneId), Instant>,
    recent: VecDeque<Instant>,
}

impl ToastLimiter {
    const PER_PANE: Duration = Duration::from_secs(3);
    const WINDOW: Duration = Duration::from_secs(30);
    const MAX_IN_WINDOW: usize = 10;

    pub fn allow(&mut self, machine: Uuid, pane: PaneId, now: Instant) -> bool {
        while self.recent.front().is_some_and(|t| now.saturating_duration_since(*t) > Self::WINDOW) {
            self.recent.pop_front();
        }
        if self.recent.len() >= Self::MAX_IN_WINDOW {
            return false;
        }
        if let Some(last) = self.per_pane.get(&(machine, pane)) {
            if now.saturating_duration_since(*last) < Self::PER_PANE {
                return false;
            }
        }
        if self.per_pane.len() > 256 {
            self.per_pane.retain(|_, t| now.saturating_duration_since(*t) < Self::WINDOW);
        }
        self.per_pane.insert((machine, pane), now);
        self.recent.push_back(now);
        true
    }
}

/// The activation argument of a toast: where a click goes.
pub fn encode_target(machine: Uuid, pane: PaneId) -> String {
    format!("forgetty:focus?m={}&p={}", machine.hyphenated(), pane.0.hyphenated())
}

/// Strict inverse of `encode_target`. Anything else is not ours.
pub fn parse_target(s: &str) -> Option<(Uuid, PaneId)> {
    let rest = s.strip_prefix("forgetty:focus?m=")?;
    let (m, p) = rest.split_once("&p=")?;
    if m.len() != 36 || p.len() != 36 {
        return None;
    }
    Some((Uuid::parse_str(m).ok()?, PaneId(Uuid::parse_str(p).ok()?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_mode_decides_ring_and_toast() {
        assert!(mode_allows_ring(NotificationMode::All) && mode_allows_toast(NotificationMode::All));
        assert!(mode_allows_ring(NotificationMode::RingOnly) && !mode_allows_toast(NotificationMode::RingOnly));
        assert!(!mode_allows_ring(NotificationMode::None) && !mode_allows_toast(NotificationMode::None));
    }

    const AWAY: Visibility = Visibility { window_active: false, machine_active: true, pane_focused: true };
    const LOOKING: Visibility = Visibility { window_active: true, machine_active: true, pane_focused: true };

    #[test]
    fn sanitize_drops_bidi_zero_width_and_controls() {
        assert_eq!(sanitize("a\u{202e}b\u{200b}c\u{feff}d\u{7}e\u{2066}f", 40), "abcdef");
        assert_eq!(sanitize("  two   words\tand\nlines ", 40), "two words and lines");
        assert_eq!(sanitize(&"x".repeat(100), 10).chars().count(), 10);
    }

    #[test]
    fn status_rules() {
        use PaneStatus::*;
        assert!(status_wants_toast(Working, Done, AWAY));
        assert!(status_wants_toast(Working, Waiting, AWAY));
        assert!(status_wants_toast(Working, Failed, AWAY));
        assert!(!status_wants_toast(Done, Done, AWAY), "no repeat");
        assert!(!status_wants_toast(None, Working, AWAY));
        assert!(!status_wants_toast(Working, Idle, AWAY));
        assert!(!status_wants_toast(Working, Done, LOOKING));
        // Each of the three conditions alone brings the toast back.
        assert!(status_wants_toast(Working, Done, Visibility { pane_focused: false, ..LOOKING }));
        assert!(status_wants_toast(Working, Done, Visibility { machine_active: false, ..LOOKING }));
        assert!(notification_wants_toast(AWAY));
        assert!(!notification_wants_toast(LOOKING));
    }

    #[test]
    fn text_is_cleaned() {
        assert_eq!(sanitize("a\u{1b}[31mb\u{7}\nc\u{202e}d  e", 50), "a[31mb cd e");
        assert_eq!(sanitize("abcdef", 4), "abc\u{2026}");
        let t = status_text("box\u{1b}", "", PaneStatus::Done, "exit\r\n0");
        assert_eq!(t.title, "Terminal");
        assert_eq!(t.lines, vec!["Finished: exit 0".to_string(), "on box".to_string()]);
        let n = note_text("m", "tab", "", "");
        assert_eq!(n.title, "tab");
        assert_eq!(n.lines[0], "Notification");
    }

    #[test]
    fn limiter() {
        let (m, p, q) = (Uuid::new_v4(), PaneId::new(), PaneId::new());
        let mut l = ToastLimiter::default();
        let t0 = Instant::now();
        assert!(l.allow(m, p, t0));
        assert!(!l.allow(m, p, t0 + Duration::from_secs(1)));
        assert!(l.allow(m, q, t0 + Duration::from_secs(1)));
        assert!(l.allow(m, p, t0 + Duration::from_secs(4)));
        // Global cap: 10 in 30 s.
        let mut l = ToastLimiter::default();
        for i in 0..10 {
            assert!(l.allow(m, PaneId::new(), t0 + Duration::from_millis(i)));
        }
        assert!(!l.allow(m, PaneId::new(), t0 + Duration::from_secs(1)));
        assert!(l.allow(m, PaneId::new(), t0 + Duration::from_secs(31)));
    }

    #[test]
    fn target_roundtrip_and_strictness() {
        let (m, p) = (Uuid::new_v4(), PaneId::new());
        assert_eq!(parse_target(&encode_target(m, p)), Some((m, p)));
        assert_eq!(parse_target("forgetty:focus?m=x&p=y"), None);
        assert_eq!(parse_target("calc.exe"), None);
        assert_eq!(parse_target(&format!("{}&extra", encode_target(m, p))), None);
    }

    #[test]
    fn notices_lose_control_bidi_and_newlines_and_are_cut() {
        let t = notice_text("Failed: bad\n\x1b[31mred\x07\u{202e}gnp.exe\u{2066}\r\nnext\tline\u{200b}");
        assert!(!t.contains('\n') && !t.contains('\r') && !t.contains('\x1b') && !t.contains('\x07'), "{t:?}");
        assert!(!t.contains('\u{202e}') && !t.contains('\u{2066}') && !t.contains('\u{200b}'), "{t:?}");
        assert!(t.starts_with("Failed: bad") && t.contains("next line"), "{t:?}");
        let long = notice_text(&"x".repeat(1000));
        assert_eq!(long.chars().count(), NOTICE_MAX);
        assert!(long.ends_with('\u{2026}'));
    }
}
