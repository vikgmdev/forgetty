//! The paste size warning. GTK asks before it pastes more than `paste_warn_size` bytes
//! (default 5 KiB, 0 turns the warning off). The newline and control-byte checks that already
//! guard a Windows paste live in `forgetty_vt::paste`; this adds the size rule.

/// GTK's default.
pub const DEFAULT_WARN_SIZE: usize = 5 * 1024;

/// True when `text` is over the limit. A limit of 0 never warns.
pub fn is_large(text: &str, limit: usize) -> bool {
    limit > 0 && text.len() > limit
}

/// "12.3 KiB (12600 bytes)" or "812 bytes".
pub fn size_label(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else {
        format!("{:.1} KiB ({bytes} bytes)", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_is_in_bytes_and_zero_turns_it_off() {
        let t = "x".repeat(DEFAULT_WARN_SIZE);
        assert!(!is_large(&t, DEFAULT_WARN_SIZE));
        assert!(is_large(&format!("{t}x"), DEFAULT_WARN_SIZE));
        // Bytes, not chars: 3000 two-byte chars are 6000 bytes.
        assert!(is_large(&"\u{e9}".repeat(3000), DEFAULT_WARN_SIZE));
        assert!(!is_large(&"x".repeat(1_000_000), 0));
    }

    #[test]
    fn labels() {
        assert_eq!(size_label(812), "812 bytes");
        assert_eq!(size_label(12_600), "12.3 KiB (12600 bytes)");
    }
}
