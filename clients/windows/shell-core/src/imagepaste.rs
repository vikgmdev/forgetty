//! Pasting an image (F8): the clipboard holds a picture and no text, so the app saves it as a
//! file in the cache folder and pastes the path (a program such as an agent CLI reads the file).
//! This module holds the parts that need no window: the file name, the cache folder and the
//! conversion of a Windows bitmap (what a screenshot tool leaves as `CF_DIB`) to RGBA for the PNG
//! encoder. The same naming as the GTK app: `paste-YYYYMMDD-HHmmss-<8 hex>.png`.

use std::path::{Path, PathBuf};

/// A picture larger than this is refused (a PNG of it would also be slow to write).
pub const MAX_PIXELS: u64 = 64 * 1024 * 1024;
/// A clipboard payload larger than this is refused before it is decoded.
pub const MAX_BYTES: usize = 256 * 1024 * 1024;

/// `%LOCALAPPDATA%\forgetty\cache\clipboard`.
pub fn cache_dir(local_app_data: Option<&Path>) -> Option<PathBuf> {
    Some(local_app_data?.join("forgetty").join("cache").join("clipboard"))
}

/// Seconds since the epoch as `YYYYMMDD-HHmmss` (UTC). The file name only needs to be unique.
pub fn timestamp(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem = epoch_secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub fn file_name(epoch_secs: u64, id8: &str, ext: &str) -> String {
    let id: String = id8.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    let ext: String = ext.chars().filter(|c| c.is_ascii_alphanumeric()).take(5).collect();
    format!("paste-{}-{id}.{ext}", timestamp(epoch_secs))
}

/// The program a pane was started with, as far as quoting and writing a path is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Cmd,
    PowerShell,
    /// WSL (`wsl.exe`, a distro launcher): `/mnt/c/...`.
    Wsl,
    /// Git Bash and MSYS2: `/c/...`.
    GitBash,
    /// Cygwin: `/cygdrive/c/...`.
    Cygwin,
    /// A program that is not known as a shell.
    Unknown,
}

/// What to paste for a spawn program, from its argv (the program is `argv[0]`, a name or a full
/// path). The prompt text is never read: a program can print any prompt.
pub fn target_of(argv: &[String]) -> Target {
    let Some(prog) = argv.first() else { return Target::Unknown };
    let full = prog.replace('/', "\\").to_ascii_lowercase();
    let name = full.rsplit('\\').next().unwrap_or("");
    let name = name.strip_suffix(".exe").unwrap_or(name);
    match name {
        "cmd" => Target::Cmd,
        "pwsh" | "powershell" => Target::PowerShell,
        "wsl" | "wslg" => Target::Wsl,
        "bash" | "sh" | "zsh" | "fish" | "dash" => {
            if full.contains("\\cygwin") {
                Target::Cygwin
            } else if full.contains("\\git\\") || full.contains("\\msys") || full.contains("\\mingw") {
                Target::GitBash
            } else if full.ends_with("\\windows\\system32\\bash.exe") || full.ends_with("\\windows\\sysnative\\bash.exe") {
                Target::Wsl
            } else {
                Target::Unknown
            }
        }
        n if ["ubuntu", "debian", "kali", "opensuse", "suse", "sles", "archlinux", "fedora", "oraclelinux", "almalinux"].iter().any(|d| n.starts_with(d)) => {
            Target::Wsl
        }
        _ => Target::Unknown,
    }
}

/// `C:\Users\a` as `<prefix>/c/Users/a` (`prefix` is `/mnt`, `` or `/cygdrive`). Any other path is
/// only turned to forward slashes.
fn drive_path(p: &str, prefix: &str) -> String {
    let b = p.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        format!("{prefix}/{}/{}", (b[0] as char).to_ascii_lowercase(), p[3..].replace('\\', "/"))
    } else {
        p.replace('\\', "/")
    }
}

/// `C:\Users\a` as `/mnt/c/Users/a` (WSL).
pub fn wsl_path(p: &str) -> String {
    drive_path(p, "/mnt")
}

/// A path of these characters needs no quote in any shell here. Not the comma (cmd and PowerShell
/// split on it), not `%`, `$`, `^`, `&`, `;`, `(`, `)`, `~`, `#`, `[`, `{`.
fn plain(p: &str) -> bool {
    let b = p.as_bytes();
    let starts_ok = (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':') || p.starts_with('/');
    starts_ok && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '\\' | '/' | '-' | '+'))
}

/// The characters PowerShell reads as a single quote (a quote typed with a typographic key too).
const PS_QUOTES: [char; 5] = ['\'', '\u{2018}', '\u{2019}', '\u{201a}', '\u{201b}'];

/// `path` as one word for the program, in the form the program reads. A path of plain characters
/// stays as it is. Otherwise:
/// - cmd: double quotes (a Windows path holds none). A `%` cannot be quoted at the prompt: refused.
/// - PowerShell: single quotes, where `$` and the backtick mean nothing. Every quote character
///   (the typographic ones too) is doubled.
/// - WSL, Git Bash, Cygwin: the path in the form of that program, in single quotes (a quote is
///   written `'\''`).
/// - unknown: double quotes, when the path has no character that one of cmd, PowerShell or bash
///   would act on inside them; else refused.
///
/// `Err` is the reason, for a notice: nothing is pasted then.
pub fn quote_path(target: Target, path: &str) -> Result<String, String> {
    if path.chars().any(|c| c.is_control()) {
        return Err("the path has a control character".into());
    }
    let p = match target {
        Target::Wsl => drive_path(path, "/mnt"),
        Target::GitBash => drive_path(path, ""),
        Target::Cygwin => drive_path(path, "/cygdrive"),
        _ => path.to_string(),
    };
    if plain(&p) {
        return Ok(p);
    }
    match target {
        Target::Cmd => {
            // `%` is expanded inside quotes, `!` when delayed expansion is on (`cmd /v:on`).
            if p.contains('"') || p.contains('%') || p.contains('!') {
                return Err("the path has a quote, a percent sign or an exclamation mark, which cmd cannot take in one word".into());
            }
            Ok(format!("\"{p}\""))
        }
        Target::PowerShell => {
            let mut out = String::with_capacity(p.len() + 2);
            out.push('\'');
            for c in p.chars() {
                out.push(c);
                if PS_QUOTES.contains(&c) {
                    out.push(c);
                }
            }
            out.push('\'');
            Ok(out)
        }
        Target::Wsl | Target::GitBash | Target::Cygwin => Ok(format!("'{}'", p.replace('\'', "'\\''"))),
        Target::Unknown => {
            if p.chars().any(|c| matches!(c, '"' | '$' | '`' | '%' | '!' | '^')) {
                return Err("the program is not a known shell and the path has characters that a shell would act on".into());
            }
            Ok(format!("\"{p}\""))
        }
    }
}

/// What is pasted into the pane: the quoted path and a space (GTK pastes the path and a space).
pub fn paste_text(path: &Path, target: Target) -> Result<String, String> {
    Ok(format!("{} ", quote_path(target, &path.to_string_lossy())?))
}

#[derive(Debug)]
pub struct Rgba {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// A BMP file (14 byte file header, then the DIB header and the pixels) of 24 or 32 bits without
/// a palette. Other kinds are refused with a message.
pub fn bmp_to_rgba(b: &[u8]) -> Result<Rgba, String> {
    let bad = || "The picture on the clipboard is not in a format that can be saved.".to_string();
    if b.len() > MAX_BYTES || b.get(..2) != Some(b"BM") {
        return Err(bad());
    }
    let offset = le32(b, 10).ok_or_else(bad)? as usize;
    let header = le32(b, 14).ok_or_else(bad)? as usize;
    if header < 40 {
        return Err(bad());
    }
    let w = le32(b, 18).ok_or_else(bad)? as i32;
    let h = le32(b, 22).ok_or_else(bad)? as i32;
    let bpp = le16(b, 28).ok_or_else(bad)?;
    let compression = le32(b, 30).ok_or_else(bad)?;
    // 0 = BI_RGB, 3 = BI_BITFIELDS (the usual masks of a 32 bit screen capture: BGRA).
    if w <= 0 || h == 0 || !(bpp == 24 || bpp == 32) || !(compression == 0 || compression == 3 || compression == 6) {
        return Err(bad());
    }
    let top_down = h < 0;
    let (w, h) = (w as u32, h.unsigned_abs());
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err("The picture on the clipboard is too large to save.".into());
    }
    let bytes_per = usize::from(bpp / 8);
    let stride = (w as usize * bytes_per).div_ceil(4) * 4;
    let need = offset.checked_add(stride.checked_mul(h as usize).ok_or_else(bad)?).ok_or_else(bad)?;
    if b.len() < need {
        return Err(bad());
    }
    let mut data = Vec::with_capacity(w as usize * h as usize * 4);
    for row in 0..h as usize {
        let src_row = if top_down { row } else { h as usize - 1 - row };
        let line = &b[offset + src_row * stride..offset + src_row * stride + w as usize * bytes_per];
        for px in line.chunks_exact(bytes_per) {
            data.extend_from_slice(&[px[2], px[1], px[0], if bytes_per == 4 { px[3] } else { 255 }]);
        }
    }
    // A 32 bit bitmap without an alpha channel has zero in that byte everywhere.
    if bytes_per == 4 && data.chunks_exact(4).all(|p| p[3] == 0) {
        for p in data.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
    Ok(Rgba { w, h, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(0), "19700101-000000");
        assert_eq!(timestamp(951_782_400), "20000229-000000"); // leap day
        assert_eq!(timestamp(1_792_985_107), "20261026-032507");
        assert_eq!(timestamp(1_000_000_000), "20010909-014640");
    }

    #[test]
    fn names_are_safe() {
        assert_eq!(file_name(0, "ab12cd34ef", "png"), "paste-19700101-000000-ab12cd34.png");
        assert_eq!(file_name(0, "../..", "p/ng"), "paste-19700101-000000-.png");
    }

    #[test]
    fn cache_folder() {
        let d = cache_dir(Some(Path::new("C:/Users/v/AppData/Local"))).unwrap();
        assert!(d.ends_with("forgetty/cache/clipboard"));
        assert!(cache_dir(None).is_none());
    }

    #[test]
    fn the_target_comes_from_the_spawn_program() {
        let t = |a: &[&str]| target_of(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(t(&["cmd.exe"]), Target::Cmd);
        assert_eq!(t(&[r"C:\Windows\System32\cmd.exe", "/k", "x"]), Target::Cmd);
        assert_eq!(t(&["pwsh.exe", "-NoLogo"]), Target::PowerShell);
        assert_eq!(t(&[r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"]), Target::PowerShell);
        assert_eq!(t(&["wsl.exe", "-d", "Ubuntu"]), Target::Wsl);
        assert_eq!(t(&["ubuntu2204.exe"]), Target::Wsl);
        assert_eq!(t(&[r"C:\Windows\System32\bash.exe"]), Target::Wsl);
        assert_eq!(t(&[r"C:\Program Files\Git\bin\bash.exe", "--login"]), Target::GitBash);
        assert_eq!(t(&[r"C:\msys64\usr\bin\bash.exe"]), Target::GitBash);
        assert_eq!(t(&[r"C:\cygwin64\bin\bash.exe"]), Target::Cygwin);
        // A bare bash, or any other program, is not known.
        assert_eq!(t(&["bash"]), Target::Unknown);
        assert_eq!(t(&["nu.exe"]), Target::Unknown);
        assert_eq!(t(&[]), Target::Unknown);
    }

    #[test]
    fn a_plain_path_stays_as_it_is() {
        for t in [Target::Cmd, Target::PowerShell, Target::Unknown] {
            let p = r"C:\Users\a\AppData\Local\forgetty\cache\clipboard\paste-20261027-050934-0ad8abf1.png";
            assert_eq!(quote_path(t, p).unwrap(), p);
        }
        assert_eq!(paste_text(Path::new("C:/a/p.png"), Target::Cmd).unwrap(), "C:/a/p.png ");
    }

    #[test]
    fn the_path_form_is_the_one_of_the_program() {
        let p = r"C:\Users\a\p.png";
        assert_eq!(quote_path(Target::Wsl, p).unwrap(), "/mnt/c/Users/a/p.png");
        assert_eq!(quote_path(Target::GitBash, p).unwrap(), "/c/Users/a/p.png");
        assert_eq!(quote_path(Target::Cygwin, p).unwrap(), "/cygdrive/c/Users/a/p.png");
        assert_eq!(quote_path(Target::Cmd, p).unwrap(), p);
        let sp = r"C:\Users\a b\p.png";
        assert_eq!(quote_path(Target::Wsl, sp).unwrap(), "'/mnt/c/Users/a b/p.png'");
        assert_eq!(quote_path(Target::GitBash, sp).unwrap(), "'/c/Users/a b/p.png'");
        assert_eq!(quote_path(Target::Cygwin, sp).unwrap(), "'/cygdrive/c/Users/a b/p.png'");
    }

    #[test]
    fn a_path_with_a_space_is_quoted_for_each_program() {
        let p = r"C:\Users\a b\p.png";
        assert_eq!(quote_path(Target::Cmd, p).unwrap(), format!("\"{p}\""));
        assert_eq!(quote_path(Target::Unknown, p).unwrap(), format!("\"{p}\""));
        assert_eq!(quote_path(Target::PowerShell, p).unwrap(), format!("'{p}'"));
        assert_eq!(paste_text(Path::new(p), Target::PowerShell).unwrap(), format!("'{p}' "));
    }

    #[test]
    fn quotes_and_dollars_in_a_path_cannot_break_out() {
        // PowerShell: the single quote is doubled, and so is each typographic one.
        assert_eq!(quote_path(Target::PowerShell, r"C:\Users\O'Neil\p.png").unwrap(), r"'C:\Users\O''Neil\p.png'");
        assert_eq!(quote_path(Target::PowerShell, "C:\\Users\\O\u{2019}Neil\\p.png").unwrap(), "'C:\\Users\\O\u{2019}\u{2019}Neil\\p.png'");
        assert_eq!(quote_path(Target::PowerShell, r"C:\Users\$x\p.png").unwrap(), r"'C:\Users\$x\p.png'");
        assert_eq!(quote_path(Target::PowerShell, r"C:\Users\a,b\p.png").unwrap(), r"'C:\Users\a,b\p.png'");
        // POSIX programs: a quote is closed, escaped and opened again.
        assert_eq!(quote_path(Target::Wsl, r"C:\Users\O'Neil\p.png").unwrap(), r"'/mnt/c/Users/O'\''Neil/p.png'");
        assert_eq!(quote_path(Target::GitBash, r"C:\Users\a;rm\p.png").unwrap(), "'/c/Users/a;rm/p.png'");
        // cmd: `&` is inside the quotes; a percent sign cannot be quoted, so it is refused.
        assert_eq!(quote_path(Target::Cmd, r"C:\Users\a&b\p.png").unwrap(), r#""C:\Users\a&b\p.png""#);
        assert!(quote_path(Target::Cmd, r"C:\Users\%x%\p.png").is_err());
        assert!(quote_path(Target::Cmd, r"C:\Users\a!b\p.png").is_err());
        // An unknown program: double quotes, unless a shell would act inside them.
        assert!(quote_path(Target::Unknown, r"C:\Users\a&b\p.png").is_ok());
        for bad in [r"C:\Users\$x\p.png", r"C:\Users\`x\p.png", r"C:\Users\%x%\p.png", r"C:\Users\a!b\p.png"] {
            assert!(quote_path(Target::Unknown, bad).is_err(), "{bad}");
        }
        // Never a newline or an escape in a pasted path.
        assert!(quote_path(Target::PowerShell, "C:\\a\nb.png").is_err());
        assert!(quote_path(Target::Cmd, "C:\\a\u{1b}b.png").is_err());
    }

    fn bmp(w: i32, h: i32, bpp: u16, comp: u32, pixels: &[u8]) -> Vec<u8> {
        let mut b = b"BM".to_vec();
        b.extend_from_slice(&(54u32 + pixels.len() as u32).to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&54u32.to_le_bytes());
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&bpp.to_le_bytes());
        b.extend_from_slice(&comp.to_le_bytes());
        b.extend_from_slice(&[0; 20]);
        b.extend_from_slice(pixels);
        b
    }

    #[test]
    fn bmp_24_bit_bottom_up_with_row_padding() {
        // 2x2: bottom row first. Rows are padded to 4 bytes (6 -> 8).
        let px = [
            0, 0, 255, /* red */ 0, 255, 0, /* green */ 9, 9, // bottom row: red, green
            255, 0, 0, /* blue */ 255, 255, 255, /* white */ 9, 9, // top row: blue, white
        ];
        let r = bmp_to_rgba(&bmp(2, 2, 24, 0, &px)).unwrap();
        assert_eq!((r.w, r.h), (2, 2));
        assert_eq!(&r.data[..8], &[0, 0, 255, 255, 255, 255, 255, 255], "top row: blue, white");
        assert_eq!(&r.data[8..], &[255, 0, 0, 255, 0, 255, 0, 255], "bottom row: red, green");
    }

    #[test]
    fn bmp_32_bit_top_down_and_missing_alpha() {
        let px = [10, 20, 30, 0, 40, 50, 60, 0];
        let r = bmp_to_rgba(&bmp(2, -1, 32, 0, &px)).unwrap();
        assert_eq!(r.data, vec![30, 20, 10, 255, 60, 50, 40, 255]);
        // With alpha present it is kept.
        let px = [10, 20, 30, 128, 40, 50, 60, 0];
        let r = bmp_to_rgba(&bmp(2, 1, 32, 3, &px)).unwrap();
        assert_eq!(r.data, vec![30, 20, 10, 128, 60, 50, 40, 0]);
    }

    #[test]
    fn bmp_refusals() {
        assert!(bmp_to_rgba(b"").is_err());
        assert!(bmp_to_rgba(b"PNG not a bitmap at all, long enough to be read as a header..").is_err());
        assert!(bmp_to_rgba(&bmp(2, 2, 8, 0, &[0; 16])).is_err(), "palette");
        assert!(bmp_to_rgba(&bmp(2, 2, 24, 1, &[0; 16])).is_err(), "RLE");
        assert!(bmp_to_rgba(&bmp(2, 2, 24, 0, &[0; 4])).is_err(), "short data");
        assert!(bmp_to_rgba(&bmp(0, 2, 24, 0, &[0; 4])).is_err(), "no width");
        let big = bmp_to_rgba(&bmp(100_000, 100_000, 24, 0, &[0; 4])).unwrap_err();
        assert!(big.contains("too large"), "{big}");
    }
}
