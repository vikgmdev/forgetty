//! Keymap presets in the `--smoke` gate: the config file switches between the `windows` and the
//! `gtk` chord sets while the app runs. Checks the chord tables the shell uses, the palette line,
//! Ctrl+V and Ctrl+C in the pane (GTK set only), and the GTK workspace chords on the real shell.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use forgetty_shell_core::keys::{Action, Chord, KeymapPreset};
use forgetty_shell_core::PaneId;
use gpui::{Context, KeyDownEvent, Window};
use serde_json::json;

use super::{check, fail, key, record, snap, ShellSmoke};
use crate::shell::AppShell;

#[derive(Default)]
pub struct KeymapPhase {
    path: PathBuf,
    ws0: usize,
}

fn rows_have(shell: &AppShell, cx: &gpui::App, pane: PaneId, text: &str) -> bool {
    snap(shell, cx, pane).is_some_and(|s| s.rows.iter().any(|r| r.contains(text)))
}

fn has(shell: &AppShell, ctrl: bool, alt: bool, shift: bool, k: &str) -> Option<Action> {
    shell.keymap.lookup(&Chord::new(ctrl, alt, shift, false, k))
}

/// A key as the window delivers it: the shell first, the pane when the shell did not take it.
/// Returns true when the shell took it.
fn pane_key(shell: &mut AppShell, pane: PaneId, chord: &str, window: &mut Window, cx: &mut Context<AppShell>) -> bool {
    if shell.handle_key(&key(chord), window, cx) {
        return true;
    }
    let Some(v) = shell.views.get(&pane).cloned() else { return false };
    let ev = KeyDownEvent { keystroke: key(chord), is_held: false, prefer_character_input: false };
    v.update(cx, |v, vcx| v.on_key_down(&ev, window, vcx));
    false
}

impl ShellSmoke {
    fn keymap_put(&self, text: &str) {
        if let Err(e) = std::fs::write(&self.keymap.path, text) {
            fail(&format!("keymap: cannot write the config file: {e}"));
        }
    }

    fn keymap_next(&mut self) {
        self.sub += 1;
        self.sub_t0 = Instant::now();
    }

    pub(super) fn keymap(&mut self, shell: &mut AppShell, window: &mut Window, cx: &mut Context<AppShell>) {
        if self.sub_t0.elapsed() > Duration::from_secs(15) {
            record("keymap_stuck_at", json!(self.sub));
            record("keymap_state", json!(format!("preset={:?} ws={} active={}", shell.cfg.keymap, shell.layout.workspaces.len(), shell.layout.active_workspace)));
            fail(&format!("keymap checks stuck at step {}", self.sub));
        }
        macro_rules! until {
            ($c:expr) => {
                if !$c {
                    return;
                }
            };
        }
        let Some(pane) = shell.active_pane() else { return };
        let notice = |shell: &AppShell, cx: &gpui::App| -> String {
            shell.views.get(&pane).and_then(|v| v.read(cx).notice_text()).unwrap_or_default()
        };
        match self.sub {
            0 => {
                let Some(path) = crate::config::config_path() else { fail("keymap: no config path") };
                self.keymap.path = path;
                self.keymap.ws0 = shell.layout.workspaces.len();
                self.keymap_put("keymap = \"windows\"\n");
                self.keymap_next();
            }
            // ---- the windows set -------------------------------------------------------------
            1 => {
                until!(shell.cfg.keymap == KeymapPreset::Windows);
                check(
                    "keymap_windows_set_in_force",
                    shell.keymap.preset() == KeymapPreset::Windows
                        && has(shell, true, true, false, "left") == Some(Action::FocusLeft)
                        && has(shell, false, true, false, "left").is_none()
                        && has(shell, true, false, true, "q") == Some(Action::CloseTab)
                        && has(shell, true, true, false, "n").is_none(),
                );
                check("keymap_palette_says_which_set", shell.palette_footer().contains("Windows shortcuts"));
                let n = notice(shell, cx);
                record("keymap_windows_notice", json!(n));
                check("keymap_reload_says_keys", n.contains("keys"));
                // Ctrl+V belongs to the program in this set.
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("WIN-NOPASTE".to_string()));
                let taken = pane_key(shell, pane, "ctrl+v", window, cx);
                check("keymap_windows_ctrl_v_is_not_a_shortcut", !taken);
                self.keymap_next();
            }
            2 => {
                until!(self.sub_t0.elapsed() > Duration::from_millis(700));
                check("keymap_windows_ctrl_v_goes_to_the_program", !rows_have(shell, cx, pane, "WIN-NOPASTE"));
                self.keymap_put("keymap = \"gtk\"\n");
                self.keymap_next();
            }
            // ---- the gtk set -----------------------------------------------------------------
            3 => {
                until!(shell.cfg.keymap == KeymapPreset::Gtk);
                check(
                    "keymap_gtk_set_in_force",
                    shell.keymap.preset() == KeymapPreset::Gtk
                        && has(shell, false, true, false, "left") == Some(Action::FocusLeft)
                        && has(shell, true, true, false, "left").is_none()
                        && has(shell, true, false, true, "q") == Some(Action::Quit)
                        && has(shell, true, true, false, "n") == Some(Action::NewWorkspace)
                        && has(shell, false, true, false, "2") == Some(Action::GotoWorkspace(2))
                        && has(shell, true, false, false, "2").is_none(),
                );
                check("keymap_palette_says_gtk", shell.palette_footer().contains("GTK shortcuts"));
                let live = shell.views.get(&pane).is_some_and(|v| v.read(cx).cfg.keymap == KeymapPreset::Gtk);
                check("keymap_reload_reaches_the_panes", live);
                let n = notice(shell, cx);
                record("keymap_gtk_notice", json!(n));
                check("keymap_gtk_reload_says_keys", n.contains("keys"));
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("gtk-paste-ok".to_string()));
                pane_key(shell, pane, "ctrl+v", window, cx);
                self.keymap_next();
            }
            4 => {
                until!(rows_have(shell, cx, pane, "gtk-paste-ok"));
                check("keymap_gtk_ctrl_v_pastes", true);
                // Escape clears the line the paste typed.
                if let Some(v) = shell.views.get(&pane).cloned() {
                    v.update(cx, |v, vcx| {
                        v.send_bytes(&[0x1b]);
                        v.select_all(vcx);
                    });
                }
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("x-before".to_string()));
                let taken = pane_key(shell, pane, "ctrl+c", window, cx);
                check("keymap_gtk_ctrl_c_is_taken_with_a_selection", taken);
                let cleared = shell.views.get(&pane).is_some_and(|v| v.read(cx).sel.is_none());
                check("keymap_gtk_ctrl_c_clears_the_selection", cleared);
                // The next Ctrl+C is the interrupt: the shell leaves it to the program.
                check("keymap_gtk_second_ctrl_c_is_the_interrupt", !shell.handle_key(&key("ctrl+c"), window, cx));
                // Alt+9 with no ninth workspace is the program's key.
                check("keymap_gtk_alt_digit_without_a_workspace_is_the_programs", shell.layout.workspaces.len() < 9 && !shell.handle_key(&key("alt+9"), window, cx));
                // Ctrl+Shift+Q asks first while a local shell is open; Enter says no.
                shell.request_quit(cx);
                check("keymap_quit_asks_with_a_local_shell", shell.confirm.as_ref().is_some_and(|c| c.text.contains("local shell")));
                shell.handle_key(&key("enter"), window, cx);
                check("keymap_quit_enter_cancels", shell.confirm.is_none());
                let clip = cx.read_from_clipboard().and_then(|i| i.text()).unwrap_or_default();
                record("keymap_ctrl_c_clip", json!(clip));
                check("keymap_gtk_ctrl_c_copies_a_selection", !clip.is_empty() && clip != "x-before");
                self.press(shell, window, cx, "ctrl+alt+n");
                self.keymap_next();
            }
            5 => {
                until!(shell.layout.workspaces.len() == self.keymap.ws0 + 1);
                check("keymap_gtk_new_workspace_chord", true);
                self.press(shell, window, cx, "alt+1");
                self.keymap_next();
            }
            6 => {
                until!(shell.layout.active_workspace == 0);
                check("keymap_gtk_alt_digit_goes_to_the_workspace", true);
                self.press(shell, window, cx, "ctrl+alt+pagedown");
                self.keymap_next();
            }
            7 => {
                until!(shell.layout.active_workspace == 1);
                check("keymap_gtk_next_workspace_chord", true);
                self.press(shell, window, cx, "alt+1");
                self.keymap_next();
            }
            8 => {
                until!(shell.layout.active_workspace == 0);
                shell.delete_workspace(1, cx);
                shell.confirm_yes(cx);
                self.keymap_next();
            }
            9 => {
                until!(shell.layout.workspaces.len() == self.keymap.ws0);
                check("keymap_workspace_removed", true);
                self.keymap_put("");
                self.goto(super::Phase::Settings);
            }
            _ => {}
        }
    }
}
