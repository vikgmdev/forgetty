//! `forgetty server run` — the durable per-user terminal server (AD-017).
//!
//! Starts `SessionManager`, the iroh endpoint and the JSON-RPC socket server
//! without any GTK or display-server dependency. Intended to run under
//! `systemd --user` with linger. A client disconnect never ends a pane or the
//! server. The layout and byte logs are saved under the state dir and restored
//! on the next start.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use forgetty_config::{load_config, validate_remote_settings, Config, RemoteOverrides};
use forgetty_core::PaneId;
use forgetty_pty::PtySize;
use forgetty_session::{SessionEvent, SessionManager};
use forgetty_socket::SocketServer;
use forgetty_sync::{identity::load_or_generate, SyncEndpoint};

use forgetty_server::iroh_terminal::{handle_terminal_stream, FORGETTY_STREAM_ALPN};
use forgetty_server::rpc_v2::RpcV2;
use forgetty_server::sockpath;

/// Options for `forgetty server run`.
#[derive(Args, Debug, Default)]
pub struct RunArgs {
    /// Path to the config file (default `~/.config/forgetty/config.toml`).
    #[arg(long)]
    pub config_file: Option<PathBuf>,

    /// Override the Unix socket path (default `$XDG_RUNTIME_DIR/forgetty.sock`).
    #[arg(long, hide = true)]
    pub socket_path: Option<PathBuf>,

    /// Override the server id (default: the id stored in the state dir).
    /// Used by the paused GTK client, which runs one server per window.
    #[arg(long, hide = true)]
    pub session_id: Option<uuid::Uuid>,

    /// UDP port of the iroh endpoint (default 47297). The port stays the same
    /// across restarts, so clients can redial the last direct address. `0`
    /// picks a free port. Also read from `FORGETTY_PORT` (the gates set it to
    /// `0` so they do not fight over the fixed port).
    #[arg(long)]
    pub port: Option<u16>,

    /// Relay server of the iroh endpoint, for example
    /// `https://relay.example.org`. Repeat for several. Replaces `relay_urls`
    /// in the config file. Without either, iroh's default relays stay in use.
    /// The pairing QR carries these URLs.
    #[arg(long = "relay-url", value_name = "URL")]
    pub relay_url: Vec<String>,

    /// Keep the n0 DNS discovery service on although relays are configured.
    /// With `--relay-url` / `relay_urls` it is off by default: the server then
    /// publishes nothing to n0, and the apps use the relays in the pairing
    /// payload plus the cached direct address.
    #[arg(long)]
    pub n0_discovery: bool,

    /// Bind the iroh endpoint to this IP address only, on the fixed port.
    /// Repeat for several. Replaces `bind_addrs` in the config file. Without
    /// either, every address is bound (the default). The pairing QR carries
    /// these addresses.
    #[arg(long = "bind", value_name = "IP")]
    pub bind: Vec<String>,

    /// Use no relay and publish nothing to n0: the server never contacts a
    /// relay or n0 DNS. For a server that clients reach over WireGuard only.
    /// Same as `relay = "none"` in the config file.
    #[arg(long)]
    pub no_relay: bool,
}

/// Default UDP port of the iroh endpoint (PROTOCOL_V2 section 1).
pub const DEFAULT_PORT: u16 = 47297;

/// QUIC settings of the server: keep-alive 5 s, idle timeout 30 s (section 1).
fn quic_config() -> iroh::endpoint::QuicTransportConfig {
    use forgetty_proto::{IDLE_TIMEOUT_SECS, KEEP_ALIVE_SECS};
    iroh::endpoint::QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_secs(KEEP_ALIVE_SECS))
        .max_idle_timeout(Some(
            iroh::endpoint::IdleTimeout::try_from(Duration::from_secs(IDLE_TIMEOUT_SECS))
                .expect("30 s fits"),
        ))
        .build()
}

/// `server run` exit code when another server already holds the instance lock.
/// `server start` uses it to tell "someone else is starting" from a real failure.
pub const EXIT_ALREADY_RUNNING: i32 = 75; // keep in step with cli.rs

/// `server run` exit code for a configuration error (sysexits `EX_CONFIG`): a
/// bad or missing setting that a restart cannot fix. `server start` prints the
/// error at once; the systemd units do not restart on it.
pub const EXIT_CONFIG: i32 = 78; // keep in step with cli.rs

/// A configuration the server refuses: a config file that exists but cannot be
/// read or parsed, or a bad setting (`relay`, `--bind`, a relay URL, a
/// conflict). `server run` exits with [`EXIT_CONFIG`] (no restart loop): falling back to defaults would hide the
/// mistake, and a typo in `relay = "none"` would turn the relays back on.
#[derive(Debug)]
pub struct ConfigInvalid(pub String);

impl std::fmt::Display for ConfigInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid configuration, not starting: {}", self.0)
    }
}

impl std::error::Error for ConfigInvalid {}

/// Do all the addresses stay on this machine? Then no remote device can pair.
/// Allowed (the tests bind `127.0.0.1`), but worth a warning at startup.
pub fn loopback_only(addrs: &[std::net::IpAddr]) -> bool {
    !addrs.is_empty() && addrs.iter().all(std::net::IpAddr::is_loopback)
}

/// Load the config. A missing file (the default location, or a `--config-file`
/// that does not exist) means the defaults. A file that exists and fails to
/// read or parse is an error that names the file and the line.
pub fn load_server_config(path: Option<&std::path::Path>) -> anyhow::Result<Config> {
    if let Some(p) = path {
        if !p.exists() {
            warn!("config file {} not found, using defaults", p.display());
            return Ok(Config { source: Some(p.to_path_buf()), ..Config::default() });
        }
    }
    load_config(path).map_err(|e| ConfigInvalid(e.to_string()).into())
}

/// Backoff of the background iroh bind (address not there yet): from 1 s up to
/// 10 s, a WARN at most once a minute.
const BIND_RETRY_MIN: Duration = Duration::from_secs(1);
const BIND_RETRY_MAX: Duration = Duration::from_secs(10);
const BIND_WARN_EVERY: Duration = Duration::from_secs(60);

/// How often the server looks for a `--bind` address that was not there at
/// start.
const BIND_WATCH_EVERY: Duration = Duration::from_secs(5);

/// Is `ip` an address of this machine? A UDP socket can be bound to it. (With
/// the `qa-tools` feature, an address listed in `<state dir>/qa-hidden-addrs`
/// counts as missing: tests make an address "appear" by emptying the file.)
fn address_present(ip: std::net::IpAddr) -> bool {
    #[cfg(feature = "qa-tools")]
    if let Ok(list) = std::fs::read_to_string(forgetty_core::platform::data_dir().join("qa-hidden-addrs")) {
        if list.lines().any(|l| l.trim().parse::<std::net::IpAddr>().is_ok_and(|h| h == ip)) {
            return false;
        }
    }
    std::net::UdpSocket::bind((ip, 0)).is_ok()
}

/// Failed rebinds per address. A failed rebind drops the connections, so an
/// address that fails [`RetryBook::MAX_FAILED`] times is given up until the
/// server restarts, and each retry waits longer (10 s, then 20 s).
#[derive(Default)]
struct RetryBook {
    failed: std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>,
}

impl RetryBook {
    const MAX_FAILED: u32 = 3;

    fn given_up(&self, ip: &std::net::IpAddr) -> bool {
        self.failed.get(ip).is_some_and(|(n, _)| *n >= Self::MAX_FAILED)
    }

    /// May the address be tried now?
    fn due(&self, ip: &std::net::IpAddr, now: std::time::Instant) -> bool {
        self.failed.get(ip).is_none_or(|(_, next)| *next <= now)
    }

    /// A rebind with this address failed after it dropped the connections:
    /// the failure count, and how long to wait.
    fn failure(&mut self, ip: std::net::IpAddr, now: std::time::Instant) -> (u32, Duration) {
        let n = self.failed.get(&ip).map_or(0, |(n, _)| *n) + 1;
        let wait = Duration::from_secs(10 * u64::from(n));
        self.failed.insert(ip, (n, now + wait));
        (n, wait)
    }

    fn success(&mut self, ip: &std::net::IpAddr) {
        self.failed.remove(ip);
    }
}

/// The addresses of `appeared` that a bind error names (`ip:port`, or
/// `[ip]:port` for IPv6). Empty when the error names none of them: then the
/// failure cannot be attributed.
fn named_in_error(error: &str, appeared: &[std::net::IpAddr]) -> Vec<std::net::IpAddr> {
    appeared
        .iter()
        .copied()
        .filter(|ip| {
            let needle = if ip.is_ipv4() { format!("{ip}:") } else { format!("[{ip}]:") };
            error.match_indices(&needle).any(|(at, _)| {
                // Not the tail of a longer address (110.0.0.2 for 10.0.0.2).
                !error[..at].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '.' || c == ':')
            })
        })
        .collect()
}

/// Watch the `--bind` addresses that were missing at start. When one appears
/// (several at once count as one), bind the endpoint again with the full set:
/// iroh cannot add a socket to a running endpoint, so the connections drop once
/// and the clients reconnect. The key, node id and port stay.
fn watch_missing_addresses(ep: Arc<SyncEndpoint>, all: Vec<std::net::IpAddr>) {
    let list = |v: &[std::net::IpAddr]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
    let missing_now = {
        let (ep, all) = (Arc::clone(&ep), all.clone());
        move || -> Vec<std::net::IpAddr> {
            let have = ep.bind_addrs();
            all.iter().copied().filter(|ip| !have.contains(ip)).collect()
        }
    };
    let missing = missing_now();
    if missing.is_empty() {
        return;
    }
    for ip in &missing {
        warn!(
            "--bind {ip} is not on this machine yet; serving on {}; it is added when it appears",
            list(&ep.bind_addrs())
        );
    }
    tokio::spawn(async move {
        let mut book = RetryBook::default();
        let mut warned = false;
        loop {
            tokio::time::sleep(BIND_WATCH_EVERY).await;
            // The addresses not bound now: those that never came, and those a
            // failed rebind gave up on (not the ones given up for good).
            let missing: Vec<std::net::IpAddr> = missing_now()
                .into_iter()
                .filter(|ip| !book.given_up(ip))
                .collect();
            if missing.is_empty() {
                return;
            }
            let now = std::time::Instant::now();
            let appeared: Vec<std::net::IpAddr> = missing
                .iter()
                .copied()
                .filter(|ip| book.due(ip, now))
                .filter(|ip| address_present(*ip))
                .collect();
            if appeared.is_empty() {
                continue;
            }
            let current = ep.bind_addrs();
            let set: Vec<std::net::IpAddr> = all
                .iter()
                .copied()
                .filter(|ip| current.contains(ip) || appeared.contains(ip))
                .collect();
            let names = list(&appeared);
            match ep.rebind(set).await {
                Ok(()) => {
                    info!(
                        "--bind {names} is there: iroh endpoint bound again on {} (connections reconnect once)",
                        list(&ep.bind_addrs())
                    );
                    for ip in &appeared {
                        book.success(ip);
                    }
                    warned = false;
                }
                Err(f) if !f.dropped => {
                    // Nothing changed: try again at the next look.
                    if !warned {
                        warn!("--bind {names} is there but cannot be added yet ({}); trying again", f.error);
                        warned = true;
                    }
                }
                Err(f) => {
                    let bound = ep.bound_sockets();
                    let bound_text = if bound.is_empty() {
                        "no endpoint is bound now, the server keeps trying to bind in the background".to_string()
                    } else {
                        format!(
                            "bound now: {}",
                            bound.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
                        )
                    };
                    // A strike for the address the error names; when it names
                    // none of them, for all that were added.
                    let text = f.error.to_string();
                    let named = named_in_error(&text, &appeared);
                    let blamed = if named.is_empty() { appeared.clone() } else { named };
                    for ip in &blamed {
                        let (n, wait) = book.failure(*ip, std::time::Instant::now());
                        if n >= RetryBook::MAX_FAILED {
                            warn!(
                                "--bind {ip} could not be added {n} times ({}); the server stops trying for it until it restarts; {bound_text}",
                                f.error
                            );
                        } else {
                            error!(
                                "--bind {ip} could not be added ({}); {bound_text}; trying again in {} s",
                                f.error,
                                wait.as_secs()
                            );
                        }
                    }
                }
            }
        }
    });
}

/// Does a failed iroh bind wait for the address, or end the process? The
/// operator chose the addresses (`--bind`): wait, the tunnel may come up. With
/// the default bind only "address not available" (`io::ErrorKind::
/// AddrNotAvailable`, found in the error chain) waits; any other error (a busy
/// port) ends the process with a non-zero status, and the service manager
/// restarts it.
pub fn bind_is_retryable(chose_addrs: bool, error: &forgetty_sync::SyncError) -> bool {
    use forgetty_sync::SyncError;
    match error {
        SyncError::BindAddrNotAvailable(_) => true,
        SyncError::Bind(_) => chose_addrs,
        _ => false,
    }
}

/// Another server holds the instance lock.
#[derive(Debug)]
pub struct AlreadyRunning(pub PathBuf);

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "another forgetty server is already running (lock {})", self.0.display())
    }
}

impl std::error::Error for AlreadyRunning {}

/// Exclusive lock on `<state_dir>/server.lock`. Held while the server reads or
/// creates its first-run state (`server_id`, `identity.key`), so two servers
/// that start at once cannot each invent a different identity.
///
/// Released on drop, or by the kernel when the process dies.
pub struct StateLock(#[allow(dead_code)] std::fs::File);

/// Take the state-dir lock. Waits up to 30 s for another starter to finish.
pub fn lock_state_dir() -> anyhow::Result<StateLock> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let dir = forgetty_core::platform::data_dir();
    forgetty_core::fsutil::ensure_private_dir(&dir)?;
    let path = dir.join("server.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(forgetty_core::fsutil::FILE_MODE)
        .open(&path)?;
    forgetty_core::fsutil::fix_file_mode(&path)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        // SAFETY: the fd is valid for the lifetime of `file`; flock takes no pointers.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(StateLock(file));
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::WouldBlock {
            anyhow::bail!("lock {}: {err}", path.display());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("lock {} is held by another starter for over 30 s", path.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Read the stable server id from `<data_dir>/server_id`, creating it on first run.
///
/// Needs the state lock, so the check-then-create cannot race. A file that is
/// there but unreadable or not a UUID is an error: the server must not start
/// with a new identity and an empty layout while saved sessions sit under the
/// old id.
pub fn load_or_create_server_id(_lock: &StateLock) -> anyhow::Result<uuid::Uuid> {
    let path = forgetty_core::platform::data_dir().join("server_id");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            return uuid::Uuid::parse_str(text.trim()).map_err(|_| {
                anyhow::anyhow!(
                    "server_id file {} is corrupt (not a UUID). Refusing to start with a new \
                     identity: saved sessions belong to the old id. Restore the file, or delete \
                     it to start over with a new identity",
                    path.display()
                )
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!("cannot read {}: {e}", path.display()),
    }
    let id = uuid::Uuid::new_v4();
    if forgetty_core::fsutil::create_private_if_absent(&path, format!("{id}\n").as_bytes())? {
        return Ok(id);
    }
    // Created by someone else between the read and the create (not possible
    // under the lock, but cheap to handle): use theirs.
    let text = std::fs::read_to_string(&path)?;
    uuid::Uuid::parse_str(text.trim())
        .map_err(|_| anyhow::anyhow!("server_id file {} is corrupt", path.display()))
}

/// Take the single-instance lock `<data_dir>/server-<id>.lock`.
///
/// The returned file must stay open for the life of the process. The lock is
/// released by the kernel when the process exits, however it exits.
fn acquire_instance_lock(server_id: uuid::Uuid) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let dir = forgetty_core::platform::data_dir();
    forgetty_core::fsutil::ensure_private_dir(&dir)?;
    let path = dir.join(format!("server-{server_id}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(forgetty_core::fsutil::FILE_MODE)
        .open(&path)?;
    // SAFETY: the fd is valid for the lifetime of `file`; flock takes no pointers.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(AlreadyRunning(path).into());
    }
    Ok(file)
}

fn init_tracing() {
    let builder = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .with_target(false);
    // Interactive terminal: compact and coloured. Otherwise (systemd journal,
    // log file): plain, and the journal adds its own timestamps.
    if std::io::stderr().is_terminal() {
        builder.compact().init();
    } else {
        builder.with_ansi(false).without_time().init();
    }
}

/// Run the server in the foreground until SIGTERM or SIGINT.
pub async fn run(args: RunArgs) -> anyhow::Result<()> {
    init_tracing();
    forgetty_server::crash::install();

    // First-run state is created under the state-dir lock: the server id, the
    // instance lock and the identity key. Two servers that start at once
    // serialize here, the second finds the instance lock taken and exits.
    let (session_id, secret_key, _instance_lock) = {
        let state_lock = lock_state_dir()?;
        // Stable server id: survives restarts, so the session file is found again.
        let session_id = match args.session_id {
            Some(id) => id,
            None => load_or_create_server_id(&state_lock)?,
        };
        let instance_lock = acquire_instance_lock(session_id)?;
        let secret_key = load_or_generate()?;
        // Credential files written by an older build keep their old mode until here.
        forgetty_core::fsutil::fix_file_mode(&forgetty_sync::registry::registry_path())?;
        (session_id, secret_key, instance_lock)
    };

    // The socket gives a shell. Keep it in a private directory, and refuse to
    // start if that directory is unsafe (squatted, symlink, wrong owner/mode).
    // A socket path given with --socket-path is the caller's responsibility.
    if args.socket_path.is_none() {
        let dir = sockpath::socket_dir();
        sockpath::prepare_socket_dir(&dir)
            .map_err(|e| anyhow::anyhow!("socket directory {} is unsafe: {e}", dir.display()))?;
    }

    // Load config (canonicalize path first so relative paths survive CWD changes).
    let config_path = args.config_file.map(|p| {
        std::fs::canonicalize(&p).unwrap_or_else(|_| {
            warn!("Could not canonicalize config path {:?}, using as-is", p);
            p
        })
    });

    let config: Config = load_server_config(config_path.as_deref())?;

    // One check of the remote settings, shared with the settings editor: a
    // setting the server refuses (exit 78) is a setting the editor refuses.
    let remote = validate_remote_settings(
        &config,
        &RemoteOverrides {
            no_relay: args.no_relay,
            relay_urls: args.relay_url.clone(),
            bind: args.bind.clone(),
            bind_may_come_later: false,
        },
    )
    .map_err(|e| ConfigInvalid(e.to_string()))?;
    for w in &remote.warnings {
        warn!("{w}");
    }
    let no_relay = remote.no_relay;
    let relay_urls: Vec<iroh::RelayUrl> =
        remote.relay_urls.into_iter().map(iroh::RelayUrl::from).collect();
    let bind_addrs = remote.bind_addrs;
    // Own relays: no n0 discovery unless asked for (--n0-discovery).
    let n0_discovery = !no_relay && (relay_urls.is_empty() || args.n0_discovery);
    if bind_addrs.is_empty() {
        info!("bind: every address");
    } else {
        info!("bind: {}", bind_addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", "));
    }
    if no_relay && loopback_only(&bind_addrs) {
        warn!("bound to loopback only: no remote device can pair");
    }

    info!("session_id: {session_id}");

    // Create the platform-agnostic session manager.
    let session_manager = Arc::new(SessionManager::new());

    // V2-007: thread byte-log config values from TOML into SessionManager.
    session_manager.set_byte_log_config(config.byte_log_ring_kb, config.byte_log_max_mb);

    // P7.1: shell integration for the shells new panes start. The scripts are
    // part of this binary; they are written to the data dir so the shells can
    // read them. A failure is a warning: panes then start without them.
    if config.shell_integration {
        let dir = forgetty_core::platform::data_dir().join("shell-integration");
        match forgetty_session::ShellIntegration::install(&dir) {
            Ok(si) => {
                info!("shell integration: on ({})", si.dir().display());
                session_manager.set_shell_integration(Some(si));
            }
            Err(e) => warn!("shell integration: cannot write {}: {e}", dir.display()),
        }
    } else {
        info!("shell integration: off (shell_integration = false)");
    }

    // Cold-start restore: reload the UUID-named session file into the live SessionLayout.
    //
    // This runs synchronously before the socket server starts accepting connections
    // so that by the time GTK connects and calls `get_layout`, the daemon's layout
    // already reflects the last-saved session. Failures are non-fatal — a fresh
    // start (empty layout) is acceptable if the file is absent or corrupt.
    //
    // Split structure is fully reconstructed: each tab is created with its root pane
    // via `create_tab`, then `split_pane_with_ratio` is called recursively to
    // rebuild the split tree with the original ratios preserved.
    {
        let default_size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        match forgetty_workspace::load_session_for(session_id) {
            Ok(Some(state)) if !state.workspaces.is_empty() => {
                let total: usize = state.workspaces.iter().map(|ws| ws.tabs.len()).sum();
                info!(
                    "cold-start restore: found {} workspace(s), {} tab(s) total",
                    state.workspaces.len(),
                    total,
                );

                // Ensure the daemon has enough workspace slots for all saved workspaces.
                // SessionLayout::new_default() creates workspace[0]; create the rest.
                for (ws_idx, saved_ws) in state.workspaces.iter().enumerate() {
                    if ws_idx == 0 {
                        // FIX-001: workspace[0] was seeded by `SessionLayout::new_default()`
                        // with the literal name "Default". The saved name takes precedence —
                        // this is what lets a user's rename of the Default workspace
                        // survive a daemon restart. Ignore errors: a malformed saved
                        // name is non-fatal (restore loop is already tolerant of
                        // per-workspace failures).
                        if let Err(e) = session_manager.rename_workspace(0, &saved_ws.name) {
                            warn!(
                                "cold-start restore: failed to rename workspace 0 to '{}': {e}",
                                saved_ws.name
                            );
                        }
                    } else {
                        let (_, created_idx) = session_manager.create_workspace(&saved_ws.name);
                        debug_assert_eq!(
                            created_idx, ws_idx,
                            "workspace index mismatch during cold-start restore"
                        );
                    }
                    // FIX-010: restore the persisted per-workspace accent colour.
                    // Missing / null colour field in pre-FIX-010 JSON → no call
                    // (deserialises to `None`). Non-fatal if the daemon rejects
                    // the hex — colour is cosmetic.
                    if saved_ws.color.is_some() {
                        if let Err(e) =
                            session_manager.set_workspace_color(ws_idx, saved_ws.color.as_deref())
                        {
                            warn!(
                                "cold-start restore: failed to set colour on workspace {ws_idx}: {e}"
                            );
                        }
                    }
                }

                // Now restore tabs for ALL workspaces, preserving split structure.
                for (ws_idx, workspace) in state.workspaces.iter().enumerate() {
                    for tab in &workspace.tabs {
                        // Create the root pane using the leftmost leaf's CWD.
                        let root_cwd = first_leaf_cwd(&tab.pane_tree);
                        let effective_root_cwd =
                            if root_cwd.is_dir() { Some(root_cwd.to_path_buf()) } else { None };
                        // FIX-013: reuse the saved leftmost-leaf pane_id when present so
                        // `logs/{pane_uuid}.log` is reopened by `ByteLog::new` and the
                        // saved scrollback replays. Falls back to a fresh UUID for
                        // pre-FIX-013 sessions where `pane_id` is `None`.
                        let saved_root_pane_id = first_leaf_pane_id(&tab.pane_tree);
                        let create_result = match saved_root_pane_id {
                            Some(pid) => session_manager.create_tab_with_pane_id(
                                ws_idx,
                                effective_root_cwd,
                                default_size,
                                None,
                                forgetty_core::PaneId(pid),
                            ),
                            None => session_manager.create_tab(
                                ws_idx,
                                effective_root_cwd,
                                default_size,
                                None,
                            ),
                        };
                        match create_result {
                            Ok((root_pane_id, _tab_id)) => {
                                debug!("cold-start restore: created root pane {root_pane_id} for workspace {ws_idx}");
                                // Restore the full split tree rooted at this pane.
                                restore_subtree(
                                    &session_manager,
                                    root_pane_id,
                                    &tab.pane_tree,
                                    default_size,
                                );
                            }
                            Err(e) => {
                                warn!("cold-start restore: create_tab failed for workspace {ws_idx}: {e}");
                            }
                        }
                    }

                    // FIX-009: heal legacy empty workspaces from pre-FIX-009 sessions.
                    // If a saved workspace has `tabs: []` — either because the user hit
                    // the carcass path before the daemon-side auto-spawn shipped, or
                    // because every restored tab failed to spawn here — give it a
                    // default shell so the user never sees a "0 tabs" sidebar row on
                    // first launch after upgrading.
                    //
                    // Unlike the live-mutation path in `close_tab`, this heal runs for
                    // every empty workspace including workspace 0 — cold restart is a
                    // one-time repair so the user always lands in a usable state.
                    if workspace.tabs.is_empty() {
                        match session_manager.create_tab(ws_idx, None, default_size, None) {
                            Ok(_) => {
                                info!(
                                    "cold-start restore: auto-seeded empty workspace {ws_idx} ({}) with default tab (FIX-009)",
                                    workspace.name
                                );
                            }
                            Err(e) => {
                                warn!(
                                    "cold-start restore: auto-seed for empty workspace {ws_idx} failed: {e}"
                                );
                            }
                        }
                    }

                    // FIX-005A: propagate the saved active_tab into the live SessionLayout.
                    // `create_tab` above intentionally does not advance active_tab (per AD-008 —
                    // that is UI state owned by GTK). We restore it explicitly here as a
                    // control-plane operation, so that `get_layout` returns the correct
                    // active_tab to the GTK client, and `build_widgets_from_layout` honors it.
                    //
                    // `set_active_tab` bounds-checks against the live tab count, which may be
                    // smaller than `workspace.active_tab` if some tabs failed to restore.
                    // Treat any error as a warning and continue — cosmetic restore failure
                    // is strictly better than aborting cold restart.
                    if let Err(e) = session_manager.set_active_tab(ws_idx, workspace.active_tab) {
                        warn!(
                            "cold-start restore: failed to set active_tab={} on workspace {ws_idx}: {e}",
                            workspace.active_tab
                        );
                    }
                }

                // FIX-005A: propagate the saved active_workspace into the live SessionLayout.
                // Matches the active_tab restore above. Bounds-checked; a stale saved index
                // (e.g., active_workspace was the 3rd one but only 2 restored) degrades to
                // a warning.
                if let Err(e) = session_manager.set_active_workspace(state.active_workspace) {
                    warn!(
                        "cold-start restore: failed to set active_workspace={}: {e}",
                        state.active_workspace
                    );
                }

                // FIX-005B: propagate each saved tab's `active_pane_id` into the
                // live SessionLayout. Unlike `active_tab` (a stable index),
                // `active_pane_id` is a UUID and the daemon mints fresh PaneIds
                // for every restored leaf. We walk the saved + live pane_trees
                // in lock-step (`live_pane_id_for_saved`) to translate the
                // saved UUID into the live PaneId at the same tree position.
                //
                // Pre-collect every `(ws_idx, live_tab_id, live_pane_id)` tuple
                // before calling `set_active_pane` so the layout snapshot is
                // not held across the manager's mutex re-acquire. Orphan saved
                // UUIDs (saved leaf no longer in the restored tree, e.g. partial
                // split-restore failure) degrade to `warn!` per AC-7.
                //
                // Ordering note: this restore happens BEFORE the layout-event
                // watcher's `tokio::spawn` below — `ActivePaneChanged` events
                // fire into the broadcast channel's zero subscribers and are
                // silently dropped (`Err(NoReceivers)`), preserving the FIX-005A
                // "quiet startup" invariant.
                let mut restore_targets: Vec<(usize, uuid::Uuid, PaneId)> = Vec::new();
                {
                    let live_layout = session_manager.layout();
                    for (ws_idx, saved_workspace) in state.workspaces.iter().enumerate() {
                        let Some(live_ws) = live_layout.workspaces.get(ws_idx) else {
                            continue;
                        };
                        for (i, saved_tab) in saved_workspace.tabs.iter().enumerate() {
                            let Some(live_tab) = live_ws.tabs.get(i) else { continue };
                            let Some(saved_pane_uuid) = saved_tab.active_pane_id else {
                                continue;
                            };
                            match live_pane_id_for_saved(
                                &saved_tab.pane_tree,
                                &live_tab.pane_tree,
                                saved_pane_uuid,
                            ) {
                                Some(live_pane_id) => {
                                    restore_targets.push((ws_idx, live_tab.id, live_pane_id));
                                }
                                None => {
                                    warn!(
                                        "cold-start restore: saved active_pane_id={saved_pane_uuid} \
                                         not found in workspace {ws_idx} tab {} pane_tree \
                                         (orphan; falling back to first leaf)",
                                        live_tab.id
                                    );
                                }
                            }
                        }
                    }
                }
                // Labels (`set_label`) survive a restart. Tabs match by position,
                // panes by their place in the tree, the same way as the active pane.
                {
                    let live_layout = session_manager.layout();
                    for (ws_idx, saved_workspace) in state.workspaces.iter().enumerate() {
                        let Some(live_ws) = live_layout.workspaces.get(ws_idx) else { continue };
                        for (i, saved_tab) in saved_workspace.tabs.iter().enumerate() {
                            let Some(live_tab) = live_ws.tabs.get(i) else { continue };
                            if !saved_tab.label.is_empty() {
                                if let Err(e) = session_manager.set_label(
                                    forgetty_session::LabelTarget::Tab,
                                    live_tab.id,
                                    &saved_tab.label,
                                ) {
                                    warn!("cold-start restore: tab label: {e}");
                                }
                            }
                            for (saved_pane, label) in &saved_tab.pane_labels {
                                let Some(live_pane) = live_pane_id_for_saved(
                                    &saved_tab.pane_tree,
                                    &live_tab.pane_tree,
                                    *saved_pane,
                                ) else {
                                    continue;
                                };
                                if let Err(e) = session_manager.set_label(
                                    forgetty_session::LabelTarget::Pane,
                                    live_pane.0,
                                    label,
                                ) {
                                    warn!("cold-start restore: pane label: {e}");
                                }
                            }
                        }
                    }
                }
                // `remain_on_exit` survives a restart. Pane ids are reused on
                // restore, so the saved ids still match.
                for id in &state.remain_on_exit_panes {
                    session_manager.set_remain_on_exit(PaneId(*id), true);
                }
                for (ws_idx, tab_id, pane_id) in restore_targets {
                    if let Err(e) = session_manager.set_active_pane(ws_idx, tab_id, Some(pane_id)) {
                        warn!(
                            "cold-start restore: failed to set active_pane on workspace {ws_idx} \
                             tab {tab_id}: {e}"
                        );
                    }
                }
            }
            Ok(Some(_)) => {
                debug!("cold-start restore: session file has no workspaces, starting fresh");
            }
            Ok(None) => {
                debug!("cold-start restore: no session file found, starting fresh");
            }
            Err(e) => {
                warn!("cold-start restore: failed to load session file: {e}");
            }
        }
    }

    // V2-007 AC-10: prune orphan byte-log files (UUID not matching any live pane).
    // Runs AFTER cold-start restore so restored panes' logs are recognised as live,
    // and BEFORE the socket server accepts connections so there is no race.
    //
    // V2-007 fix cycle 1: under AD-001 (one daemon per window) N daemons start
    // concurrently and each only knows about its *own* in-memory panes. Pruning
    // against just this daemon's `list_panes()` would delete every sibling
    // daemon's log file. Instead we pass the **union** of (a) this daemon's
    // freshly-restored in-memory panes (whose UUIDs may not yet be in any
    // saved JSON if a save has not happened since restore) and (b) every
    // pane persisted in any active-or-trashed session JSON on disk. Every
    // legitimate log on disk is in (b) or was just created by (a); the union
    // is therefore a safe superset of all legitimate UUIDs.
    {
        let in_memory: Vec<uuid::Uuid> =
            session_manager.list_panes().into_iter().map(|id| id.0).collect();
        let persisted: Vec<uuid::Uuid> = forgetty_workspace::all_persisted_pane_ids();

        let mut live_ids: Vec<uuid::Uuid> =
            Vec::with_capacity(in_memory.len().saturating_add(persisted.len()));
        live_ids.extend(in_memory.iter().copied());
        live_ids.extend(persisted.iter().copied());
        live_ids.sort();
        live_ids.dedup();

        forgetty_workspace::prune_orphan_logs(&live_ids);
        debug!(
            "prune_orphan_logs: checked against {} union pane(s) ({} in-memory, {} persisted)",
            live_ids.len(),
            in_memory.len(),
            persisted.len(),
        );
    }

    let socket_path = args.socket_path.unwrap_or_else(sockpath::default_socket_path);

    // Bind the socket server (with session_id so shutdown_save can write the correct file).
    let socket_server = SocketServer::new_with_session(socket_path.clone(), session_id)?;

    // Write the session file before the socket accepts connections, so the
    // layout on disk always matches what the first client sees.
    save_session_from_layout(&session_manager, session_id);

    info!("forgetty-server started, socket at {}", socket_path.display());

    // Per-pane drain tasks are spawned by the SessionManager automatically
    // when each pane is created (via spawn_drain_task inside create_pane /
    // create_tab / split_pane / split_pane_with_ratio). No shared polling loop
    // needed here — each pane wakes its drain task only when PTY bytes arrive.

    // Dirty-flag: set to true when a layout mutation event (PaneCreated /
    // PaneClosed) is observed. Shared between the watcher task and the
    // debounce-save task.
    let dirty_flag = Arc::new(AtomicBool::new(false));

    // Layout-event watcher task: on layout mutations, save immediately AND
    // set the dirty flag so the debounce task continues to coalesce any
    // follow-up changes within its 5-second window. The immediate save is
    // required to close the multi-daemon startup race (V2-007 fix cycle 7)
    // where a sibling daemon's `prune_orphan_logs` runs before the 5-second
    // debounce fires and wrongly deletes the just-created pane's log file
    // (AD-001 × AD-013 interaction). Layout mutations are rare (user-driven
    // open/close/split), so the extra JSON serialize + atomic write per event
    // is cheap (<1 ms on typical WorkspaceState sizes).
    {
        let sm = Arc::clone(&session_manager);
        let dirty = Arc::clone(&dirty_flag);
        tokio::spawn(async move {
            let mut rx = sm.subscribe_output();
            loop {
                match rx.recv().await {
                    Ok(SessionEvent::PaneCreated { .. })
                    | Ok(SessionEvent::PaneClosed { .. })
                    | Ok(SessionEvent::TabCreated { .. })
                    | Ok(SessionEvent::TabClosed { .. })
                    | Ok(SessionEvent::PaneSplit { .. })
                    | Ok(SessionEvent::TabMoved { .. })
                    | Ok(SessionEvent::ActiveTabChanged { .. })
                    | Ok(SessionEvent::ActiveWorkspaceChanged { .. })
                    | Ok(SessionEvent::WorkspaceCreated { .. })
                    | Ok(SessionEvent::WorkspaceRenamed { .. })
                    | Ok(SessionEvent::LabelChanged { .. })
                    | Ok(SessionEvent::WorkspaceDeleted { .. })
                    | Ok(SessionEvent::WorkspaceColorChanged { .. })
                    | Ok(SessionEvent::WorkspacesReordered { .. })
                    | Ok(SessionEvent::ActivePaneChanged { .. }) => {
                        // Save immediately so sibling daemons' prune passes
                        // see this pane's UUID in the persisted-union (V2-007
                        // fix cycle 7). Leave the dirty flag set as well so
                        // the debounce task coalesces any rapid follow-up
                        // changes within its window (belt-and-suspenders).
                        save_session_from_layout(&sm, session_id);
                        dirty.store(true, Ordering::Relaxed);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Fell behind — treat as dirty (many events fired).
                        dirty.store(true, Ordering::Relaxed);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Debounce-save task: if the dirty flag is set, save once every 5 seconds.
    // This coalesces rapid layout mutations (e.g. three tabs opened at once)
    // into a single disk write.
    {
        let sm = Arc::clone(&session_manager);
        let dirty = Arc::clone(&dirty_flag);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if dirty.swap(false, Ordering::Relaxed) {
                    save_session_from_layout(&sm, session_id);
                }
            }
        });
    }

    // Periodic safety-save task: unconditionally saves the layout every 60
    // seconds regardless of the dirty flag. Guarantees at most 60 seconds of
    // layout changes are lost even if the event watcher misses something.
    {
        let sm = Arc::clone(&session_manager);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                save_session_from_layout(&sm, session_id);
            }
        });
    }

    // Load identity and bind iroh endpoint (V2-011 / AD-015).
    //
    // `forgetty-sync` owns only the pairing ALPN. The terminal streaming ALPN
    // is registered here with a closure that forwards to the terminal-side
    // handler in `forgetty_server::iroh_terminal`, keeping all terminal-
    // specific behaviour out of the transport crate.
    let sm_for_alpn = Arc::clone(&session_manager);
    let rpc_v2 = RpcV2::new(Arc::clone(&session_manager), session_id);
    let port = args
        .port
        .or_else(|| std::env::var("FORGETTY_PORT").ok().and_then(|v| v.parse().ok()))
        .unwrap_or(DEFAULT_PORT);
    info!("n0 discovery: {}", if n0_discovery { "on" } else { "off" });
    if no_relay {
        info!("relays: none (the server never contacts a relay or n0)");
    } else if relay_urls.is_empty() {
        info!("relays: iroh defaults");
    } else {
        info!(
            "relays: {}",
            relay_urls.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(", ")
        );
    }
    // The iroh endpoint can come up late: a WireGuard-only server started
    // before its tunnel is up has no address to bind yet. The local socket
    // server and the panes do not wait for it.
    let sync_slot: forgetty_socket::SyncSlot = Default::default();
    #[cfg(feature = "qa-tools")]
    forgetty_server::qa_hook::install(Arc::clone(&rpc_v2), Arc::clone(&sync_slot));
    let chose_addrs = !bind_addrs.is_empty();
    let bind_label = if chose_addrs {
        bind_addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
    } else {
        "the bind address".to_string()
    };
    let all_bind_addrs = bind_addrs.clone();
    let make_endpoint = {
        let (rpc_v2, relay_urls) = (Arc::clone(&rpc_v2), relay_urls);
        move |bind_addrs: Vec<std::net::IpAddr>| {
            let sm_for_alpn = Arc::clone(&sm_for_alpn);
            SyncEndpoint::builder(secret_key.clone())
                .bind_port(port)
                .relay_urls(relay_urls.clone())
                .n0_discovery(n0_discovery)
                .relays_disabled(no_relay)
                .bind_addrs(bind_addrs)
                .transport_config(quic_config())
                .register_alpn(forgetty_proto::ALPN_RPC, rpc_v2.alpn_handler())
                .register_alpn(
                    FORGETTY_STREAM_ALPN,
                    Arc::new(move |conn, registry| {
                        let sm = Arc::clone(&sm_for_alpn);
                        Box::pin(async move {
                            handle_terminal_stream(conn, sm, registry).await;
                        })
                    }),
                )
                .build()
        }
    };
    let bring_up = {
        let (rpc_v2, slot) = (Arc::clone(&rpc_v2), Arc::clone(&sync_slot));
        move |ep: SyncEndpoint| -> Arc<SyncEndpoint> {
            info!(
                "iroh endpoint bound, node_id={}, udp={:?}, alpns=[forgetty/rpc/2, forgetty/pair/2, forgetty/pair/1 (refuses), forgetty/stream/1]",
                ep.node_id(),
                ep.bound_sockets()
            );
            let ep = Arc::new(ep);
            rpc_v2.set_sync(Arc::clone(&ep));
            let _ = slot.set(Arc::clone(&ep));
            // Spawn iroh accept loop.
            let for_loop = Arc::clone(&ep);
            tokio::spawn(async move {
                for_loop.accept_loop().await;
            });
            ep
        }
    };
    // Bind the `--bind` addresses that exist. The rest are watched for.
    let usable = {
        let all = all_bind_addrs.clone();
        move || -> Result<Vec<std::net::IpAddr>, forgetty_sync::SyncError> {
            if all.is_empty() {
                return Ok(Vec::new());
            }
            let there: Vec<std::net::IpAddr> =
                all.iter().copied().filter(|ip| address_present(*ip)).collect();
            if there.is_empty() {
                Err(forgetty_sync::SyncError::BindAddrNotAvailable(
                    "none of the --bind addresses is on this machine".into(),
                ))
            } else {
                Ok(there)
            }
        }
    };
    let first = match usable() {
        Ok(set) => make_endpoint(set).await,
        Err(e) => Err(e),
    };
    match first {
        Ok(ep) => {
            let ep = bring_up(ep);
            watch_missing_addresses(ep, all_bind_addrs.clone());
        }
        Err(e) if bind_is_retryable(chose_addrs, &e) => {
            // Not fatal: the process keeps the panes and the local socket and
            // tries again until the address is there.
            warn!(
                "iroh endpoint: cannot bind {bind_label} ({e}); remote access is off until \
                 {bind_label} is available; retrying in the background"
            );
            tokio::spawn(async move {
                let mut wait = BIND_RETRY_MIN;
                let mut last_warn = std::time::Instant::now();
                loop {
                    tokio::time::sleep(wait).await;
                    let attempt = match usable() {
                        Ok(set) => make_endpoint(set).await,
                        Err(e) => Err(e),
                    };
                    match attempt {
                        Ok(ep) => {
                            let ep = bring_up(ep);
                            watch_missing_addresses(ep, all_bind_addrs.clone());
                            return;
                        }
                        Err(e) if bind_is_retryable(chose_addrs, &e) => {
                            if last_warn.elapsed() >= BIND_WARN_EVERY {
                                warn!(
                                    "iroh endpoint: still cannot bind {bind_label} ({e}); remote \
                                     access is off until {bind_label} is available"
                                );
                                last_warn = std::time::Instant::now();
                            } else {
                                debug!("iroh endpoint: bind failed again ({e})");
                            }
                        }
                        Err(e) => {
                            error!("iroh endpoint: {e}; giving up on the network side");
                            return;
                        }
                    }
                    wait = (wait * 2).min(BIND_RETRY_MAX);
                }
            });
        }
        Err(e) => {
            warn!("totem-sync: failed to bind iroh endpoint: {e}");
            return Err(anyhow::anyhow!("iroh bind failed: {e}"));
        }
    }

    // Spawn the socket server with full SessionManager + SyncEndpoint integration.
    let _socket_task = {
        let sm = Arc::clone(&session_manager);
        let slot = Arc::clone(&sync_slot);
        tokio::spawn(async move {
            if let Err(e) = socket_server.run_with_slot(sm, slot).await {
                error!("Socket server error: {e}");
            }
        })
    };

    // Wait for SIGTERM (from systemd stop) or SIGINT (Ctrl-C in --foreground mode).
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => { info!("Received SIGTERM"); }
        _ = sigint.recv()  => { info!("Received SIGINT");  }
    }
    // Freeze the layout before anything else. Children that die with the
    // server (a signal to the whole group) must not shrink the saved layout.
    session_manager.begin_shutdown();

    info!("forgetty-server shutting down");
    forgetty_socket::graceful_exit(
        &session_manager,
        sync_slot.get().map(|e| &**e),
        Some(session_id),
        &socket_path,
    )
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Snapshot the live session layout and write it to `sessions/{server_id}.json`.
///
/// Used by the layout-event watcher, the debounce task and the periodic safety
/// save. Failures are logged and never propagated.
fn save_session_from_layout(session_manager: &SessionManager, session_id: uuid::Uuid) {
    forgetty_socket::save_layout(session_manager, session_id);
    debug!(session_id = %session_id, "server: session layout saved");
}

/// Return the CWD of the leftmost (first) leaf in a `PaneTreeState`.
///
/// Used by cold-start restore to seed the initial `create_tab` call for a tab
/// whose root node may be a `Split`.
fn first_leaf_cwd(tree: &forgetty_workspace::PaneTreeState) -> &std::path::Path {
    match tree {
        forgetty_workspace::PaneTreeState::Leaf { cwd, .. } => cwd,
        forgetty_workspace::PaneTreeState::Split { first, .. } => first_leaf_cwd(first),
    }
}

/// Walk to the leftmost leaf of a saved pane tree and return its `pane_id` (FIX-013).
///
/// Returns `None` if the leaf has no recorded pane_id (pre-FIX-013 session JSONs
/// where `PaneTreeState::Leaf::pane_id` deserialised via `#[serde(default)]`).
fn first_leaf_pane_id(tree: &forgetty_workspace::PaneTreeState) -> Option<uuid::Uuid> {
    match tree {
        forgetty_workspace::PaneTreeState::Leaf { pane_id, .. } => *pane_id,
        forgetty_workspace::PaneTreeState::Split { first, .. } => first_leaf_pane_id(first),
    }
}

/// FIX-005B: walk the saved pane_tree and the live pane_tree in lock-step,
/// returning the **live** PaneId of the leaf at the same position as the
/// saved leaf whose `pane_id == saved_target`.
///
/// Cold restart re-mints `PaneId::new()` for every restored leaf, so the
/// saved UUID is never directly present in the live layout. The trees are
/// built in matching shape by `restore_subtree` (recursive on the saved
/// tree), so a parallel walk lets us translate "the leaf that used to be
/// `saved_target`" → "this live PaneId".
///
/// Returns `None` if the trees diverge in shape (would only happen if a
/// future refactor of `restore_subtree` broke the invariant) or if no
/// saved leaf matches `saved_target` (orphan UUID — caller falls back to
/// the first leaf).
fn live_pane_id_for_saved(
    saved: &forgetty_workspace::PaneTreeState,
    live: &forgetty_session::workspace::PaneTreeLayout,
    saved_target: uuid::Uuid,
) -> Option<PaneId> {
    use forgetty_session::workspace::PaneTreeLayout as Live;
    use forgetty_workspace::PaneTreeState as Saved;

    match (saved, live) {
        (Saved::Leaf { pane_id: saved_id, .. }, Live::Leaf { pane_id: live_id }) => {
            if *saved_id == Some(saved_target) {
                Some(*live_id)
            } else {
                None
            }
        }
        (Saved::Split { first: sa, second: sb, .. }, Live::Split { first: la, second: lb, .. }) => {
            live_pane_id_for_saved(sa, la, saved_target)
                .or_else(|| live_pane_id_for_saved(sb, lb, saved_target))
        }
        // Tree shape mismatch — should not happen because `restore_subtree`
        // builds the live tree from the saved tree, but degrade safely.
        _ => None,
    }
}

/// Recursively restore a saved pane sub-tree into the live session manager.
///
/// `anchor_id` is the live pane that corresponds to the root of `tree`. For
/// `Split` nodes the function calls `split_pane_with_ratio` to place the
/// second child, then recurses into both halves. Leaf nodes are no-ops because
/// `anchor_id` is already the live pane for that position.
fn restore_subtree(
    session_manager: &forgetty_session::SessionManager,
    anchor_id: PaneId,
    tree: &forgetty_workspace::PaneTreeState,
    size: forgetty_pty::PtySize,
) {
    match tree {
        forgetty_workspace::PaneTreeState::Leaf { cwd, .. } => {
            // Explicitly set the anchor pane's cached CWD to the saved value
            // so that snapshot_to_workspace_state returns the correct CWD even
            // before the drain loop has refreshed it from /proc/{pid}/cwd.
            if cwd.is_dir() {
                session_manager.set_pane_cwd(anchor_id, cwd.clone());
            }
        }
        forgetty_workspace::PaneTreeState::Split { direction, ratio, first, second } => {
            let second_cwd = first_leaf_cwd(second);
            let effective_cwd =
                if second_cwd.is_dir() { Some(second_cwd.to_path_buf()) } else { None };

            // FIX-013: reuse the saved leftmost-leaf pane_id of the `second` subtree
            // when present so its byte log is replayed on cold restart. The `first`
            // subtree's leaves stay anchored on the existing `anchor_id` chain.
            let saved_second_pane_id = first_leaf_pane_id(second);
            let split_result = match saved_second_pane_id {
                Some(pid) => session_manager.split_pane_with_ratio_and_pane_id(
                    anchor_id,
                    direction,
                    *ratio,
                    size,
                    effective_cwd,
                    forgetty_core::PaneId(pid),
                ),
                None => session_manager.split_pane_with_ratio(
                    anchor_id,
                    direction,
                    *ratio,
                    size,
                    effective_cwd,
                ),
            };

            match split_result {
                Ok(second_pane_id) => {
                    restore_subtree(session_manager, anchor_id, first, size);
                    restore_subtree(session_manager, second_pane_id, second, size);
                }
                Err(e) => {
                    warn!("cold-start restore: split_pane_with_ratio failed: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_bind_error_is_attributed_to_the_address_it_names() {
        let ips: Vec<std::net::IpAddr> =
            ["10.0.0.2", "10.0.0.3", "fd00::2"].iter().map(|s| s.parse().unwrap()).collect();
        let named = |e: &str| named_in_error(e, &ips);
        assert_eq!(named("Failed to bind iroh endpoint: 10.0.0.3:47297: Address in use"), vec![ips[1]]);
        assert_eq!(named("x: [fd00::2]:47297: nope"), vec![ips[2]]);
        // Not the tail of a longer address.
        assert!(named("110.0.0.2:5: nope").is_empty());
        // Names none: cannot be attributed.
        assert!(named("Failed to bind sockets: Address already in use (os error 98)").is_empty());
        // Names an address that was bound before: not one of the new ones.
        assert!(named("10.0.0.1:47297: nope").is_empty());
    }

    #[test]
    fn an_address_that_keeps_failing_is_given_up_and_the_others_keep_their_retries() {
        let (a, b): (std::net::IpAddr, std::net::IpAddr) =
            ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        let t0 = std::time::Instant::now();
        let mut book = RetryBook::default();
        assert!(book.due(&a, t0) && !book.given_up(&a));
        assert_eq!(book.failure(a, t0), (1, Duration::from_secs(10)));
        assert!(!book.due(&a, t0 + Duration::from_secs(9)));
        assert!(book.due(&a, t0 + Duration::from_secs(10)));
        // Another address does not wait for a's cooldown.
        assert!(book.due(&b, t0 + Duration::from_secs(1)));
        assert_eq!(book.failure(a, t0).0, 2);
        assert!(!book.given_up(&a));
        assert_eq!(book.failure(a, t0).0, 3);
        assert!(book.given_up(&a) && !book.given_up(&b));
        book.success(&a);
        assert!(!book.given_up(&a));
    }

    use super::*;

    #[test]
    fn a_config_that_does_not_parse_is_an_error_and_a_missing_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        // Missing: defaults.
        let missing = dir.path().join("none.toml");
        assert!(load_server_config(Some(&missing)).unwrap().relay.is_none());
        // Valid.
        let ok = dir.path().join("ok.toml");
        std::fs::write(&ok, "relay = \"none\"\n").unwrap();
        assert_eq!(load_server_config(Some(&ok)).unwrap().relay.as_deref(), Some("none"));
        // A typo: an error that names the file and the line, not the defaults.
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "font_size = 13.0\nrelay = \"none\n").unwrap();
        let err = load_server_config(Some(&bad)).unwrap_err();
        assert!(err.downcast_ref::<ConfigInvalid>().is_some(), "{err}");
        let text = err.to_string();
        assert!(text.contains("bad.toml"), "{text}");
        assert!(text.contains("line 2"), "{text}");
    }

    /// Bind with `--bind addr` on `port` and return the real error.
    async fn bind_error(addr: &str, port: u16) -> forgetty_sync::SyncError {
        let key = iroh::SecretKey::generate(&mut rand::rng());
        match SyncEndpoint::builder(key)
            .bind_port(port)
            .relays_disabled(true)
            .bind_addrs(vec![addr.parse().unwrap()])
            .build()
            .await
        {
            Ok(_) => panic!("bind on {addr}:{port} worked"),
            Err(e) => e,
        }
    }

    #[tokio::test]
    async fn a_failed_bind_waits_only_for_a_chosen_or_missing_address() {
        use forgetty_sync::SyncError;
        // An address that is not on this machine: EADDRNOTAVAIL, found by its
        // io kind in the error chain. It waits, with or without --bind.
        let missing = bind_error("192.0.2.1", 0).await;
        assert!(matches!(missing, SyncError::BindAddrNotAvailable(_)), "{missing:?}");
        assert!(bind_is_retryable(true, &missing));
        assert!(bind_is_retryable(false, &missing));
        // A busy port: waits with --bind, ends the process without.
        let holder = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let busy = bind_error("127.0.0.1", holder.local_addr().unwrap().port()).await;
        assert!(matches!(busy, SyncError::Bind(_)), "{busy:?}");
        assert!(bind_is_retryable(true, &busy));
        assert!(!bind_is_retryable(false, &busy));
    }

    #[test]
    fn loopback_only_binds_are_flagged() {
        let ips =
            |v: &[&str]| v.iter().map(|s| s.parse().unwrap()).collect::<Vec<std::net::IpAddr>>();
        assert!(loopback_only(&ips(&["127.0.0.1"])));
        assert!(loopback_only(&ips(&["127.0.0.1", "::1"])));
        assert!(!loopback_only(&ips(&["127.0.0.1", "10.0.0.2"])));
        assert!(!loopback_only(&ips(&["10.230.1.14"])));
        assert!(!loopback_only(&[]));
    }

    #[test]
    fn server_id_is_stable_and_lock_is_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let prior = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: this is the only test in this crate that touches the environment.
        unsafe { std::env::set_var("XDG_DATA_HOME", tmp.path()) };

        let state = lock_state_dir().unwrap();
        let first = load_or_create_server_id(&state).unwrap();
        let second = load_or_create_server_id(&state).unwrap();
        assert_eq!(first, second, "the id survives a restart");

        let lock = acquire_instance_lock(first).unwrap();
        let err = acquire_instance_lock(first).unwrap_err();
        assert!(err.to_string().contains("already running"));
        assert!(err.downcast_ref::<AlreadyRunning>().is_some());
        drop(lock);
        assert!(acquire_instance_lock(first).is_ok(), "lock is free after the holder exits");

        // A corrupt id file is an error. It is not replaced and the file is untouched.
        let id_path = forgetty_core::platform::data_dir().join("server_id");
        std::fs::write(&id_path, "garbage").unwrap();
        let err = load_or_create_server_id(&state).unwrap_err();
        assert!(err.to_string().contains("corrupt"), "{err}");
        assert_eq!(std::fs::read_to_string(&id_path).unwrap(), "garbage");

        // The state lock is exclusive between processes: a second lock attempt
        // from another open file description blocks, so test it with flock(2).
        {
            use std::os::unix::io::AsRawFd;
            let other =
                std::fs::File::open(forgetty_core::platform::data_dir().join("server.lock"))
                    .unwrap();
            // SAFETY: valid fd, no pointers.
            let rc = unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            assert_ne!(rc, 0, "server.lock is held while the StateLock lives");
        }

        unsafe {
            match prior {
                Some(p) => std::env::set_var("XDG_DATA_HOME", p),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }
}
