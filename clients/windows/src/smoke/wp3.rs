//! WP3 (config) in the `--smoke` gate, on the local ConPTY backend: the config file is written
//! and the running app must follow it (the watcher is on the smoke's own file, never the user's).
//! Font, theme by name, bell, notification mode, paste rules, profiles with arguments and a start
//! directory, the shell for a plain tab, the new-tab menu, zoom, the theme list, a bad file and a
//! deleted file (both keep the last good settings and say so).
//!
//! One state machine, stepped per painted frame, like WP2.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use forgetty_shell_core::config::{theme_colors, NotificationMode};
use forgetty_shell_core::keys::Action;
use forgetty_shell_core::menu::{Item, MenuCmd};
use forgetty_shell_core::{PaneId, Uuid};
use gpui::{Context, Window};
use serde_json::json;

use super::{check, fail, key, record, snap, typed, ShellSmoke};
use crate::config::Config;
use crate::shell::AppShell;

#[derive(Default)]
pub struct Wp3 {
    path: PathBuf,
    tabs0: usize,
    cols0: usize,
    cell0: f32,
    pane0: Option<PaneId>,
    new_tabs: Vec<Uuid>,
    toasts0: usize,
}

/// The first file: every key this work package reads, one typo, two profiles.
const F1: &str = r#"
font_size = 14
bell_mode = "both"
notification_mode = "ringonly"
paste_warn_newline = false
paste_warn_size = 100
theme = "Dracula"
default_profile = "Echo"
bell_mdoe = "none"

[[profiles]]
name = "Echo"
command = "cmd.exe /k echo profile-ok"
directory = "C:\\"

[[profiles]]
name = "Plain"
command = "cmd.exe"
"#;

/// Programs that are not there or not a full path, a network start folder, a folder that is not
/// there. Two profiles survive; the shell is refused.
const HARD: &str = r#"
shell = "ghost-prog.exe"
default_profile = "Share"

[[profiles]]
name = "Ghost"
command = "ghost-prog.exe"

[[profiles]]
name = "Relative"
command = ".\\cmd.exe"

[[profiles]]
name = "Share"
command = "cmd.exe"
directory = '\\localhost\c$'

[[profiles]]
name = "Missing"
command = "cmd.exe"
directory = "C:\\forgetty-no-such-folder"
"#;

fn custom_theme(bg: &str) -> String {
    format!("name = \"Smoke Custom\"\n[colors]\nforeground = \"#eeeeee\"\nbackground = \"{bg}\"\n")
}

fn with_mode(mode: &str) -> String {
    F1.replace("ringonly", mode).replace("font_size = 14", "font_size = 13")
}

/// The defaults, with the backdrop this machine allows (Windows before 11 22H2 has no Mica).
fn is_default(cfg: &Config) -> bool {
    let mut d = Config::default();
    d.backdrop = cfg.backdrop;
    *cfg == d
}

fn rows_have(shell: &AppShell, cx: &gpui::App, pane: PaneId, text: &str) -> bool {
    snap(shell, cx, pane).is_some_and(|s| s.rows.iter().any(|r| r.contains(text)))
}

fn menu_has(s: &AppShell, label: &str) -> bool {
    s.menu.as_ref().is_some_and(|o| o.menu.items.iter().any(|i| matches!(i, Item::Cmd { label: l, enabled: true, .. } if l == label)))
}

impl ShellSmoke {
    fn wp3_next(&mut self) {
        self.sub += 1;
        self.sub_t0 = Instant::now();
    }

    fn wp3_put(&self, text: &str) {
        if let Err(e) = std::fs::write(&self.wp3.path, text) {
            fail(&format!("wp3: cannot write the config file: {e}"));
        }
    }

    pub(super) fn wp3(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>) {
        if self.sub_t0.elapsed() > Duration::from_secs(12) {
            record("wp3_stuck_at", json!(self.sub));
            record(
                "wp3_state",
                json!(format!(
                    "font={} notice={:?} tabs={} bell={:?} mode={:?} profiles={} theme={:?}",
                    shell.cfg.font_size_pt,
                    shell.active_pane().and_then(|p| shell.views.get(&p)).and_then(|v| v.read(cx).notice_text()),
                    shell.layout.tab_count(),
                    shell.cfg.bell_mode,
                    shell.cfg.notification_mode,
                    shell.cfg.profiles.len(),
                    shell.cfg.theme_name
                )),
            );
            fail(&format!("wp3 checks stuck at step {}", self.sub));
        }
        macro_rules! until {
            ($c:expr) => {
                if !$c {
                    return;
                }
            };
        }
        let notice = |shell: &AppShell, cx: &gpui::App| -> String {
            shell.active_pane().and_then(|p| shell.views.get(&p)).and_then(|v| v.read(cx).notice_text()).unwrap_or_default()
        };
        match self.sub {
            // ---- the file is followed ----------------------------------------------------
            0 => {
                let Some(path) = crate::config::config_path() else { fail("wp3: no config path") };
                self.wp3.path = path;
                self.wp3.tabs0 = shell.layout.tab_count();
                let Some(pane) = shell.active_pane() else { fail("wp3: no active pane") };
                self.wp3.pane0 = Some(pane);
                let v = shell.views.get(&pane).unwrap().read(cx);
                self.wp3.cols0 = v.cols;
                self.wp3.cell0 = f32::from(v.metrics.cell_w);
                check("config_watch_started", shell.conf_watch.is_some());
                check("config_starts_with_the_defaults", is_default(&shell.cfg));
                self.wp3_put(F1);
                self.wp3_next();
            }
            1 => {
                until!(shell.cfg.font_size_pt == 14.0 && shell.cfg.profiles.len() == 2);
                let pane = self.wp3.pane0.unwrap();
                let v = shell.views.get(&pane).unwrap().read(cx);
                check("reload_applies_font_to_live_panes", v.font_pt() == 14.0 && f32::from(v.metrics.cell_w) > self.wp3.cell0);
                check("reload_applies_bell_mode", shell.cfg.bell_mode == forgetty_shell_core::bell::BellMode::Both && v.cfg.bell_mode == shell.cfg.bell_mode);
                check("reload_applies_notification_mode", shell.cfg.notification_mode == NotificationMode::RingOnly);
                check("reload_applies_paste_rules", !v.cfg.paste_warn_newline && v.cfg.paste_warn_size == 100);
                let dracula = theme_colors("Dracula");
                check(
                    "reload_applies_theme_by_name",
                    shell.cfg.theme_name.as_deref() == Some("Dracula")
                        && dracula.is_some_and(|t| shell.cfg.background == t.background && shell.cfg.ansi == t.ansi)
                        && shell.theme.bg == shell.cfg.background
                        && v.cfg.ansi == shell.cfg.ansi,
                );
                let n = notice(shell, cx);
                record("wp3_reload_notice", json!(n));
                check("reload_says_what_changed", n.starts_with("Configuration reloaded:") && n.contains("font"));
                check("reload_warns_about_a_typo_with_a_suggestion", n.contains("bell_mdoe") && n.contains("bell_mode"));
                let cmd = crate::reload::smoke_cmd();
                check(
                    "profiles_loaded_with_arguments_and_the_program_found_on_path",
                    shell.cfg.profiles[0].argv == [cmd.as_str(), "/k", "echo", "profile-ok"] && cmd.contains(":\\"),
                );
                check(
                    "profile_directory_is_checked_when_the_file_is_read",
                    shell.cfg.profiles[0].start_dir.as_deref() == Some("C:\\") && shell.cfg.profiles[0].dir_problem.is_none(),
                );
                self.wp3_next();
            }
            2 => {
                let pane = self.wp3.pane0.unwrap();
                until!(shell.views.get(&pane).is_some_and(|v| v.read(cx).cols < self.wp3.cols0));
                check("bigger_font_gives_fewer_columns", true);
                // ---- zoom ---------------------------------------------------------------
                let font = |shell: &AppShell, cx: &gpui::App| shell.views.get(&pane).map(|v| v.read(cx).font_pt()).unwrap_or(0.0);
                self.press(shell, window, cx, "ctrl+=");
                check("zoom_in_chord", font(shell, cx) == 15.0);
                self.press(shell, window, cx, "ctrl+-");
                self.press(shell, window, cx, "ctrl+-");
                check("zoom_out_chord", font(shell, cx) == 13.0);
                self.press(shell, window, cx, "ctrl+0");
                check("zoom_reset_chord_returns_to_the_configured_size", font(shell, cx) == 14.0 && shell.views.get(&pane).unwrap().read(cx).zoom_pt.is_none());
                for _ in 0..80 {
                    shell.run_action(Action::ZoomIn, window, cx);
                }
                check("zoom_stops_at_72", font(shell, cx) == 72.0);
                for _ in 0..80 {
                    shell.run_action(Action::ZoomOut, window, cx);
                }
                check("zoom_stops_at_6", font(shell, cx) == 6.0);
                shell.run_action(Action::ZoomReset, window, cx);
                shell.run_action(Action::ZoomIn, window, cx);
                check("zoom_before_a_font_reload", font(shell, cx) == 15.0);
                // A new font in the file drops the zoom.
                self.wp3_put(&with_mode("ringonly"));
                self.wp3_next();
            }
            3 => {
                let pane = self.wp3.pane0.unwrap();
                until!(shell.cfg.font_size_pt == 13.0);
                let v = shell.views.get(&pane).unwrap().read(cx);
                check("font_reload_drops_the_zoom", v.zoom_pt.is_none() && v.font_pt() == 13.0);
                // ---- profiles -----------------------------------------------------------
                self.press(shell, window, cx, "ctrl+shift+1");
                self.wp3_next();
            }
            4 => {
                until!(shell.layout.tab_count() == self.wp3.tabs0 + 1);
                let (Some(tab), Some(pane)) = (shell.active_tab().map(|t| t.id), shell.active_pane()) else { return };
                self.wp3.new_tabs.push(tab);
                // `cmd.exe /k echo profile-ok`, started in C:\ : the echo and the prompt show both.
                until!(rows_have(shell, cx, pane, "profile-ok") && snap(shell, cx, pane).is_some_and(|s| s.rows.iter().any(|r| r.trim_end() == "C:\\>")));
                check("profile_chord_starts_the_program_with_its_arguments", true);
                check("profile_starts_in_its_directory", true);
                // A plain new tab is the default profile ("Echo").
                shell.new_tab(cx);
                self.wp3_next();
            }
            5 => {
                until!(shell.layout.tab_count() == self.wp3.tabs0 + 2);
                let (Some(tab), Some(pane)) = (shell.active_tab().map(|t| t.id), shell.active_pane()) else { return };
                until!(rows_have(shell, cx, pane, "profile-ok"));
                self.wp3.new_tabs.push(tab);
                check("plain_new_tab_uses_the_default_profile", true);
                // The new-tab menu lists the profiles, a plain tab and the splits.
                shell.open_new_tab_menu(false, cx);
                check(
                    "new_tab_menu_lists_profiles_new_tab_and_splits",
                    menu_has(shell, "Echo") && menu_has(shell, "Plain") && menu_has(shell, "New Tab") && menu_has(shell, "Split Right"),
                );
                check("new_tab_menu_offers_pairing", menu_has(shell, "Pair machine..."));
                shell.run_menu_cmd(MenuCmd::PairMachine, window, cx);
                check("new_tab_menu_pairing_opens_the_dialog", shell.pair.is_some() && shell.menu.is_none());
                shell.pair = None;
                shell.open_new_tab_menu(false, cx);
                shell.run_menu_cmd(MenuCmd::Profile(2), window, cx);
                self.wp3_next();
            }
            6 => {
                until!(shell.layout.tab_count() == self.wp3.tabs0 + 3);
                if let Some(t) = shell.active_tab().map(|t| t.id) {
                    self.wp3.new_tabs.push(t);
                }
                check("new_tab_menu_opens_a_profile", shell.menu.is_none());
                // ---- notification mode: a note from a pane nobody looks at ------------------------
                self.wp3.toasts0 = shell.toast_log.len();
                shell.attention.clear();
                let pane = self.wp3.pane0.unwrap();
                self.local.inject_osc(pane, b"\x1b]9;ring only\x07");
                self.wp3_next();
            }
            7 => {
                let pane = self.wp3.pane0.unwrap();
                until!(shell.attention.contains(&pane) && self.sub_t0.elapsed() > Duration::from_millis(400));
                check("ringonly_rings_the_pane", true);
                check("ringonly_raises_no_toast", shell.toast_log.len() == self.wp3.toasts0);
                shell.attention.clear();
                self.wp3_put(&with_mode("none"));
                self.wp3_next();
            }
            8 => {
                until!(shell.cfg.notification_mode == NotificationMode::None && self.sub_t0.elapsed() > Duration::from_millis(700));
                let pane = self.wp3.pane0.unwrap();
                self.local.inject_osc(pane, b"\x1b]9;nothing\x07");
                self.wp3_next();
            }
            9 => {
                until!(self.sub_t0.elapsed() > Duration::from_millis(500));
                check("mode_none_has_no_ring_and_no_toast", shell.attention.is_empty() && shell.toast_log.len() == self.wp3.toasts0);
                self.wp3_put(&with_mode("all"));
                self.wp3_next();
            }
            10 => {
                until!(shell.cfg.notification_mode == NotificationMode::All && self.sub_t0.elapsed() > Duration::from_millis(700));
                let pane = self.wp3.pane0.unwrap();
                // Earlier phases may have used up the toast rate limit.
                shell.toast_limiter = Default::default();
                self.local.inject_osc(pane, b"\x1b]9;everything\x07");
                self.wp3_next();
            }
            11 => {
                let pane = self.wp3.pane0.unwrap();
                until!(shell.toast_log.len() > self.wp3.toasts0);
                check("mode_all_toasts_and_rings", shell.attention.contains(&pane));
                // ---- paste rules --------------------------------------------------------------
                let active = shell.active_pane().unwrap();
                let view = shell.views.get(&active).cloned().unwrap();
                view.update(cx, |v, _| v.capture_input = Some(Vec::new()));
                let multi = view.update(cx, |v, _| v.send_paste("one\ntwo"));
                check("paste_newline_warning_can_be_turned_off", !multi);
                let big = view.update(cx, |v, _| v.send_paste(&"x".repeat(101)));
                view.update(cx, |v, _| v.resolve_paste(false));
                check("paste_warn_size_follows_the_file", big);
                self.wp3_put(&with_mode("all").replace("paste_warn_newline = false", "paste_warn_newline = true"));
                self.wp3_next();
            }
            12 => {
                until!(shell.cfg.paste_warn_newline);
                let active = shell.active_pane().unwrap();
                let view = shell.views.get(&active).cloned().unwrap();
                let multi = view.update(cx, |v, _| v.send_paste("one\ntwo"));
                view.update(cx, |v, _| v.resolve_paste(false));
                check("paste_newline_warning_comes_back_on_reload", multi);
                view.update(cx, |v, _| v.capture_input = None);
                // ---- a bad file keeps the last good settings ------------------------------------
                self.wp3_put("bell_mode = \"loud\"\n");
                self.wp3_next();
            }
            13 => {
                let n = notice(shell, cx);
                until!(n.starts_with("Configuration not applied"));
                check("bad_file_keeps_the_last_good_settings", shell.cfg.font_size_pt == 13.0 && shell.cfg.profiles.len() == 2);
                record("wp3_bad_file_notice", json!(n));
                check("bad_file_names_the_problem", n.contains("loud") || n.contains("bell_mode"));
                self.wp3_put("font_size = [\n");
                self.wp3_next();
            }
            14 => {
                until!(notice(shell, cx).contains("TOML parse error") && self.sub_t0.elapsed() > Duration::from_millis(300));
                check("syntax_error_keeps_the_last_good_settings", shell.cfg.font_size_pt == 13.0);
                // ---- a deleted file keeps the last good settings --------------------------------
                if let Err(e) = std::fs::remove_file(&self.wp3.path) {
                    fail(&format!("wp3: cannot remove the config file: {e}"));
                }
                self.wp3_next();
            }
            15 => {
                until!(notice(shell, cx).contains("is gone"));
                check("deleted_file_keeps_the_last_good_settings", shell.cfg.font_size_pt == 13.0 && shell.cfg.profiles.len() == 2);
                // ---- the shell for a plain tab, with arguments ------------------------------------
                self.wp3_put("shell = \"cmd.exe /k echo shell-ok\"\n");
                self.wp3_next();
            }
            16 => {
                let cmd = crate::reload::smoke_cmd();
                let want = [cmd.as_str(), "/k", "echo", "shell-ok"];
                until!(self.local.default_shell() == want && shell.cfg.profiles.is_empty());
                check("reload_changes_the_shell_for_new_tabs", true);
                shell.new_tab(cx);
                self.wp3_next();
            }
            17 => {
                until!(shell.layout.tab_count() == self.wp3.tabs0 + 4);
                let (Some(tab), Some(pane)) = (shell.active_tab().map(|t| t.id), shell.active_pane()) else { return };
                until!(rows_have(shell, cx, pane, "shell-ok"));
                self.wp3.new_tabs.push(tab);
                check("shell_with_arguments_starts_a_plain_tab", true);
                // ---- the theme list ---------------------------------------------------------------
                shell.open_theme_list(cx);
                check("theme_list_has_the_catalog", shell.palette.as_ref().is_some_and(|p| p.themes.is_some() && p.items.len() > 400));
                for c in "dracula".chars() {
                    shell.handle_key(&typed(c), window, cx);
                }
                let first = shell.palette.as_ref().and_then(|p| p.items.first()).map(|i| i.title.clone()).unwrap_or_default();
                record("wp3_theme_first", json!(first));
                check("theme_list_filters_by_name", first.to_lowercase().starts_with("dracula"));
                shell.handle_key(&key("enter"), window, cx);
                let colors = theme_colors(&first);
                check(
                    "choosing_a_theme_applies_it_to_the_session",
                    shell.palette.is_none() && shell.theme_override.as_deref() == Some(first.as_str()) && colors.is_some_and(|t| shell.cfg.background == t.background),
                );
                self.wp3_put("theme = \"Nord\"\nshell = \"cmd.exe\"\n");
                self.wp3_next();
            }
            18 => {
                until!(shell.cfg.theme_name.as_deref() == Some("Nord"));
                check("file_colors_replace_a_picked_theme", shell.theme_override.is_none());
                check("shell_program_is_found_on_path", shell.cfg.shell_argv.as_ref().is_some_and(|a| a.len() == 1 && a[0].contains(":\\")));
                // ---- programs that cannot be run, and a network folder ------------------------------
                self.wp3_put(HARD);
                self.wp3_next();
            }
            19 => {
                until!(shell.cfg.profiles.len() == 2 && notice(shell, cx).contains("not on PATH"));
                check("shell_not_on_path_is_refused_with_a_notice", shell.cfg.shell.is_none() && shell.cfg.shell_argv.is_none());
                let names: Vec<&str> = shell.cfg.profiles.iter().map(|p| p.name.as_str()).collect();
                check("profiles_with_a_missing_or_relative_program_are_skipped", names == ["Share", "Missing"]);
                let (share, gone) = (&shell.cfg.profiles[0], &shell.cfg.profiles[1]);
                check(
                    "network_folder_is_refused_before_any_stat",
                    share.start_dir.is_none() && share.dir_problem.as_deref().is_some_and(|p| p.contains("network")),
                );
                check("missing_folder_is_reported", gone.start_dir.is_none() && gone.dir_problem.as_deref().is_some_and(|p| p.contains("does not exist")));
                check("fallback_shell_is_used_when_the_shell_is_refused", !self.local.default_shell().iter().any(|a| a.contains("ghost")));
                // The default profile is "Share": a plain tab says why its folder is not used.
                shell.new_tab(cx);
                // The notice goes to the pane that is active now, before the new tab takes over.
                let n = notice(shell, cx);
                record("wp3_network_dir_notice", json!(n));
                check("profile_with_a_network_folder_says_so_and_starts_in_the_home_folder", n.contains("network") && n.contains("Share"));
                self.wp3_next();
            }
            20 => {
                until!(shell.layout.tab_count() == self.wp3.tabs0 + 5);
                let (Some(tab), Some(pane)) = (shell.active_tab().map(|t| t.id), shell.active_pane()) else { return };
                self.wp3.new_tabs.push(tab);
                until!(rows_have(shell, cx, pane, "C:\\>"));
                check("profile_with_a_network_folder_still_starts", true);
                // ---- the themes folder next to the config file ------------------------------------------
                let dir = self.wp3.path.parent().map(|d| d.join("themes")).unwrap_or_default();
                if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join("smoke.toml"), custom_theme("#102030"))) {
                    fail(&format!("wp3: cannot write the custom theme: {e}"));
                }
                self.wp3_next();
            }
            21 => {
                until!(shell.themes.as_ref().is_some_and(|t| t.names.iter().any(|n| n == "Smoke Custom")));
                check("themes_folder_next_to_forgetty_config_is_watched", true);
                self.wp3_put("theme = \"Smoke Custom\"\n");
                self.wp3_next();
            }
            22 => {
                until!(shell.cfg.background == 0x102030 && shell.cfg.theme_name.as_deref() == Some("Smoke Custom"));
                check("custom_theme_loads_by_name", shell.theme.bg == 0x102030);
                let dir = self.wp3.path.parent().map(|d| d.join("themes")).unwrap_or_default();
                if let Err(e) = std::fs::write(dir.join("smoke.toml"), custom_theme("#203040")) {
                    fail(&format!("wp3: cannot write the custom theme: {e}"));
                }
                self.wp3_next();
            }
            23 => {
                until!(shell.cfg.background == 0x203040);
                check("editing_a_custom_theme_file_is_live", shell.theme.bg == 0x203040);
                // The theme list follows the folder, and a pick comes from memory.
                shell.pick_theme("Smoke Custom", cx);
                check("picking_a_custom_theme_uses_the_list", shell.theme_override.as_deref() == Some("Smoke Custom"));
                // ---- Open configuration file takes only a .toml path ---------------------------------
                let other = self.wp3.path.with_file_name("not-a-config.txt");
                std::env::set_var("FORGETTY_CONFIG", &other);
                shell.open_config_file(cx);
                let n = notice(shell, cx);
                std::env::set_var("FORGETTY_CONFIG", &self.wp3.path);
                record("wp3_open_refusal_notice", json!(n));
                check("open_configuration_refuses_a_path_that_is_not_toml", n.contains("not a .toml") && !other.exists());
                // ---- back to the defaults, tabs closed --------------------------------------------
                let dir = self.wp3.path.parent().map(|d| d.join("themes")).unwrap_or_default();
                let _ = std::fs::remove_file(dir.join("smoke.toml"));
                self.wp3_put("");
                self.wp3_next();
            }
            24 => {
                until!(is_default(&shell.cfg) && shell.themes.as_ref().is_some_and(|t| !t.names.iter().any(|n| n == "Smoke Custom")));
                check("empty_file_is_the_defaults_again", true);
                check("removed_custom_theme_leaves_the_list", true);
                for t in std::mem::take(&mut self.wp3.new_tabs) {
                    shell.close_tab(t, cx);
                }
                self.wp3_next();
            }
            25 => {
                until!(shell.layout.tab_count() == self.wp3.tabs0);
                check("wp3_tabs_closed", true);
                self.goto(super::Phase::RSetup);
            }
            _ => {}
        }
    }
}
