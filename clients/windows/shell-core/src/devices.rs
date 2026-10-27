//! The Devices page of the Settings window (M1): the devices paired with this PC's local server,
//! Revoke, and "Pair new device" (a QR code for another device to scan). Pure state and key
//! handling: the app runs `forgetty devices --json`, `forgetty revoke` and `forgetty pair` and
//! feeds the results in. Linux tests cover this file.

use crate::localserver::{is_own_device, qr_from_ascii, Device, QrGrid};
use crate::notify::sanitize;
use crate::textarea::KeyIn;

/// What the app does after a key or a click.
#[derive(Clone, Debug, PartialEq)]
pub enum DevEffect {
    None,
    Close,
    Refresh,
    Revoke(String),
    StartPair,
    CancelPair,
    CopyText(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Avail {
    /// The local server is up: the list can be read.
    Yes,
    /// The in-process terminal is in use, and why.
    No(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct DevRow {
    pub index: usize,
    pub title: String,
    pub short_id: String,
    pub paired: String,
    pub last_seen: String,
    pub own: bool,
    pub selected: bool,
    /// Revoke was asked once: the next press confirms.
    pub armed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QrView {
    pub w: usize,
    pub h: usize,
    /// Runs of dark modules `(x, y, length)`: a few hundred quads and not thousands.
    pub runs: Vec<(usize, usize, usize)>,
    pub can_copy: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DevicesView {
    pub avail: Avail,
    pub loading: bool,
    pub rows: Vec<DevRow>,
    pub qr: Option<QrView>,
    pub message: Option<(bool, String)>,
}

struct Qr {
    grid: QrGrid,
    payload: Option<String>,
}

pub struct DevicesState {
    avail: Avail,
    list: Vec<Device>,
    own_id: String,
    sel: usize,
    armed: Option<String>,
    loading: bool,
    qr: Option<Qr>,
    /// A pairing window is open (its command runs), shown or not yet.
    pairing: bool,
    msg: Option<(bool, String)>,
}

impl Default for DevicesState {
    fn default() -> Self {
        DevicesState {
            avail: Avail::No("The local server is not running.".into()),
            list: Vec::new(),
            own_id: String::new(),
            sel: 0,
            armed: None,
            loading: false,
            qr: None,
            pairing: false,
            msg: None,
        }
    }
}

/// `2026-10-27T09:30:12.123Z` as `2026-10-27 09:30`. Anything else is shown cleaned and cut.
fn when(s: &Option<String>) -> String {
    let Some(s) = s.as_deref().filter(|s| !s.is_empty()) else { return "never".into() };
    let b = s.as_bytes();
    if b.len() >= 16 && b[4] == b'-' && b[7] == b'-' && (b[10] == b'T' || b[10] == b' ') && b[13] == b':' && s.is_char_boundary(16) {
        return format!("{} {}", &s[..10], &s[11..16]);
    }
    sanitize(s, 24)
}

/// Runs of dark modules, row by row.
pub fn qr_runs(g: &QrGrid) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for y in 0..g.h {
        let mut x = 0;
        while x < g.w {
            if g.is_dark(x, y) {
                let start = x;
                while x < g.w && g.is_dark(x, y) {
                    x += 1;
                }
                out.push((start, y, x - start));
            } else {
                x += 1;
            }
        }
    }
    out
}

impl DevicesState {
    /// The app tells whether the local server is up. Going away drops what was shown.
    pub fn set_avail(&mut self, a: Avail) {
        if a != self.avail {
            self.avail = a;
            self.list.clear();
            self.armed = None;
            self.qr = None;
            self.pairing = false;
        }
    }

    pub fn is_pairing(&self) -> bool {
        self.pairing
    }

    /// The page opened or the list is read again.
    pub fn begin_refresh(&mut self) {
        self.loading = matches!(self.avail, Avail::Yes);
    }

    pub fn loaded(&mut self, r: Result<Vec<Device>, String>, own_id: String) {
        self.loading = false;
        match r {
            Ok(mut l) => {
                // This app first, then by name.
                l.sort_by(|a, b| {
                    is_own_device(b, &own_id).cmp(&is_own_device(a, &own_id)).then_with(|| a.shown_name().to_lowercase().cmp(&b.shown_name().to_lowercase()))
                });
                self.list = l;
                self.own_id = own_id;
                self.sel = self.sel.min(self.list.len().saturating_sub(1));
                if self.armed.as_ref().is_some_and(|a| !self.list.iter().any(|d| &d.device_id == a)) {
                    self.armed = None;
                }
            }
            Err(e) => self.msg = Some((false, sanitize(&e, 160))),
        }
    }

    pub fn revoked(&mut self, r: Result<String, String>) {
        self.armed = None;
        self.msg = Some(match r {
            Ok(name) => (true, format!("Revoked {}", sanitize(&name, 48))),
            Err(e) => (false, format!("Could not revoke: {}", sanitize(&e, 120))),
        });
    }

    /// The QR code of a pairing window, as `forgetty pair` printed it.
    pub fn pair_output(&mut self, text: &str) {
        if !self.pairing {
            return;
        }
        let payload = text.lines().find(|l| l.trim_start().starts_with("payload:")).map(|l| l.trim_start()["payload:".len()..].trim().to_string());
        match qr_from_ascii(text) {
            Some(grid) => self.qr = Some(Qr { grid, payload }),
            None => {
                self.pairing = false;
                self.msg = Some((false, "The pairing code could not be shown.".into()));
            }
        }
    }

    /// The pairing window ended: a device paired, it expired, or it failed.
    pub fn pair_finished(&mut self, r: Result<String, String>) {
        self.pairing = false;
        self.qr = None;
        self.msg = Some(match r {
            Ok(name) => (true, format!("Paired {}", sanitize(&name, 48))),
            Err(e) => (false, sanitize(&e, 120)),
        });
    }

    fn selected(&self) -> Option<&Device> {
        self.list.get(self.sel)
    }

    pub fn key(&mut self, k: &KeyIn) -> DevEffect {
        let yes = matches!(self.avail, Avail::Yes);
        if self.pairing {
            return match k.key {
                "escape" => {
                    self.qr = None;
                    DevEffect::CancelPair
                }
                "c" if !k.ctrl => match self.qr.as_ref().and_then(|q| q.payload.clone()) {
                    Some(p) => DevEffect::CopyText(p),
                    None => DevEffect::None,
                },
                _ => DevEffect::None,
            };
        }
        match k.key {
            "escape" if self.armed.is_some() => {
                self.armed = None;
                DevEffect::None
            }
            "escape" => DevEffect::Close,
            "up" if self.sel > 0 => {
                self.sel -= 1;
                self.armed = None;
                DevEffect::None
            }
            "down" if self.sel + 1 < self.list.len() => {
                self.sel += 1;
                self.armed = None;
                DevEffect::None
            }
            "f5" | "l" if yes => {
                self.msg = None;
                self.loading = true;
                DevEffect::Refresh
            }
            "p" if yes => {
                self.msg = None;
                self.pairing = true;
                DevEffect::StartPair
            }
            "delete" | "r" | "enter" if yes => self.revoke_key(k.key == "enter"),
            _ => DevEffect::None,
        }
    }

    fn revoke_key(&mut self, confirm_only: bool) -> DevEffect {
        let Some(d) = self.selected().cloned() else { return DevEffect::None };
        if is_own_device(&d, &self.own_id) {
            self.armed = None;
            self.msg = Some((false, "This is the app itself. It pairs with the server again by itself.".into()));
            return DevEffect::None;
        }
        if self.armed.as_deref() == Some(d.device_id.as_str()) {
            self.armed = None;
            self.msg = None;
            return DevEffect::Revoke(d.device_id);
        }
        // Enter only confirms; Delete arms.
        if confirm_only {
            return DevEffect::None;
        }
        self.armed = Some(d.device_id);
        self.msg = None;
        DevEffect::None
    }

    pub fn press(&mut self, name: &str) -> DevEffect {
        let k = |key: &'static str| KeyIn { key, ..Default::default() };
        match name {
            "dev-refresh" => self.key(&k("f5")),
            "dev-pair" => self.key(&k("p")),
            "dev-revoke" => self.key(&k("delete")),
            "dev-confirm" => self.key(&k("enter")),
            "dev-cancel" => self.key(&k("escape")),
            "dev-copy" => self.key(&k("c")),
            "close" => DevEffect::Close,
            _ => DevEffect::None,
        }
    }

    pub fn click_row(&mut self, i: usize) {
        if i < self.list.len() && i != self.sel {
            self.sel = i;
            self.armed = None;
        }
    }

    pub fn hint(&self) -> String {
        if self.pairing {
            return "Scan the code with the other device. Esc: cancel. C: copy the payload text.".into();
        }
        match self.avail {
            Avail::No(_) => "Esc: close. Ctrl+Tab: next page.".into(),
            Avail::Yes => "Up/Down: device. Delete: revoke (press twice). P: pair a new device. F5: read again. Esc: close.".into(),
        }
    }

    pub fn view(&self) -> DevicesView {
        let rows = self
            .list
            .iter()
            .enumerate()
            .map(|(i, d)| DevRow {
                index: i,
                title: if is_own_device(d, &self.own_id) { format!("This PC (local app), {}", d.shown_name()) } else { d.shown_name() },
                short_id: d.short_id(),
                paired: when(&d.paired_at),
                last_seen: when(&d.last_seen),
                own: is_own_device(d, &self.own_id),
                selected: i == self.sel,
                armed: self.armed.as_deref() == Some(d.device_id.as_str()),
            })
            .collect();
        DevicesView {
            avail: self.avail.clone(),
            loading: self.loading,
            rows,
            qr: self.qr.as_ref().map(|q| QrView { w: q.grid.w, h: q.grid.h, runs: qr_runs(&q.grid), can_copy: q.payload.is_some() }),
            message: self.msg.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str) -> Device {
        Device { device_id: id.into(), name: name.into(), paired_at: Some("2026-10-27T09:30:12.5Z".into()), last_seen: None }
    }

    fn key(k: &'static str) -> KeyIn<'static> {
        KeyIn { key: k, ..Default::default() }
    }

    fn up() -> DevicesState {
        let mut s = DevicesState::default();
        s.set_avail(Avail::Yes);
        s.loaded(Ok(vec![dev("zz", "Pixel"), dev("own", "WIN"), dev("aa", "iPad")]), "own".into());
        s
    }

    #[test]
    fn own_device_first_then_by_name() {
        let v = up().view();
        let names: Vec<&str> = v.rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(names, vec!["This PC (local app), WIN", "iPad", "Pixel"]);
        assert!(v.rows[0].own && !v.rows[1].own);
        assert_eq!(v.rows[1].paired, "2026-10-27 09:30");
        assert_eq!(v.rows[1].last_seen, "never");
    }

    #[test]
    fn revoke_needs_two_presses_and_not_for_the_app_itself() {
        let mut s = up();
        // The app itself is first and selected.
        assert_eq!(s.key(&key("delete")), DevEffect::None);
        assert!(s.view().message.unwrap().1.contains("the app itself"));
        s.key(&key("down"));
        assert_eq!(s.key(&key("enter")), DevEffect::None, "Enter alone does not arm");
        assert!(!s.view().rows[1].armed);
        assert_eq!(s.key(&key("delete")), DevEffect::None);
        assert!(s.view().rows[1].armed);
        // Moving away disarms.
        s.key(&key("down"));
        assert!(!s.view().rows[1].armed);
        s.key(&key("up"));
        s.key(&key("r"));
        assert_eq!(s.key(&key("enter")), DevEffect::Revoke("aa".into()));
        assert!(!s.view().rows[1].armed);
        s.revoked(Ok("iPad".into()));
        assert_eq!(s.view().message, Some((true, "Revoked iPad".into())));
        s.revoked(Err("nope\u{7}".into()));
        assert!(!s.view().message.unwrap().0);
    }

    #[test]
    fn escape_disarms_first_then_closes() {
        let mut s = up();
        s.key(&key("down"));
        s.key(&key("delete"));
        assert_eq!(s.key(&key("escape")), DevEffect::None);
        assert_eq!(s.key(&key("escape")), DevEffect::Close);
    }

    #[test]
    fn without_a_server_nothing_but_close() {
        let mut s = DevicesState::default();
        assert!(matches!(s.view().avail, Avail::No(_)));
        for k in ["p", "f5", "delete", "enter", "r"] {
            assert_eq!(s.key(&key(k)), DevEffect::None, "{k}");
        }
        assert_eq!(s.key(&key("escape")), DevEffect::Close);
        // Going away drops the list.
        s.set_avail(Avail::Yes);
        s.loaded(Ok(vec![dev("a", "x")]), String::new());
        assert_eq!(s.view().rows.len(), 1);
        s.set_avail(Avail::No("gone".into()));
        assert!(s.view().rows.is_empty());
    }

    fn fake_qr_text() -> String {
        let n = 29usize;
        let dark: Vec<Vec<bool>> = (0..n)
            .map(|y| (0..n).map(|x| (2..n - 2).contains(&x) && (2..n - 2).contains(&y) && (x * 7 + y * 13 + x * y) % 3 == 0).collect())
            .collect();
        let mut out = String::new();
        for pair in dark.chunks(2) {
            for x in 0..n {
                let t = pair[0][x];
                let b = pair.get(1).map(|r| r[x]).unwrap_or(false);
                out.push(match (t, b) {
                    (true, true) => '\u{2588}',
                    (true, false) => '\u{2580}',
                    (false, true) => '\u{2584}',
                    _ => ' ',
                });
            }
            out.push('\n');
        }
        format!("{out}payload: {{\"v\":2}}\nScan the QR code\n")
    }

    #[test]
    fn pairing_shows_a_qr_cancels_and_finishes() {
        let mut s = up();
        assert_eq!(s.key(&key("p")), DevEffect::StartPair);
        assert!(s.is_pairing());
        // Output before the pairing window is ignored when we are not pairing.
        s.pair_output(&fake_qr_text());
        let v = s.view();
        let q = v.qr.expect("qr");
        assert_eq!((q.w, q.h), (29, 30));
        assert!(q.can_copy && !q.runs.is_empty());
        // Runs never leave the grid and never overlap.
        let mut seen = std::collections::HashSet::new();
        for (x, y, l) in &q.runs {
            assert!(*l >= 1 && x + l <= q.w && *y < q.h);
            for i in 0..*l {
                assert!(seen.insert((x + i, *y)));
            }
        }
        // Other keys do nothing while the code shows.
        assert_eq!(s.key(&key("delete")), DevEffect::None);
        assert_eq!(s.key(&key("c")), DevEffect::CopyText("{\"v\":2}".into()));
        assert_eq!(s.key(&key("escape")), DevEffect::CancelPair);
        assert!(s.view().qr.is_none());
        s.pair_finished(Err("The pairing window closed.".into()));
        assert!(!s.is_pairing());
        s.key(&key("p"));
        s.pair_finished(Ok("Pixel 9".into()));
        assert_eq!(s.view().message, Some((true, "Paired Pixel 9".into())));
    }

    #[test]
    fn a_bad_code_ends_the_pairing_with_a_message() {
        let mut s = up();
        s.key(&key("p"));
        s.pair_output("nothing to see\n");
        assert!(!s.is_pairing());
        assert!(s.view().message.is_some());
    }

    #[test]
    fn dates() {
        assert_eq!(when(&Some("2026-10-27T09:30:12Z".into())), "2026-10-27 09:30");
        assert_eq!(when(&Some("yesterday\u{7}".into())), "yesterday");
        assert_eq!(when(&None), "never");
        assert_eq!(when(&Some(String::new())), "never");
        // Not a date, multibyte at the cut: no panic.
        assert_eq!(when(&Some("\u{e9}".repeat(20))).chars().count(), 20);
    }

    #[test]
    fn presses_map_to_keys() {
        let mut s = up();
        assert_eq!(s.press("dev-refresh"), DevEffect::Refresh);
        assert_eq!(s.press("dev-pair"), DevEffect::StartPair);
        assert_eq!(s.press("dev-cancel"), DevEffect::CancelPair);
        assert_eq!(s.press("close"), DevEffect::Close);
        assert_eq!(s.press("nope"), DevEffect::None);
    }
}
