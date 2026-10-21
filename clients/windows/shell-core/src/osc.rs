//! Scans a pane's output bytes for the OSC sequences the shell cares about:
//! title (0, 2), cwd (7, and 9;9 from ConPTY), and shell-integration marks
//! (133 A/C/D) which give the program status. The bytes are not modified.
//! Sequences may be split across reads.

use crate::backend::PaneStatus;

#[derive(Clone, Debug, PartialEq)]
pub enum OscEvent {
    Title(String),
    Cwd(String),
    Status(PaneStatus, String),
}

#[derive(Default)]
pub struct OscScanner {
    state: State,
    buf: Vec<u8>,
    /// 133;D result is sticky until the next command starts.
    last: PaneStatus,
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
                    if !p.is_empty() {
                        out.push(OscEvent::Cwd(p.to_string()));
                    }
                }
            }
            "133" => {
                let mut parts = rest.split(';');
                match parts.next().unwrap_or("") {
                    "A" | "B" => {
                        if self.last == PaneStatus::None || self.last == PaneStatus::Working {
                            self.last = PaneStatus::Waiting;
                            out.push(OscEvent::Status(PaneStatus::Waiting, String::new()));
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

/// `file://host/C:/dir/file` or `file:///home/x` to a plain path.
pub fn parse_file_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let mut bytes = Vec::with_capacity(path.len());
    let raw = path.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let Ok(v) = u8::from_str_radix(&path[i + 1..i + 3], 16) {
                bytes.push(v);
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
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        return Some(s[1..].replace('/', "\\"));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_cwd_status_split_across_reads() {
        let mut s = OscScanner::new();
        let mut ev = s.feed(b"hi \x1b]0;my ti");
        assert!(ev.is_empty());
        ev.extend(s.feed(b"tle\x07\x1b]7;file://PC/C:/Users/a%20b\x1b\\"));
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
                OscEvent::Status(PaneStatus::Waiting, "".into()),
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
        let mut s = OscScanner::new();
        assert!(s.feed(b"\x1b[31mred\x1b[0m \x1b]0;t\x18more").is_empty());
    }
}
