mod cli;

use clap::Parser;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let _args = cli::Args::parse();
    tracing::info!("Starting Forgetty v{}", env!("CARGO_PKG_VERSION"));

    // TODO: Phase 4 — launch the terminal application
    println!("Forgetty — The AI-first agentic terminal");
    println!("v{}", env!("CARGO_PKG_VERSION"));
}
