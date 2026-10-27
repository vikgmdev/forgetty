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

/// What is pasted into the pane: the path and a space, as GTK does.
pub fn paste_text(path: &Path) -> String {
    format!("{} ", path.to_string_lossy())
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
        assert_eq!(paste_text(Path::new("C:/a b/p.png")), "C:/a b/p.png ");
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
