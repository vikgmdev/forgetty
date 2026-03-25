//! Input event processing.
//!
//! Translates winit keyboard events into terminal input byte sequences.
//! Uses `event.text` for regular character input (correct with shift/caps)
//! and `event.logical_key` for named keys (arrows, function keys, etc.)
//! and Ctrl+key combos.

use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Encode a winit key event to terminal bytes.
///
/// Returns `None` if the key event should not produce terminal output
/// (e.g., key release events, modifier-only keys).
pub fn encode_key(event: &KeyEvent, modifiers: ModifiersState) -> Option<Vec<u8>> {
    // Only handle key presses, not releases.
    if event.state != ElementState::Pressed {
        return None;
    }

    let ctrl = modifiers.control_key();

    // 1. Handle Ctrl+key combos first (these produce control characters)
    if ctrl {
        if let Key::Character(c) = &event.logical_key {
            let bytes = c.as_str().as_bytes();
            if bytes.len() == 1 {
                let b = bytes[0].to_ascii_lowercase();
                match b {
                    b'a'..=b'z' => return Some(vec![b - b'a' + 1]),
                    b'[' => return Some(vec![0x1b]),
                    b'\\' => return Some(vec![0x1c]),
                    b']' => return Some(vec![0x1d]),
                    b'^' => return Some(vec![0x1e]),
                    b'_' => return Some(vec![0x1f]),
                    b'@' => return Some(vec![0x00]),
                    _ => {}
                }
            }
        }
        // Ctrl+Space
        if event.logical_key == Key::Named(NamedKey::Space) {
            return Some(vec![0x00]);
        }
    }

    // 2. Handle named keys (arrows, function keys, Enter, etc.)
    if let Key::Named(named) = &event.logical_key {
        return encode_named_key(*named);
    }

    // 3. For regular text input, use event.text — this correctly handles
    //    shift, caps lock, and dead keys on all platforms.
    if let Some(text) = &event.text {
        let s = text.as_str();
        if !s.is_empty() && !ctrl {
            return Some(s.as_bytes().to_vec());
        }
    }

    None
}

/// Encode a named key to terminal bytes.
fn encode_named_key(key: NamedKey) -> Option<Vec<u8>> {
    match key {
        NamedKey::Space => Some(vec![b' ']),
        NamedKey::Enter => Some(vec![b'\r']),
        NamedKey::Backspace => Some(vec![0x7f]),
        NamedKey::Tab => Some(vec![b'\t']),
        NamedKey::Escape => Some(vec![0x1b]),

        // Arrow keys
        NamedKey::ArrowUp => Some(b"\x1b[A".to_vec()),
        NamedKey::ArrowDown => Some(b"\x1b[B".to_vec()),
        NamedKey::ArrowRight => Some(b"\x1b[C".to_vec()),
        NamedKey::ArrowLeft => Some(b"\x1b[D".to_vec()),

        // Navigation
        NamedKey::Home => Some(b"\x1b[H".to_vec()),
        NamedKey::End => Some(b"\x1b[F".to_vec()),
        NamedKey::PageUp => Some(b"\x1b[5~".to_vec()),
        NamedKey::PageDown => Some(b"\x1b[6~".to_vec()),
        NamedKey::Insert => Some(b"\x1b[2~".to_vec()),
        NamedKey::Delete => Some(b"\x1b[3~".to_vec()),

        // Function keys
        NamedKey::F1 => Some(b"\x1bOP".to_vec()),
        NamedKey::F2 => Some(b"\x1bOQ".to_vec()),
        NamedKey::F3 => Some(b"\x1bOR".to_vec()),
        NamedKey::F4 => Some(b"\x1bOS".to_vec()),
        NamedKey::F5 => Some(b"\x1b[15~".to_vec()),
        NamedKey::F6 => Some(b"\x1b[17~".to_vec()),
        NamedKey::F7 => Some(b"\x1b[18~".to_vec()),
        NamedKey::F8 => Some(b"\x1b[19~".to_vec()),
        NamedKey::F9 => Some(b"\x1b[20~".to_vec()),
        NamedKey::F10 => Some(b"\x1b[21~".to_vec()),
        NamedKey::F11 => Some(b"\x1b[23~".to_vec()),
        NamedKey::F12 => Some(b"\x1b[24~".to_vec()),

        _ => None,
    }
}
