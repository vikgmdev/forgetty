//! Sanitising of attacker-controlled text.
//!
//! Anything a remote peer sends as free text (device name, client kind, and
//! later the `Hello` fields) goes through [`untrusted`] before it is logged,
//! stored or printed. One helper, used everywhere, so a hostile name cannot
//! forge log lines, drive the operator's terminal with escape sequences, or
//! flip text direction.

/// Longest accepted untrusted string, in characters (names, ids).
pub const MAX_UNTRUSTED_CHARS: usize = 64;

/// Clean `raw` for logging, storing and display.
///
/// - Removes control characters (C0, DEL, C1), which covers newlines, tabs and
///   ESC. A complete ANSI CSI sequence (`ESC [ ... final`) is removed whole,
///   so no stray `[31m` is left behind.
/// - Removes zero-width and bidirectional override characters.
/// - Keeps at most [`MAX_UNTRUSTED_CHARS`] characters, and reads no more input
///   than needed, so a 10 KB name costs almost nothing.
/// - Trims surrounding whitespace.
///
/// The result can be empty. Callers pick their own fallback.
pub fn untrusted(raw: &str) -> String {
    untrusted_max(raw, MAX_UNTRUSTED_CHARS)
}

/// Same as [`untrusted`] with a caller-chosen character cap, for longer free
/// text such as a pane title, a status message or a notification body.
pub fn untrusted_max(raw: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if count >= max_chars {
            break;
        }
        if c == '\u{1b}' {
            // Swallow a whole CSI sequence: ESC [ <params 0x30-0x3F>* <intermediates 0x20-0x2F>* <final 0x40-0x7E>
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_control() || is_invisible_format(c) {
            continue;
        }
        out.push(c);
        count += 1;
    }
    out.trim().to_string()
}

/// Zero-width, bidi-control and BOM characters.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_pass_through() {
        assert_eq!(untrusted("Vick's Pixel 9"), "Vick's Pixel 9");
        assert_eq!(untrusted("  laptop  "), "laptop");
    }

    #[test]
    fn newlines_cannot_forge_log_lines() {
        let out = untrusted("phone\n INFO pair: paired device name=\"root\"\r\nWARN x\t!");
        assert!(!out.contains('\n') && !out.contains('\r') && !out.contains('\t'));
        assert_eq!(out, "phone INFO pair: paired device name=\"root\"WARN x!");
    }

    #[test]
    fn ansi_escapes_are_removed_whole() {
        assert_eq!(untrusted("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(untrusted("a\u{1b}[2J\u{1b}[Hb"), "ab");
        assert_eq!(untrusted("\u{1b}]0;title\u{7}x"), "]0;titlex");
        assert!(!untrusted("\u{1b}[31").contains('\u{1b}'));
        assert!(!untrusted("x\u{1b}").contains('\u{1b}'));
    }

    #[test]
    fn c0_c1_and_del_are_removed() {
        assert_eq!(untrusted("a\u{0}b\u{7}c\u{7f}d\u{85}e\u{9b}f"), "abcdef");
    }

    #[test]
    fn bidi_and_zero_width_are_removed() {
        assert_eq!(untrusted("a\u{202e}b\u{200b}c\u{feff}d\u{2066}e"), "abcde");
    }

    #[test]
    fn a_10kb_name_is_cut_to_64_chars() {
        let big = "x".repeat(10 * 1024);
        let out = untrusted(&big);
        assert_eq!(out.chars().count(), MAX_UNTRUSTED_CHARS);
    }

    #[test]
    fn length_counts_chars_not_bytes() {
        let big = "é".repeat(500);
        let out = untrusted(&big);
        assert_eq!(out.chars().count(), MAX_UNTRUSTED_CHARS);
        assert!(out.len() > MAX_UNTRUSTED_CHARS);
    }

    #[test]
    fn a_10kb_blob_of_escapes_and_newlines_is_empty() {
        let blob = "\u{1b}[31m\n".repeat(2000);
        assert_eq!(untrusted(&blob), "");
    }

    #[test]
    fn untrusted_max_uses_the_given_cap() {
        let big = "y".repeat(1000);
        assert_eq!(untrusted_max(&big, 200).chars().count(), 200);
        assert_eq!(untrusted_max("a\u{1b}[31mb", 10), "ab");
    }

    #[test]
    fn empty_stays_empty() {
        assert_eq!(untrusted(""), "");
    }
}
