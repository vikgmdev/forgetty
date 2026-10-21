//! forgetty-win: V3 Windows client spike. GPUI window + libghostty-vt screen
//! state + local ConPTY shell. `--smoke` runs the headless CI measurement.

mod element;
mod keys;
mod pty;
mod view;

use std::time::{Duration, Instant};

use gpui::{px, size, App, Bounds, WindowBounds, WindowOptions, prelude::*};
use gpui_platform::application;

use view::{Metrics, TerminalView, COLS, ROWS};

const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);

fn fail(msg: &str) -> ! {
    println!(
        "{}",
        serde_json::json!({"ok": false, "error": msg, "phase": view::phase_name()})
    );
    eprintln!("forgetty-win: {msg} (phase: {})", view::phase_name());
    std::process::exit(1);
}

fn main() {
    let start = Instant::now();
    let smoke = std::env::args().any(|a| a == "--smoke");

    if smoke {
        // Watchdog: independent of the GPUI loop so a hung window still exits 1.
        std::thread::spawn(move || {
            std::thread::sleep(SMOKE_TIMEOUT);
            fail("timeout");
        });
    }

    application().run(move |cx: &mut App| {
        let m = Metrics::measure(cx);
        let bounds = Bounds::centered(
            None,
            size(m.cell_w * COLS as f32, m.line_h * ROWS as f32 + px(0.0)),
            cx,
        );
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| {
                    TerminalView::new(smoke.then_some(start), cx)
                        .unwrap_or_else(|e| fail(&format!("terminal setup: {e:#}")))
                });
                let handle = gpui::Focusable::focus_handle(view.read(cx), cx);
                window.focus(&handle, cx);
                view
            },
        );
        if let Err(e) = opened {
            fail(&format!("open_window: {e:#}"));
        }
        cx.activate(true);
    });
}
