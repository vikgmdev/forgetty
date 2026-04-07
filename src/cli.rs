use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "forgetty")]
#[command(about = "The AI-first agentic terminal emulator")]
#[command(version)]
pub struct Args {
    /// Path to configuration file
    #[arg(long = "config-file")]
    pub config_file: Option<PathBuf>,

    /// Initial working directory
    #[arg(long = "working-directory")]
    pub working_directory: Option<PathBuf>,

    /// Set the window class (WM_CLASS) for window manager rules
    #[arg(long)]
    pub class: Option<String>,

    /// Execute a command instead of the default shell.
    /// All arguments after -e are passed to the command.
    /// Flags like --working-directory must come BEFORE -e.
    #[arg(short = 'e', long = "execute", trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
    pub execute: Vec<String>,

    /// Skip session restore and open a fresh single-tab window
    #[arg(long)]
    pub no_restore: bool,

    /// Session UUID to attach to (for restore). If not set, a new session is created.
    #[arg(long)]
    pub session_id: Option<uuid::Uuid>,

    /// Restore all saved sessions (open one window per session file).
    #[arg(long)]
    pub restore_all: bool,

    /// Open an ephemeral session that is never persisted. The terminal works
    /// normally but no session file is written on close.
    #[arg(long)]
    pub temp: bool,

    /// Restore a specific trashed session by UUID.
    ///
    /// Moves the session file from `sessions/trash/` back to `sessions/`,
    /// then launches the session. Used by the undo-close notification action.
    #[arg(long)]
    pub restore_session: Option<uuid::Uuid>,
}
