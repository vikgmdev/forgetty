//! `forgetty`: the durable terminal server and its CLI in one binary (AD-017).
//!
//! ```text
//! forgetty server run | start | stop [--if-idle] | status [--json]
//! forgetty ls [--json]
//! forgetty new [--cwd DIR] [-- CMD...]
//! forgetty send -p PANE TEXT
//! forgetty send-key -p PANE KEY
//! forgetty client ls [--json]
//! forgetty pair | devices | revoke ID
//! ```
//!
//! Exit codes: 0 ok, 1 failed, 2 bad command line, 3 target not found or
//! ambiguous, 4 server unreachable.

mod cli;
mod server;

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
    /// Inspect attached clients.
    Client {
        #[command(subcommand)]
        command: ClientCommand,
    },
    /// Open a 60 s pairing window and print the QR code.
    Pair {
        /// Window length in seconds (testing only).
        #[arg(long, default_value_t = 60, hide = true)]
        ttl: u64,
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
    let dir = std::fs::canonicalize(dir)?;
    for (var, sub) in
        [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_RUNTIME_DIR", "run")]
    {
        let path = dir.join(sub);
        std::fs::create_dir_all(&path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
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
                    cli::EXIT_FAILED
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
        Command::Client { command: ClientCommand::Ls { json } } => {
            cli::finish(cli::client_ls(json))
        }
        Command::Pair { ttl } => cli::finish(cli::pair(ttl)),
        Command::Devices { json } => cli::finish(cli::devices(json)),
        Command::Revoke { id } => cli::finish(cli::revoke(&id)),
    };
    std::process::exit(code);
}
