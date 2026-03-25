mod cli;

use clap::Parser;
use forgetty_config::defaults::default_config;
use forgetty_ui::app::App;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let _args = cli::Args::parse();
    tracing::info!("Starting Forgetty v{}", env!("CARGO_PKG_VERSION"));

    if let Err(e) = App::run(default_config()) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
