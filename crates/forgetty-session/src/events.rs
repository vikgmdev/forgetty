//! Session event types and OSC notification definitions.
//!
//! `SessionEvent` is the broadcast channel payload for future consumers (daemon,
//! Android client, MCP server). `NotificationPayload` and `NotificationSource`
//! describe OSC protocol events; they originated in `forgetty-gtk` but are
//! platform-agnostic so they live here.

use bytes::Bytes;
use forgetty_core::PaneId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Which OSC protocol triggered this notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationSource {
    /// OSC 9 (ConEmu/Windows Terminal style).
    Osc9,
    /// OSC 99 (Kitty notification protocol, simplified).
    Osc99,
    /// OSC 777 (DesktopNotify/URxvt style).
    Osc777,
}

/// Payload produced by the OSC notification scanner.
///
/// Passed from the PTY scanner to the GTK layer via the `on_notify` callback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationPayload {
    /// Notification summary/title for the desktop notification.
    pub title: String,
    /// Notification body text.
    pub body: String,
    /// Widget name of the DrawingArea that emitted this notification.
    pub pane_name: String,
    /// Which protocol triggered this notification (None for BEL).
    pub source: Option<NotificationSource>,
}

/// Events broadcast to subscribers (daemon, Android, MCP server, etc.).
///
/// In T-048 GTK does not yet consume this channel — it still polls via
/// `drain_output()`. The channel is wired now so T-050 can activate it
/// without further structural changes.
///
/// As of T-063 this channel also carries layout mutation events
/// (`TabCreated`, `TabClosed`, `PaneSplit`, `TabMoved`, `ActiveTabChanged`).
/// Subscribers that only care about one class of event should filter in the
/// receive loop.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Raw output bytes from a pane's PTY.
    PtyOutput { pane_id: PaneId, data: Bytes },
    /// A new pane was created.
    PaneCreated { pane_id: PaneId },
    /// A pane was closed.
    PaneClosed { pane_id: PaneId },
    /// An OSC notification was detected in the PTY stream.
    Notification { pane_id: PaneId, payload: NotificationPayload },

    // -----------------------------------------------------------------------
    // Layout mutation events (T-063)
    // -----------------------------------------------------------------------

    /// A new tab was created in the given workspace.
    TabCreated {
        workspace_idx: usize,
        tab_id: Uuid,
        pane_id: PaneId,
    },

    /// A tab was closed (all its panes have been killed).
    TabClosed {
        workspace_idx: usize,
        tab_id: Uuid,
    },

    /// An existing pane was split, producing a new sibling pane.
    PaneSplit {
        tab_id: Uuid,
        parent_pane_id: PaneId,
        new_pane_id: PaneId,
        /// "horizontal" | "vertical"
        direction: String,
    },

    /// A tab was moved to a new position within its workspace.
    TabMoved {
        workspace_idx: usize,
        tab_id: Uuid,
        new_index: usize,
    },

    /// The active tab index changed for a workspace.
    ActiveTabChanged {
        workspace_idx: usize,
        tab_idx: usize,
    },

    // -----------------------------------------------------------------------
    // Workspace mutation events (T-067)
    // -----------------------------------------------------------------------

    /// A new workspace was created.
    WorkspaceCreated {
        workspace_idx: usize,
        workspace_id: Uuid,
        name: String,
    },
}

// ---------------------------------------------------------------------------
// OSC notification scanner (moved from forgetty-gtk/src/terminal.rs)
// ---------------------------------------------------------------------------

/// Scan raw PTY bytes for OSC 9, OSC 99, or OSC 777 notification sequences.
///
/// This is an O(n) byte scan with no heap allocation in the common case
/// (no ESC ] bytes present). Called on every PTY chunk before feeding to
/// the VT parser, so it must stay fast.
///
/// Returns the first notification found in the buffer, or `None`.
///
/// Handles both BEL (`\x07`) and ST (`\x1b\`) terminators per ECMA-48.
pub fn scan_osc_notification(data: &[u8]) -> Option<NotificationPayload> {
    let len = data.len();
    let mut i = 0;

    while i + 1 < len {
        // Look for ESC ] (OSC start)
        if data[i] != 0x1b || data[i + 1] != b']' {
            i += 1;
            continue;
        }

        // Found ESC ] at position i -- find the terminator (BEL or ST).
        let osc_start = i + 2; // byte after ESC ]

        // Scan forward for BEL (0x07) or ST (ESC \)
        let term_pos = {
            let mut pos = None;
            let mut j = osc_start;
            while j < len {
                if data[j] == 0x07 {
                    pos = Some((j, false)); // BEL terminator
                    break;
                }
                if j + 1 < len && data[j] == 0x1b && data[j + 1] == b'\\' {
                    pos = Some((j, true)); // ST terminator (ESC \)
                    break;
                }
                j += 1;
            }
            pos
        };

        let (term_idx, _is_st) = match term_pos {
            Some(t) => t,
            None => break, // unterminated OSC -- bail out
        };

        let osc_body = &data[osc_start..term_idx];

        if let Some(payload) = parse_osc_body(osc_body) {
            return Some(payload);
        }

        // Advance past the terminator
        i = term_idx + 1;
    }

    None
}

/// Parse an OSC body (bytes between ESC ] and the terminator).
///
/// Checks for OSC 9, 99, or 777 notification formats and extracts title/body.
/// Returns `None` for any other OSC sequence.
fn parse_osc_body(body: &[u8]) -> Option<NotificationPayload> {
    // OSC 777 ; notify ; <title> ; <body>
    if body.starts_with(b"777;notify;") {
        let rest = &body[b"777;notify;".len()..];
        let (title, notif_body) = if let Some(sep) = rest.iter().position(|&b| b == b';') {
            let title = String::from_utf8_lossy(&rest[..sep]).into_owned();
            let body_text = String::from_utf8_lossy(&rest[sep + 1..]).into_owned();
            (title, body_text)
        } else {
            (String::from_utf8_lossy(rest).into_owned(), String::new())
        };
        return Some(NotificationPayload {
            title,
            body: notif_body,
            pane_name: String::new(),
            source: Some(NotificationSource::Osc777),
        });
    }

    // OSC 99 ; <params> ; <title/body>  (Kitty notification, simplified)
    if body.starts_with(b"99;") {
        let rest = &body[b"99;".len()..];
        let text = if let Some(sep) = rest.iter().rposition(|&b| b == b';') {
            String::from_utf8_lossy(&rest[sep + 1..]).into_owned()
        } else {
            String::from_utf8_lossy(rest).into_owned()
        };
        return Some(NotificationPayload {
            title: "Forgetty".to_string(),
            body: text,
            pane_name: String::new(),
            source: Some(NotificationSource::Osc99),
        });
    }

    // OSC 9 ; <text>  (ConEmu style)
    // IMPORTANT: skip OSC 9;4;<percent> which is the ConEmu progress bar protocol.
    if body.starts_with(b"9;") {
        let rest = &body[b"9;".len()..];
        // Skip progress bar sequences: 9;4;<n> or 9;4
        if rest.starts_with(b"4;") || rest == b"4" {
            return None;
        }
        let text = String::from_utf8_lossy(rest).into_owned();
        return Some(NotificationPayload {
            title: "Forgetty".to_string(),
            body: text,
            pane_name: String::new(),
            source: Some(NotificationSource::Osc9),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_osc9_notification() {
        let data = b"\x1b]9;hello world\x07";
        let result = scan_osc_notification(data);
        assert!(result.is_some());
        let p = result.unwrap();
        assert_eq!(p.body, "hello world");
        assert_eq!(p.source, Some(NotificationSource::Osc9));
    }

    #[test]
    fn test_scan_osc777_notification() {
        let data = b"\x1b]777;notify;My Title;My Body\x07";
        let result = scan_osc_notification(data);
        assert!(result.is_some());
        let p = result.unwrap();
        assert_eq!(p.title, "My Title");
        assert_eq!(p.body, "My Body");
        assert_eq!(p.source, Some(NotificationSource::Osc777));
    }

    #[test]
    fn test_scan_no_notification() {
        let data = b"hello world\nno osc here";
        let result = scan_osc_notification(data);
        assert!(result.is_none());
    }

    #[test]
    fn test_skip_progress_bar_osc9() {
        let data = b"\x1b]9;4;50\x07";
        let result = scan_osc_notification(data);
        assert!(result.is_none());
    }
}
