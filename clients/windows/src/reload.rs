//! Config hot reload: the file watcher feeds `on_reload`, which applies what can change live
//! (font, colors, bell, notification mode, keys, profiles, paste rules, shell for new tabs,
//! backdrop, padding) and says what it did. A file that does not parse changes nothing.
//! Also "Open configuration file", "Reload configuration" and the theme list.
//!
//! The file is read and parsed on the watcher's worker thread (`forgetty_shell_core::confwatch`).
//! This side only applies the result and costs nothing while the file is quiet.

use forgetty_shell_core::config;
use forgetty_shell_core::confwatch::{self, Reload};
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

/// The shell for a plain new tab: `shell` in the file, else FORGETTY_SHELL, else COMSPEC.
pub fn default_shell(cfg: &Config) -> Vec<String> {
    let is_file = |p: &str| std::path::Path::new(p).is_file();
    cfg.shell_argv(is_file)
        .or_else(|| std::env::var("FORGETTY_SHELL").ok().map(|s| vec![s]))
        .or_else(|| std::env::var("COMSPEC").ok().map(|s| vec![s]))
        .unwrap_or_else(|| vec!["cmd.exe".into()])
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
        match confwatch::watch(path) {
            Ok(w) => {
                let rx = w.rx.clone();
                self.conf_watch = Some(w);
                cx.spawn(async move |this, cx| {
                    while let Ok(r) = rx.recv().await {
                        if this.update(cx, |s, cx| s.on_reload(r, cx)).is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(e) => eprintln!("forgetty-win: config is not watched: {e}"),
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
        if let Some(t) = self.theme_override.as_deref().and_then(config::theme_colors) {
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

    /// A theme chosen in the palette: for this session. The file keeps its own `theme`.
    pub fn pick_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(colors) = config::theme_colors(name) else {
            self.notice_active(format!("Theme {name} not found"), cx);
            return;
        };
        self.theme_override = Some(name.to_string());
        let mut effective = self.file_cfg.clone().with_colors(colors);
        effective.theme_name = Some(name.to_string());
        let ch = config::diff(&self.cfg, &effective);
        self.cfg = effective;
        self.theme = Theme::from_cfg(&self.cfg);
        let cfg = self.cfg.clone();
        let views = self.views.values().cloned().chain(self.parked.values().flat_map(|s| s.views.values().cloned()));
        for v in views {
            let (c, ch) = (cfg.clone(), ch.clone());
            v.update(cx, |v, cx| v.apply_config(c, &ch, cx));
        }
        self.notice_active(format!("Theme {name} for this session. Set theme = \"{name}\" in config.toml to keep it."), cx);
        cx.notify();
    }

    /// Open `config.toml` in the editor Windows has for it, creating a commented file first.
    pub fn open_config_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = crate::config::config_path() else {
            self.notice_active("No configuration folder (APPDATA is not set)".into(), cx);
            return;
        };
        match crate::config::open_in_editor(&path) {
            Ok(()) => {}
            Err(e) => self.notice_active(format!("Cannot open {}: {e}", path.display()), cx),
        }
    }
}
