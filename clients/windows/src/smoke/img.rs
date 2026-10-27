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
        match self.sub {
            0 => {
                // A pane of another machine takes no picture.
                view.update(cx, |v, _| v.image_paste = false);
                cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(gpui::ImageFormat::Png, png_2x2())));
                view.update(cx, |v, cx| v.paste(cx));
                check("image_paste_remote_pane_says_so", view.read(cx).notice_text().is_some_and(|t| t.contains("this PC only")));
                // A pane of this PC saves the file and pastes its path.
                view.update(cx, |v, _| v.image_paste = true);
                view.update(cx, |v, cx| v.paste(cx));
                self.sub = 1;
                self.sub_t0 = Instant::now();
            }
            _ => {
                let Some(s) = snap(shell, cx, p) else { return };
                if let Some(row) = s.rows.iter().find(|r| r.contains("paste-") && r.contains(".png")) {
                    record("image_paste_row", json!(row.trim()));
                    let dir = forgetty_shell_core::imagepaste::cache_dir(std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from).as_deref());
                    let newest = dir.as_ref().and_then(|d| std::fs::read_dir(d).ok()).and_then(|rd| {
                        rd.filter_map(Result::ok)
                            .filter(|e| e.file_name().to_string_lossy().starts_with("paste-"))
                            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
                    });
                    let ok = newest.as_ref().is_some_and(|e| std::fs::read(e.path()).is_ok_and(|b| b.starts_with(&[0x89, b'P', b'N', b'G'])));
                    check("image_paste_saves_a_png_and_pastes_its_path", ok);
                    if let Some(e) = newest {
                        let _ = std::fs::remove_file(e.path());
                    }
                    // Clear the line the path was typed on.
                    view.update(cx, |v, _| v.send_bytes(b"\x1b"));
                    self.goto(Phase::LsStart);
                } else if self.sub_t0.elapsed() > Duration::from_secs(15) {
                    fail("image paste: the path did not appear in the pane");
                }
            }
        }
    }
}
