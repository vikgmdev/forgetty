//! "Check for updates" policy: which GitHub release counts as an update. No
//! network code here (the app does the one HTTPS request, and only when the
//! user asks) so this is tested on every platform.

use std::cmp::Ordering;

/// Releases of the Windows client are tagged `win-v<major>.<minor>.<patch>`.
pub const TAG_PREFIX: &str = "win-v";
/// Pages the app opens must start with this.
pub const RELEASES_URL: &str = "https://github.com/vikgmdev/forgetty/releases/";
pub const API_HOST: &str = "api.github.com";
pub const API_PATH: &str = "/repos/vikgmdev/forgetty/releases?per_page=30";
/// Larger answers are cut off.
pub const MAX_BODY: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateState {
    /// Nothing asked yet. The app never asks on its own.
    Idle,
    Checking,
    UpToDate,
    Available { version: String, url: String },
    Failed(String),
}

impl UpdateState {
    pub fn line(&self, current: &str) -> String {
        match self {
            UpdateState::Idle => String::new(),
            UpdateState::Checking => "Checking...".into(),
            UpdateState::UpToDate => format!("Forgetty {current} is the latest version."),
            UpdateState::Available { version, .. } => format!("Version {version} is available. Click to open the download page."),
            UpdateState::Failed(e) => format!("Could not check: {e}"),
        }
    }
}

/// `1.2.3` to `(1, 2, 3)`. Pre-release suffixes are not accepted.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.trim().split('.');
    let v = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if it.next().is_some() {
        return None;
    }
    Some(v)
}

/// The newest `win-v` release newer than `current`, from the GitHub API answer
/// (an array of releases). Drafts and pre-releases are skipped. A page outside
/// this repository's releases is never returned.
pub fn newest_release(json: &str, current: &str) -> Result<Option<(String, String)>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("bad answer: {e}"))?;
    let list = v.as_array().ok_or_else(|| "bad answer: not a list".to_string())?;
    let cur = parse_version(current).ok_or_else(|| format!("bad current version {current}"))?;
    let mut best: Option<((u32, u32, u32), String, String)> = None;
    for r in list {
        let flag = |k: &str| r.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
        if flag("draft") || flag("prerelease") {
            continue;
        }
        let (Some(tag), Some(url)) = (r.get("tag_name").and_then(|x| x.as_str()), r.get("html_url").and_then(|x| x.as_str())) else {
            continue;
        };
        let Some(ver) = tag.strip_prefix(TAG_PREFIX).and_then(parse_version) else { continue };
        if !url.starts_with(RELEASES_URL) || url.chars().any(|c| c.is_control() || c == ' ') {
            continue;
        }
        if best.as_ref().map_or(true, |(b, _, _)| ver.cmp(b) == Ordering::Greater) {
            best = Some((ver, tag.trim_start_matches(TAG_PREFIX).to_string(), url.to_string()));
        }
    }
    Ok(best.filter(|(v, _, _)| *v > cur).map(|(_, version, url)| (version, url)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(tag: &str, url: &str, draft: bool, pre: bool) -> serde_json::Value {
        serde_json::json!({"tag_name": tag, "html_url": url, "draft": draft, "prerelease": pre})
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.3-beta"), None);
        assert!((0, 10, 0) > (0, 9, 9));
    }

    #[test]
    fn picks_the_newest_windows_release() {
        let base = "https://github.com/vikgmdev/forgetty/releases/tag/";
        let list = serde_json::json!([
            rel("server-v9.0.0", &format!("{base}server-v9.0.0"), false, false),
            rel("win-v0.2.0", &format!("{base}win-v0.2.0"), false, false),
            rel("win-v0.10.0", &format!("{base}win-v0.10.0"), false, false),
            rel("win-v1.0.0", &format!("{base}win-v1.0.0"), true, false),
            rel("win-v2.0.0", &format!("{base}win-v2.0.0"), false, true),
        ])
        .to_string();
        let (v, url) = newest_release(&list, "0.1.0").unwrap().unwrap();
        assert_eq!((v.as_str(), url.as_str()), ("0.10.0", format!("{base}win-v0.10.0").as_str()));
        assert_eq!(newest_release(&list, "0.10.0").unwrap(), None);
        assert_eq!(newest_release(&list, "3.0.0").unwrap(), None);
    }

    #[test]
    fn refuses_pages_outside_the_repository() {
        let list = serde_json::json!([rel("win-v5.0.0", "https://evil.example.com/releases/", false, false)]).to_string();
        assert_eq!(newest_release(&list, "0.1.0").unwrap(), None);
        let list = serde_json::json!([rel("win-v5.0.0", "https://github.com/vikgmdev/forgetty/releases/tag/win-v5.0.0 --flag", false, false)]).to_string();
        assert_eq!(newest_release(&list, "0.1.0").unwrap(), None);
        assert!(newest_release("{}", "0.1.0").is_err());
        assert!(newest_release("nope", "0.1.0").is_err());
    }

    #[test]
    fn nothing_runs_until_asked() {
        assert_eq!(UpdateState::Idle.line("0.1.0"), "");
    }
}
