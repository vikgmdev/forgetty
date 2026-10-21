//! forgetty-win: V3 Windows client. GPUI window + libghostty-vt screen state
//! + a `PaneSource` (local ConPTY today, remote durable-server pane later).
//! `--smoke` runs the headless CI gate.

mod config;
mod element;
mod encoder;
mod smoke;
mod source;
mod view;

use std::time::Instant;

use gpui::{prelude::*, px, size, App, Bounds, WindowBounds, WindowOptions};
use gpui_platform::application;

use config::Config;
use source::{LocalConPty, PaneSource};
use view::{Metrics, TerminalView, INITIAL_COLS, INITIAL_ROWS};

fn main() {
    let start = Instant::now();
    let smoke = std::env::args().any(|a| a == "--smoke");

    if smoke {
        // Watchdog: independent of the GPUI loop so a hung window still exits 1.
        std::thread::spawn(|| {
            std::thread::sleep(smoke::TOTAL_TIMEOUT);
            smoke::fail("timeout");
        });
    }

    let (cfg, cfg_err) = if smoke { (Config::default(), None) } else { Config::load() };
    if let Some(e) = cfg_err {
        eprintln!("forgetty-win: config error, using defaults: {e}");
    }

    application().run(move |cx: &mut App| {
        let m = Metrics::measure(cx, &cfg);
        let bounds = Bounds::centered(
            None,
            size(m.cell_w * INITIAL_COLS as f32, m.line_h * INITIAL_ROWS as f32 + px(0.0)),
            cx,
        );

        let shell = if smoke {
            "cmd.exe".to_string()
        } else {
            cfg.shell
                .clone()
                .or_else(|| std::env::var("FORGETTY_SHELL").ok())
                .or_else(|| std::env::var("COMSPEC").ok())
                .unwrap_or_else(|| "cmd.exe".into())
        };
        let cwd = if smoke { Some("C:\\") } else { None };
        let source: Box<dyn PaneSource> =
            match LocalConPty::spawn(&shell, cwd, INITIAL_ROWS as u16, INITIAL_COLS as u16) {
                Ok(s) => Box::new(s),
                Err(e) => fatal(smoke, &format!("local pty: {e:#}")),
            };

        let smoke_state = smoke.then(|| Box::new(smoke::Smoke::new(start)));
        let opened = cx.open_window(
            WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
            |window, cx| {
                let view = cx.new(|cx| TerminalView::new(cfg, source, smoke_state, cx));
                let handle = gpui::Focusable::focus_handle(view.read(cx), cx);
                window.focus(&handle, cx);
                // DECSET 1004 focus reporting + cursor hollow/solid on blur.
                view.update(cx, |_, cx| {
                    cx.observe_window_activation(window, |v, window, cx| {
                        v.on_activation(window.is_window_active(), cx)
                    })
                    .detach();
                });
                view
            },
        );
        if let Err(e) = opened {
            fatal(smoke, &format!("open_window: {e:#}"));
        }
        cx.activate(true);
    });
}

fn fatal(smoke: bool, msg: &str) -> ! {
    if smoke {
        smoke::fail(msg)
    }
    eprintln!("forgetty-win: {msg}");
    std::process::exit(1);
}
