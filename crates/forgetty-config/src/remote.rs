//! The settings that decide how a server is reached from outside: `relay`,
//! `relay_urls` and `bind_addrs`, with their command-line overrides.
//!
//! One set of rules, used by the server at startup, by the settings editor
//! before it writes a file, and by the near-key check of the loader. A text the
//! server would refuse (exit 78) is a text the editor refuses.

use std::net::{IpAddr, SocketAddr};

use url::Url;

/// A setting that makes the server refuse to start (exit 78), or the editor
/// refuse to save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// What the command line adds to the file: `--no-relay`, `--relay-url` (replaces
/// `relay_urls`) and `--bind` (replaces `bind_addrs`). The editor has none.
#[derive(Debug, Clone, Default)]
pub struct RemoteOverrides {
    pub no_relay: bool,
    pub relay_urls: Vec<String>,
    pub bind: Vec<String>,
}

/// The checked remote settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSettings {
    /// No relay and nothing published to n0.
    pub no_relay: bool,
    /// Relay URLs, http or https, without duplicates. Empty: iroh's defaults.
    pub relay_urls: Vec<Url>,
    /// Addresses to bind, without duplicates. Empty: every address.
    pub bind_addrs: Vec<IpAddr>,
}

/// Is `value` the relay setting that switches relays off? Trimmed, any case.
pub fn is_relay_none(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("none")
}

/// One relay URL (trimmed): http or https, with a host.
pub fn parse_relay_url(value: &str) -> Result<Url, ConfigError> {
    let parsed = Url::parse(value.trim())
        .map_err(|e| ConfigError(format!("invalid relay URL {value:?}: {e}")))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ConfigError(format!(
            "invalid relay URL {value:?}: scheme must be http or https"
        )));
    }
    if parsed.host().is_none() {
        return Err(ConfigError(format!("invalid relay URL {value:?}: no host")));
    }
    Ok(parsed)
}

/// One bind address (trimmed, brackets allowed): an IP, no port.
pub fn parse_bind_addr(value: &str) -> Result<IpAddr, ConfigError> {
    value
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .parse()
        .map_err(|e| ConfigError(format!("invalid bind address {value:?}: {e} (an IP, no port)")))
}

/// Parse relay URLs, in order, without duplicates.
pub fn parse_relay_urls(urls: &[String]) -> Result<Vec<Url>, ConfigError> {
    let mut out: Vec<Url> = Vec::new();
    for u in urls {
        let parsed = parse_relay_url(u)?;
        if !out.contains(&parsed) {
            out.push(parsed);
        }
    }
    Ok(out)
}

/// Parse bind addresses, in order, without duplicates.
pub fn parse_bind_addrs(addrs: &[String]) -> Result<Vec<IpAddr>, ConfigError> {
    let mut out: Vec<IpAddr> = Vec::new();
    for a in addrs {
        let ip = parse_bind_addr(a)?;
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    Ok(out)
}

/// Is the relay switched off, by `--no-relay` or `relay = "none"`? Any other
/// value of `relay` is an error: a typo must not turn the relays on silently.
pub fn relays_disabled(
    no_relay_flag: bool,
    relay_setting: Option<&str>,
) -> Result<bool, ConfigError> {
    match relay_setting.map(str::trim) {
        None | Some("") => Ok(no_relay_flag),
        Some(v) if is_relay_none(v) => Ok(true),
        Some(v) => Err(ConfigError(format!(
            "invalid relay setting {v:?}: the only value is \"none\" (use relay_urls for relays)"
        ))),
    }
}

/// With no relay a device reaches the server on its bind addresses and nothing
/// else, so at least one must be an address a device can pair with: not
/// `0.0.0.0` or `::`, not link-local, not multicast or broadcast (the same rule
/// as `forgetty_proto::pinned_addr_problem`, which the client applies to the
/// QR).
pub fn check_pairable_bind(addrs: &[IpAddr]) -> Result<(), ConfigError> {
    // Any non-zero port: the rule is about the IP here.
    let pairable =
        |ip: &IpAddr| forgetty_proto::pinned_addr_problem(&SocketAddr::new(*ip, 1)).is_none();
    if addrs.iter().any(pairable) {
        return Ok(());
    }
    let list = addrs.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
    Err(ConfigError(format!(
        "no address a device can pair with: --no-relay needs a bind address that is not \
         0.0.0.0, ::, link-local, multicast or broadcast (bound: {list})"
    )))
}

/// Check the remote settings of `config` with the command-line `overrides`.
/// The server calls this at startup and exits 78 on an error; the settings
/// editor calls it with no overrides before it writes a file.
pub fn validate_remote_settings(
    config: &crate::Config,
    overrides: &RemoteOverrides,
) -> Result<RemoteSettings, ConfigError> {
    // --relay-url replaces the config file list. None: iroh defaults.
    let relay_source =
        if overrides.relay_urls.is_empty() { &config.relay_urls } else { &overrides.relay_urls };
    let relay_urls = parse_relay_urls(relay_source)?;
    let no_relay = relays_disabled(overrides.no_relay, config.relay.as_deref())?;
    if no_relay && !relay_urls.is_empty() {
        return Err(ConfigError(
            "relays are switched off (--no-relay / relay = \"none\") but relay URLs are set".into(),
        ));
    }
    let bind_source = if overrides.bind.is_empty() { &config.bind_addrs } else { &overrides.bind };
    let bind_addrs = parse_bind_addrs(bind_source)?;
    if no_relay {
        // Without relays the server is reached on its direct addresses only,
        // and a server that binds every address would put them all in the QR.
        if bind_addrs.is_empty() {
            return Err(ConfigError(
                "relays are switched off (--no-relay / relay = \"none\") but no bind address \
                 is set: add --bind IP (or bind_addrs in the config file), for example the \
                 WireGuard address"
                    .into(),
            ));
        }
        check_pairable_bind(&bind_addrs)?;
    }
    Ok(RemoteSettings { no_relay, relay_urls, bind_addrs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn relay_urls_parse_and_dedupe() {
        let urls = vec![
            "https://relay.example.org".to_string(),
            "https://relay.example.org/".to_string(),
            " http://127.0.0.1:3340 ".to_string(),
            "HTTPS://RELAY.example.org".to_string(),
        ];
        let parsed = parse_relay_urls(&urls).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].to_string(), "https://relay.example.org/");
        assert_eq!(parsed[1].to_string(), "http://127.0.0.1:3340/");
        assert!(parse_relay_urls(&[]).unwrap().is_empty());
    }

    #[test]
    fn bad_relay_urls_are_refused() {
        assert!(parse_relay_urls(&["not a url".to_string()]).is_err());
        let e = parse_relay_urls(&["ftp://relay.example.org".to_string()]).unwrap_err();
        assert!(e.to_string().contains("http or https"), "{e}");
    }

    #[test]
    fn bind_addresses_parse_and_dedupe() {
        let got = parse_bind_addrs(&["10.230.1.14".into(), " 10.230.1.14 ".into(), "[::1]".into()])
            .unwrap();
        assert_eq!(got.len(), 2);
        assert!(parse_bind_addrs(&[]).unwrap().is_empty());
        assert!(parse_bind_addrs(&["10.0.0.1:47297".into()]).is_err());
        assert!(parse_bind_addrs(&["not an ip".into()]).is_err());
    }

    #[test]
    fn relay_none_and_the_flag_switch_relays_off_and_a_typo_is_an_error() {
        assert!(!relays_disabled(false, None).unwrap());
        assert!(relays_disabled(true, None).unwrap());
        for v in ["none", "NONE", "None", " none ", "\tNone\n"] {
            assert!(is_relay_none(v), "{v:?}");
            assert!(relays_disabled(false, Some(v)).unwrap(), "{v:?}");
        }
        assert!(relays_disabled(false, Some("nope")).is_err());
    }

    #[test]
    fn no_relay_needs_a_bind_address_a_device_can_pair_with() {
        assert!(check_pairable_bind(&[ip("10.230.1.14")]).is_ok());
        assert!(check_pairable_bind(&[ip("0.0.0.0"), ip("127.0.0.1")]).is_ok());
        for bad in ["0.0.0.0", "::", "169.254.1.1", "fe80::1", "224.0.0.1", "ff02::1"] {
            let err = check_pairable_bind(&[ip(bad)]).unwrap_err().to_string();
            assert!(err.contains("no address a device can pair with"), "{bad}: {err}");
        }
        // One good address among bad ones is enough.
        assert!(check_pairable_bind(&[ip("0.0.0.0"), ip("10.0.0.2")]).is_ok());
    }

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_command_line_replaces_the_file_lists() {
        let c = config("bind_addrs = [\"10.0.0.1\"]\nrelay_urls = [\"https://a.example.org\"]\n");
        let s = validate_remote_settings(&c, &RemoteOverrides::default()).unwrap();
        assert_eq!(s.bind_addrs, vec![ip("10.0.0.1")]);
        assert_eq!(s.relay_urls.len(), 1);
        let over = RemoteOverrides {
            bind: vec!["10.0.0.2".into()],
            relay_urls: vec!["https://b.example.org".into()],
            ..Default::default()
        };
        let s = validate_remote_settings(&c, &over).unwrap();
        assert_eq!(s.bind_addrs, vec![ip("10.0.0.2")]);
        assert_eq!(s.relay_urls[0].as_str(), "https://b.example.org/");
        assert!(!s.no_relay);
    }

    #[test]
    fn no_relay_needs_a_bind_address() {
        let c = config("relay = \"none\"\n");
        let e = validate_remote_settings(&c, &RemoteOverrides::default()).unwrap_err();
        assert!(e.to_string().contains("no bind address is set"), "{e}");
        let over = RemoteOverrides { bind: vec!["10.0.0.2".into()], ..Default::default() };
        assert!(validate_remote_settings(&c, &over).unwrap().no_relay);
    }

    /// The cases of `every_configuration_error_exits_78` in forgetty-client
    /// (`tests/wireguard_only.rs`), as config text. The settings editor must
    /// refuse each one the server refuses.
    #[test]
    fn every_server_configuration_error_is_refused_by_the_editor_too() {
        let cases = [
            ("rellay = \"none\"\nbind_addrs = [\"127.0.0.1\"]\n", "did you mean `relay`"),
            ("bind_addr = [\"127.0.0.1\"]\n", "bind_addrs"),
            ("relay = \"nne\"\nbind_addrs = [\"127.0.0.1\"]\n", "invalid relay setting"),
            (
                "relay = \"none\"\nbind_addrs = [\"0.0.0.0\"]\n",
                "no address a device can pair with",
            ),
            (
                "relay = \"none\"\nbind_addrs = [\"169.254.1.1\"]\n",
                "no address a device can pair with",
            ),
            ("relay = \"none\"\nbind_addrs = [\"127.0.0.1:47297\"]\n", "invalid bind address"),
            (
                "bind_addrs = [\"127.0.0.1\"]\nrelay_urls = [\"ftp://x.example.org\"]\n",
                "relay URL",
            ),
            (
                "relay = \"none\"\nbind_addrs = [\"127.0.0.1\"]\nrelay_urls = [\"https://r.example.org\"]\n",
                "relay URLs are set",
            ),
            ("relay = \"none\"\n", "no bind address is set"),
            // Case and spaces do not slip past the near-key guard.
            ("Relay = \"None\"\n", "did you mean `relay`"),
            ("rellay = \" NONE\"\n", "did you mean `relay`"),
            ("rellay_urls = [\"HTTPS://r.example.org\"]\n", "did you mean `relay_urls`"),
            ("bind_addr = \" [::1] \"\n", "bind_addrs"),
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (text, needle) in cases {
            let err = crate::parse_and_save_config_at(text, Some(&path)).unwrap_err().to_string();
            assert!(err.contains(needle), "{text:?}: wanted {needle:?} in: {err}");
            assert!(!path.exists(), "{text:?} wrote a file");
        }
    }
}
