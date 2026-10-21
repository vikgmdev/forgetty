//! OSC scanner for the PTY drain path (AD-019).
//!
//! A byte-level state machine, not a VT parser. It looks only for OSC
//! sequences (`ESC ] ... BEL` or `ESC ] ... ESC \`) and extracts the few the
//! server cares about:
//!
//! | OSC | Meaning |
//! |---|---|
//! | 0, 2 | window title |
//! | 7 | working directory (`file://host/path`) |
//! | 9, 99, 777 | desktop notifications |
//! | 133 `;A` | shell prompt start |
//! | 7501 | Program Status Protocol |
//!
//! The scanner keeps its state across chunk boundaries, so a sequence split at
//! any byte is still seen whole. Each sequence body is buffered up to
//! [`MAX_OSC_BODY`] bytes. A longer sequence is discarded (and counted), the
//! rest of it skipped up to its terminator. OSC numbers the server does not use
//! are skipped without buffering.
//!
//! Everything in a sequence is untrusted. Text is decoded, checked and passed
//! through [`forgetty_core::sanitize`] before it leaves this module.

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use forgetty_core::sanitize::untrusted_max;

/// Largest OSC body the scanner buffers, in bytes (4 KiB, AD-019).
pub const MAX_OSC_BODY: usize = 4096;

/// Longest title kept, in characters.
pub const MAX_TITLE_CHARS: usize = 256;
/// Longest working directory kept, in characters.
pub const MAX_CWD_CHARS: usize = 1024;
/// Longest notification title or body kept, in characters.
pub const MAX_NOTIFY_CHARS: usize = 512;
/// Longest status message kept, in characters.
pub const MAX_STATUS_MSG_CHARS: usize = 512;
/// Longest status record title kept, in characters.
pub const MAX_STATUS_TITLE_CHARS: usize = 128;

// OSC 7501 limits (Program Status Protocol, "Limits").
const MAX_KEY_LEN: usize = 16;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG_DECODED: usize = 2048;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE_DECODED: usize = 192;
const MAX_APP_LEN: usize = 32;
const MAX_ID_LEN: usize = 128;
const MAX_ID_SEGMENT: usize = 32;
const MAX_ID_DEPTH: usize = 8;

/// Standard base64, padding optional (as the 7501 spec asks).
const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// `state` of one OSC 7501 report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Clear,
}

/// One validated OSC 7501 report. Text is already sanitised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    /// Record id. `None` is the root record.
    pub id: Option<String>,
    pub state: ReportState,
    /// `permission`, `question` or `auth`, only with `Blocked`.
    pub kind: Option<String>,
    /// 0 to 100, only with `Working` or `Blocked`.
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

/// What the scanner found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 0 or 2. Sanitised, can be empty.
    Title(String),
    /// OSC 7. An absolute path, sanitised.
    Cwd(String),
    /// OSC 7501.
    Status(StatusReport),
    /// OSC 9, 99 or 777.
    Notification { title: Option<String>, body: String },
    /// OSC 133 `A`: a new shell prompt begins.
    PromptStart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Title,
    Cwd,
    Notify9,
    Notify99,
    Notify777,
    Prompt,
    Status,
}

impl Kind {
    fn from_number(n: u32) -> Option<Kind> {
        match n {
            0 | 2 => Some(Kind::Title),
            7 => Some(Kind::Cwd),
            9 => Some(Kind::Notify9),
            99 => Some(Kind::Notify99),
            133 => Some(Kind::Prompt),
            777 => Some(Kind::Notify777),
            7501 => Some(Kind::Status),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    /// Outside any escape sequence.
    Ground,
    /// The previous chunk ended with ESC.
    Esc,
    /// After `ESC ]`, reading the number up to `;`.
    OscNum,
    /// Buffering the body of an OSC we use.
    OscBody(Kind),
    /// Inside an OSC we do not use, or an oversized one. Wait for its end.
    OscSkip,
}

/// Pending kitty (OSC 99) notification parts, until the `d=1` chunk arrives.
#[derive(Debug, Default)]
struct Pending99 {
    title: String,
    body: String,
}

/// The scanner. One per pane. Cheap to create, no allocation until the first
/// interesting sequence.
pub struct OscScanner {
    st: St,
    /// ESC seen inside an OSC: the next byte decides `ST` or abort.
    in_esc: bool,
    num: u32,
    ndigits: u8,
    buf: Vec<u8>,
    pending99: Option<Pending99>,
    /// Sequences dropped because they were longer than [`MAX_OSC_BODY`].
    discarded_oversized: u64,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for OscScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OscScanner")
            .field("st", &self.st)
            .field("buffered", &self.buf.len())
            .field("discarded_oversized", &self.discarded_oversized)
            .finish()
    }
}

impl OscScanner {
    pub fn new() -> Self {
        Self {
            st: St::Ground,
            in_esc: false,
            num: 0,
            ndigits: 0,
            buf: Vec::new(),
            pending99: None,
            discarded_oversized: 0,
        }
    }

    /// Number of sequences discarded for exceeding [`MAX_OSC_BODY`].
    pub fn discarded_oversized(&self) -> u64 {
        self.discarded_oversized
    }

    /// Scan one chunk of PTY output. `sink` gets every event found, in order.
    pub fn feed(&mut self, data: &[u8], sink: &mut impl FnMut(OscEvent)) {
        let mut i = 0usize;
        while i < data.len() {
            match self.st {
                St::Ground => {
                    // Fast path: look for `]` (rarer than ESC, which every CSI
                    // carries) and check that ESC comes right before it.
                    let start = i;
                    let mut found = None;
                    while let Some(p) = memchr::memchr(b']', &data[i..]) {
                        let j = i + p;
                        if j > start && data[j - 1] == 0x1b {
                            found = Some(j);
                            break;
                        }
                        i = j + 1;
                        if i >= data.len() {
                            break;
                        }
                    }
                    match found {
                        Some(j) => {
                            self.begin_osc();
                            i = j + 1;
                        }
                        None => {
                            if data[data.len() - 1] == 0x1b {
                                self.st = St::Esc;
                            }
                            return;
                        }
                    }
                }
                St::Esc => {
                    let b = data[i];
                    if b == b']' {
                        self.begin_osc();
                        i += 1;
                    } else if b == 0x1b {
                        // ESC ESC: stay in Esc.
                        i += 1;
                    } else {
                        self.st = St::Ground;
                    }
                }
                St::OscNum => {
                    let b = data[i];
                    i += 1;
                    if self.in_esc {
                        self.in_esc = false;
                        if b == b'\\' {
                            // `ESC ]` NUM `ESC \`: no body, nothing to report.
                            self.st = St::Ground;
                        } else {
                            // Aborted by a new escape: reprocess `b` after ESC.
                            self.st = St::Esc;
                            i -= 1;
                        }
                        continue;
                    }
                    match b {
                        b'0'..=b'9' => {
                            self.ndigits = self.ndigits.saturating_add(1);
                            if self.ndigits > 5 {
                                self.st = St::OscSkip;
                            } else {
                                self.num = self.num * 10 + u32::from(b - b'0');
                            }
                        }
                        b';' => {
                            match (self.ndigits > 0).then(|| Kind::from_number(self.num)).flatten()
                            {
                                Some(k) => {
                                    self.buf.clear();
                                    self.st = St::OscBody(k);
                                }
                                None => self.st = St::OscSkip,
                            }
                        }
                        0x07 | 0x18 | 0x1a => self.st = St::Ground,
                        0x1b => self.in_esc = true,
                        _ => self.st = St::OscSkip,
                    }
                }
                St::OscBody(kind) => {
                    if self.in_esc {
                        self.in_esc = false;
                        let b = data[i];
                        if b == b'\\' {
                            i += 1;
                            self.finish_body(kind, sink);
                            self.st = St::Ground;
                        } else {
                            // Aborted by a new escape. `b` is reprocessed.
                            self.buf.clear();
                            self.st = St::Esc;
                        }
                        continue;
                    }
                    let rest = &data[i..];
                    match rest.iter().position(|&b| matches!(b, 0x07 | 0x18 | 0x1a | 0x1b)) {
                        None => {
                            self.push_body(rest);
                            i = data.len();
                        }
                        Some(p) => {
                            self.push_body(&rest[..p]);
                            let term = rest[p];
                            i += p + 1;
                            if !matches!(self.st, St::OscBody(_)) {
                                // push_body overflowed into OscSkip: the
                                // terminator ends the skipped sequence.
                                self.after_skip_terminator(term);
                                continue;
                            }
                            match term {
                                0x07 => {
                                    self.finish_body(kind, sink);
                                    self.st = St::Ground;
                                }
                                0x1b => self.in_esc = true,
                                _ => {
                                    // CAN or SUB cancels the sequence.
                                    self.buf.clear();
                                    self.st = St::Ground;
                                }
                            }
                        }
                    }
                }
                St::OscSkip => {
                    if self.in_esc {
                        self.in_esc = false;
                        if data[i] == b'\\' {
                            i += 1;
                            self.st = St::Ground;
                        } else {
                            self.st = St::Esc;
                        }
                        continue;
                    }
                    let rest = &data[i..];
                    match rest.iter().position(|&b| matches!(b, 0x07 | 0x18 | 0x1a | 0x1b)) {
                        None => i = data.len(),
                        Some(p) => {
                            let term = rest[p];
                            i += p + 1;
                            self.after_skip_terminator(term);
                        }
                    }
                }
            }
        }
    }

    fn begin_osc(&mut self) {
        self.st = St::OscNum;
        self.in_esc = false;
        self.num = 0;
        self.ndigits = 0;
        self.buf.clear();
    }

    /// A terminator byte arrived while skipping.
    fn after_skip_terminator(&mut self, term: u8) {
        match term {
            0x1b => self.in_esc = true,
            _ => {
                // BEL ends it, CAN and SUB cancel it.
                self.st = St::Ground;
                self.in_esc = false;
            }
        }
    }

    /// Append to the body buffer. Over the cap: drop the buffer and skip the rest.
    fn push_body(&mut self, bytes: &[u8]) {
        if self.buf.len() + bytes.len() > MAX_OSC_BODY {
            self.buf.clear();
            self.discarded_oversized += 1;
            self.st = St::OscSkip;
        } else {
            self.buf.extend_from_slice(bytes);
        }
    }

    fn finish_body(&mut self, kind: Kind, sink: &mut impl FnMut(OscEvent)) {
        let body = std::mem::take(&mut self.buf);
        match kind {
            Kind::Title => sink(OscEvent::Title(clean_text(&body, MAX_TITLE_CHARS))),
            Kind::Cwd => {
                if let Some(p) = parse_cwd(&body) {
                    sink(OscEvent::Cwd(p));
                }
            }
            Kind::Status => {
                if let Some(r) = parse_status(&body) {
                    sink(OscEvent::Status(r));
                }
            }
            Kind::Notify9 => {
                if let Some(ev) = parse_osc9(&body) {
                    sink(ev);
                }
            }
            Kind::Notify777 => {
                if let Some(ev) = parse_osc777(&body) {
                    sink(ev);
                }
            }
            Kind::Notify99 => {
                if let Some(ev) = self.parse_osc99(&body) {
                    sink(ev);
                }
            }
            Kind::Prompt => {
                if body.first() == Some(&b'A') && (body.len() == 1 || body[1] == b';') {
                    sink(OscEvent::PromptStart);
                }
            }
        }
        // Give the allocation back for the next sequence.
        self.buf = body;
        self.buf.clear();
    }

    /// kitty desktop notification: `99 ; metadata ; payload`.
    fn parse_osc99(&mut self, body: &[u8]) -> Option<OscEvent> {
        let split = body.iter().position(|&b| b == b';')?;
        let (meta, payload) = (&body[..split], &body[split + 1..]);
        let mut done = true;
        let mut ptype: &[u8] = b"title";
        let mut b64 = false;
        for pair in meta.split(|&b| b == b':') {
            let Some(eq) = pair.iter().position(|&b| b == b'=') else { continue };
            let (k, v) = (&pair[..eq], &pair[eq + 1..]);
            match k {
                b"d" => done = v != b"0",
                b"p" => ptype = v,
                b"e" => b64 = v == b"1",
                _ => {}
            }
        }
        let is_title = ptype == b"title";
        if !is_title && ptype != b"body" {
            return None;
        }
        let text = if b64 {
            if payload.len() > MAX_OSC_BODY {
                return None;
            }
            let raw = B64.decode(payload).ok()?;
            clean_text(&raw, MAX_NOTIFY_CHARS)
        } else {
            clean_text(payload, MAX_NOTIFY_CHARS)
        };
        let pend = self.pending99.get_or_insert_with(Pending99::default);
        let slot = if is_title { &mut pend.title } else { &mut pend.body };
        if slot.chars().count() + text.chars().count() <= MAX_NOTIFY_CHARS {
            slot.push_str(&text);
        }
        if !done {
            return None;
        }
        let Pending99 { title, body } = self.pending99.take()?;
        notification(
            (!title.is_empty()).then_some(title),
            if body.is_empty() { None } else { Some(body) },
        )
    }
}

/// Build a notification from optional title and body. Empty ones give `None`.
fn notification(title: Option<String>, body: Option<String>) -> Option<OscEvent> {
    match (title, body) {
        (None, None) => None,
        (Some(t), None) => Some(OscEvent::Notification { title: None, body: t }),
        (t, Some(b)) => Some(OscEvent::Notification { title: t, body: b }),
    }
}

/// UTF-8 (lossy) text, sanitised and capped.
fn clean_text(raw: &[u8], max_chars: usize) -> String {
    // Read at most 4 bytes per kept char (UTF-8), so a huge body costs little.
    let slice = &raw[..raw.len().min(max_chars.saturating_mul(4).saturating_add(8))];
    untrusted_max(&String::from_utf8_lossy(slice), max_chars)
}

/// OSC 9: iTerm2 / Windows Terminal notification, body is the text. ConEmu
/// sub-commands (`9;4;...` progress and friends) start with `digit ;` and are
/// not notifications.
fn parse_osc9(body: &[u8]) -> Option<OscEvent> {
    if body.first().is_some_and(u8::is_ascii_digit)
        && (body.len() == 1 || body.get(1) == Some(&b';'))
    {
        return None;
    }
    let text = clean_text(body, MAX_NOTIFY_CHARS);
    (!text.is_empty()).then_some(OscEvent::Notification { title: None, body: text })
}

/// OSC 777: `notify ; title ; body` (VTE, urxvt). Other 777 verbs are ignored.
fn parse_osc777(body: &[u8]) -> Option<OscEvent> {
    let mut it = body.splitn(3, |&b| b == b';');
    if it.next()? != b"notify" {
        return None;
    }
    let title = clean_text(it.next()?, MAX_NOTIFY_CHARS);
    let text = it.next().map(|b| clean_text(b, MAX_NOTIFY_CHARS)).unwrap_or_default();
    notification((!title.is_empty()).then_some(title), (!text.is_empty()).then_some(text))
}

/// OSC 7: `file://host/path` with percent-escapes. Only an absolute path is
/// accepted, and no decoded control character.
fn parse_cwd(body: &[u8]) -> Option<String> {
    let rest = body.strip_prefix(b"file://")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let path = &rest[slash..];
    let mut out = Vec::with_capacity(path.len());
    let mut i = 0;
    while i < path.len() {
        let b = path[i];
        if b == b'%' {
            let hi = hex(*path.get(i + 1)?)?;
            let lo = hex(*path.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    if out.iter().any(|&b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let s = String::from_utf8(out).ok()?;
    let clean = untrusted_max(&s, MAX_CWD_CHARS);
    clean.starts_with('/').then_some(clean)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn is_value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

fn is_segment_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-')
}

fn valid_app(v: &[u8]) -> bool {
    !v.is_empty() && v.len() <= MAX_APP_LEN && v.iter().all(|&b| is_segment_byte(b))
}

fn valid_id(v: &[u8]) -> bool {
    if v.is_empty() || v.len() > MAX_ID_LEN {
        return false;
    }
    let mut depth = 0;
    for seg in v.split(|&b| b == b'/') {
        depth += 1;
        if seg.is_empty() || seg.len() > MAX_ID_SEGMENT || !seg.iter().all(|&b| is_segment_byte(b))
        {
            return false;
        }
    }
    depth <= MAX_ID_DEPTH
}

/// Decode a base64 free-text value. `None` discards the whole report: too
/// long, bad base64, not UTF-8, or a control character in the text.
fn decode_text(v: &[u8], max_encoded: usize, max_decoded: usize) -> Option<String> {
    if v.len() > max_encoded {
        return None;
    }
    let raw = B64.decode(v).ok()?;
    if raw.len() > max_decoded {
        return None;
    }
    let s = String::from_utf8(raw).ok()?;
    if s.chars().any(|c| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')) {
        return None;
    }
    Some(s)
}

fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

/// Parse the body of an OSC 7501 report (the part after `7501;`).
///
/// Returns `None` when the report is ignored or discarded whole: no or unknown
/// `state`, a bad `id`, an over-limit or undecodable `msg` or `title`.
pub fn parse_status(body: &[u8]) -> Option<StatusReport> {
    if trim_ascii(body) == b"?" {
        return None; // feature detection, no reply on the hot path
    }
    let mut state: Option<&[u8]> = None;
    let mut id: Option<&[u8]> = None;
    let mut kind: Option<&[u8]> = None;
    let mut progress: Option<&[u8]> = None;
    let mut app: Option<&[u8]> = None;
    let mut title: Option<&[u8]> = None;
    let mut msg: Option<&[u8]> = None;

    for pair in body.split(|&b| b == b':') {
        let pair = trim_ascii(pair);
        if pair.is_empty() {
            continue;
        }
        let Some(eq) = pair.iter().position(|&b| b == b'=') else { continue };
        let key = trim_ascii(&pair[..eq]);
        let val = trim_ascii(&pair[eq + 1..]);
        if key.is_empty() || key.len() > MAX_KEY_LEN || !key.iter().all(u8::is_ascii_lowercase) {
            continue;
        }
        if !val.iter().all(|&b| is_value_byte(b)) {
            continue;
        }
        match key {
            b"state" => state = Some(val),
            b"id" => id = Some(val),
            b"kind" => kind = Some(val),
            b"progress" => progress = Some(val),
            b"app" => app = Some(val),
            b"title" => title = Some(val),
            b"msg" => msg = Some(val),
            _ => {}
        }
    }

    let state = match state? {
        b"idle" => ReportState::Idle,
        b"working" => ReportState::Working,
        b"done" => ReportState::Done,
        b"blocked" => ReportState::Blocked,
        b"error" => ReportState::Error,
        b"clear" => ReportState::Clear,
        _ => return None,
    };
    let id = match id {
        None => None,
        Some(v) if valid_id(v) => Some(String::from_utf8_lossy(v).into_owned()),
        Some(_) => return None,
    };
    if state == ReportState::Clear {
        return Some(StatusReport {
            id,
            state,
            kind: None,
            progress: None,
            app: None,
            title: None,
            msg: None,
        });
    }

    // Limits are checked for every key before the report is accepted.
    let msg = match msg {
        Some(v) => Some(decode_text(v, MAX_MSG_ENCODED, MAX_MSG_DECODED)?),
        None => None,
    };
    let title = match title {
        Some(v) => Some(decode_text(v, MAX_TITLE_ENCODED, MAX_TITLE_DECODED)?),
        None => None,
    };
    let kind = match (state, kind) {
        (ReportState::Blocked, Some(k @ (b"permission" | b"question" | b"auth"))) => {
            Some(String::from_utf8_lossy(k).into_owned())
        }
        _ => None,
    };
    let progress = match (state, progress) {
        (ReportState::Working | ReportState::Blocked, Some(v))
            if !v.is_empty() && v.len() <= 3 && v.iter().all(u8::is_ascii_digit) =>
        {
            std::str::from_utf8(v).ok().and_then(|s| s.parse::<u8>().ok()).filter(|p| *p <= 100)
        }
        _ => None,
    };
    let app = app.filter(|v| valid_app(v)).map(|v| String::from_utf8_lossy(v).into_owned());

    Some(StatusReport {
        id,
        state,
        kind,
        progress,
        app,
        title: title.map(|t| untrusted_max(&t, MAX_STATUS_TITLE_CHARS)).filter(|t| !t.is_empty()),
        msg: msg.map(|m| untrusted_max(&m, MAX_STATUS_MSG_CHARS)).filter(|m| !m.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_chunks(chunks: &[&[u8]]) -> (Vec<OscEvent>, OscScanner) {
        let mut sc = OscScanner::new();
        let mut out = Vec::new();
        for c in chunks {
            sc.feed(c, &mut |e| out.push(e));
        }
        (out, sc)
    }

    fn scan(data: &[u8]) -> Vec<OscEvent> {
        scan_chunks(&[data]).0
    }

    /// Feed `data` split in two at every boundary, and byte by byte. All must
    /// give the same events as one chunk.
    fn assert_split_invariant(data: &[u8]) -> Vec<OscEvent> {
        let whole = scan(data);
        for cut in 0..=data.len() {
            let (ev, _) = scan_chunks(&[&data[..cut], &data[cut..]]);
            assert_eq!(ev, whole, "split at {cut} of {:?}", String::from_utf8_lossy(data));
        }
        let singles: Vec<&[u8]> = data.chunks(1).collect();
        let (ev, _) = scan_chunks(&singles);
        assert_eq!(ev, whole, "byte-by-byte");
        // Three-way splits for the shorter inputs.
        if data.len() <= 40 {
            for a in 0..=data.len() {
                for b in a..=data.len() {
                    let (ev, _) = scan_chunks(&[&data[..a], &data[a..b], &data[b..]]);
                    assert_eq!(ev, whole, "split at {a},{b}");
                }
            }
        }
        whole
    }

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[test]
    fn title_osc0_and_osc2_bel_and_st() {
        for data in [
            &b"\x1b]0;my-title\x07"[..],
            b"\x1b]2;my-title\x07",
            b"\x1b]0;my-title\x1b\\",
            b"\x1b]2;my-title\x1b\\",
        ] {
            let ev = assert_split_invariant(data);
            assert_eq!(ev, vec![OscEvent::Title("my-title".into())]);
        }
    }

    #[test]
    fn osc1_icon_name_is_ignored() {
        assert!(assert_split_invariant(b"\x1b]1;icon\x07").is_empty());
    }

    #[test]
    fn title_is_sanitised() {
        let ev = scan("\x1b]2;a\u{202e}b\u{200b}c\x07".as_bytes());
        assert_eq!(ev, vec![OscEvent::Title("abc".into())]);
        // Control bytes cannot reach the title: CAN cancels, ESC aborts.
        let ev = scan(b"\x1b]2;ab\x01c\x07");
        assert_eq!(ev, vec![OscEvent::Title("abc".into())]);
    }

    #[test]
    fn title_is_capped() {
        let mut data = b"\x1b]2;".to_vec();
        data.extend(std::iter::repeat_n(b'x', 3000));
        data.push(0x07);
        match &scan(&data)[..] {
            [OscEvent::Title(t)] => assert_eq!(t.chars().count(), MAX_TITLE_CHARS),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cwd_osc7() {
        let ev = assert_split_invariant(b"\x1b]7;file://host/tmp/some%20dir\x07");
        assert_eq!(ev, vec![OscEvent::Cwd("/tmp/some dir".into())]);
        let ev = assert_split_invariant(b"\x1b]7;file:///var/tmp\x1b\\");
        assert_eq!(ev, vec![OscEvent::Cwd("/var/tmp".into())]);
    }

    #[test]
    fn cwd_rejects_bad_input() {
        assert!(scan(b"\x1b]7;http://host/tmp\x07").is_empty());
        assert!(scan(b"\x1b]7;file://host\x07").is_empty());
        assert!(scan(b"\x1b]7;file://host/tmp/%0Aevil\x07").is_empty());
        assert!(scan(b"\x1b]7;file://host/tmp/%zz\x07").is_empty());
        assert!(scan(b"\x1b]7;file://host/tmp/%4\x07").is_empty());
    }

    #[test]
    fn notifications_osc9_99_777() {
        let ev = assert_split_invariant(b"\x1b]9;build finished\x07");
        assert_eq!(ev, vec![OscEvent::Notification { title: None, body: "build finished".into() }]);

        let ev = assert_split_invariant(b"\x1b]777;notify;Deploy;all green\x07");
        assert_eq!(
            ev,
            vec![OscEvent::Notification { title: Some("Deploy".into()), body: "all green".into() }]
        );

        let ev = assert_split_invariant(
            b"\x1b]99;i=1:d=0:p=title;Hello\x1b\\\x1b]99;i=1:p=body;World\x1b\\",
        );
        assert_eq!(
            ev,
            vec![OscEvent::Notification { title: Some("Hello".into()), body: "World".into() }]
        );

        let ev = scan(b"\x1b]99;;Plain\x07");
        assert_eq!(ev, vec![OscEvent::Notification { title: None, body: "Plain".into() }]);

        let enc = format!("\x1b]99;e=1:p=body;{}\x07", b64("decoded"));
        assert_eq!(
            scan(enc.as_bytes()),
            vec![OscEvent::Notification { title: None, body: "decoded".into() }]
        );
    }

    #[test]
    fn osc9_conemu_subcommands_are_not_notifications() {
        assert!(scan(b"\x1b]9;4;1;50\x07").is_empty());
        assert!(scan(b"\x1b]9;9;/some/path\x07").is_empty());
        assert!(scan(b"\x1b]9;\x07").is_empty());
        // A text body that starts with a digit but is not `d;` is a notification.
        assert_eq!(scan(b"\x1b]9;4 tests failed\x07").len(), 1);
    }

    #[test]
    fn notification_text_is_sanitised_and_capped() {
        let ev = scan("\x1b]9;hi\u{202e}there\x07".as_bytes());
        assert_eq!(ev, vec![OscEvent::Notification { title: None, body: "hithere".into() }]);
        let mut data = b"\x1b]9;".to_vec();
        data.extend(std::iter::repeat_n(b'z', 4000));
        data.push(0x07);
        match &scan(&data)[..] {
            [OscEvent::Notification { body, .. }] => {
                assert_eq!(body.chars().count(), MAX_NOTIFY_CHARS)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn prompt_start_osc133() {
        assert_eq!(assert_split_invariant(b"\x1b]133;A\x07"), vec![OscEvent::PromptStart]);
        assert_eq!(assert_split_invariant(b"\x1b]133;A;k=i\x1b\\"), vec![OscEvent::PromptStart]);
        assert!(scan(b"\x1b]133;B\x07").is_empty());
        assert!(scan(b"\x1b]133;D;0\x07").is_empty());
        assert!(scan(b"\x1b]133;AB\x07").is_empty());
    }

    #[test]
    fn status_waiting_with_message() {
        let msg = b64("Apply 3 to add?");
        let data = format!("\x1b]7501;state=blocked:kind=permission:app=terraform:msg={msg}\x07");
        let ev = assert_split_invariant(data.as_bytes());
        assert_eq!(
            ev,
            vec![OscEvent::Status(StatusReport {
                id: None,
                state: ReportState::Blocked,
                kind: Some("permission".into()),
                progress: None,
                app: Some("terraform".into()),
                title: None,
                msg: Some("Apply 3 to add?".into()),
            })]
        );
    }

    #[test]
    fn status_st_terminator_and_all_states() {
        for (name, st) in [
            ("idle", ReportState::Idle),
            ("working", ReportState::Working),
            ("done", ReportState::Done),
            ("blocked", ReportState::Blocked),
            ("error", ReportState::Error),
            ("clear", ReportState::Clear),
        ] {
            let data = format!("\x1b]7501;state={name}\x1b\\");
            match &assert_split_invariant(data.as_bytes())[..] {
                [OscEvent::Status(r)] => assert_eq!(r.state, st),
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    #[test]
    fn status_progress_only_with_working_or_blocked() {
        let r = parse_status(b"state=working:progress=40").unwrap();
        assert_eq!(r.progress, Some(40));
        let r = parse_status(b"state=done:progress=40").unwrap();
        assert_eq!(r.progress, None);
        assert_eq!(parse_status(b"state=working:progress=101").unwrap().progress, None);
        assert_eq!(parse_status(b"state=working:progress=-3").unwrap().progress, None);
        assert_eq!(parse_status(b"state=working:progress=4x").unwrap().progress, None);
        assert_eq!(parse_status(b"state=working:progress=").unwrap().progress, None);
    }

    #[test]
    fn status_kind_only_with_blocked_and_known() {
        assert_eq!(parse_status(b"state=blocked:kind=auth").unwrap().kind.as_deref(), Some("auth"));
        assert_eq!(parse_status(b"state=blocked:kind=nope").unwrap().kind, None);
        assert_eq!(parse_status(b"state=working:kind=auth").unwrap().kind, None);
    }

    #[test]
    fn status_requires_known_state() {
        assert!(parse_status(b"app=x").is_none());
        assert!(parse_status(b"state=waiting").is_none());
        assert!(parse_status(b"state=").is_none());
        assert!(parse_status(b"").is_none());
        assert!(parse_status(b"?").is_none());
    }

    #[test]
    fn status_malformed_pairs_are_skipped_not_fatal() {
        // no '=', empty key, byte outside the value set, key not [a-z]+.
        let r = parse_status(b"junk:=x:state=working:app=a b:Bad=1:msg=$$$:extra=1").unwrap();
        assert_eq!(r.state, ReportState::Working);
        assert_eq!(r.app, None, "'a b' has a space, the pair is skipped");
        assert_eq!(r.msg, None, "'$$$' is outside the value set, the pair is skipped");
        // whitespace around keys and values is trimmed
        let r = parse_status(b" state = done : app = cargo ").unwrap();
        assert_eq!(r.state, ReportState::Done);
        assert_eq!(r.app.as_deref(), Some("cargo"));
        // a too-long key is skipped
        let r = parse_status(b"state=idle:abcdefghijklmnopq=1").unwrap();
        assert_eq!(r.state, ReportState::Idle);
    }

    #[test]
    fn status_last_duplicate_key_wins() {
        let r = parse_status(b"state=working:state=done").unwrap();
        assert_eq!(r.state, ReportState::Done);
    }

    #[test]
    fn status_unknown_keys_are_ignored() {
        let r = parse_status(b"state=done:future=1:zzz=abc").unwrap();
        assert_eq!(r.state, ReportState::Done);
    }

    #[test]
    fn status_bad_base64_discards_the_whole_report() {
        assert!(parse_status(b"state=blocked:msg=A").is_none());
        assert!(parse_status(b"state=blocked:msg=Zm9v=bar").is_none());
        // `!!!` is outside the value set: that pair is skipped, the report stays.
        assert_eq!(parse_status(b"state=blocked:msg=!!!").unwrap().msg, None);
        assert!(parse_status(b"state=done:title=Zm9v=bar").is_none());
    }

    #[test]
    fn status_control_chars_in_text_discard_the_report() {
        for bad in ["a\nb", "tab\there", "nul\0x", "esc\u{1b}[31m", "c1\u{85}x", "del\u{7f}"] {
            let data = format!("state=blocked:msg={}", b64(bad));
            assert!(parse_status(data.as_bytes()).is_none(), "{bad:?}");
            let data = format!("state=blocked:title={}", b64(bad));
            assert!(parse_status(data.as_bytes()).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn status_invalid_utf8_discards_the_report() {
        let enc = base64::engine::general_purpose::STANDARD.encode([0xff, 0xfe, 0x41]);
        assert!(parse_status(format!("state=done:msg={enc}").as_bytes()).is_none());
    }

    #[test]
    fn status_padding_is_optional() {
        let r = parse_status(b"state=done:msg=Zm9v").unwrap();
        assert_eq!(r.msg.as_deref(), Some("foo"));
        let r = parse_status(b"state=done:msg=Zm8").unwrap();
        assert_eq!(r.msg.as_deref(), Some("fo"));
        let r = parse_status(b"state=done:msg=Zm8=").unwrap();
        assert_eq!(r.msg.as_deref(), Some("fo"));
    }

    #[test]
    fn status_text_limits() {
        // msg: 2048 decoded bytes is the maximum.
        let ok = "m".repeat(2048);
        assert!(parse_status(format!("state=done:msg={}", b64(&ok)).as_bytes()).is_some());
        let too_big = "m".repeat(2049);
        assert!(parse_status(format!("state=done:msg={}", b64(&too_big)).as_bytes()).is_none());
        // title: 192 decoded bytes is the maximum.
        let ok = "t".repeat(192);
        assert!(parse_status(format!("state=done:title={}", b64(&ok)).as_bytes()).is_some());
        let too_big = "t".repeat(193);
        assert!(parse_status(format!("state=done:title={}", b64(&too_big)).as_bytes()).is_none());
        // encoded size is checked before decoding
        let huge = "A".repeat(MAX_MSG_ENCODED + 4);
        assert!(parse_status(format!("state=done:msg={huge}").as_bytes()).is_none());
    }

    #[test]
    fn status_message_is_sanitised_for_bidi() {
        let r =
            parse_status(format!("state=done:msg={}", b64("ok\u{202e}evil\u{200b}")).as_bytes())
                .unwrap();
        assert_eq!(r.msg.as_deref(), Some("okevil"));
    }

    #[test]
    fn status_id_grammar() {
        assert_eq!(
            parse_status(b"state=working:id=build/test").unwrap().id.as_deref(),
            Some("build/test")
        );
        for bad in [
            "id=",
            "id=/a",
            "id=a/",
            "id=a//b",
            "id=a/b/c/d/e/f/g/h/i", // 9 levels
        ] {
            let data = format!("state=working:{bad}");
            assert!(parse_status(data.as_bytes()).is_none(), "{bad}");
        }
        let seg33 = "s".repeat(33);
        assert!(parse_status(format!("state=working:id={seg33}").as_bytes()).is_none());
        let seg32 = "s".repeat(32);
        assert!(parse_status(format!("state=working:id={seg32}").as_bytes()).is_some());
        let eight = "a/b/c/d/e/f/g/h";
        assert!(parse_status(format!("state=working:id={eight}").as_bytes()).is_some());
        let long_id = ["abcdefghijklmnopqrstuvwxyz"; 6].join("/"); // 6*26+5 = 161 > 128
        assert!(parse_status(format!("state=working:id={long_id}").as_bytes()).is_none());
    }

    #[test]
    fn status_bad_app_is_dropped_but_report_kept() {
        let r = parse_status(b"state=working:app=bad!").unwrap_or_else(|| panic!("kept"));
        // '!' is outside the value set so the pair itself is skipped.
        assert_eq!(r.app, None);
        let long = "a".repeat(33);
        let r = parse_status(format!("state=working:app={long}").as_bytes()).unwrap();
        assert_eq!(r.app, None);
    }

    #[test]
    fn clear_keeps_only_the_id() {
        let r = parse_status(b"state=clear:id=us-east:msg=zzz").unwrap();
        assert_eq!(r.state, ReportState::Clear);
        assert_eq!(r.id.as_deref(), Some("us-east"));
        assert_eq!(r.msg, None);
    }

    #[test]
    fn oversized_sequence_is_discarded_and_scanning_recovers() {
        let mut data = b"\x1b]2;".to_vec();
        data.extend(std::iter::repeat_n(b'a', MAX_OSC_BODY + 10));
        data.push(0x07);
        data.extend_from_slice(b"\x1b]2;after\x07");
        let (ev, sc) = scan_chunks(&[&data]);
        assert_eq!(ev, vec![OscEvent::Title("after".into())]);
        assert_eq!(sc.discarded_oversized(), 1);

        // Same, delivered in small chunks.
        let chunks: Vec<&[u8]> = data.chunks(7).collect();
        let (ev, sc) = scan_chunks(&chunks);
        assert_eq!(ev, vec![OscEvent::Title("after".into())]);
        assert_eq!(sc.discarded_oversized(), 1);
    }

    #[test]
    fn body_exactly_at_the_cap_is_accepted() {
        let mut data = b"\x1b]2;".to_vec();
        // Body after "2;" is 4096 bytes: the cap counts the body only.
        data.extend(std::iter::repeat_n(b'a', MAX_OSC_BODY));
        data.push(0x07);
        let (ev, sc) = scan_chunks(&[&data]);
        assert_eq!(ev.len(), 1);
        assert_eq!(sc.discarded_oversized(), 0);
        let mut data = b"\x1b]2;".to_vec();
        data.extend(std::iter::repeat_n(b'a', MAX_OSC_BODY + 1));
        data.push(0x07);
        let (ev, sc) = scan_chunks(&[&data]);
        assert!(ev.is_empty());
        assert_eq!(sc.discarded_oversized(), 1);
    }

    #[test]
    fn oversized_sequence_ending_with_st_recovers() {
        let mut data = b"\x1b]7501;".to_vec();
        data.extend(std::iter::repeat_n(b'a', MAX_OSC_BODY * 2));
        data.extend_from_slice(b"\x1b\\\x1b]2;next\x07");
        let ev = scan(&data);
        assert_eq!(ev, vec![OscEvent::Title("next".into())]);
    }

    #[test]
    fn unused_osc_numbers_are_skipped_without_buffering() {
        // OSC 8 hyperlink, OSC 52 clipboard (huge), OSC 1337 image.
        let mut data = b"\x1b]8;;http://x\x07link\x1b]8;;\x07".to_vec();
        data.extend_from_slice(b"\x1b]52;c;");
        data.extend(std::iter::repeat_n(b'Q', 200_000));
        data.push(0x07);
        data.extend_from_slice(b"\x1b]2;ok\x07");
        let (ev, sc) = scan_chunks(&[&data]);
        assert_eq!(ev, vec![OscEvent::Title("ok".into())]);
        assert_eq!(sc.discarded_oversized(), 0, "uninteresting OSCs are not counted");
        assert!(sc.buf.capacity() <= 8192, "no buffering for skipped OSCs");
    }

    #[test]
    fn csi_and_plain_text_produce_nothing() {
        assert!(assert_split_invariant(b"\x1b[31mred\x1b[0m plain \x1b[2J\x1b[H").is_empty());
        assert!(scan(b"just text\r\n").is_empty());
    }

    #[test]
    fn esc_then_non_bracket_does_not_start_an_osc() {
        // ESC P (DCS) containing "]" text must not start an OSC.
        assert!(scan(b"\x1bP1;2]2;nope\x07").is_empty());
        // ESC ESC ] still starts one.
        assert_eq!(assert_split_invariant(b"\x1b\x1b]2;x\x07"), vec![OscEvent::Title("x".into())]);
    }

    #[test]
    fn new_escape_aborts_a_running_osc() {
        // The first OSC is cut short by ESC [ ..., and a new OSC follows.
        let data = b"\x1b]2;broken\x1b[31m\x1b]2;good\x07";
        assert_eq!(assert_split_invariant(data), vec![OscEvent::Title("good".into())]);
        // ESC ] inside an OSC starts a fresh one.
        let data = b"\x1b]2;broken\x1b]2;good2\x07";
        assert_eq!(assert_split_invariant(data), vec![OscEvent::Title("good2".into())]);
    }

    #[test]
    fn can_and_sub_cancel_a_sequence() {
        assert!(scan(b"\x1b]2;gone\x18tail\x07").is_empty());
        assert!(scan(b"\x1b]2;gone\x1atail\x07").is_empty());
        assert_eq!(scan(b"\x1b]2;gone\x18\x1b]2;kept\x07"), vec![OscEvent::Title("kept".into())]);
    }

    #[test]
    fn bad_numbers_are_skipped() {
        assert!(scan(b"\x1b]x;y\x07").is_empty());
        assert!(scan(b"\x1b]123456789;y\x07").is_empty());
        assert!(scan(b"\x1b];y\x07").is_empty());
        assert!(scan(b"\x1b]2\x07").is_empty());
        assert_eq!(scan(b"\x1b]2;\x07"), vec![OscEvent::Title(String::new())]);
    }

    #[test]
    fn many_sequences_in_one_chunk_arrive_in_order() {
        let data = b"a\x1b]2;one\x07b\x1b]7;file:///x\x07c\x1b]9;note\x07d\x1b]133;A\x07";
        let ev = assert_split_invariant(data);
        assert_eq!(
            ev,
            vec![
                OscEvent::Title("one".into()),
                OscEvent::Cwd("/x".into()),
                OscEvent::Notification { title: None, body: "note".into() },
                OscEvent::PromptStart,
            ]
        );
    }

    #[test]
    fn utf8_split_across_chunks_in_title() {
        let data = "\x1b]2;héllo wörld ✓\x07".as_bytes();
        let ev = assert_split_invariant(data);
        assert_eq!(ev, vec![OscEvent::Title("héllo wörld ✓".into())]);
    }

    #[test]
    fn st_split_between_esc_and_backslash() {
        let (ev, _) = scan_chunks(&[b"\x1b]2;t\x1b", b"\\"]);
        assert_eq!(ev, vec![OscEvent::Title("t".into())]);
        let (ev, _) = scan_chunks(&[b"\x1b", b"]2;t\x07"]);
        assert_eq!(ev, vec![OscEvent::Title("t".into())]);
        let (ev, _) = scan_chunks(&[b"\x1b]", b"2;t\x07"]);
        assert_eq!(ev, vec![OscEvent::Title("t".into())]);
        let (ev, _) = scan_chunks(&[b"\x1b]2", b";t\x07"]);
        assert_eq!(ev, vec![OscEvent::Title("t".into())]);
    }

    #[test]
    fn kitty_chunks_are_bounded() {
        // Many d=0 chunks cannot grow the pending text without limit.
        let mut sc = OscScanner::new();
        let mut n = 0;
        let chunk = format!("\x1b]99;d=0:p=title;{}\x07", "x".repeat(400));
        for _ in 0..50 {
            sc.feed(chunk.as_bytes(), &mut |_| n += 1);
        }
        assert_eq!(n, 0);
        let p = sc.pending99.as_ref().unwrap();
        assert!(p.title.chars().count() <= MAX_NOTIFY_CHARS);
    }
}
