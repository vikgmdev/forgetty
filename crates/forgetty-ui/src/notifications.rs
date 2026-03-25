//! Desktop notification integration.
//!
//! Handles detecting notifications triggered by terminal escape
//! sequences (OSC 9 / OSC 777) and bell characters.

use forgetty_vt::TerminalEvent;

/// A notification detected from terminal output.
#[derive(Debug, Clone)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub urgency: Urgency,
}

/// Notification urgency level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

/// Check terminal events for notifications (Bell character).
///
/// Returns a list of notifications that should be shown to the user.
pub fn check_notifications(events: &[TerminalEvent]) -> Vec<Notification> {
    let mut notifications = Vec::new();

    for event in events {
        match event {
            TerminalEvent::Bell => {
                notifications.push(Notification {
                    title: "Terminal Bell".to_string(),
                    body: "Bell triggered in terminal".to_string(),
                    urgency: Urgency::Normal,
                });
            }
            TerminalEvent::TitleChanged(title) => {
                // Some programs use title changes as a form of notification.
                // We emit a low-urgency notification for tracking, but don't
                // push it to the desktop.
                notifications.push(Notification {
                    title: "Title Changed".to_string(),
                    body: title.clone(),
                    urgency: Urgency::Low,
                });
            }
            _ => {}
        }
    }

    notifications
}
