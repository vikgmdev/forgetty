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
    /// The bind address may come from `--bind` where the server runs, so a
    /// config with `relay = "none"` and no `bind_addrs` is not an error here
    /// (the settings editor sets this). A bind list that is present is still
    /// checked.
    pub bind_may_come_later: bool,
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
    /// Settings of the file that a command-line flag overrode. The caller logs
    /// them at WARN.
    pub warnings: Vec<String>,
}

/// Is `value` the relay setting that switches relays off? Trimmed, any case.
pub fn is_relay_none(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("none")
}

/// Is `value` the relay setting that asks for iroh's default relays? Trimmed, any case. Same as
/// leaving `relay` out on Linux. On Windows, where the default is no relay, this turns them on.
pub fn is_relay_default(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("default")
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
        Some(v) if is_relay_default(v) => Ok(false),
        Some(v) => Err(ConfigError(format!(
            "invalid relay setting {v:?}: the values are \"none\" and \"default\" (use relay_urls \
             for own relays)"
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
///
/// A command-line flag wins over the file: with `--no-relay` the file's `relay`
/// and `relay_urls` are ignored (not even parsed), with `--relay-url` the file's
/// `relay`, and with `--bind` the file's `bind_addrs`. Each ignored setting is
/// reported in [`RemoteSettings::warnings`]. A conflict inside one source is an
/// error: `--no-relay` with `--relay-url`, or `relay = "none"` with
/// `relay_urls` in the file.
pub fn validate_remote_settings(
    config: &crate::Config,
    overrides: &RemoteOverrides,
) -> Result<RemoteSettings, ConfigError> {
    validate_remote_settings_with(config, overrides, cfg!(windows))
}

/// The address a server with `local_defaults` binds when nothing says otherwise.
pub const LOCAL_DEFAULT_BIND: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

/// [`validate_remote_settings`], with the platform defaults chosen by the caller.
///
/// `local_defaults` (Windows): a server nobody configured is reachable from this machine only.
/// With no `--no-relay`, `relay`, `--relay-url` or `relay_urls` it uses no relay and publishes
/// nothing to n0, and with no `--bind` or `bind_addrs` it binds `127.0.0.1` only. A user who
/// wants other devices to reach it sets `bind_addrs` (an explicit opt-in), and relays come back
/// with `relay_urls` or `relay = "default"`. Without `local_defaults` (Linux) the defaults are
/// iroh's relays and every address.
pub fn validate_remote_settings_with(
    config: &crate::Config,
    overrides: &RemoteOverrides,
    local_defaults: bool,
) -> Result<RemoteSettings, ConfigError> {
    let mut warnings = Vec::new();
    let file_relay = config.relay.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let (no_relay, relay_urls) = if overrides.no_relay {
        if !overrides.relay_urls.is_empty() {
            return Err(ConfigError(
                "relays are switched off (--no-relay) but relay URLs are set (--relay-url)".into(),
            ));
        }
        if !config.relay_urls.is_empty() || file_relay.is_some_and(|v| !is_relay_none(v)) {
            warnings.push("config relay settings ignored: --no-relay given".to_string());
        }
        (true, Vec::new())
    } else if !overrides.relay_urls.is_empty() {
        if file_relay.is_some() {
            warnings.push("config relay settings ignored: --relay-url given".to_string());
        }
        (false, parse_relay_urls(&overrides.relay_urls)?)
    } else {
        let relay_urls = parse_relay_urls(&config.relay_urls)?;
        let mut no_relay = relays_disabled(false, config.relay.as_deref())?;
        // Nothing asked for relays: a local-only server uses none.
        if local_defaults && file_relay.is_none() && relay_urls.is_empty() {
            no_relay = true;
        }
        if no_relay && !relay_urls.is_empty() {
            return Err(ConfigError(
                "relays are switched off (relay = \"none\") but relay URLs are set \
                 (relay_urls)"
                    .into(),
            ));
        }
        (no_relay, relay_urls)
    };
    let bind_addrs = if overrides.bind.is_empty() {
        let mut addrs = parse_bind_addrs(&config.bind_addrs)?;
        if addrs.is_empty() && local_defaults {
            addrs.push(LOCAL_DEFAULT_BIND);
        }
        addrs
    } else {
        if !config.bind_addrs.is_empty() {
            warnings.push("config bind_addrs ignored: --bind given".to_string());
        }
        parse_bind_addrs(&overrides.bind)?
    };
    if no_relay {
        // Without relays the server is reached on its direct addresses only,
        // and a server that binds every address would put them all in the QR.
        if bind_addrs.is_empty() && overrides.bind_may_come_later {
            return Ok(RemoteSettings { no_relay, relay_urls, bind_addrs, warnings });
        }
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
    Ok(RemoteSettings { no_relay, relay_urls, bind_addrs, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    /// The Linux defaults, on every platform: these tests describe the rules without the
    /// local-only defaults.
    fn linux(config: &Config, o: &RemoteOverrides) -> Result<RemoteSettings, ConfigError> {
        validate_remote_settings_with(config, o, false)
    }

    fn local(config: &Config, o: &RemoteOverrides) -> Result<RemoteSettings, ConfigError> {
        validate_remote_settings_with(config, o, true)
    }

    #[test]
    fn local_defaults_bind_loopback_only_with_no_relay() {
        let s = local(&Config::default(), &RemoteOverrides::default()).unwrap();
        assert!(s.no_relay);
        assert!(s.relay_urls.is_empty());
        assert_eq!(s.bind_addrs, vec![LOCAL_DEFAULT_BIND]);
        // Linux keeps iroh's relays and every address.
        let s =
            validate_remote_settings_with(&Config::default(), &RemoteOverrides::default(), false)
                .unwrap();
        assert!(!s.no_relay && s.bind_addrs.is_empty());
    }

    #[test]
    fn local_defaults_give_way_to_explicit_settings() {
        // Other addresses are an opt-in; relays stay off unless asked for.
        let c = Config { bind_addrs: vec!["10.230.1.14".into()], ..Config::default() };
        let s = local(&c, &RemoteOverrides::default()).unwrap();
        assert_eq!(s.bind_addrs, vec!["10.230.1.14".parse::<IpAddr>().unwrap()]);
        assert!(s.no_relay);
        // The command line wins over the file.
        let o = RemoteOverrides { bind: vec!["10.9.9.9".into()], ..Default::default() };
        assert_eq!(local(&c, &o).unwrap().bind_addrs, vec!["10.9.9.9".parse::<IpAddr>().unwrap()]);
        // Own relays, or the default relays on request.
        let c =
            Config { relay_urls: vec!["https://relay.example.org".into()], ..Config::default() };
        let s = local(&c, &RemoteOverrides::default()).unwrap();
        assert!(!s.no_relay);
        assert_eq!(s.bind_addrs, vec![LOCAL_DEFAULT_BIND]);
        let c = Config { relay: Some("default".into()), ..Config::default() };
        assert!(!local(&c, &RemoteOverrides::default()).unwrap().no_relay);
        let o = RemoteOverrides {
            relay_urls: vec!["https://relay.example.org".into()],
            ..Default::default()
        };
        assert!(!local(&Config::default(), &o).unwrap().no_relay);
    }

    #[test]
    fn relay_default_is_accepted_and_a_typo_still_is_not() {
        assert!(!relays_disabled(false, Some("default")).unwrap());
        assert!(!relays_disabled(false, Some(" Default ")).unwrap());
        assert!(relays_disabled(false, Some("nope")).is_err());
    }

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
        let s = linux(&c, &RemoteOverrides::default()).unwrap();
        assert_eq!(s.bind_addrs, vec![ip("10.0.0.1")]);
        assert_eq!(s.relay_urls.len(), 1);
        let over = RemoteOverrides {
            bind: vec!["10.0.0.2".into()],
            relay_urls: vec!["https://b.example.org".into()],
            ..Default::default()
        };
        let s = linux(&c, &over).unwrap();
        assert_eq!(s.bind_addrs, vec![ip("10.0.0.2")]);
        assert_eq!(s.relay_urls[0].as_str(), "https://b.example.org/");
        assert!(!s.no_relay);
    }

    #[test]
    fn a_command_line_flag_wins_over_the_file() {
        let flag = |no_relay: bool, urls: &[&str], bind: &[&str]| RemoteOverrides {
            no_relay,
            relay_urls: urls.iter().map(|s| s.to_string()).collect(),
            bind: bind.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        // --no-relay: the file's relay settings are ignored, even bad ones.
        for file in [
            "relay_urls = [\"https://r.example.org\"]\n",
            "relay_urls = [\"ftp://bad\"]\n",
            "relay = \"nne\"\n",
            "relay = \"none\"\nrelay_urls = [\"https://r.example.org\"]\n",
        ] {
            let s = linux(&config(file), &flag(true, &[], &["10.0.0.1"]))
                .unwrap_or_else(|e| panic!("{file:?}: {e}"));
            assert!(s.no_relay && s.relay_urls.is_empty(), "{file:?}");
            assert_eq!(
                s.warnings,
                vec!["config relay settings ignored: --no-relay given".to_string()],
                "{file:?}"
            );
        }
        // relay = "none" alone in the file is the same as the flag: no warning.
        let s = linux(&config("relay = \"none\"\n"), &flag(true, &[], &["10.0.0.1"])).unwrap();
        assert!(s.warnings.is_empty());
        // --relay-url: the file's relay = "none" is ignored.
        let s = linux(&config("relay = \"none\"\n"), &flag(false, &["https://r.example.org"], &[]))
            .unwrap();
        assert!(!s.no_relay && s.relay_urls.len() == 1);
        assert_eq!(
            s.warnings,
            vec!["config relay settings ignored: --relay-url given".to_string()]
        );
        // --bind: the file's bind_addrs is ignored, even a bad one.
        let s = linux(&config("bind_addrs = [\"not an ip\"]\n"), &flag(false, &[], &["10.0.0.2"]))
            .unwrap();
        assert_eq!(s.bind_addrs, vec![ip("10.0.0.2")]);
        assert_eq!(s.warnings, vec!["config bind_addrs ignored: --bind given".to_string()]);
        // A conflict inside one source is still an error.
        let e =
            linux(&config(""), &flag(true, &["https://r.example.org"], &["10.0.0.1"])).unwrap_err();
        assert!(e.to_string().contains("relay URLs are set"), "{e}");
        let e = linux(
            &config("relay = \"none\"\nrelay_urls = [\"https://r.example.org\"]\n"),
            &flag(false, &[], &["10.0.0.1"]),
        )
        .unwrap_err();
        assert!(e.to_string().contains("relay URLs are set"), "{e}");
    }

    #[test]
    fn no_relay_needs_a_bind_address() {
        let c = config("relay = \"none\"\n");
        let e = linux(&c, &RemoteOverrides::default()).unwrap_err();
        assert!(e.to_string().contains("no bind address is set"), "{e}");
        let over = RemoteOverrides { bind: vec!["10.0.0.2".into()], ..Default::default() };
        assert!(linux(&c, &over).unwrap().no_relay);
    }

    #[test]
    fn the_editor_allows_no_relay_without_a_bind_but_warns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let saved =
            crate::parse_and_save_config_checked_at("relay = \"none\"\n", Some(&path)).unwrap();
        assert_eq!(saved.warnings.len(), 1, "{:?}", saved.warnings);
        assert!(
            saved.warnings[0].contains("relay = none needs --bind or bind_addrs"),
            "{:?}",
            saved.warnings
        );
        assert!(path.exists());
        // A bind list that is there is still checked, and a good one needs no warning.
        let bad = "relay = \"none\"\nbind_addrs = [\"0.0.0.0\"]\n";
        assert!(crate::parse_and_save_config_checked_at(bad, Some(&path)).is_err());
        let good = "relay = \"none\"\nbind_addrs = [\"10.0.0.1\"]\n";
        let saved = crate::parse_and_save_config_checked_at(good, Some(&path)).unwrap();
        assert!(saved.warnings.is_empty());
        // The server itself still refuses it with no --bind.
        let c = config("relay = \"none\"\n");
        assert!(linux(&c, &RemoteOverrides::default()).is_err());
    }

    /// The cases of `every_configuration_error_exits_78` in forgetty-client
    /// (`tests/wireguard_only.rs`), as config text. The settings editor must
    /// refuse each one the server refuses (it skips only the missing bind).
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
