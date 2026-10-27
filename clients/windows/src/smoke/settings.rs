//! The Settings window in the `--smoke` gate, on the real shell and a real `config.toml`:
//! rows, number entry, the keybinding editor (capture, conflict, reset), theme and font, a
//! `[theme]` table, a file that does not load (no save from the pages, repair in the TOML
//! editor), text the editor refuses, the shortcuts window (F1) and a render of every page.
//! Every change must come back through the live reload, and the comments and unknown keys of
//! the file must still be there.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use forgetty_shell_core::bell::BellMode;
use forgetty_shell_core::keys::{Action, Chord, KeymapPreset};
use forgetty_shell_core::settings::Page;
use forgetty_shell_core::settings_ui::{Body, View, Zone};
use gpui::{Context, Window};
use serde_json::json;

use super::{check, fail, key, record, typed, ShellSmoke};
use crate::shell::AppShell;

#[derive(Default)]
pub struct SettingsPhase {
    path: PathBuf,
    size0: f32,
    family0: String,
    pages: usize,
    page_t: Option<Instant>,
}

const START: &str = "# my smoke config\nbell_mode = \"visual\"  # how loud\nfuture_key = 1\n\n[keybindings]\n# keep this note\nsearch = \"ctrl+alt+s\"\n";

fn chord(s: &str) -> Chord {
    Chord::parse(s).expect("chord")
}

impl ShellSmoke {
    fn st_put(&self, text: &str) {
        if let Err(e) = std::fs::write(&self.settings.path, text) {
            fail(&format!("settings: cannot write the config file: {e}"));
        }
    }

    fn st_file(&self) -> String {
        std::fs::read_to_string(&self.settings.path).unwrap_or_default()
    }

    fn st_next(&mut self) {
        self.sub += 1;
        self.sub_t0 = Instant::now();
    }

    fn st_send(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>, chords: &[&str]) {
        for c in chords {
            // GPUI names the space bar "space" and types a blank with it.
            let ks = if *c == "space" { typed(' ') } else { key(c) };
            shell.handle_key(&ks, window, cx);
        }
    }

    fn st_type(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>, text: &str) {
        for ch in text.chars() {
            shell.handle_key(&typed(ch), window, cx);
        }
    }

    pub(super) fn settings(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>) {
        if self.sub_t0.elapsed() > Duration::from_secs(20) {
            record("settings_stuck_at", json!(self.sub));
            let status = shell.settings.as_ref().and_then(|w| w.st.status().map(|s| s.text.clone()));
            record("settings_state", json!(format!("open={} status={:?} file={:?}", shell.settings.is_some(), status, self.st_file())));
            fail(&format!("settings checks stuck at step {}", self.sub));
        }
        macro_rules! until {
            ($c:expr) => {
                if !$c {
                    return;
                }
            };
        }
        // A write is on its way: the window takes no keys until it is done, so neither do we.
        if shell.settings.as_ref().is_some_and(|w| w.st.busy) {
            return;
        }
        let settle = self.sub_t0.elapsed() > Duration::from_millis(150);
        match self.sub {
            0 => {
                let Some(path) = crate::config::config_path() else { fail("settings: no config path") };
                self.settings.path = path;
                self.settings.size0 = shell.file_cfg.font_size_pt;
                self.settings.family0 = shell.file_cfg.font_family.clone();
                self.st_put(START);
                self.st_next();
            }
            1 => {
                until!(shell.file_cfg.keybindings.contains_key("search"));
                check("settings_chord_opens_the_window", {
                    self.st_send(shell, window, cx, &["ctrl+."]);
                    shell.settings.is_some()
                });
                self.st_next();
            }
            // ---- a choice row: the shortcut set --------------------------------------------------
            2 => {
                until!(settle);
                self.st_send(shell, window, cx, &["down", "right"]);
                self.st_next();
            }
            3 => {
                until!(shell.file_cfg.keymap == KeymapPreset::Windows && shell.keymap.preset() == KeymapPreset::Windows);
                let f = self.st_file();
                check("settings_choice_reaches_the_file_and_reloads_live", f.contains("keymap = \"windows\""));
                check(
                    "settings_edit_keeps_comments_and_unknown_keys",
                    f.contains("# my smoke config") && f.contains("bell_mode = \"visual\"  # how loud") && f.contains("future_key = 1") && f.contains("# keep this note") && f.contains("search = \"ctrl+alt+s\""),
                );
                self.st_send(shell, window, cx, &["left"]);
                self.st_next();
            }
            4 => {
                until!(shell.file_cfg.keymap == KeymapPreset::Gtk && shell.keymap.preset() == KeymapPreset::Gtk);
                check("settings_choice_goes_back", true);
                shell.settings_page(Page::Terminal, cx);
                self.st_next();
            }
            // ---- a toggle and its default --------------------------------------------------------
            5 => {
                until!(settle);
                self.st_send(shell, window, cx, &["down", "down", "down", "space"]);
                self.st_next();
            }
            6 => {
                until!(!shell.file_cfg.smart_copy);
                check("settings_toggle_writes_the_key", self.st_file().contains("smart_copy = false"));
                self.st_send(shell, window, cx, &["delete"]);
                self.st_next();
            }
            7 => {
                until!(shell.file_cfg.smart_copy);
                check("settings_delete_removes_the_key_for_the_default", !self.st_file().contains("smart_copy"));
                // The number entry: scrollback, 10 MiB to 5 MiB.
                self.st_send(shell, window, cx, &["up", "up", "up", "enter", "backspace", "backspace"]);
                self.st_type(shell, window, cx, "5");
                self.st_send(shell, window, cx, &["enter"]);
                self.st_next();
            }
            8 => {
                until!(shell.file_cfg.scrollback_bytes == 5 * 1024 * 1024);
                check("settings_number_entry_saved_in_bytes", self.st_file().contains("scrollback_bytes = 5242880"));
                // A value out of range stays in the entry with the reason, and writes nothing.
                self.st_send(shell, window, cx, &["enter", "backspace"]);
                self.st_type(shell, window, cx, "9999");
                self.st_send(shell, window, cx, &["enter"]);
                let v: Option<View> = shell.settings_prepare();
                let err = v.and_then(|v| match v.body {
                    Body::Rows(r) => r.first().and_then(|x| x.entry.as_ref()).and_then(|e| e.error.clone()),
                    _ => None,
                });
                record("settings_entry_error", json!(err));
                check("settings_bad_number_is_refused_in_place", err.is_some_and(|e| e.contains("Between")));
                self.st_send(shell, window, cx, &["escape"]);
                check("settings_escape_closes_only_the_entry", shell.settings.is_some() && shell.file_cfg.scrollback_bytes == 5 * 1024 * 1024);
                shell.settings_page(Page::Keybindings, cx);
                self.st_next();
            }
            // ---- the keybinding editor ------------------------------------------------------------
            9 => {
                until!(settle);
                self.st_type(shell, window, cx, "sidebar");
                self.st_send(shell, window, cx, &["enter"]);
                let capturing = shell.settings_prepare().is_some_and(|v| matches!(v.body, Body::Keys(k) if k.capture.is_some()));
                check("settings_keybinding_capture_opens", capturing);
                // Ctrl+Alt with a letter is AltGr on many keyboards: refused with a message, the
                // capture stays open and nothing is written.
                self.st_send(shell, window, cx, &["ctrl+alt+h"]);
                let refused = shell.settings_prepare().is_some_and(|v| matches!(v.body, Body::Keys(k) if k.capture.as_ref().is_some_and(|(_, m)| m.contains("AltGr"))));
                check("settings_keybinding_refuses_altgr_chord", refused && !self.st_file().contains("ctrl+alt+h"));
                self.st_send(shell, window, cx, &["ctrl+shift+h"]);
                self.st_next();
            }
            10 => {
                until!(shell.keymap.lookup(&chord("ctrl+shift+h")) == Some(Action::ToggleSidebar));
                let f = self.st_file();
                check("settings_keybinding_saved_and_live", f.contains("toggle_sidebar = \"ctrl+shift+h\""));
                check("settings_keybinding_keeps_the_note_and_the_other_entry", f.contains("# keep this note") && f.contains("search = \"ctrl+alt+s\"") && f.contains("future_key = 1"));
                check("settings_old_chord_is_free", shell.keymap.lookup(&chord("ctrl+alt+b")).is_none());
                // A taken chord asks first.
                self.st_send(shell, window, cx, &["enter", "ctrl+shift+t"]);
                let prompt = shell.settings_prepare().and_then(|v| match v.body {
                    Body::Keys(k) => k.capture.map(|c| c.1),
                    _ => None,
                });
                record("settings_conflict_prompt", json!(prompt));
                check("settings_conflict_names_the_owner", prompt.is_some_and(|p| p.contains("New tab")));
                check("settings_conflict_writes_nothing_yet", !self.st_file().contains("new_tab"));
                self.st_send(shell, window, cx, &["enter"]);
                self.st_next();
            }
            11 => {
                until!(shell.keymap.lookup(&chord("ctrl+shift+t")) == Some(Action::ToggleSidebar));
                check("settings_conflict_moves_the_chord", shell.keymap.chords(Action::NewTab).is_empty());
                // Reset one: the entry goes, the default chord is back.
                self.st_send(shell, window, cx, &["ctrl+r"]);
                self.st_next();
            }
            12 => {
                until!(shell.keymap.lookup(&chord("ctrl+alt+b")) == Some(Action::ToggleSidebar));
                check("settings_reset_row_removes_the_entry", !self.st_file().contains("toggle_sidebar"));
                // Reset all.
                self.st_send(shell, window, cx, &["ctrl+shift+r"]);
                self.st_next();
            }
            13 => {
                until!(shell.keymap.lookup(&chord("ctrl+shift+t")) == Some(Action::NewTab));
                let f = self.st_file();
                check("settings_reset_all_removes_the_entries_and_keeps_the_rest", !f.contains("new_tab") && !f.contains("search =") && f.contains("future_key = 1") && f.contains("# my smoke config"));
                shell.settings_page(Page::Appearance, cx);
                self.st_next();
            }
            // ---- theme and font -----------------------------------------------------------------
            14 => {
                until!(settle);
                let look = shell.settings_prepare().and_then(|v| match v.body {
                    Body::Look(l) => Some(l),
                    _ => None,
                });
                let Some(look) = look else { fail("settings: the Appearance page has no look view") };
                record("settings_theme_count", json!(look.themes.total));
                record("settings_font_count", json!(look.fonts.total));
                check("settings_lists_themes_and_fonts", look.themes.total > 100 && look.fonts.total > 0);
                check("settings_preview_has_colors", look.preview_colors.is_some());
                self.st_send(shell, window, cx, &["down", "enter"]);
                self.st_next();
            }
            15 => {
                until!(shell.file_cfg.theme_name.is_some());
                let f = self.st_file();
                check("settings_theme_pick_writes_the_name", f.contains("theme = \""));
                check("settings_theme_pick_keeps_the_file", f.contains("# my smoke config") && f.contains("future_key = 1"));
                check("settings_theme_pick_changes_the_colors", shell.theme.bg == shell.cfg.background);
                // Size: Tab to the size, one step, apply.
                self.st_send(shell, window, cx, &["tab", "tab", "right", "enter"]);
                self.st_next();
            }
            16 => {
                until!((shell.file_cfg.font_size_pt - (self.settings.size0 + 0.5)).abs() < 0.01);
                check("settings_font_size_saved", self.st_file().contains("font_size = "));
                // Family: the first font on screen that is not in use.
                let pick = shell.settings_prepare().and_then(|v| match v.body {
                    Body::Look(l) => l.fonts.items.iter().find(|f| !f.2).map(|f| f.0.clone()),
                    _ => None,
                });
                record("settings_font_pick", json!(pick));
                let Some(name) = pick else { fail("settings: no other font to pick") };
                shell.settings_look_pick(Zone::Font, name, cx);
                self.st_send(shell, window, cx, &["enter"]);
                self.st_next();
            }
            17 => {
                until!(shell.file_cfg.font_family != self.settings.family0);
                check("settings_font_family_saved", self.st_file().contains("font_family = "));
                check("settings_font_reaches_the_panes", shell.cfg.font_family == shell.file_cfg.font_family);
                // Colors typed by hand in a [theme] table are not written over.
                self.st_put("# t\n[theme]\nbackground = \"#101010\"\nforeground = \"#eeeeee\"\n");
                self.st_next();
            }
            18 => {
                until!(self.sub_t0.elapsed() > Duration::from_millis(700));
                shell.settings_look(Zone::Theme, None, cx);
                self.st_send(shell, window, cx, &["enter"]);
                self.st_next();
            }
            19 => {
                let s = shell.settings.as_ref().and_then(|w| w.st.status().cloned());
                until!(s.is_some());
                let s = s.unwrap();
                record("settings_theme_table_status", json!(s.text));
                check("settings_theme_table_refuses_a_named_theme", !s.ok && s.text.contains("[theme]"));
                check("settings_theme_table_file_untouched", self.st_file().starts_with("# t\n[theme]\nbackground = \"#101010\""));
                // A file that does not load.
                self.st_put("bell_mode = \n");
                self.st_next();
            }
            // ---- a file that does not load ---------------------------------------------------------
            20 => {
                until!(self.sub_t0.elapsed() > Duration::from_millis(700));
                shell.settings_page(Page::General, cx);
                self.st_next();
            }
            21 => {
                until!(settle);
                self.st_send(shell, window, cx, &["down", "down", "down", "space"]);
                self.st_next();
            }
            22 => {
                let s = shell.settings.as_ref().and_then(|w| w.st.status().cloned());
                until!(s.is_some());
                let s = s.unwrap();
                record("settings_broken_status", json!(s.text));
                check("settings_never_saves_over_a_file_that_does_not_load", !s.ok && s.text.contains("does not load") && self.st_file() == "bell_mode = \n");
                shell.settings_page(Page::Advanced, cx);
                self.st_next();
            }
            23 => {
                let loaded = shell.settings_prepare().is_some_and(|v| matches!(v.body, Body::Toml(t) if t.loaded && t.lines.first().is_some_and(|l| l.1.starts_with("bell_mode"))));
                until!(loaded);
                check("settings_toml_editor_shows_the_broken_file", true);
                // Repair: the value after the equals sign.
                self.st_send(shell, window, cx, &["ctrl+end", "backspace"]);
                self.st_type(shell, window, cx, "\"both\"");
                self.st_send(shell, window, cx, &["ctrl+s"]);
                self.st_next();
            }
            24 => {
                until!(shell.file_cfg.bell_mode == BellMode::Both);
                check("settings_toml_editor_repairs_the_file", self.st_file().contains("bell_mode = \"both\""));
                check("settings_toml_editor_keeps_a_backup", self.settings.path.with_extension("toml.bak").exists());
                // Text that does not load is refused with the line.
                self.st_send(shell, window, cx, &["enter"]);
                self.st_type(shell, window, cx, "[[[");
                self.st_send(shell, window, cx, &["ctrl+s"]);
                self.st_next();
            }
            25 => {
                let msg = shell.settings_prepare().and_then(|v| match v.body {
                    Body::Toml(t) if !shell.settings.as_ref().is_some_and(|w| w.st.busy) => t.message,
                    _ => None,
                });
                until!(msg.is_some());
                record("settings_toml_refusal", json!(msg));
                check("settings_toml_editor_refuses_bad_text", msg.is_some_and(|m| m.contains("Not saved")) && !self.st_file().contains("[[["));
                // Esc asks before it throws the text away, the second Esc closes.
                self.st_send(shell, window, cx, &["escape"]);
                check("settings_unsaved_text_asks_before_closing", shell.settings.is_some());
                self.st_send(shell, window, cx, &["escape"]);
                check("settings_second_escape_closes", shell.settings.is_none());
                // The shortcuts window.
                self.st_send(shell, window, cx, &["f1"]);
                check("shortcuts_window_opens_on_f1", shell.shortcuts.is_some());
                self.st_send(shell, window, cx, &["escape"]);
                check("shortcuts_window_closes_on_escape", shell.shortcuts.is_none());
                shell.open_settings(None, cx);
                self.settings.pages = 0;
                self.settings.page_t = Some(Instant::now());
                self.st_next();
            }
            // ---- every page renders ---------------------------------------------------------------
            26 => {
                let due = self.settings.page_t.is_none_or(|t| t.elapsed() > Duration::from_millis(150));
                until!(due);
                if self.settings.pages < Page::ALL.len() {
                    let p = Page::ALL[self.settings.pages];
                    shell.settings_page(p, cx);
                    self.settings.pages += 1;
                    self.settings.page_t = Some(Instant::now());
                    return;
                }
                check("settings_every_page_renders", self.settings.pages == Page::ALL.len() && shell.settings.is_some());
                shell.close_settings(window, cx);
                self.st_put("");
                self.sub_t0 = Instant::now();
                self.goto(super::Phase::RSetup);
            }
            _ => {}
        }
    }
}
