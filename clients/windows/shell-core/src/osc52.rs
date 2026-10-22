//! OSC 52 (a program sets the clipboard), write only.
//!
//! - `Scanner` finds `ESC ] 52 ; <selection> ; <base64> (BEL | ESC \)` in a pane's output. Sequences
//!   may be split across reads. A query (`?`) is a read: it is counted and never answered.
//! - The text is capped at `MAX_BYTES` and cleaned like a paste (control characters removed), so
//!   what lands on the clipboard cannot run commands when it is pasted later.
//! - `Policies` is the per-machine choice: `Off`, `Ask` (the default) or `Allow`. It is kept in a
//!   small JSON file next to the paired machines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The most clipboard text one request may carry (decoded).
pub const MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    Off,
    #[default]
    Ask,
    Allow,
}

impl Policy {
    pub fn label(self) -> &'static str {
        match self {
            Policy::Off => "off",
            Policy::Ask => "ask",
            Policy::Allow => "allow",
        }
    }
}

/// What a pane asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Put `text` on the clipboard. `text` is already cleaned.
    Write { text: String },
    /// A write above `MAX_BYTES`: dropped.
    TooLarge,
    /// A query for the clipboard: never answered.
    ReadDenied,
}

#[derive(Default, PartialEq, Clone, Copy)]
enum State {
    #[default]
    Ground,
    Esc,
    /// After `ESC ]`: collecting up to the terminator.
    Osc,
    OscEsc,
    /// An OSC that is not 52, or too long: ignored up to its terminator.
    Skip,
    SkipEsc,
}

#[derive(Default)]
pub struct Scanner {
    state: State,
    buf: Vec<u8>,
    overflow: bool,
}

/// Bytes of base64 that decode to `MAX_BYTES`, plus the `52;c;` head.
const MAX_BUF: usize = MAX_BYTES / 3 * 4 + 64;

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Request> {
        let mut out = Vec::new();
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => match b {
                    b']' => {
                        self.state = State::Osc;
                        self.buf.clear();
                        self.overflow = false;
                    }
                    0x1b => {}
                    _ => self.state = State::Ground,
                },
                State::Osc => match b {
                    0x07 => self.finish(&mut out),
                    0x1b => self.state = State::OscEsc,
                    // CAN and SUB cancel a sequence.
                    0x18 | 0x1a => self.state = State::Ground,
                    _ => {
                        if self.buf.len() >= MAX_BUF {
                            self.overflow = true;
                        } else {
                            self.buf.push(b);
                        }
                        // Not a 52 sequence: stop collecting it.
                        if self.buf.len() >= 3 && !self.buf.starts_with(b"52;") {
                            self.buf.clear();
                            self.state = State::Skip;
                        }
                    }
                },
                State::OscEsc => {
                    if b == b'\\' {
                        self.finish(&mut out);
                    } else {
                        // ESC inside an OSC aborts it and starts a new escape.
                        self.state = if b == b']' { State::Osc } else { State::Ground };
                        self.buf.clear();
                        self.overflow = false;
                    }
                }
                State::Skip => match b {
                    0x07 | 0x18 | 0x1a => self.state = State::Ground,
                    0x1b => self.state = State::SkipEsc,
                    _ => {}
                },
                State::SkipEsc => {
                    self.state = if b == b'\\' {
                        State::Ground
                    } else if b == b']' {
                        self.buf.clear();
                        self.overflow = false;
                        State::Osc
                    } else {
                        State::Ground
                    };
                }
            }
        }
        out
    }

    fn finish(&mut self, out: &mut Vec<Request>) {
        self.state = State::Ground;
        let overflow = std::mem::take(&mut self.overflow);
        let buf = std::mem::take(&mut self.buf);
        let Some(rest) = buf.strip_prefix(b"52;") else { return };
        // `<selection>;<data>`. Windows has one clipboard, so every selection is the same.
        let data = match rest.iter().position(|&c| c == b';') {
            Some(i) => &rest[i + 1..],
            None => return,
        };
        if data == b"?" {
            out.push(Request::ReadDenied);
            return;
        }
        if overflow {
            out.push(Request::TooLarge);
            return;
        }
        if data.is_empty() {
            return; // "clear the clipboard": not honored
        }
        match decode_base64(data) {
            Some(bytes) if bytes.len() > MAX_BYTES => out.push(Request::TooLarge),
            Some(bytes) => out.push(Request::Write { text: clean(&String::from_utf8_lossy(&bytes)) }),
            None => {}
        }
    }
}

/// Clean text like a paste: control characters other than TAB, CR and LF, DEL and C1 controls go.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|&c| match c {
            '\t' | '\r' | '\n' => true,
            c if (c as u32) < 0x20 => false,
            '\u{7f}' => false,
            c if (0x80..=0x9f).contains(&(c as u32)) => false,
            // Bidi controls reorder what the user sees when they read it back.
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' => false,
            _ => true,
        })
        .collect()
}

/// Standard base64 with padding (the smoke and the manual-test helpers build requests with it).
pub fn encode_base64(s: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity(s.len() / 3 * 4 + 4);
    for c in s.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                o.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                o.push('=');
            }
        }
    }
    o
}

fn decode_base64(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut pad = 0;
    for &c in data {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            b' ' | b'\r' | b'\n' | b'\t' => continue,
            _ => return None,
        };
        if pad > 0 {
            return None; // data after the padding
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The per-machine policies, kept in `osc52.json`.
#[derive(Default)]
pub struct Policies {
    path: Option<PathBuf>,
    map: HashMap<String, Policy>,
}

impl Policies {
    /// In memory only (tests, and builds without a data directory).
    pub fn memory() -> Self {
        Self::default()
    }

    pub fn open(dir: &Path) -> Self {
        let path = dir.join("osc52.json");
        let map = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Self { path: Some(path), map }
    }

    /// `Ask` for a machine that was never decided.
    pub fn get(&self, machine: &str) -> Policy {
        self.map.get(machine).copied().unwrap_or_default()
    }

    pub fn set(&mut self, machine: &str, policy: Policy) {
        self.map.insert(machine.to_string(), policy);
        if let Some(p) = &self.path {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(t) = serde_json::to_string_pretty(&self.map) {
                let _ = std::fs::write(p, t);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &[u8]) -> String {
        encode_base64(s)
    }

    fn osc(sel: &str, data: &str, st: &str) -> Vec<u8> {
        format!("\x1b]52;{sel};{data}{st}").into_bytes()
    }

    #[test]
    fn a_write_is_decoded_with_either_terminator() {
        let mut s = Scanner::new();
        let r = s.feed(&osc("c", &b64(b"hello"), "\x07"));
        assert_eq!(r, vec![Request::Write { text: "hello".into() }]);
        let r = s.feed(&osc("", &b64("h\u{e9}llo w\u{f6}rld".as_bytes()), "\x1b\\"));
        assert_eq!(r, vec![Request::Write { text: "h\u{e9}llo w\u{f6}rld".into() }]);
    }

    #[test]
    fn a_sequence_split_across_reads_is_found_once() {
        let mut s = Scanner::new();
        let all = [b"some output ".to_vec(), osc("c", &b64(b"split text"), "\x07"), b" more".to_vec()].concat();
        let mut got = Vec::new();
        for ch in all.chunks(3) {
            got.extend(s.feed(ch));
        }
        assert_eq!(got, vec![Request::Write { text: "split text".into() }]);
    }

    #[test]
    fn a_query_is_a_read_and_is_never_a_write() {
        let mut s = Scanner::new();
        assert_eq!(s.feed(&osc("c", "?", "\x07")), vec![Request::ReadDenied]);
    }

    #[test]
    fn text_is_cleaned_like_a_paste() {
        let mut s = Scanner::new();
        let r = s.feed(&osc("c", &b64(b"ls\x1b[31m -la\x07\r\nrm\x7f -rf\xc2\x9b x"), "\x07"));
        assert_eq!(r, vec![Request::Write { text: "ls[31m -la\r\nrm -rf x".into() }]);
        assert_eq!(clean("a\u{202e}b"), "ab");
    }

    #[test]
    fn more_than_one_mebibyte_is_dropped() {
        let mut s = Scanner::new();
        let big = vec![b'a'; MAX_BYTES + 1];
        assert_eq!(s.feed(&osc("c", &b64(&big), "\x07")), vec![Request::TooLarge]);
        // Exactly the cap still passes, and the scanner is back to normal after a drop.
        let ok = vec![b'b'; MAX_BYTES];
        let r = s.feed(&osc("c", &b64(&ok), "\x07"));
        assert!(matches!(&r[..], [Request::Write { text }] if text.len() == MAX_BYTES));
        // A body far beyond the cap does not grow the buffer without bound.
        let huge = "A".repeat(MAX_BUF * 3);
        let mut s = Scanner::new();
        let r = s.feed(&osc("c", &huge, "\x07"));
        assert_eq!(r, vec![Request::TooLarge]);
        assert!(s.buf.capacity() < MAX_BUF * 2);
    }

    #[test]
    fn other_osc_sequences_and_bad_data_are_ignored() {
        let mut s = Scanner::new();
        assert!(s.feed(b"\x1b]0;a title\x07\x1b]7;file:///c\x1b\\\x1b]52;c;!!!!\x07").is_empty());
        // An empty body (clear) is not honored.
        assert!(s.feed(&osc("c", "", "\x07")).is_empty());
        // And the scanner still works after all that.
        assert_eq!(s.feed(&osc("c", &b64(b"ok"), "\x07")).len(), 1);
    }

    #[test]
    fn policy_defaults_to_ask_and_is_remembered() {
        let dir = std::env::temp_dir().join(format!("forgetty-osc52-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Policies::open(&dir);
        assert_eq!(p.get("m1"), Policy::Ask);
        p.set("m1", Policy::Allow);
        p.set("m2", Policy::Off);
        let p = Policies::open(&dir);
        assert_eq!((p.get("m1"), p.get("m2"), p.get("m3")), (Policy::Allow, Policy::Off, Policy::Ask));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
