//! forgetty-win: V3 Windows client. GPUI app shell (tabs, splits, sidebar,
//! palette) over a `SessionBackend`, terminal panes on libghostty-vt.
//! `--smoke` runs the headless CI gate.

mod chrome;
mod config;
mod element;
mod encoder;
mod lineedit;
mod screenshot;
mod shell;
mod smoke;
mod view;

use std::sync::Arc;
use std::time::Instant;

use forgetty_shell_core::{LocalBackend, LocalConfig, SessionBackend};
use gpui::{prelude::*, px, size, App, Bounds, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowOptions};
use gpui_platform::application;

use config::{Backdrop, Config};
use shell::AppShell;

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

    let (mut cfg, cfg_err) = if smoke { (Config::default(), None) } else { Config::load() };
    // Mica needs Windows 11 22H2 (build 22621). Older systems get a solid background.
    if matches!(cfg.backdrop, Backdrop::Mica | Backdrop::MicaAlt) && windows_build() < 22621 {
        cfg.backdrop = Backdrop::None;
    }
    if let Some(e) = cfg_err {
        eprintln!("forgetty-win: config error, using defaults: {e}");
    }

    application().run(move |cx: &mut App| {
        let shell_cmd = if smoke {
            "cmd.exe".to_string()
        } else {
            cfg.shell
                .clone()
                .or_else(|| std::env::var("FORGETTY_SHELL").ok())
                .or_else(|| std::env::var("COMSPEC").ok())
                .unwrap_or_else(|| "cmd.exe".into())
        };
        let local = LocalBackend::new(LocalConfig {
            shell: vec![shell_cmd],
            default_cwd: if smoke { Some("C:\\".into()) } else { None },
            machine_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This PC".into()),
        });
        let backend: Arc<dyn SessionBackend> = Arc::new(local.clone());
        let smoke_state = smoke.then(|| Box::new(smoke::ShellSmoke::new(start, local.clone())));

        let bounds = Bounds::centered(None, size(px(1000.0), px(680.0)), cx);
        let background = if smoke {
            WindowBackgroundAppearance::Opaque
        } else {
            match cfg.backdrop {
                Backdrop::Mica => WindowBackgroundAppearance::MicaBackdrop,
                Backdrop::MicaAlt => WindowBackgroundAppearance::MicaAltBackdrop,
                Backdrop::Blur => WindowBackgroundAppearance::Blurred,
                Backdrop::None => WindowBackgroundAppearance::Opaque,
            }
        };
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Forgetty".into()),
                    appears_transparent: !(smoke && smoke::ab() == 1),
                    traffic_light_position: None,
                }),
                window_background: background,
                window_min_size: Some(size(px(480.0), px(300.0))),
                // The smoke measures key-to-frame latency: no inactive-window frame cap on CI.
                inactive_frame_interval: if smoke && smoke::ab() != 4 { None } else { Some(std::time::Duration::from_micros(33_333)) },
                ..Default::default()
            },
            move |window, cx| {
                let shell = cx.new(|cx| AppShell::new(cfg, backend, smoke_state, window, cx));
                shell.update(cx, |_, cx| {
                    cx.observe_window_activation(window, |s, window, cx| s.on_window_activation(window, cx))
                        .detach();
                });
                shell
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

#[cfg(windows)]
fn windows_build() -> u32 {
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd: [u16; 128],
    }
    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
    }
    let mut v = OsVersionInfo { size: std::mem::size_of::<OsVersionInfo>() as u32, major: 0, minor: 0, build: 0, platform: 0, csd: [0; 128] };
    // SAFETY: plain out-struct with its size field set.
    if unsafe { RtlGetVersion(&mut v) } == 0 {
        v.build
    } else {
        0
    }
}

#[cfg(not(windows))]
fn windows_build() -> u32 {
    0
}
