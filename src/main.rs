mod cli;

use clap::Parser;
use forgetty_config::load_config;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let _args = cli::Args::parse();
    tracing::info!("Starting Forgetty v{}", env!("CARGO_PKG_VERSION"));

    let config = match load_config(None) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to load config, using defaults: {e}");
            forgetty_config::Config::default()
        }
    };

    if let Err(e) = forgetty_gtk::app::run(config) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
