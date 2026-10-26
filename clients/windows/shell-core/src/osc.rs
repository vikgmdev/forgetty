//! Scans a pane's output bytes for the OSC sequences the shell cares about:
//! title (0, 2), cwd (7, and 9;9 from ConPTY), and shell-integration marks
//! (133 A/C/D) which give the program status, and the notification sequences (OSC 9, 99, 777).
//! The bytes are not modified.
//! Sequences may be split across reads.

use crate::backend::PaneStatus;

#[derive(Clone, Debug, PartialEq)]
pub enum OscEvent {
    Title(String),
    Cwd(String),
    Status(PaneStatus, String),
    /// A program asked for a desktop notification (OSC 9, 99 or 777). The text is untrusted.
    Notify { title: String, body: String },
}

#[derive(Default)]
pub struct OscScanner {
    state: State,
    buf: Vec<u8>,
    /// 133;D result is sticky until the next command starts.
    last: PaneStatus,
    /// A kitty notification (OSC 99) that is still arriving in chunks (`d=0`).
    chunked: Option<(String, String)>,
}

#[derive(Default, PartialEq, Clone, Copy)]
enum State {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
}

const MAX_OSC: usize = 4096;

impl OscScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
        let mut out = Vec::new();
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => {
                    if b == b']' {
                        self.state = State::Osc;
                        self.buf.clear();
                    } else if b == 0x1b {
                        // stay in Esc
                    } else {
                        self.state = State::Ground;
                    }
                }
                State::Osc => match b {
                    0x07 => self.finish(&mut out),
                    0x1b => self.state = State::OscEsc,
                    // CAN and SUB abort a sequence.
                    0x18 | 0x1a => self.state = State::Ground,
                    _ => {
                        if self.buf.len() < MAX_OSC {
                            self.buf.push(b);
                        } else {
                            self.state = State::Ground;
                        }
                    }
                },
                State::OscEsc => {
                    if b == b'\\' {
                        self.finish(&mut out);
                    } else {
                        // Not a string terminator: the OSC is aborted, ESC starts a new sequence.
                        self.state = if b == b']' { State::Osc } else { State::Ground };
                        self.buf.clear();
                    }
                }
            }
        }
        out
    }

    fn finish(&mut self, out: &mut Vec<OscEvent>) {
        self.state = State::Ground;
        let text = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        let (num, rest) = match text.split_once(';') {
            Some((n, r)) => (n, r),
            None => (text.as_str(), ""),
        };
        match num {
            "0" | "2" => out.push(OscEvent::Title(rest.to_string())),
            "7" => {
                if let Some(p) = parse_file_url(rest) {
                    out.push(OscEvent::Cwd(p));
                }
            }
            "9" => {
                // ConPTY / Windows Terminal: OSC 9;9;"C:\path"
                if let Some(p) = rest.strip_prefix("9;") {
                    let p = p.trim().trim_matches('"');
                    if !p.is_empty() && is_safe_local_path(p) {
                        out.push(OscEvent::Cwd(p.to_string()));
                    }
                } else if !is_conemu_command(rest) && !rest.trim().is_empty() {
                    // OSC 9;text (iTerm2, ConEmu): a notification with a body only.
                    out.push(OscEvent::Notify { title: String::new(), body: rest.to_string() });
                }
            }
            "777" => {
                // OSC 777;notify;title;body (rxvt-unicode). Other 777 verbs are not ours.
                if let Some(r) = rest.strip_prefix("notify;") {
                    let (title, body) = r.split_once(';').unwrap_or((r, ""));
                    out.push(OscEvent::Notify { title: title.to_string(), body: body.to_string() });
                }
            }
            "99" => self.kitty_notification(rest, out),
            "133" => {
                let mut parts = rest.split(';');
                match parts.next().unwrap_or("") {
                    "A" | "B" => {
                        // A prompt is `idle`: it neither shows an icon nor raises a toast.
                        if self.last == PaneStatus::None || self.last == PaneStatus::Working {
                            self.last = PaneStatus::Idle;
                            out.push(OscEvent::Status(PaneStatus::Idle, String::new()));
                        }
                    }
                    "C" => {
                        self.last = PaneStatus::Working;
                        out.push(OscEvent::Status(PaneStatus::Working, String::new()));
                    }
                    "D" => {
                        let code = parts.next().and_then(|c| c.trim().parse::<i32>().ok()).unwrap_or(0);
                        let st = if code == 0 { PaneStatus::Done } else { PaneStatus::Failed };
                        self.last = st;
                        out.push(OscEvent::Status(st, format!("exit {code}")));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// `OSC 9;<n>[;...]` with a number is a ConEmu command (progress bar, sleep, message box...),
/// not a notification. GTK skips only `4`; the other numbered commands are skipped here too.
fn is_conemu_command(rest: &str) -> bool {
    let head = rest.split(';').next().unwrap_or("");
    let numbered = !head.is_empty() && head.len() <= 2 && head.bytes().all(|b| b.is_ascii_digit());
    numbered && (rest.len() == head.len() || rest.as_bytes()[head.len()] == b';')
}

impl OscScanner {
    /// OSC 99 (kitty): `99;key=value:key=value;payload`. Handled: payload type `title` (the
    /// default) and `body`, and chunks (`d=0` means more follows). Not handled, and dropped:
    /// `close`, query (`?`), `alive`, base64 payloads (`e=1`) and icons.
    fn kitty_notification(&mut self, rest: &str, out: &mut Vec<OscEvent>) {
        let (meta, payload) = rest.split_once(';').unwrap_or((rest, ""));
        let (mut kind, mut done, mut encoded) = ("title", true, false);
        for kv in meta.split(':') {
            match kv.split_once('=') {
                Some(("p", v)) => kind = v,
                Some(("d", v)) => done = v != "0",
                Some(("e", v)) => encoded = v == "1",
                _ => {}
            }
        }
        if encoded || !matches!(kind, "title" | "body") {
            self.chunked = None;
            return;
        }
        let (mut title, mut body) = self.chunked.take().unwrap_or_default();
        let slot = if kind == "title" { &mut title } else { &mut body };
        if slot.len() + payload.len() <= 2048 {
            slot.push_str(payload);
        }
        if done {
            if !title.is_empty() || !body.is_empty() {
                out.push(OscEvent::Notify { title, body });
            }
        } else {
            self.chunked = Some((title, body));
        }
    }
}

/// Host names that mean "this machine" in an OSC 7 URL.
fn is_local_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    if h.is_empty() || h == "localhost" || h == "127.0.0.1" || h == "[::1]" || h == "::1" {
        return true;
    }
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .any(|n| !n.is_empty() && n.to_ascii_lowercase() == h)
}

/// A path that is safe to show and to use as a local directory: no UNC share,
/// no device namespace (`\\?\`, `\\.\`), no NT namespace (`\??\`, which reaches `\??\UNC\srv\share`),
/// no `//server` form, with either separator in any mix. Escape codes
/// come from untrusted program output, and touching a UNC path on Windows
/// sends NTLM credentials to the named server.
pub fn is_safe_local_path(p: &str) -> bool {
    let b = p.as_bytes();
    if b.is_empty() || b.contains(&0) {
        return false;
    }
    let sep = |c: u8| c == b'\\' || c == b'/';
    if b.len() >= 2 && sep(b[0]) && sep(b[1]) {
        return false;
    }
    if b.len() >= 4 && sep(b[0]) && b[1] == b'?' && b[2] == b'?' && sep(b[3]) {
        return false;
    }
    true
}

fn hex_val(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// `file://host/C:/dir/file` or `file:///home/x` to a plain local path.
/// Returns `None` for non-local hosts and for unsafe paths. The result is a
/// display hint only, never a spawn directory.
pub fn parse_file_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    if !is_local_host(&rest[..slash]) {
        return None;
    }
    let raw = rest[slash..].as_bytes();
    // Decode on bytes: a multi-byte char must never be split.
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let (Some(h), Some(l)) = (hex_val(raw[i + 1]), hex_val(raw[i + 2])) {
                bytes.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let s = String::from_utf8_lossy(&bytes).into_owned();
    // "/C:/dir" is a Windows drive path.
    let b = s.as_bytes();
    let out = if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        s[1..].replace('/', "\\")
    } else {
        s
    };
    is_safe_local_path(&out).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_cwd_status_split_across_reads() {
        let mut s = OscScanner::new();
        let mut ev = s.feed(b"hi \x1b]0;my ti");
        assert!(ev.is_empty());
        ev.extend(s.feed(b"tle\x07\x1b]7;file://localhost/C:/Users/a%20b\x1b\\"));
        assert_eq!(ev[0], OscEvent::Title("my title".into()));
        assert_eq!(ev[1], OscEvent::Cwd("C:\\Users\\a b".into()));
        let ev = s.feed(b"\x1b]9;9;\"D:\\x\"\x07");
        assert_eq!(ev, vec![OscEvent::Cwd("D:\\x".into())]);
    }

    #[test]
    fn status_marks() {
        let mut s = OscScanner::new();
        let ev = s.feed(b"\x1b]133;A\x07\x1b]133;C\x07\x1b]133;D;2\x07\x1b]133;A\x07");
        assert_eq!(
            ev,
            vec![
                OscEvent::Status(PaneStatus::Idle, "".into()),
                OscEvent::Status(PaneStatus::Working, "".into()),
                OscEvent::Status(PaneStatus::Failed, "exit 2".into()),
            ]
        );
        let ev = s.feed(b"\x1b]133;C\x07\x1b]133;D;0\x07");
        assert_eq!(ev[1], OscEvent::Status(PaneStatus::Done, "exit 0".into()));
    }

    #[test]
    fn unix_url_and_garbage() {
        assert_eq!(parse_file_url("file:///home/x/y"), Some("/home/x/y".into()));
        assert_eq!(parse_file_url("http://x"), None);
        // Non-local hosts, UNC and device paths never become a cwd.
        assert_eq!(parse_file_url("file://evil/share"), None);
        assert_eq!(parse_file_url("file://evil/C:/x"), None);
        assert_eq!(parse_file_url("file://localhost//evil/share"), None);
        assert_eq!(parse_file_url("file:////evil/share"), None);
        assert!(!is_safe_local_path("\\\\evil\\share"));
        assert!(!is_safe_local_path("\\\\?\\C:\\x"));
        assert!(!is_safe_local_path("\\\\.\\pipe\\x"));
        assert!(!is_safe_local_path("//evil/share"));
        assert!(!is_safe_local_path("\\??\\UNC\\evil\\share"));
        assert!(!is_safe_local_path("/??/UNC/evil/share"));
        assert!(!is_safe_local_path("\\??/C:\\x"));
        assert!(!is_safe_local_path("/??\\C:\\x"));
        assert!(is_safe_local_path("C:\\??\\x"), "only a leading prefix is the NT namespace");
        assert!(is_safe_local_path("C:\\Users\\a"));
        let mut s = OscScanner::new();
        assert!(s.feed(b"\x1b]7;file://evil/share\x07\x1b]9;9;\"\\\\evil\\share\"\x07").is_empty());
        // Multi-byte chars around a percent sign must not panic.
        let _ = parse_file_url("file:///a%\u{e9}\u{e9}b%4\u{e9}");
        let _ = parse_file_url("file:///%\u{1F600}");
        let mut s = OscScanner::new();
        assert!(s.feed(b"\x1b[31mred\x1b[0m \x1b]0;t\x18more").is_empty());
    }

    fn notes(bytes: &[u8]) -> Vec<(String, String)> {
        OscScanner::new()
            .feed(bytes)
            .into_iter()
            .filter_map(|e| match e {
                OscEvent::Notify { title, body } => Some((title, body)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn osc9_and_osc777_notifications() {
        assert_eq!(notes(b"\x1b]9;build done\x07"), vec![("".into(), "build done".into())]);
        assert_eq!(notes(b"\x1b]9;build; done\x1b\\"), vec![("".into(), "build; done".into())]);
        assert_eq!(notes(b"\x1b]777;notify;My Title;My Body\x07"), vec![("My Title".into(), "My Body".into())]);
        assert_eq!(notes(b"\x1b]777;notify;Only title\x07"), vec![("Only title".into(), "".into())]);
        assert_eq!(notes(b"\x1b]777;notify;t;a;b;c\x07"), vec![("t".into(), "a;b;c".into())]);
        assert!(notes(b"\x1b]777;precmd\x07").is_empty());
        assert!(notes(b"\x1b]9;\x07").is_empty());
    }

    #[test]
    fn conemu_commands_are_not_notifications() {
        // 9;4 is the progress bar (GTK skips it); the other numbered commands go too.
        for s in ["\x1b]9;4;50\x07", "\x1b]9;4\x07", "\x1b]9;1;500\x07", "\x1b]9;3;title\x07", "\x1b]9;12\x07"] {
            assert!(notes(s.as_bytes()).is_empty(), "{s:?}");
        }
        // Text that merely starts with a digit is a notification.
        assert_eq!(notes(b"\x1b]9;5 minutes left\x07").len(), 1);
        assert_eq!(notes(b"\x1b]9;100% done\x07").len(), 1);
        // 9;9 is still the working directory, not a notification.
        let ev = OscScanner::new().feed(b"\x1b]9;9;\"C:\\x\"\x07");
        assert_eq!(ev, vec![OscEvent::Cwd("C:\\x".into())]);
    }

    #[test]
    fn kitty_notifications() {
        assert_eq!(notes(b"\x1b]99;;Hello\x07"), vec![("Hello".into(), "".into())]);
        assert_eq!(notes(b"\x1b]99;i=1:p=body;the body\x07"), vec![("".into(), "the body".into())]);
        // Title and body in chunks.
        assert_eq!(
            notes(b"\x1b]99;i=1:d=0:p=title;Build\x07\x1b]99;i=1:p=body;passed\x07"),
            vec![("Build".into(), "passed".into())]
        );
        // Not handled: close, query, base64.
        assert!(notes(b"\x1b]99;i=1:p=close;\x07").is_empty());
        assert!(notes(b"\x1b]99;i=1:p=?;\x07").is_empty());
        assert!(notes(b"\x1b]99;e=1:p=title;SGVsbG8=\x07").is_empty());
        // A started chunk run that never ends does not leak into the next notification.
        let mut s = OscScanner::new();
        s.feed(b"\x1b]99;d=0:p=title;stale\x07\x1b]99;p=close;\x07");
        let ev = s.feed(b"\x1b]99;;fresh\x07");
        assert_eq!(ev, vec![OscEvent::Notify { title: "fresh".into(), body: "".into() }]);
    }

    #[test]
    fn a_notification_split_across_reads_arrives_once() {
        let mut s = OscScanner::new();
        assert!(s.feed(b"x\x1b]777;notify;Ti").is_empty());
        let ev = s.feed(b"tle;Bo");
        assert!(ev.is_empty());
        let ev = s.feed(b"dy\x07tail");
        assert_eq!(ev, vec![OscEvent::Notify { title: "Title".into(), body: "Body".into() }]);
    }

    /// Random bytes (biased towards escape and URL fragments) never panic.
    #[test]
    fn fuzz_no_panic() {
        let mut x: u64 = 0x9E3779B97F4A7C15;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let frags: [&[u8]; 14] = [
            b"\x1b]", b"\x1b\\", b"\x07", b"7;file://", b"133;", b"9;9;", b"0;", b"%", b"%4", b"%\xc3\xa9",
            b"\xf0\x9f\x98\x80", b"C:/", b"\x18", b";D;",
        ];
        let mut sc = OscScanner::new();
        for _ in 0..100_000 {
            let n = (rnd() % 48) as usize;
            let mut buf = Vec::with_capacity(n);
            while buf.len() < n {
                if rnd() % 3 == 0 {
                    buf.extend_from_slice(frags[(rnd() % frags.len() as u64) as usize]);
                } else {
                    buf.push(rnd() as u8);
                }
            }
            let _ = sc.feed(&buf);
            if let Ok(t) = std::str::from_utf8(&buf) {
                let _ = parse_file_url(t);
                let _ = parse_file_url(&format!("file:///{t}"));
            }
        }
    }
}
