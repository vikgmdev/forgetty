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
pub fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        build_ui(app, &config);
    });

    // GTK expects argv-style arguments; pass empty since clap already parsed.
    let exit_code = app.run_with_args::<&str>(&[]);

    if exit_code != gtk4::glib::ExitCode::SUCCESS {
        return Err(format!("GTK application exited with code: {:?}", exit_code).into());
    }

    Ok(())
}

/// Build the main application window with a live terminal.
fn build_ui(app: &adw::Application, config: &Config) {
    info!("Building Forgetty GTK4 window");

    let header = adw::HeaderBar::new();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);

    // Create the terminal widget and wire up PTY I/O
    match crate::terminal::create_terminal(config) {
        Ok((drawing_area, _state)) => {
            content.append(&drawing_area);

            let window = adw::ApplicationWindow::builder()
                .application(app)
                .title("Forgetty")
                .default_width(DEFAULT_WIDTH)
                .default_height(DEFAULT_HEIGHT)
                .content(&content)
                .build();

            window.present();

            // Grab focus on the drawing area so key events are delivered
            drawing_area.grab_focus();
        }
        Err(e) => {
            tracing::error!("Failed to create terminal: {e}");

            // Fall back to an empty window with an error label
            let label = gtk4::Label::new(Some(&format!("Failed to create terminal: {e}")));
            label.set_hexpand(true);
            label.set_vexpand(true);
            content.append(&label);

            let window = adw::ApplicationWindow::builder()
                .application(app)
                .title("Forgetty")
                .default_width(DEFAULT_WIDTH)
                .default_height(DEFAULT_HEIGHT)
                .content(&content)
                .build();

            window.present();
        }
    }
}
