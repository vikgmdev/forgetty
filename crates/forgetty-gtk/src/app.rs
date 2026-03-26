//! GTK4 application entry point.
//!
//! Creates and runs the adw::Application, managing the window lifecycle
//! (open, resize, close) with native GNOME client-side decorations.

use forgetty_config::Config;
use libadwaita as adw;
use libadwaita::prelude::*;
use tracing::info;

/// The application ID used for D-Bus registration and desktop integration.
const APP_ID: &str = "dev.forgetty.Forgetty";

/// Default window width in pixels.
const DEFAULT_WIDTH: i32 = 960;

/// Default window height in pixels.
const DEFAULT_HEIGHT: i32 = 640;

/// Run the GTK4/libadwaita application.
///
/// This function blocks until the window is closed. It initialises libadwaita,
/// creates the main application window with CSD header bar, and enters the
/// GTK main loop.
pub fn run(_config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_activate(build_ui);

    // GTK expects argv-style arguments; pass empty since clap already parsed.
    let exit_code = app.run_with_args::<&str>(&[]);

    if exit_code != gtk4::glib::ExitCode::SUCCESS {
        return Err(format!("GTK application exited with code: {:?}", exit_code).into());
    }

    Ok(())
}

/// Build the main application window.
fn build_ui(app: &adw::Application) {
    info!("Building Forgetty GTK4 window");

    let header = adw::HeaderBar::new();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Forgetty")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&content)
        .build();

    window.present();
}
