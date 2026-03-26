//! Minimal keyboard input handling for the terminal.
//!
//! Translates GTK key events into byte sequences written to the PTY.
//! This is deliberately minimal — just enough for typing, Enter, Backspace,
//! Tab, Escape, Ctrl+key, and arrow keys. The full ghostty key encoder
//! (Kitty keyboard protocol, modifier bits, etc.) is deferred to T-003.

use gtk4::gdk;

/// Translate a GTK key press event into bytes to write to the PTY.
///
/// Returns `Some(bytes)` if the key should produce PTY input, or `None`
/// if the key should be ignored (e.g. standalone modifier presses).
pub fn key_to_pty_bytes(keyval: gdk::Key, state: gdk::ModifierType) -> Option<Vec<u8>> {
    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    let shift = state.contains(gdk::ModifierType::SHIFT_MASK);

    // Ctrl+key combinations — compute control character
    if ctrl {
        if let Some(name) = keyval.name() {
            let name = name.to_lowercase();
            match name.as_str() {
                "a" => return Some(vec![0x01]),
                "b" => return Some(vec![0x02]),
                "c" => return Some(vec![0x03]),
                "d" => return Some(vec![0x04]),
                "e" => return Some(vec![0x05]),
                "f" => return Some(vec![0x06]),
                "g" => return Some(vec![0x07]),
                "h" => return Some(vec![0x08]),
                "i" => return Some(vec![0x09]),
                "j" => return Some(vec![0x0a]),
                "k" => return Some(vec![0x0b]),
                "l" => return Some(vec![0x0c]),
                "m" => return Some(vec![0x0d]),
                "n" => return Some(vec![0x0e]),
                "o" => return Some(vec![0x0f]),
                "p" => return Some(vec![0x10]),
                "q" => return Some(vec![0x11]),
                "r" => return Some(vec![0x12]),
                "s" => return Some(vec![0x13]),
                "t" => return Some(vec![0x14]),
                "u" => return Some(vec![0x15]),
                "v" => return Some(vec![0x16]),
                "w" => return Some(vec![0x17]),
                "x" => return Some(vec![0x18]),
                "y" => return Some(vec![0x19]),
                "z" => return Some(vec![0x1a]),
                "bracketleft" => return Some(vec![0x1b]),
                "backslash" => return Some(vec![0x1c]),
                "bracketright" => return Some(vec![0x1d]),
                _ => {}
            }
        }
    }

    // Special keys
    match keyval {
        k if k == gdk::Key::Return || k == gdk::Key::KP_Enter => Some(b"\r".to_vec()),
        k if k == gdk::Key::BackSpace => Some(vec![0x7f]),
        k if k == gdk::Key::Tab => {
            if shift {
                Some(b"\x1b[Z".to_vec()) // Shift+Tab (backtab)
            } else {
                Some(b"\t".to_vec())
            }
        }
        k if k == gdk::Key::Escape => Some(b"\x1b".to_vec()),
        k if k == gdk::Key::Delete => Some(b"\x1b[3~".to_vec()),
        k if k == gdk::Key::Home => Some(b"\x1b[H".to_vec()),
        k if k == gdk::Key::End => Some(b"\x1b[F".to_vec()),
        k if k == gdk::Key::Page_Up => Some(b"\x1b[5~".to_vec()),
        k if k == gdk::Key::Page_Down => Some(b"\x1b[6~".to_vec()),
        k if k == gdk::Key::Insert => Some(b"\x1b[2~".to_vec()),

        // Arrow keys
        k if k == gdk::Key::Up => Some(b"\x1b[A".to_vec()),
        k if k == gdk::Key::Down => Some(b"\x1b[B".to_vec()),
        k if k == gdk::Key::Right => Some(b"\x1b[C".to_vec()),
        k if k == gdk::Key::Left => Some(b"\x1b[D".to_vec()),

        // Function keys
        k if k == gdk::Key::F1 => Some(b"\x1bOP".to_vec()),
        k if k == gdk::Key::F2 => Some(b"\x1bOQ".to_vec()),
        k if k == gdk::Key::F3 => Some(b"\x1bOR".to_vec()),
        k if k == gdk::Key::F4 => Some(b"\x1bOS".to_vec()),
        k if k == gdk::Key::F5 => Some(b"\x1b[15~".to_vec()),
        k if k == gdk::Key::F6 => Some(b"\x1b[17~".to_vec()),
        k if k == gdk::Key::F7 => Some(b"\x1b[18~".to_vec()),
        k if k == gdk::Key::F8 => Some(b"\x1b[19~".to_vec()),
        k if k == gdk::Key::F9 => Some(b"\x1b[20~".to_vec()),
        k if k == gdk::Key::F10 => Some(b"\x1b[21~".to_vec()),
        k if k == gdk::Key::F11 => Some(b"\x1b[23~".to_vec()),
        k if k == gdk::Key::F12 => Some(b"\x1b[24~".to_vec()),

        _ => {
            // Printable characters: use the Unicode value from the keyval
            if let Some(ch) = keyval.to_unicode() {
                if !ch.is_control() || ch == ' ' {
                    let mut buf = [0u8; 4];
                    let s = ch.encode_utf8(&mut buf);
                    return Some(s.as_bytes().to_vec());
                }
            }
            None
        }
    }
}
