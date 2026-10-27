//! `forgetty`: the durable terminal server and its CLI in one binary (AD-017).
//!
//! ```text
//! forgetty server run | start | stop [--if-idle] | status [--json]
//! forgetty ls [--json]
//! forgetty new [--cwd DIR] [-- CMD...]
//! forgetty send -p PANE TEXT
//! forgetty send-key -p PANE KEY
//! forgetty close -p PANE
//! forgetty capture -p PANE [--trim] [--lines N] [--json]
//! forgetty events [--json] [-p PANE]
//! forgetty run [--keep] [--wait] [--cwd DIR] -- CMD...
//! forgetty wait -p PANE [--status done|waiting|...] [--timeout S]
//! forgetty client ls [--json]
//! forgetty pair [--cancel] | devices | revoke ID
//! forgetty label [-t tab|pane|workspace] TARGET LABEL
//! forgetty api list [-q] [--json] [--markdown]
//! forgetty api describe METHOD [--json]
//! forgetty api call [-p PANE] METHOD [JSON | @file | -]
//! forgetty skill setup|status|show|remove
//! ```
//!
//! A pane, tab or workspace target is a full id, a label, a position or a
//! unique id suffix of 4+ characters, in that order.
//!
//! Exit codes: 0 ok, 1 failed, 2 bad command line (or `bad_request`), 3 target
//! not found or ambiguous (or `method_not_found`), 4 server unreachable,
//! 5 `wait` timed out.

mod cli;
mod server;
mod skill;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "forgetty", version)]
#[command(about = "Forgetty durable terminal server and CLI")]
struct Cli {
    /// Keep all state under DIR: `data/`, `config/` and `run/` (socket).
    ///
    /// Sets XDG_DATA_HOME, XDG_CONFIG_HOME and XDG_RUNTIME_DIR for this
    /// process. Use it for tests and throwaway instances.
    #[arg(long, global = true, value_name = "DIR")]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run, start, stop or inspect the server.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// List panes.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Open a new tab with one pane and print its id.
    New {
        /// Starting directory (default: the current directory).
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Keep the pane after its process exits: it shows the exit status and
        /// stays listed (`exited`) until closed.
        #[arg(long)]
        keep: bool,
        /// Command to run instead of the shell, after `--`.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Type text into a pane (no newline unless `--enter`).
    Send {
        /// Pane id or unique id suffix of 4+ characters.
        #[arg(short = 'p', long)]
        pane: String,
        /// Also press enter after the text.
        #[arg(long)]
        enter: bool,
        text: String,
    },
    /// Send one key to a pane: enter, tab, esc, ctrl+c, up, down, left, right.
    SendKey {
        /// Pane id or unique id suffix of 4+ characters.
        #[arg(short = 'p', long)]
        pane: String,
        key: String,
    },
    /// Close a pane. A kept pane (`new --keep`) that already exited goes too.
    Close {
        /// Pane id or unique id suffix of 4+ characters.
        #[arg(short = 'p', long)]
        pane: String,
    },
    /// Print the visible screen of a pane as text.
    Capture {
        /// Pane id or unique id suffix of 4+ characters.
        #[arg(short = 'p', long)]
        pane: String,
        /// Remove trailing spaces and trailing blank lines.
        #[arg(long)]
        trim: bool,
        /// Keep only the last N lines.
        #[arg(long, value_name = "N")]
        lines: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Stream server events (title, cwd, status, notifications, pane and
    /// layout changes) until Ctrl+C.
    Events {
        #[arg(long)]
        json: bool,
        /// Only events of this pane (id or unique id suffix of 4+ characters).
        #[arg(short = 'p', long)]
        pane: Option<String>,
    },
    /// Run a command in a new pane and print the pane id. With `--wait`,
    /// block and exit with the command's exit code.
    Run {
        /// Keep the pane after the command exits (see `new --keep`).
        #[arg(long)]
        keep: bool,
        /// Wait for the command to exit and return its exit code.
        #[arg(long)]
        wait: bool,
        /// Starting directory (default: the current directory).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Command and arguments, after `--`.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Wait until a pane reaches a status, or (without `--status`) until its
    /// process exits. Exit 0 when reached, 5 on timeout.
    Wait {
        /// Pane id or unique id suffix of 4+ characters.
        #[arg(short = 'p', long)]
        pane: String,
        /// idle, working, waiting, done or failed.
        #[arg(long)]
        status: Option<String>,
        /// Give up after this many seconds.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<f64>,
    },
    /// Inspect attached clients.
    Client {
        #[command(subcommand)]
        command: ClientCommand,
    },
    /// Set the label of a pane, tab or workspace. An empty label clears it.
    ///
    /// TARGET is a full id, a label (case-insensitive), a 1-based position or
    /// a unique id suffix of 4+ characters. Several matches give exit 3.
    Label {
        /// What to label.
        #[arg(short = 't', long = "type", value_enum, default_value = "pane")]
        kind: cli::LabelKind,
        #[arg(long)]
        json: bool,
        target: String,
        label: String,
    },
    /// Inspect and call the server API (JSON-RPC methods with JSON Schemas).
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
    /// Install a skill that teaches a coding agent to use this CLI.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Open a 60 s pairing window and print the QR code.
    Pair {
        /// Window length in seconds (testing only).
        #[arg(long, default_value_t = 60, hide = true)]
        ttl: u64,
        /// Close a pairing window now, so its QR code stops working. With `--window-id` that
        /// window. Without it, the window the last `forgetty pair` of this server opened
        /// (never a window another program opened, such as the app's own).
        #[arg(long)]
        cancel: bool,
        /// The window to close with `--cancel` (printed as `window_id:` by `forgetty pair`).
        #[arg(long, requires = "cancel")]
        window_id: Option<u64>,
    },
    /// List paired devices.
    Devices {
        #[arg(long)]
        json: bool,
    },
    /// Revoke a paired device (id or unique prefix of 4+ characters).
    Revoke { id: String },
}

#[derive(Subcommand, Debug)]
enum ServerCommand {
    /// Run the server in the foreground (for systemd).
    Run(server::RunArgs),
    /// Start the server detached if it is not running.
    Start,
    /// Stop the server.
    Stop {
        /// Refuse while any client is attached.
        #[arg(long)]
        if_idle: bool,
    },
    /// Show pid, uptime, versions, pane and client counts, iroh node id.
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ApiCommand {
    /// List every method.
    List {
        /// Names only, one per line.
        #[arg(short = 'q', long)]
        quiet: bool,
        #[arg(long)]
        json: bool,
        /// Print `docs/socket-api.md` (needs no server).
        #[arg(long)]
        markdown: bool,
    },
    /// Show a method: description, params and result schemas, examples.
    Describe {
        method: String,
        #[arg(long)]
        json: bool,
    },
    /// Call a method and print the result as JSON.
    ///
    /// PARAMS is JSON text, `@file` (read a file) or `-` (read stdin).
    /// With `-p`, the pane id goes into `pane_id` of the params.
    Call {
        /// Pane id, label, position or id suffix, for methods that take `pane_id`.
        #[arg(short = 'p', long)]
        pane: Option<String>,
        method: String,
        params: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SkillCommand {
    /// Write the skill file.
    Setup {
        #[arg(long, value_enum, default_value = "claude")]
        agent: skill::Agent,
        #[arg(long, value_enum, default_value = "user")]
        scope: skill::Scope,
    },
    /// Show where the skill is installed and whether it is current.
    Status {
        #[arg(long, value_enum)]
        agent: Option<skill::Agent>,
        #[arg(long, value_enum)]
        scope: Option<skill::Scope>,
        #[arg(long)]
        json: bool,
    },
    /// Print the skill text.
    Show,
    /// Delete the skill file.
    Remove {
        #[arg(long, value_enum, default_value = "claude")]
        agent: skill::Agent,
        #[arg(long, value_enum, default_value = "user")]
        scope: skill::Scope,
    },
}

#[derive(Subcommand, Debug)]
enum ClientCommand {
    /// List attached clients.
    Ls {
        #[arg(long)]
        json: bool,
    },
}

/// Point the XDG base directories at `dir` (see `--state-dir`).
///
/// Runs before any thread starts: `set_var` is not safe once other threads
/// read the environment.
fn apply_state_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // `canonicalize` gives a `\\?\C:\...` path on Windows. A plain absolute path is what the
    // rest of the server (and a shell started from it) expects.
    #[cfg(windows)]
    let dir = std::path::absolute(dir)?;
    #[cfg(not(windows))]
    let dir = std::fs::canonicalize(dir)?;
    for (var, sub) in
        [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_RUNTIME_DIR", "run")]
    {
        let path = dir.join(sub);
        std::fs::create_dir_all(&path)?;
        forgetty_core::fsutil::acl::restrict_dir_to_user(&path)?;
        std::env::set_var(var, &path);
    }
    Ok(())
}

fn main() {
    // clap exits with code 2 on a bad command line.
    let cli = Cli::parse();

    if let Some(dir) = &cli.state_dir {
        if let Err(e) = apply_state_dir(dir) {
            eprintln!("forgetty: --state-dir {}: {e}", dir.display());
            std::process::exit(cli::EXIT_FAILED);
        }
    }

    // A CLI command whose output is piped to `head` should end quietly, not
    // panic. The server keeps Rust's default (SIGPIPE ignored): it relies on
    // write errors from closed sockets.
    #[cfg(unix)]
    if !matches!(cli.command, Command::Server { command: ServerCommand::Run(_) }) {
        // SAFETY: resetting a signal disposition before any thread starts.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }

    let code = match cli.command {
        Command::Server { command: ServerCommand::Run(args) } => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            match runtime.block_on(server::run(args)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("forgetty server: {e}");
                    if e.downcast_ref::<server::AlreadyRunning>().is_some() {
                        server::EXIT_ALREADY_RUNNING
                    } else if e.downcast_ref::<server::ConfigInvalid>().is_some() {
                        server::EXIT_CONFIG
                    } else {
                        cli::EXIT_FAILED
                    }
                }
            }
        }
        Command::Server { command: ServerCommand::Start } => cli::finish(cli::server_start()),
        Command::Server { command: ServerCommand::Stop { if_idle } } => {
            cli::finish(cli::server_stop(if_idle))
        }
        Command::Server { command: ServerCommand::Status { json } } => {
            cli::finish(cli::server_status(json))
        }
        Command::Ls { json } => cli::finish(cli::ls(json)),
        Command::New { cwd, json, keep, command } => {
            cli::finish(cli::new_pane(cwd, command, keep, json))
        }
        Command::Send { pane, enter, text } => cli::finish(cli::send(&pane, &text, enter)),
        Command::SendKey { pane, key } => cli::finish(cli::send_key(&pane, &key)),
        Command::Close { pane } => cli::finish(cli::close(&pane)),
        Command::Capture { pane, trim, lines, json } => {
            cli::finish(cli::capture(&pane, trim, lines, json))
        }
        Command::Events { json, pane } => cli::finish(cli::events(pane.as_deref(), json)),
        Command::Run { keep, wait, cwd, command } => {
            cli::finish_code(cli::run(keep, wait, cwd, command))
        }
        Command::Wait { pane, status, timeout } => {
            cli::finish_code(cli::wait(&pane, status.as_deref(), timeout))
        }
        Command::Client { command: ClientCommand::Ls { json } } => {
            cli::finish(cli::client_ls(json))
        }
        Command::Label { kind, json, target, label } => {
            cli::finish(cli::label(kind, &target, &label, json))
        }
        Command::Api { command } => cli::finish(match command {
            ApiCommand::List { quiet, json, markdown } => cli::api_list(quiet, json, markdown),
            ApiCommand::Describe { method, json } => cli::api_describe(&method, json),
            ApiCommand::Call { pane, method, params } => {
                cli::api_call(pane.as_deref(), &method, params.as_deref())
            }
        }),
        Command::Skill { command } => cli::finish(match command {
            SkillCommand::Setup { agent, scope } => skill::setup(agent, scope),
            SkillCommand::Status { agent, scope, json } => skill::status(agent, scope, json),
            SkillCommand::Show => {
                skill::show();
                Ok(())
            }
            SkillCommand::Remove { agent, scope } => skill::remove(agent, scope),
        }),
        Command::Pair { cancel: true, window_id, .. } => cli::finish(cli::pair_cancel(window_id)),
        Command::Pair { ttl, .. } => cli::finish(cli::pair(ttl)),
        Command::Devices { json } => cli::finish(cli::devices(json)),
        Command::Revoke { id } => cli::finish(cli::revoke(&id)),
    };
    std::process::exit(code);
}
