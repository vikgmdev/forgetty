//! F8 in the `--smoke` gate: a picture and no text on the clipboard. Paste saves it in the cache
//! folder and puts the path into the pane. A pane of a remote machine refuses with a notice.

use std::time::{Duration, Instant};

use gpui::{ClipboardItem, Context, Window};
use serde_json::json;

use super::{check, fail, record, snap, Phase, ShellSmoke};
use crate::shell::AppShell;

fn png_2x2() -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, 2, 2);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().expect("png header");
    w.write_image_data(&[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]).expect("png data");
    drop(w);
    out
}

impl ShellSmoke {
    pub(super) fn img_step(&mut self, shell: &mut AppShell, _window: &mut Window, cx: &mut Context<AppShell>, _now_t: Instant) {
        let Some(p) = shell.active_pane() else { return };
        let Some(view) = shell.views.get(&p).cloned() else { return };
        // One paste: the pane is told which program it was started with (the quoting follows it,
        // never the prompt), the line is cleared, the picture pasted.
        let paste_as = |shell: &mut AppShell, cx: &mut Context<AppShell>, argv: Vec<String>| {
            shell.set_pane_argv(p, argv, cx);
            view.update(cx, |v, _| v.send_bytes(b"\x1b"));
            view.update(cx, |v, cx| v.paste(cx));
        };
        match self.sub {
            0 => {
                // A folder with a space in its name: the pasted path must be quoted for cmd.
                let dir = std::env::temp_dir().join(format!("forgetty smoke images {}", std::process::id()));
                let _ = std::fs::create_dir_all(&dir);
                self.img_prev_local = std::env::var_os("LOCALAPPDATA");
                std::env::set_var("LOCALAPPDATA", &dir);
                self.img_dir = Some(dir);
                cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(gpui::ImageFormat::Png, png_2x2())));
                // The pane of this PC saves the file and pastes its path. (A pane of another
                // machine refuses: that is checked on a real remote pane, in the remote phases.)
                view.update(cx, |v, _| v.image_paste = true);
                paste_as(shell, cx, vec![crate::reload::smoke_cmd()]);
                self.sub = 1;
                self.sub_t0 = Instant::now();
            }
            1 => {
                let Some(s) = snap(shell, cx, p) else { return };
                // The path is longer than a row can be: join the rows, the wrap adds no space.
                let all: String = s.rows.iter().map(|r| r.trim_end()).collect();
                if all.contains("paste-") && all.contains(".png") {
                    record("image_paste_screen", json!(all));
                    let files: Vec<std::path::PathBuf> = self
                        .img_dir
                        .as_ref()
                        .map(|d| d.join("forgetty").join("cache").join("clipboard"))
                        .and_then(|d| std::fs::read_dir(d).ok())
                        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("paste-"))).collect())
                        .unwrap_or_default();
                    let newest = files.first();
                    let ok = newest.is_some_and(|p| std::fs::read(p).is_ok_and(|b| b.starts_with(&[0x89, b'P', b'N', b'G'])));
                    check("image_paste_saves_a_png_and_pastes_its_path", ok && files.len() == 1);
                    // The path has a space, the program is cmd: it is in double quotes.
                    let quoted = all.contains("\"") && all.contains("forgetty smoke images") && all.contains(".png\"");
                    check("image_paste_quotes_a_path_with_a_space_for_cmd", quoted);
                    // The file is for this user only, like the identity key.
                    #[cfg(windows)]
                    let private = newest.is_some_and(|p| forgetty_core::fsutil::acl::is_user_only(p).unwrap_or(false));
                    #[cfg(not(windows))]
                    let private = newest.is_some();
                    check("image_paste_file_is_user_only", private);
                    // The same picture for a pane started with PowerShell (the line is cmd's, only the
                    // text matters).
                    paste_as(shell, cx, vec!["pwsh.exe".into()]);
                    self.sub = 2;
                    self.sub_t0 = Instant::now();
                } else if self.sub_t0.elapsed() > Duration::from_secs(15) {
                    let notice = view.read(cx).notice_text();
                    fail(&format!("image paste: the path did not appear in the pane (notice {notice:?}, screen {all:?})"));
                }
            }
            2 | 3 => {
                let Some(s) = snap(shell, cx, p) else { return };
                let all: String = s.rows.iter().map(|r| r.trim_end()).collect();
                let (marker, name, next): (&str, &str, Vec<String>) = if self.sub == 2 {
                    ("'C:\\", "image_paste_quotes_with_single_quotes_for_powershell", vec!["wsl.exe".into()])
                } else {
                    ("'/mnt/c/", "image_paste_writes_a_wsl_path_for_wsl", Vec::new())
                };
                if all.contains(marker) && all.contains(".png'") {
                    let ok = all.contains("forgetty smoke images") && !all.contains("\"C:\\");
                    check(name, ok);
                    if self.sub == 2 {
                        paste_as(shell, cx, next);
                        self.sub = 3;
                        self.sub_t0 = Instant::now();
                    } else {
                        match self.img_prev_local.take() {
                            Some(v) => std::env::set_var("LOCALAPPDATA", v),
                            None => std::env::remove_var("LOCALAPPDATA"),
                        }
                        if let Some(d) = self.img_dir.take() {
                            let _ = std::fs::remove_dir_all(d);
                        }
                        // The pane is a plain cmd again; clear the line the path was typed on.
                        shell.set_pane_argv(p, vec![crate::reload::smoke_cmd()], cx);
                        view.update(cx, |v, _| v.send_bytes(b"\x1b"));
                        self.goto(Phase::LsStart);
                    }
                } else if self.sub_t0.elapsed() > Duration::from_secs(15) {
                    let notice = view.read(cx).notice_text();
                    fail(&format!("image paste ({name}): the path did not appear (notice {notice:?}, screen {all:?})"));
                }
            }
            _ => {}
        }
    }

    /// On a real pane of a remote machine (the test server): a picture on the clipboard is refused
    /// with a notice and no file is saved.
    pub(super) fn img_remote_refusal(&self, pane_view: &gpui::Entity<crate::view::TerminalView>, cx: &mut Context<AppShell>) -> bool {
        let cache = forgetty_shell_core::imagepaste::cache_dir(std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from).as_deref());
        let count = |d: &Option<std::path::PathBuf>| d.as_ref().and_then(|d| std::fs::read_dir(d).ok()).map_or(0, |rd| rd.count());
        let before = count(&cache);
        cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(gpui::ImageFormat::Png, png_2x2())));
        let by_construction = !pane_view.read(cx).image_paste;
        pane_view.update(cx, |v, cx| v.paste(cx));
        let said = pane_view.read(cx).notice_text().is_some_and(|t| t.contains("this PC only"));
        by_construction && said && count(&cache) == before
    }
}
