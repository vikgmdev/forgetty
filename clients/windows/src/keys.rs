//! Keystroke to PTY bytes. Spike-grade: legacy xterm encoding only.
//! MVP replaces this with the libghostty-vt key encoder (kitty protocol etc).

use gpui::Keystroke;

pub fn encode(ks: &Keystroke) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    let key = ks.key.as_str();

    // Named keys first.
    let named: Option<&[u8]> = match key {
        "enter" => Some(b"\r"),
        "backspace" => Some(b"\x7f"),
        "tab" if m.shift => Some(b"\x1b[Z"),
        "tab" => Some(b"\t"),
        "escape" => Some(b"\x1b"),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "right" => Some(b"\x1b[C"),
        "left" => Some(b"\x1b[D"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "pageup" => Some(b"\x1b[5~"),
        "pagedown" => Some(b"\x1b[6~"),
        "delete" => Some(b"\x1b[3~"),
        "insert" => Some(b"\x1b[2~"),
        "f1" => Some(b"\x1bOP"),
        "f2" => Some(b"\x1bOQ"),
        "f3" => Some(b"\x1bOR"),
        "f4" => Some(b"\x1bOS"),
        "f5" => Some(b"\x1b[15~"),
        "f6" => Some(b"\x1b[17~"),
        "f7" => Some(b"\x1b[18~"),
        "f8" => Some(b"\x1b[19~"),
        "f9" => Some(b"\x1b[20~"),
        "f10" => Some(b"\x1b[21~"),
        "f11" => Some(b"\x1b[23~"),
        "f12" => Some(b"\x1b[24~"),
        _ => None,
    };
    if let Some(b) = named {
        return Some(with_alt(m.alt, b.to_vec()));
    }

    // Ctrl + letter / a few punctuation keys -> C0 control byte.
    if m.control && !m.alt {
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let b = match c {
                'a'..='z' => Some(c as u8 - b'a' + 1),
                'A'..='Z' => Some(c as u8 - b'A' + 1),
                ' ' | '@' | '2' => Some(0),
                '[' => Some(0x1b),
                '\\' => Some(0x1c),
                ']' => Some(0x1d),
                '^' | '6' => Some(0x1e),
                '_' | '-' => Some(0x1f),
                _ => None,
            };
            if let Some(b) = b {
                return Some(vec![b]);
            }
        }
    }

    if key == "space" && m.control {
        return Some(vec![0]);
    }

    // Plain text.
    if !m.control && !m.platform {
        if let Some(text) = ks.key_char.as_deref() {
            if !text.is_empty() {
                return Some(with_alt(m.alt, text.as_bytes().to_vec()));
            }
        }
    }
    None
}

fn with_alt(alt: bool, mut bytes: Vec<u8>) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}
