//! The local server (WP6 of `docs/architecture/WINDOWS_SERVER.md`): the rules the app uses to
//! find `forgetty.exe`, decide between the server and the in-process terminal, read what the
//! CLI prints, and decide when a stopped server is started again. No process and no window code:
//! the app's `localserver` module runs the processes, this module decides and parses. All of it
//! is tested on Linux.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::notify::sanitize;

/// The CLI and server binary, next to the app.
pub const SERVER_EXE: &str = "forgetty.exe";
/// `forgetty server run` exits with this when it runs elevated (`EXIT_CONFIG`).
pub const EXIT_CONFIG: i32 = 78;
/// What the sidebar calls the machine behind the local server.
pub const THIS_PC: &str = "This PC";
/// How long the pairing window of the automatic pairing stays open.
pub const AUTO_PAIR_TTL_SECS: u32 = 30;
/// Seconds of a pairing window shown to the user ("Pair new device").
pub const USER_PAIR_TTL_SECS: u32 = 120;

/// `forgetty.exe` next to the app, and nowhere else: a program found through `PATH` or the
/// current folder could be put there by another program, and the app hands it the user's
/// terminals. A folder with that name is not the program.
pub fn find_exe(app_dir: Option<&Path>, name: &str) -> Option<PathBuf> {
    let p = app_dir?.join(name);
    p.is_file().then_some(p)
}

/// `FORGETTY_SERVER_EXE`, for a developer: an absolute path to a file on a local drive. A
/// relative path, a UNC share (`\\srv\x`) and a device path (`\\?\`, `\\.\`, `\??\`) are
/// refused with the reason. `Ok(None)`: the variable is not set (or is empty).
pub fn override_exe(value: Option<&OsStr>) -> Result<Option<PathBuf>, String> {
    let Some(v) = value.filter(|v| !v.is_empty()) else { return Ok(None) };
    let shown = sanitize(&v.to_string_lossy(), 60);
    let text = v.to_string_lossy();
    if !crate::osc::is_safe_local_path(&text) {
        return Err(format!("FORGETTY_SERVER_EXE {shown} is a network or device path"));
    }
    let p = PathBuf::from(v);
    if !p.is_absolute() {
        return Err(format!("FORGETTY_SERVER_EXE {shown} is not an absolute path"));
    }
    if !p.is_file() {
        return Err(format!("FORGETTY_SERVER_EXE {shown} is not a file"));
    }
    Ok(Some(p))
}

/// Does this process run elevated? An elevated window never uses the server: the server refuses
/// to run elevated, and a normal program could otherwise use an elevated server to run commands
/// as administrator around the UAC prompt. When it cannot be told, yes.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        forgetty_core::fsutil::acl::is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Why the app uses its own terminal and not the local server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fallback {
    /// `local_server = false` in the config: the user's choice, nothing to say.
    Disabled,
    Elevated,
    NotFound,
    Start(String),
    Pair(String),
    /// The server ran and then stopped, and did not start again.
    Stopped(String),
}

impl Fallback {
    /// The notice shown on the first pane. `None` for a choice the user made.
    pub fn notice(&self) -> Option<String> {
        let tail = "Using the built-in terminal: its tabs end when this window closes.";
        let retry = " Retry: command palette, \"Retry the local server\".";
        let why = |w: &str| sanitize(w, 100);
        Some(match self {
            Fallback::Disabled => return None,
            Fallback::Elevated => {
                "This window runs as administrator, so it uses the built-in terminal: its tabs end when the window closes.".to_string()
            }
            Fallback::NotFound => format!("The local server (forgetty.exe) was not found next to the app. {tail}"),
            Fallback::Start(w) => format!("The local server did not start ({}). {tail}{retry}", why(w)),
            Fallback::Pair(w) => format!("Cannot connect to the local server ({}). {tail}{retry}", why(w)),
            Fallback::Stopped(w) => format!("The local server stopped and did not start again ({}). {tail}{retry}", why(w)),
        })
    }
}

/// What the app does at start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    Server(PathBuf),
    InProcess(Fallback),
}

pub fn plan(enabled: bool, elevated: bool, exe: Option<PathBuf>) -> Plan {
    if !enabled {
        return Plan::InProcess(Fallback::Disabled);
    }
    if elevated {
        return Plan::InProcess(Fallback::Elevated);
    }
    match exe {
        Some(p) => Plan::Server(p),
        None => Plan::InProcess(Fallback::NotFound),
    }
}

/// The last useful line of a CLI failure: stderr, else stdout, one line, bounded. Never the
/// pipe name (it is a secret): a line that holds `\\.\pipe\` is dropped.
pub fn cli_reason(stdout: &str, stderr: &str, code: Option<i32>) -> String {
    let pick = |s: &str| {
        s.lines()
            .map(str::trim)
            .rev()
            .find(|l| !l.is_empty() && !l.contains("\\pipe\\") && !l.contains("//./pipe/"))
            .map(|l| sanitize(l.trim_start_matches("forgetty: ").trim_start_matches("error: "), 100))
    };
    if code == Some(EXIT_CONFIG) {
        if let Some(l) = pick(stderr).or_else(|| pick(stdout)) {
            return l;
        }
        return "the server refused to start".into();
    }
    pick(stderr)
        .or_else(|| pick(stdout))
        .unwrap_or_else(|| match code {
            Some(c) => format!("exit code {c}"),
            None => "ended without an exit code".into(),
        })
}

/// What `forgetty server status --json` says that the app needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerStatus {
    pub node_id: String,
    pub pid: u64,
    /// The UDP port of the server's endpoint, when `status --json` says one.
    pub port: Option<u16>,
}

/// The server's default port, used while `status --json` names none.
pub const DEFAULT_PORT: u16 = 47297;

impl ServerStatus {
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }
}

/// The port in a status document: a `port` number, else the port of the first address of a list
/// (`addresses`, `direct_addrs` or `addrs`, each `ip:port`) that is on loopback, else of the first.
fn status_port(v: &serde_json::Value) -> Option<u16> {
    let ok = |n: u64| u16::try_from(n).ok().filter(|p| *p != 0);
    if let Some(p) = v.get("port").and_then(|p| p.as_u64()).and_then(ok) {
        return Some(p);
    }
    for key in ["addresses", "direct_addrs", "addrs"] {
        let Some(list) = v.get(key).and_then(|l| l.as_array()) else { continue };
        let addrs: Vec<std::net::SocketAddr> = list.iter().filter_map(|a| a.as_str()?.trim().parse().ok()).filter(|a: &std::net::SocketAddr| a.port() != 0).collect();
        if let Some(a) = addrs.iter().find(|a| a.ip().is_loopback()).or(addrs.first()) {
            return Some(a.port());
        }
    }
    None
}

pub fn parse_status(json: &str) -> Option<ServerStatus> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    if v.get("running").and_then(|r| r.as_bool()) == Some(false) {
        return None;
    }
    let node_id = v.get("node_id")?.as_str()?.trim().to_string();
    if node_id.is_empty() {
        return None;
    }
    Some(ServerStatus { node_id, pid: v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0), port: status_port(&v) })
}

/// A device paired with the server (`forgetty devices --json`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Device {
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub paired_at: Option<String>,
    #[serde(default)]
    pub last_seen: Option<String>,
}

impl Device {
    /// Name for a list: no control characters, bounded.
    pub fn shown_name(&self) -> String {
        let n = sanitize(&self.name, 48);
        if n.is_empty() {
            "(unnamed device)".into()
        } else {
            n
        }
    }

    /// First 8 characters of the id, to tell two devices of one name apart.
    pub fn short_id(&self) -> String {
        sanitize(&self.device_id, 64).chars().take(8).collect()
    }
}

pub fn parse_devices(json: &str) -> Result<Vec<Device>, String> {
    #[derive(Deserialize)]
    struct Out {
        devices: Vec<serde_json::Value>,
    }
    let o: Out = serde_json::from_str(json.trim()).map_err(|e| format!("device list: {e}"))?;
    // One entry that does not parse is skipped; the others show.
    Ok(o.devices.into_iter().filter_map(|d| serde_json::from_value::<Device>(d).ok()).filter(|d| !d.device_id.is_empty()).collect())
}

/// Is this device the app itself? (Its id is the public key of the app's identity.)
pub fn is_own_device(d: &Device, own_id: &str) -> bool {
    !own_id.is_empty() && d.device_id == own_id
}

/// What one line of `forgetty pair` means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairLine {
    Payload,
    Paired { name: String, id: String },
    Expired,
    Other,
}

pub fn pair_line(line: &str) -> PairLine {
    let l = line.trim();
    if l.starts_with("payload:") {
        return PairLine::Payload;
    }
    if l == "expired" {
        return PairLine::Expired;
    }
    if let Some(rest) = l.strip_prefix("paired:") {
        let rest = rest.trim();
        let (name, id) = match rest.rsplit_once(" (") {
            Some((n, i)) => (n.trim(), i.trim_end_matches(')').trim()),
            None => (rest, ""),
        };
        return PairLine::Paired { name: sanitize(name, 48), id: sanitize(id, 64) };
    }
    PairLine::Other
}

/// The QR code of `forgetty pair` as dark and light modules, to draw in the "Pair new device"
/// window. The CLI prints it with half blocks for a terminal (1 text row = 2 module rows), in
/// the polarity of that terminal. The quiet zone (the ring around the code) is light, so the
/// ring tells which polarity the text has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrGrid {
    pub w: usize,
    pub h: usize,
    /// Row-major, `true` = dark module.
    pub dark: Vec<bool>,
}

impl QrGrid {
    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        x < self.w && y < self.h && self.dark[y * self.w + x]
    }
}

pub fn qr_from_ascii(text: &str) -> Option<QrGrid> {
    let is_cell = |c: char| matches!(c, ' ' | '\u{2580}' | '\u{2584}' | '\u{2588}');
    let mut rows: Vec<Vec<char>> = Vec::new();
    for line in text.lines() {
        let l = line.trim_end_matches('\r');
        if !l.is_empty() && l.chars().all(is_cell) {
            rows.push(l.chars().collect());
        } else if rows.is_empty() {
            continue;
        } else {
            break;
        }
    }
    // A QR code is at least 21 modules (version 1) plus its quiet zone.
    let w = rows.iter().map(Vec::len).max()?;
    if rows.len() < 11 || w < 21 || w > 400 || rows.len() > 400 {
        return None;
    }
    let h = rows.len() * 2;
    let mut filled = vec![false; w * h];
    for (ry, row) in rows.iter().enumerate() {
        for (x, c) in row.iter().enumerate() {
            let (top, bottom) = match c {
                '\u{2580}' => (true, false),
                '\u{2584}' => (false, true),
                '\u{2588}' => (true, true),
                _ => (false, false),
            };
            filled[(ry * 2) * w + x] = top;
            filled[(ry * 2 + 1) * w + x] = bottom;
        }
    }
    // The outer ring is the quiet zone: light. When most of it is filled, filled means light.
    let (mut ring, mut ring_filled) = (0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            if x == 0 || y == 0 || x == w - 1 || y == h - 1 {
                ring += 1;
                ring_filled += usize::from(filled[y * w + x]);
            }
        }
    }
    let invert = ring_filled * 2 > ring;
    let dark: Vec<bool> = filled.iter().map(|f| *f != invert).collect();
    // Both ways round, a real code has dark modules in a good share of its area.
    let n_dark = dark.iter().filter(|d| **d).count();
    if n_dark * 10 < dark.len() || n_dark * 10 > dark.len() * 8 {
        return None;
    }
    Some(QrGrid { w, h, dark })
}

/// What the supervisor does about a server that is not answering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Watch {
    /// The link may only be reconnecting: look again later.
    Wait,
    /// Start the server again (`forgetty server start` does nothing when one runs).
    Restart,
    /// Too many tries: the app falls back to the built-in terminal.
    GiveUp,
}

/// Link down for `down` and `tries` restarts already made, the last one `since_try` ago.
pub fn watch(down: Duration, tries: u32, since_try: Option<Duration>) -> Watch {
    // A brief drop reconnects by itself.
    if down < Duration::from_secs(3) {
        return Watch::Wait;
    }
    if tries >= MAX_RESTARTS {
        // The last restart gets time to bring the link up.
        return match since_try {
            Some(s) if s < Duration::from_secs(25) => Watch::Wait,
            _ => Watch::GiveUp,
        };
    }
    // Waits between tries: 0, 5, 15 seconds.
    let gap = Duration::from_secs(match tries {
        0 => 0,
        1 => 5,
        _ => 15,
    });
    match since_try {
        Some(s) if s < gap => Watch::Wait,
        _ => Watch::Restart,
    }
}

/// Restarts the supervisor makes before it gives up.
pub const MAX_RESTARTS: u32 = 3;
/// A server that goes down again and again is not restarted for ever: this many restarts within
/// [`RESTART_WINDOW`] end it (the built-in terminal, a notice, and "Retry the local server").
pub const MAX_RESTARTS_IN_WINDOW: usize = 5;
pub const RESTART_WINDOW: Duration = Duration::from_secs(600);

/// `ages`: how long ago each restart was made. True when one more would be the sixth in ten
/// minutes.
pub fn too_many_restarts(ages: &[Duration]) -> bool {
    ages.iter().filter(|a| **a < RESTART_WINDOW).count() >= MAX_RESTARTS_IN_WINDOW
}

/// Where the app dials its own server: loopback and `port`.
pub fn loopback_addr(port: u16) -> std::net::SocketAddr {
    std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)
}

/// What to do with the pairing the app has stored, at every start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingCheck {
    /// The stored machine is right.
    Keep,
    /// Same server, but it listens somewhere else now (another port): store this address.
    NewAddress(std::net::SocketAddr),
    /// The server will not know this app: drop the stored machine and pair again.
    Repair(&'static str),
}

/// `devices` is the server's list of paired devices, `None` when it could not be read (then the
/// stored pairing is trusted: a failed read is no proof of a revoked device).
pub fn check_pairing(
    stored_node: &str,
    stored_pins: &[std::net::SocketAddr],
    status: &ServerStatus,
    devices: Option<&[Device]>,
    own_id: &str,
) -> PairingCheck {
    if stored_node != status.node_id {
        return PairingCheck::Repair("the server's identity changed");
    }
    if let Some(list) = devices {
        if !own_id.is_empty() && !list.iter().any(|d| is_own_device(d, own_id)) {
            return PairingCheck::Repair("the server no longer lists this app (revoked or reset)");
        }
    }
    let want = loopback_addr(status.port_or_default());
    if stored_pins != [want] {
        return PairingCheck::NewAddress(want);
    }
    PairingCheck::Keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exe_is_next_to_the_app_and_nowhere_else() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("app");
        let other = d.path().join("other");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        // Another folder (a PATH entry, the current folder) does not count.
        std::fs::write(other.join("forgetty.exe"), b"x").unwrap();
        assert_eq!(find_exe(Some(&app), "forgetty.exe"), None);
        std::fs::write(app.join("forgetty.exe"), b"x").unwrap();
        assert_eq!(find_exe(Some(&app), "forgetty.exe"), Some(app.join("forgetty.exe")));
        assert_eq!(find_exe(None, "forgetty.exe"), None);
        // A folder with the name is not the program.
        let d2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d2.path().join("forgetty.exe")).unwrap();
        assert_eq!(find_exe(Some(d2.path()), "forgetty.exe"), None);
    }

    #[test]
    fn the_developer_override_takes_an_absolute_local_file_only() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("forgetty-test.exe");
        std::fs::write(&f, b"x").unwrap();
        let os = |s: &str| std::ffi::OsString::from(s);
        assert_eq!(override_exe(None), Ok(None));
        assert_eq!(override_exe(Some(&os(""))), Ok(None));
        assert_eq!(override_exe(Some(f.as_os_str())), Ok(Some(f.clone())));
        for bad in ["forgetty.exe", "./forgetty.exe", "..\\x\\forgetty.exe", "\\\\srv\\share\\forgetty.exe", "//srv/share/forgetty.exe", "\\\\?\\C:\\forgetty.exe", "\\\\.\\pipe\\x", "\\??\\UNC\\srv\\x"] {
            let e = override_exe(Some(&os(bad))).unwrap_err();
            assert!(e.starts_with("FORGETTY_SERVER_EXE"), "{bad}: {e}");
        }
        // Absolute, local, but not there (or a folder).
        let missing = d.path().join("nothing.exe");
        assert!(override_exe(Some(missing.as_os_str())).unwrap_err().contains("not a file"));
        assert!(override_exe(Some(d.path().as_os_str())).unwrap_err().contains("not a file"));
        // The reason never carries control characters.
        let e = override_exe(Some(&os("rel\u{1b}[31m"))).unwrap_err();
        assert!(!e.contains('\u{1b}'), "{e:?}");
    }

    #[test]
    fn plan_order_is_disabled_then_elevated_then_exe() {
        let exe = Some(PathBuf::from("C:/x/forgetty.exe"));
        assert_eq!(plan(false, true, exe.clone()), Plan::InProcess(Fallback::Disabled));
        assert_eq!(plan(true, true, exe.clone()), Plan::InProcess(Fallback::Elevated));
        assert_eq!(plan(true, false, None), Plan::InProcess(Fallback::NotFound));
        assert_eq!(plan(true, false, exe.clone()), Plan::Server(exe.unwrap()));
    }

    #[test]
    fn notices_are_one_line_and_a_user_choice_is_silent() {
        assert!(Fallback::Disabled.notice().is_none());
        for f in [
            Fallback::Elevated,
            Fallback::NotFound,
            Fallback::Start("bad\u{202e}\nthing".into()),
            Fallback::Pair("x".repeat(500)),
            Fallback::Stopped("gone".into()),
        ] {
            let n = f.notice().unwrap();
            assert!(!n.contains('\n') && !n.contains('\u{202e}'), "{n}");
            assert!(n.contains("built-in terminal"), "{n}");
            assert!(n.chars().count() < 330, "{n}");
        }
    }

    #[test]
    fn cli_reason_never_carries_the_pipe_name() {
        let err = "forgetty: connect \\\\.\\pipe\\forgetty-S-1-5-secret failed\nforgetty: server did not come up within 20 s. See C:\\x\\server.log\n";
        assert_eq!(cli_reason("", err, Some(1)), "server did not come up within 20 s. See C:\\x\\server.log");
        let only_pipe = "forgetty: cannot open \\\\.\\pipe\\forgetty-S-1-5-secret\n";
        assert_eq!(cli_reason("", only_pipe, Some(1)), "exit code 1");
        assert_eq!(cli_reason("", "", None), "ended without an exit code");
        assert!(cli_reason("", "error: the server does not run elevated: x\n", Some(EXIT_CONFIG)).contains("does not run elevated"));
        assert_eq!(cli_reason("", "", Some(EXIT_CONFIG)), "the server refused to start");
    }

    #[test]
    fn status_json() {
        let ok = r#"{"running":true,"pid":812,"node_id":"abc123","version":"0.1"}"#;
        assert_eq!(parse_status(ok), Some(ServerStatus { node_id: "abc123".into(), pid: 812, port: None }));
        assert_eq!(parse_status(r#"{"running":false}"#), None);
        assert_eq!(parse_status(r#"{"running":true,"node_id":null}"#), None);
        assert_eq!(parse_status("not json"), None);
        let with = |extra: &str| parse_status(&format!(r#"{{"running":true,"node_id":"n",{extra}}}"#)).unwrap();
        assert_eq!(with(r#""pid":1"#).port_or_default(), 47297);
        assert_eq!(with(r#""port":50123"#).port, Some(50123));
        assert_eq!(with(r#""port":0"#).port, None);
        assert_eq!(with(r#""port":70000"#).port, None);
        assert_eq!(with(r#""addresses":["10.0.0.2:47300","127.0.0.1:47301"]"#).port, Some(47301));
        assert_eq!(with(r#""direct_addrs":["[::1]:47400"]"#).port, Some(47400));
        assert_eq!(with(r#""addresses":["junk"]"#).port, None);
        assert_eq!(parse_status(r#"{"running":true,"node_id":" "}"#), None);
    }

    #[test]
    fn the_stored_pairing_is_checked_at_every_start() {
        let st = |node: &str, port: Option<u16>| ServerStatus { node_id: node.into(), pid: 1, port };
        let dev = |id: &str| Device { device_id: id.into(), name: String::new(), paired_at: None, last_seen: None };
        let pin = |p: u16| vec![loopback_addr(p)];
        // Same server, same address, listed: keep.
        assert_eq!(check_pairing("n", &pin(47297), &st("n", None), Some(&[dev("me")]), "me"), PairingCheck::Keep);
        // The server moved to another port: the stored address follows.
        assert_eq!(check_pairing("n", &pin(47297), &st("n", Some(50000)), Some(&[dev("me")]), "me"), PairingCheck::NewAddress(loopback_addr(50000)));
        // A pairing stored with another address list (or none) is brought to the one address.
        assert_eq!(check_pairing("n", &[], &st("n", None), None, "me"), PairingCheck::NewAddress(loopback_addr(47297)));
        // A new server identity, or this app revoked: pair again.
        assert!(matches!(check_pairing("old", &pin(47297), &st("n", None), None, "me"), PairingCheck::Repair(_)));
        assert!(matches!(check_pairing("n", &pin(47297), &st("n", None), Some(&[dev("other")]), "me"), PairingCheck::Repair(_)));
        assert!(matches!(check_pairing("n", &pin(47297), &st("n", None), Some(&[]), "me"), PairingCheck::Repair(_)));
        // A list that could not be read proves nothing.
        assert_eq!(check_pairing("n", &pin(47297), &st("n", None), None, "me"), PairingCheck::Keep);
        // Revoked wins over a changed port (the stored machine is dropped anyway).
        assert!(matches!(check_pairing("n", &pin(1), &st("n", Some(2)), Some(&[]), "me"), PairingCheck::Repair(_)));
    }

    #[test]
    fn restarts_are_capped_in_a_window() {
        let m = |s: u64| Duration::from_secs(s * 60);
        assert!(!too_many_restarts(&[]));
        assert!(!too_many_restarts(&[m(1), m(2), m(3), m(4)]));
        assert!(too_many_restarts(&[m(1), m(2), m(3), m(4), m(5)]));
        // Restarts older than ten minutes do not count.
        assert!(!too_many_restarts(&[m(1), m(2), m(3), m(4), m(11)]));
        assert!(too_many_restarts(&[m(1), m(2), m(3), m(4), m(9), m(30)]));
    }

    #[test]
    fn devices_json_skips_bad_entries() {
        let j = r#"{"devices":[{"device_id":"d1","name":"Pixel","paired_at":"2026-10-01T10:00:00Z","last_seen":null},
            {"name":"no id"}, 7, {"device_id":"","name":"empty"}, {"device_id":"d2"}]}"#;
        let d = parse_devices(j).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].shown_name(), "Pixel");
        assert_eq!(d[1].shown_name(), "(unnamed device)");
        assert_eq!(d[0].short_id(), "d1");
        assert!(parse_devices("{}").is_err());
        assert!(parse_devices("").is_err());
        assert!(is_own_device(&d[0], "d1") && !is_own_device(&d[0], "") && !is_own_device(&d[1], "d1"));
    }

    #[test]
    fn device_names_are_cleaned() {
        let d = Device { device_id: "id".into(), name: "a\u{202e}b\u{7}c".into(), paired_at: None, last_seen: None };
        assert_eq!(d.shown_name(), "abc");
    }

    #[test]
    fn pair_lines() {
        assert_eq!(pair_line("payload: {\"v\":2}"), PairLine::Payload);
        assert_eq!(pair_line("expired"), PairLine::Expired);
        assert_eq!(pair_line("paired: Pixel 9 (abcd-1234)"), PairLine::Paired { name: "Pixel 9".into(), id: "abcd-1234".into() });
        assert_eq!(pair_line("paired: C:\\x (a)"), PairLine::Paired { name: "C:\\x".into(), id: "a".into() });
        assert_eq!(pair_line("Scan the QR code"), PairLine::Other);
        assert_eq!(pair_line(""), PairLine::Other);
    }

    /// Render `dark` like the CLI does (`Dense1x2`), in either polarity.
    fn render(dark: &[Vec<bool>], light_is_filled: bool) -> String {
        let w = dark[0].len();
        let mut out = String::new();
        for pair in dark.chunks(2) {
            for x in 0..w {
                let f = |r: Option<&Vec<bool>>| r.map(|r| r[x] != light_is_filled).unwrap_or(light_is_filled);
                let (t, b) = (f(pair.first()), f(pair.get(1)));
                out.push(match (t, b) {
                    (true, true) => '\u{2588}',
                    (true, false) => '\u{2580}',
                    (false, true) => '\u{2584}',
                    (false, false) => ' ',
                });
            }
            out.push('\n');
        }
        out
    }

    /// A fake code: 25x25 modules with a quiet zone of 2.
    fn fake_code() -> Vec<Vec<bool>> {
        let n = 29;
        (0..n)
            .map(|y| {
                (0..n)
                    .map(|x| {
                        let inside = (2..n - 2).contains(&x) && (2..n - 2).contains(&y);
                        inside && ((x * 7 + y * 13 + x * y) % 3 == 0 || (x < 9 && y < 9 && (x + y) % 2 == 0))
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn qr_roundtrip_in_both_polarities() {
        let code = fake_code();
        for light_is_filled in [false, true] {
            let text = format!("{}payload: {{}}\npaired: x (y)\n", render(&code, light_is_filled));
            let g = qr_from_ascii(&text).unwrap_or_else(|| panic!("no grid, light_is_filled={light_is_filled}"));
            assert_eq!(g.w, 29);
            // The text has an odd number of module rows: the grid pads one light row.
            assert!(g.h == 30);
            for (y, row) in code.iter().enumerate() {
                for (x, d) in row.iter().enumerate() {
                    assert_eq!(g.is_dark(x, y), *d, "x={x} y={y} light_is_filled={light_is_filled}");
                }
            }
        }
    }

    #[test]
    fn qr_needs_a_code_before_the_payload_line() {
        assert!(qr_from_ascii("payload: {}\n").is_none());
        assert!(qr_from_ascii("").is_none());
        assert!(qr_from_ascii(&"  \n".repeat(30)).is_none());
        // Text above the code (a banner) is skipped.
        let code = fake_code();
        let text = format!("Forgetty pairing\n{}payload: {{}}", render(&code, false));
        assert!(qr_from_ascii(&text).is_some());
    }

    #[test]
    fn watch_waits_then_restarts_then_gives_up() {
        let s = Duration::from_secs;
        assert_eq!(watch(s(1), 0, None), Watch::Wait);
        assert_eq!(watch(s(3), 0, None), Watch::Restart);
        assert_eq!(watch(s(10), 1, Some(s(2))), Watch::Wait);
        assert_eq!(watch(s(10), 1, Some(s(6))), Watch::Restart);
        assert_eq!(watch(s(30), 2, Some(s(10))), Watch::Wait);
        assert_eq!(watch(s(30), 2, Some(s(16))), Watch::Restart);
        // The third restart gets 25 s to bring the link up.
        assert_eq!(watch(s(60), 3, Some(s(10))), Watch::Wait);
        assert_eq!(watch(s(60), 3, Some(s(26))), Watch::GiveUp);
        assert_eq!(watch(s(60), 3, None), Watch::GiveUp);
    }
}
