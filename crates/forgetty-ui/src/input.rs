//! Input event processing.
//!
//! Translates winit keyboard events into terminal input byte sequences.

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
    let _alt = modifiers.alt_key();
    let _shift = modifiers.shift_key();

    match &event.logical_key {
        // Named keys
        Key::Named(named) => encode_named_key(*named, ctrl),
        // Character keys
        Key::Character(c) => {
            let ch = c.as_str();
            if ctrl && ch.len() == 1 {
                let byte = ch.as_bytes()[0];
                // Ctrl+letter produces control characters \x01-\x1a
                match byte {
                    b'a'..=b'z' => Some(vec![byte - b'a' + 1]),
                    b'A'..=b'Z' => Some(vec![byte - b'A' + 1]),
                    // Ctrl+[ = ESC, Ctrl+\ = FS, Ctrl+] = GS, Ctrl+^ = RS, Ctrl+_ = US
                    b'[' => Some(vec![0x1b]),
                    b'\\' => Some(vec![0x1c]),
                    b']' => Some(vec![0x1d]),
                    b'^' => Some(vec![0x1e]),
                    b'_' => Some(vec![0x1f]),
                    b'@' => Some(vec![0x00]),
                    _ => None,
                }
            } else {
                Some(ch.as_bytes().to_vec())
            }
        }
        _ => None,
    }
}

/// Encode a named key to terminal bytes.
fn encode_named_key(key: NamedKey, ctrl: bool) -> Option<Vec<u8>> {
    // If ctrl is held with a named key, some have special behavior
    if ctrl && key == NamedKey::Space {
        return Some(vec![0x00]); // Ctrl+Space = NUL
    }

    match key {
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
