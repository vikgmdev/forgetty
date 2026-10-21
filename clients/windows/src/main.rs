//! forgetty-win: V3 Windows client. GPUI app shell (tabs, splits, sidebar,
//! palette) over a `SessionBackend`, terminal panes on libghostty-vt.
//! `--smoke` runs the headless CI gate.

mod chrome;
mod config;
mod element;
mod encoder;
mod lineedit;
mod machines;
mod screenshot;
mod shell;
mod smoke;
mod update;
mod view;
mod winsys;

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
            std::thread::sleep(smoke::total_timeout());
            smoke::fail("timeout");
        });
    }

    raise_timer_resolution(smoke);

    // Runtime for everything that talks to remote machines (iroh needs tokio).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("forgetty-net")
        .enable_all()
        .build()
        .expect("tokio runtime");
    let rt_handle = rt.handle().clone();
    // Where machines, the device key and the address cache live. The smoke uses
    // a fresh directory so it never touches the user's paired machines.
    let data_dir = if smoke {
        let d = std::env::temp_dir().join(format!("forgetty-smoke-{}", std::process::id()));
        std::env::set_var("FORGETTY_DATA_DIR", &d);
        Some(d)
    } else {
        forgetty_remote::store::MachineStore::default_dir()
    };

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
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                window_background: background,
                window_min_size: Some(size(px(480.0), px(300.0))),
                // The smoke measures key-to-frame latency: no inactive-window frame cap on CI.
                inactive_frame_interval: if smoke { None } else { Some(std::time::Duration::from_micros(33_333)) },
                ..Default::default()
            },
            move |window, cx| {
                let shell = cx.new(|cx| AppShell::new(cfg, backend, smoke_state, window, cx));
                shell.update(cx, |s, cx| {
                    cx.observe_window_activation(window, |s, window, cx| s.on_window_activation(window, cx))
                        .detach();
                    match data_dir {
                        Some(dir) => {
                            let store = forgetty_remote::store::MachineStore::open(dir);
                            let addr_cache = store.addr_cache();
                            s.init_remote(
                                machines::RemoteCtx {
                                    rt: rt_handle,
                                    store,
                                    device_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".into()),
                                    addr_cache,
                                },
                                winsys::start(),
                                cx,
                            );
                        }
                        None => eprintln!("forgetty-win: no %APPDATA%, remote machines are off"),
                    }
                });
                shell
            },
        );
        if let Err(e) = opened {
            fatal(smoke, &format!("open_window: {e:#}"));
        }
        cx.activate(true);
    });
    drop(rt);
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

/// Ask Windows for a 1 ms scheduler tick. The default 15.6 ms tick quantizes every
/// timed wake-up in the process (and, system-wide, other processes' timers such as
/// the ConPTY host's render pacing), which showed up as 30 ms echo latency.
fn raise_timer_resolution(smoke: bool) {
    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
    }
    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryTimerResolution(min: *mut u32, max: *mut u32, cur: *mut u32) -> i32;
    }
    let query = || {
        let (mut mn, mut mx, mut cur) = (0u32, 0u32, 0u32);
        // SAFETY: three valid out pointers.
        unsafe { NtQueryTimerResolution(&mut mn, &mut mx, &mut cur) };
        cur
    };
    let before = query();
    // SAFETY: plain winmm call; process-wide for the process lifetime.
    unsafe { timeBeginPeriod(1) };
    if smoke {
        smoke::record_pub("timer_before_100ns", serde_json::json!(before));
        smoke::record_pub("timer_after_100ns", serde_json::json!(query()));
    }
}
