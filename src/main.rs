mod cli;

use clap::Parser;
use forgetty_config::load_config;
use forgetty_gtk::app::LaunchOptions;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let args = cli::Args::parse();
    // --version and --help are handled by clap before we reach here.

    tracing::info!("Starting Forgetty v{}", env!("CARGO_PKG_VERSION"));

    // Resolve --config-file (canonicalize relative paths before anything changes CWD).
    let config_path = args.config_file.map(|p| {
        std::fs::canonicalize(&p).unwrap_or_else(|_| {
            tracing::warn!("Could not canonicalize config path {:?}, using as-is", p);
            p
        })
    });

    let config = match load_config(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to load config, using defaults: {e}");
            forgetty_config::Config::default()
        }
    };

    // Validate --working-directory: must exist and be a directory.
    let working_directory = args.working_directory.and_then(|p| match std::fs::canonicalize(&p) {
        Ok(canonical) if canonical.is_dir() => Some(canonical),
        Ok(canonical) => {
            tracing::warn!(
                "--working-directory {:?} is not a directory, falling back to home",
                canonical
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                "--working-directory {:?} does not exist ({}), falling back to home",
                p,
                e
            );
            None
        }
    });

    let launch_opts = LaunchOptions {
        working_directory,
        command: if args.execute.is_empty() { None } else { Some(args.execute) },
        class: args.class,
    };

    if let Err(e) = forgetty_gtk::app::run(config, launch_opts) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
