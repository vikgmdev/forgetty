//! Paste sanitizing and safety ("pastejacking" defense).
//!
//! Clipboard text can carry hidden control bytes or newlines that run commands
//! the user never saw. `prepare` returns the bytes to send to the PTY plus a
//! flag telling the UI to ask for confirmation first.
//!
//! Rules:
//! - Bracketed mode (DECSET 2004): ESC (0x1B) and C1 CSI (U+009B) are stripped,
//!   so the text cannot close the bracket early. Stripping marks the paste unsafe.
//! - Unbracketed mode: C0 controls other than TAB/CR/LF, DEL and all C1
//!   controls are stripped (unsafe), and any newline makes the paste unsafe
//!   (libghostty's `ghostty_paste_is_safe` agrees on the newline/bracket cases).
//! - Newlines are sent as CR.

use crate::ffi;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paste {
    /// Bytes for the PTY, already wrapped in the bracket when `bracketed`.
    pub bytes: Vec<u8>,
    /// Ask the user before sending.
    pub needs_confirm: bool,
    /// Number of control characters removed.
    pub stripped: usize,
}

fn is_stripped(c: char, bracketed: bool) -> bool {
    if bracketed {
        return c == '\u{1b}' || c == '\u{9b}';
    }
    match c {
        '\t' | '\r' | '\n' => false,
        c if (c as u32) < 0x20 => true,
        '\u{7f}' => true,
        c if (0x80..=0x9f).contains(&(c as u32)) => true,
        _ => false,
    }
}

pub fn prepare(text: &str, bracketed: bool) -> Paste {
    let has_newline = text.contains('\n') || text.contains('\r');
    let mut stripped = 0;
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if is_stripped(c, bracketed) {
            stripped += 1;
            continue;
        }
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                clean.push('\r');
            }
            '\n' => clean.push('\r'),
            c => clean.push(c),
        }
    }

    let ghostty_unsafe = !bracketed && !unsafe { ffi::ghostty_paste_is_safe(text.as_ptr(), text.len()) };
    let needs_confirm = stripped > 0 || (!bracketed && (has_newline || ghostty_unsafe));

    let bytes = if bracketed {
        let mut out = Vec::with_capacity(clean.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(clean.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        clean.into_bytes()
    };
    Paste { bytes, needs_confirm, stripped }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_bracket_end_is_stripped_in_bracketed_mode() {
        let p = prepare("echo hi\x1b[201~; rm -rf ~", true);
        assert_eq!(p.bytes, b"\x1b[200~echo hi[201~; rm -rf ~\x1b[201~");
        assert_eq!(p.stripped, 1);
        assert!(p.needs_confirm);
        // Exactly one bracket end, the real one at the very end.
        let s = String::from_utf8(p.bytes).unwrap();
        assert_eq!(s.matches("\x1b[201~").count(), 1);
    }

    #[test]
    fn multiline_unbracketed_needs_confirm() {
        let p = prepare("ls\npwd\r\nwhoami", false);
        assert_eq!(p.bytes, b"ls\rpwd\rwhoami");
        assert!(p.needs_confirm);
        assert_eq!(p.stripped, 0);
    }

    #[test]
    fn c1_csi_is_stripped() {
        let p = prepare("a\u{9b}31mb", true);
        assert_eq!(p.bytes, b"\x1b[200~a31mb\x1b[201~");
        assert!(p.needs_confirm);
        let p = prepare("a\u{9b}31mb", false);
        assert_eq!(p.bytes, b"a31mb");
        assert!(p.needs_confirm);
        assert_eq!(p.stripped, 1);
    }

    #[test]
    fn plain_single_line_pastes_without_prompt() {
        let p = prepare("cargo build --release\t# tab ok", false);
        assert_eq!(p.bytes, "cargo build --release\t# tab ok".as_bytes());
        assert!(!p.needs_confirm);
        let p = prepare("cargo build", true);
        assert_eq!(p.bytes, b"\x1b[200~cargo build\x1b[201~");
        assert!(!p.needs_confirm);
    }

    #[test]
    fn multiline_in_bracketed_mode_is_fine() {
        let p = prepare("a\nb", true);
        assert_eq!(p.bytes, b"\x1b[200~a\rb\x1b[201~");
        assert!(!p.needs_confirm);
    }

    #[test]
    fn c0_controls_stripped_unbracketed() {
        let p = prepare("a\x07b\x00c\x7fd", false);
        assert_eq!(p.bytes, b"abcd");
        assert_eq!(p.stripped, 3);
        assert!(p.needs_confirm);
    }
}
