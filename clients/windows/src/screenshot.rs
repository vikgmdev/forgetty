//! Window screenshot to PNG for the smoke gate (Win32 PrintWindow, with a
//! screen-copy fallback), so CI can upload what the layout looks like.

pub struct Shot {
    pub w: u32,
    pub h: u32,
    pub method: &'static str,
    /// Distinct colors seen (capped), a cheap "is it blank" test.
    pub distinct: usize,
    /// Share of pixels that differ from the most common color, in percent.
    pub busy_pct: f64,
}

#[cfg(not(windows))]
pub fn capture(_window: &gpui::Window, _path: &str) -> Result<Shot, String> {
    Err("screenshots need Windows".into())
}

#[cfg(windows)]
pub fn capture(window: &gpui::Window, path: &str) -> Result<Shot, String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).map_err(|e| format!("window handle: {e}"))?;
    let hwnd = match handle.as_raw() {
        RawWindowHandle::Win32(h) => h.hwnd.get(),
        other => return Err(format!("unexpected window handle {other:?}")),
    };
    win::capture_hwnd(hwnd, path)
}

fn stats(rgba: &[u8]) -> (usize, f64) {
    use std::collections::HashMap;
    let mut counts: HashMap<u32, u32> = HashMap::new();
    for px in rgba.chunks_exact(4) {
        let k = u32::from_le_bytes([px[0], px[1], px[2], 0]);
        if counts.len() < 4096 || counts.contains_key(&k) {
            *counts.entry(k).or_insert(0) += 1;
        }
    }
    let total = (rgba.len() / 4).max(1) as f64;
    let top = counts.values().copied().max().unwrap_or(0) as f64;
    (counts.len(), (1.0 - top / total) * 100.0)
}

fn write_png(path: &str, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("create {path}: {e}"))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| format!("png header: {e}"))?;
    wr.write_image_data(rgba).map_err(|e| format!("png data: {e}"))?;
    Ok(())
}

#[cfg(windows)]
mod win {
    use super::{stats, write_png, Shot};
    use std::ffi::c_void;

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    struct BitmapInfoHeader {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bit_count: u16,
        compression: u32,
        size_image: u32,
        x_ppm: i32,
        y_ppm: i32,
        clr_used: u32,
        clr_important: u32,
    }
    #[repr(C)]
    struct BitmapInfo {
        header: BitmapInfoHeader,
        colors: [u32; 1],
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetClientRect(hwnd: isize, r: *mut Rect) -> i32;
        fn ClientToScreen(hwnd: isize, p: *mut Point) -> i32;
        fn GetDC(hwnd: isize) -> isize;
        fn ReleaseDC(hwnd: isize, dc: isize) -> i32;
        fn PrintWindow(hwnd: isize, dc: isize, flags: u32) -> i32;
    }
    #[link(name = "gdi32")]
    extern "system" {
        fn CreateCompatibleDC(dc: isize) -> isize;
        fn DeleteDC(dc: isize) -> i32;
        fn CreateDIBSection(dc: isize, bmi: *const BitmapInfo, usage: u32, bits: *mut *mut c_void, section: isize, offset: u32) -> isize;
        fn SelectObject(dc: isize, obj: isize) -> isize;
        fn DeleteObject(obj: isize) -> i32;
        fn BitBlt(dst: isize, x: i32, y: i32, w: i32, h: i32, src: isize, sx: i32, sy: i32, rop: u32) -> i32;
        fn GdiFlush() -> i32;
    }

    const PW_CLIENTONLY: u32 = 1;
    const PW_RENDERFULLCONTENT: u32 = 2;
    const SRCCOPY: u32 = 0x00CC_0020;
    const CAPTUREBLT: u32 = 0x4000_0000;

    pub fn capture_hwnd(hwnd: isize, path: &str) -> Result<Shot, String> {
        unsafe {
            let mut r = Rect { left: 0, top: 0, right: 0, bottom: 0 };
            if GetClientRect(hwnd, &mut r) == 0 {
                return Err("GetClientRect failed".into());
            }
            let (w, h) = ((r.right - r.left).max(1), (r.bottom - r.top).max(1));
            let memdc = CreateCompatibleDC(0);
            if memdc == 0 {
                return Err("CreateCompatibleDC failed".into());
            }
            let bmi = BitmapInfo {
                header: BitmapInfoHeader {
                    size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                    width: w,
                    height: -h, // top-down
                    planes: 1,
                    bit_count: 32,
                    compression: 0,
                    size_image: 0,
                    x_ppm: 0,
                    y_ppm: 0,
                    clr_used: 0,
                    clr_important: 0,
                },
                colors: [0],
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(memdc, &bmi, 0, &mut bits, 0, 0);
            if dib == 0 || bits.is_null() {
                DeleteDC(memdc);
                return Err("CreateDIBSection failed".into());
            }
            let old = SelectObject(memdc, dib);
            let n = (w as usize) * (h as usize) * 4;

            let grab = |bits: *mut c_void| -> Vec<u8> {
                GdiFlush();
                let src = std::slice::from_raw_parts(bits as *const u8, n);
                let mut rgba = Vec::with_capacity(n);
                for p in src.chunks_exact(4) {
                    rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
                }
                rgba
            };

            // Try the capture paths in order until one shows real content.
            let mut method = "none";
            let mut rgba = Vec::new();
            for (name, flags) in [
                ("PrintWindow(client|full)", PW_CLIENTONLY | PW_RENDERFULLCONTENT),
                ("PrintWindow(full)", PW_RENDERFULLCONTENT),
                ("PrintWindow(client)", PW_CLIENTONLY),
            ] {
                let ok = PrintWindow(hwnd, memdc, flags) != 0;
                rgba = grab(bits);
                let (distinct, busy) = stats(&rgba);
                method = name;
                if ok && distinct >= 8 && busy >= 2.0 {
                    break;
                }
                method = "none";
            }
            if method == "none" {
                // The swap chain may not be captured: copy the composed desktop instead.
                let mut pt = Point { x: 0, y: 0 };
                ClientToScreen(hwnd, &mut pt);
                let screen = GetDC(0);
                if screen != 0 {
                    BitBlt(memdc, 0, 0, w, h, screen, pt.x, pt.y, SRCCOPY | CAPTUREBLT);
                    ReleaseDC(0, screen);
                    rgba = grab(bits);
                    method = "BitBlt(screen)";
                }
            }
            SelectObject(memdc, old);
            DeleteObject(dib);
            DeleteDC(memdc);

            let (distinct, busy_pct) = stats(&rgba);
            write_png(path, w as u32, h as u32, &rgba)?;
            Ok(Shot { w: w as u32, h: h as u32, method, distinct, busy_pct })
        }
    }
}
