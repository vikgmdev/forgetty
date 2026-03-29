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
}
