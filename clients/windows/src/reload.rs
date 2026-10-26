//! Config hot reload: the file watcher feeds `on_reload`, which applies what can change live
//! (font, colors, bell, notification mode, keys, profiles, paste rules, shell for new tabs,
//! backdrop, padding) and says what it did. A file that does not parse changes nothing.
//! Also "Open configuration file", "Reload configuration" and the theme list.
//!
//! The file is read and parsed on the watcher's worker thread (`forgetty_shell_core::confwatch`).
//! This side only applies the result and costs nothing while the file is quiet.

use std::sync::Arc;

use forgetty_shell_core::config::{self, ThemeIndex};
use forgetty_shell_core::confwatch::{self, Event, Reload};
use forgetty_shell_core::resolve::Host;
use forgetty_shell_core::keys::Keymap;
use gpui::{Context, WindowBackgroundAppearance};

use crate::config::{Backdrop, Config};
use crate::shell::{AppShell, Theme};
use crate::view::Metrics;

fn appearance(b: Backdrop) -> WindowBackgroundAppearance {
    match b {
        Backdrop::Mica => WindowBackgroundAppearance::MicaBackdrop,
        Backdrop::MicaAlt => WindowBackgroundAppearance::MicaAltBackdrop,
        Backdrop::Blur => WindowBackgroundAppearance::Blurred,
        Backdrop::None => WindowBackgroundAppearance::Opaque,
    }
}

/// The shell for a plain new tab: `shell` in the file (already resolved to a full path when the
/// file was read), else FORGETTY_SHELL, else COMSPEC, else `cmd.exe` of System32. No disk access
/// after the first call, which is at start.
pub fn default_shell(cfg: &Config) -> Vec<String> {
    cfg.shell_argv.clone().unwrap_or_else(|| fallback_shell().clone())
}

fn fallback_shell() -> &'static Vec<String> {
    static FALLBACK: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    FALLBACK.get_or_init(|| {
        // The same rules as `shell` in the file: a full path or a PATH lookup, no network path.
        let host = Host::real(None);
        for var in ["FORGETTY_SHELL", "COMSPEC"] {
            if let Ok(v) = std::env::var(var) {
                match config::command_argv(&v, &host) {
                    Ok(argv) => return argv,
                    Err(e) => eprintln!("forgetty-win: {var} is not used: {e}"),
                }
            }
        }
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        vec![format!("{}\\System32\\cmd.exe", root.trim_end_matches(['\\', '/']))]
    })
}

/// `cmd.exe` as the smoke starts it: the PATH lookup every configured program gets.
pub fn smoke_cmd() -> String {
    forgetty_shell_core::resolve::resolve_program("cmd.exe", &Host::real(None)).unwrap_or_else(|_| fallback_shell()[0].clone())
}

/// What to say at start about a config file that loaded with warnings.
pub fn startup_notice(warnings: &[String]) -> Option<String> {
    let first = warnings.first()?;
    let more = if warnings.len() > 1 { format!(" (+{} more)", warnings.len() - 1) } else { String::new() };
    Some(format!("config.toml: {first}{more}"))
}

impl AppShell {
    /// Watch the config file. Idle costs nothing: the operating system reports changes.
    pub fn start_config_watch(&mut self, cx: &mut Context<Self>) {
        let Some(path) = crate::config::config_path() else { return };
        let themes_dir = confwatch::themes_dir_for(&path);
        match confwatch::watch(path) {
            Ok(w) => {
                let rx = w.rx.clone();
                self.conf_watch = Some(w);
                cx.spawn(async move |this, cx| {
                    while let Ok(ev) = rx.recv().await {
                        if this.update(cx, |s, cx| s.on_event(ev, cx)).is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(e) => {
                eprintln!("forgetty-win: config is not watched: {e}");
                // The theme list still comes from a worker, just once.
                cx.spawn(async move |this, cx| {
                    let index = cx.background_executor().spawn(async move { Arc::new(ThemeIndex::build(&themes_dir)) }).await;
                    let _ = this.update(cx, |s, cx| s.on_themes(index, cx));
                })
                .detach();
            }
        }
    }

    /// Read the file again now (on the worker thread). The answer comes through `on_reload`.
    pub fn reload_config_now(&mut self, cx: &mut Context<Self>) {
        match self.conf_watch.as_ref() {
            Some(w) => {
                self.reload_asked = true;
                w.reload_now();
            }
            None => self.notice_active("The configuration file is not watched on this system".into(), cx),
        }
    }

    /// The worker sent something.
    pub fn on_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        match ev {
            Event::Config(r) => self.on_reload(r, cx),
            Event::Themes(index) => self.on_themes(index, cx),
        }
    }

    /// The theme list is new (at start, or the themes folder changed). A theme picked in the
    /// palette follows its file.
    fn on_themes(&mut self, index: Arc<ThemeIndex>, cx: &mut Context<Self>) {
        self.themes = Some(index);
        let picked = self.theme_override.clone();
        if let Some((name, colors)) = picked.and_then(|n| self.themes.as_ref().and_then(|t| t.colors(&n)).map(|c| (n, c))) {
            self.show_theme(&name, colors, cx);
        }
    }

    /// A read finished.
    pub fn on_reload(&mut self, r: Reload, cx: &mut Context<Self>) {
        match r {
            Reload::Failed(e) => {
                self.reload_asked = false;
                eprintln!("forgetty-win: config: {e}");
                self.notice_active(config::failed_notice(&e), cx);
            }
            Reload::Loaded(l) => self.apply_file_config(l.config, l.warnings, cx),
            // The file was deleted: what is in force stays, and the user is told.
            Reload::Missing => {
                self.reload_asked = false;
                self.notice_active(config::missing_notice(), cx);
            }
        }
    }

    /// Make `new_file` the config in force. Only what differs is touched.
    pub fn apply_file_config(&mut self, mut new_file: Config, mut warnings: Vec<String>, cx: &mut Context<Self>) {
        crate::fit_backdrop(&mut new_file);
        let asked = std::mem::take(&mut self.reload_asked);
        // A theme picked in the palette lasts until the file's own colors change.
        if config::diff(&self.file_cfg, &new_file).colors {
            self.theme_override = None;
        }
        let mut effective = new_file.clone();
        let picked = self.theme_override.as_deref().and_then(|n| self.themes.as_ref().and_then(|t| t.colors(n)));
        if let Some(t) = picked {
            effective = effective.with_colors(t);
        }
        let km = Keymap::new(&effective.keybindings);
        warnings.extend(km.warnings.iter().map(|w| format!("keybindings: {w}")));
        let ch = config::diff(&self.cfg, &effective);
        self.file_cfg = new_file;
        for w in &warnings {
            eprintln!("forgetty-win: config: {w}");
        }
        self.cfg = effective;
        self.keymap = km;
        if ch.colors || ch.backdrop {
            self.theme = Theme::from_cfg(&self.cfg);
        }
        if ch.font {
            self.metrics = Metrics::measure(cx, &self.cfg);
        }
        if ch.backdrop && !self.smoke_mode {
            self.mica = self.cfg.backdrop != Backdrop::None;
            let a = appearance(self.cfg.backdrop);
            let _ = self.win_handle.update(cx, |_, window, _| window.set_background_appearance(a));
        }
        if ch.shell {
            if let Some(local) = &self.local {
                local.set_default_shell(default_shell(&self.cfg));
            }
        }
        if ch.any() {
            let cfg = self.cfg.clone();
            let views = self.views.values().cloned().chain(self.parked.values().flat_map(|s| s.views.values().cloned()));
            for v in views {
                let (c, ch) = (cfg.clone(), ch.clone());
                v.update(cx, |v, cx| v.apply_config(c, &ch, cx));
            }
        }
        if let Some(t) = config::reload_notice(&ch, &warnings, asked) {
            self.notice_active(t, cx);
        }
        cx.notify();
    }

    /// A theme chosen in the palette: for this session. The file keeps its own `theme`. The colors
    /// come from the in-memory list, so a click reads no file.
    pub fn pick_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(index) = self.themes.clone() else {
            self.notice_active("The theme list is still loading".into(), cx);
            return;
        };
        let Some(colors) = index.colors(name) else {
            self.notice_active(format!("Theme {name} not found"), cx);
            return;
        };
        self.theme_override = Some(name.to_string());
        self.show_theme(name, colors, cx);
        self.notice_active(format!("Theme {name} for this session. Set theme = \"{name}\" in config.toml to keep it."), cx);
    }

    /// Make `name`'s colors the effective ones, and tell the panes.
    fn show_theme(&mut self, name: &str, colors: config::ThemeColors, cx: &mut Context<Self>) {
        let mut effective = self.file_cfg.clone().with_colors(colors);
        effective.theme_name = Some(name.to_string());
        let ch = config::diff(&self.cfg, &effective);
        if !ch.colors {
            return;
        }
        self.cfg = effective;
        self.theme = Theme::from_cfg(&self.cfg);
        let cfg = self.cfg.clone();
        let views = self.views.values().cloned().chain(self.parked.values().flat_map(|s| s.views.values().cloned()));
        for v in views {
            let (c, ch) = (cfg.clone(), ch.clone());
            v.update(cx, |v, cx| v.apply_config(c, &ch, cx));
        }
        cx.notify();
    }

    /// Open `config.toml` in the editor Windows has for it, creating a commented file first. The
    /// disk work runs on the background executor, not in the click handler.
    pub fn open_config_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = crate::config::config_path() else {
            self.notice_active("No configuration folder (APPDATA is not set)".into(), cx);
            return;
        };
        // The path is judged here, with no disk access, so the answer is immediate.
        if let Some(why) = config::open_refusal(&path) {
            self.notice_active(why, cx);
            return;
        }
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let result = cx.background_executor().spawn(async move { crate::config::open_in_editor(&p) }).await;
            if let Err(e) = result {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let text = if e.starts_with("Not opened") { e } else { format!("Cannot open {name}: {e}") };
                let _ = this.update(cx, |s, cx| s.notice_active(text, cx));
            }
        })
        .detach();
    }
}
