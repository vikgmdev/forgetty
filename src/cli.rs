use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "forgetty")]
#[command(about = "The AI-first agentic terminal emulator")]
#[command(version)]
pub struct Args {
    /// Path to configuration file
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Working directory for the terminal
    #[arg(long)]
    pub working_dir: Option<PathBuf>,

    /// Command to run instead of the default shell
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}
