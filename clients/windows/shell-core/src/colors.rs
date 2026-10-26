//! Accent colors of tabs and workspaces. The server keeps a workspace color as a string and does
//! not look inside it (`set_workspace_color`), so this client only writes `#rrggbb` and shows
//! only what parses as that.

/// The color presets of GTK's tab and workspace color pickers (`TAB_COLOR_PRESETS`).
pub const PRESETS: [(&str, u32); 8] = [
    ("Red", 0xe0493a),
    ("Orange", 0xf18f32),
    ("Yellow", 0xf7c536),
    ("Green", 0x5ab967),
    ("Teal", 0x30a998),
    ("Blue", 0x3987e4),
    ("Purple", 0x9d5fd8),
    ("Pink", 0xdb5da5),
];

/// `#rrggbb` or `rrggbb` (any case) to `0xrrggbb`. Anything else is `None`: a color that does
/// not parse shows as "no color", as in GTK.
pub fn parse_hex(s: &str) -> Option<u32> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(h, 16).ok()
}

/// `0xrrggbb` to `#rrggbb`.
pub fn to_hex(c: u32) -> String {
    format!("#{:06x}", c & 0x00ff_ffff)
}

/// What a client may send as a workspace color: nothing, or `#rrggbb`.
pub fn wire_color(c: Option<u32>) -> Option<String> {
    c.map(to_hex)
}

/// A color from a server layout, if it is one we can show.
pub fn from_wire(s: Option<&str>) -> Option<u32> {
    s.and_then(parse_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip_and_bad_input() {
        assert_eq!(parse_hex("#3987E4"), Some(0x3987e4));
        assert_eq!(parse_hex("3987e4"), Some(0x3987e4));
        assert_eq!(to_hex(0x3987e4), "#3987e4");
        assert_eq!(to_hex(0x000001), "#000001");
        for bad in ["", "#", "#12345", "#1234567", "#12345g", "red", "#\u{e9}23456", "#+12345", "0x3987e4"] {
            assert_eq!(parse_hex(bad), None, "{bad:?}");
        }
        assert_eq!(wire_color(None), None);
        assert_eq!(wire_color(Some(0xe0493a)).as_deref(), Some("#e0493a"));
        assert_eq!(from_wire(Some("not a color")), None);
        assert_eq!(from_wire(None), None);
    }

    #[test]
    fn presets_are_distinct_and_in_gtk_order() {
        let names: Vec<&str> = PRESETS.iter().map(|p| p.0).collect();
        assert_eq!(names, ["Red", "Orange", "Yellow", "Green", "Teal", "Blue", "Purple", "Pink"]);
        let mut cols: Vec<u32> = PRESETS.iter().map(|p| p.1).collect();
        cols.sort_unstable();
        cols.dedup();
        assert_eq!(cols.len(), 8);
    }
}
