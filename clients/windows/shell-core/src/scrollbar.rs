//! Scrollbar geometry. The terminal reports `(total, offset, len)` rows: everything,
//! the first visible row, the visible rows. This maps that to a thumb on a track and back.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thumb {
    /// Distance from the top of the track.
    pub top: f32,
    pub height: f32,
}

/// The thumb, or `None` when everything is visible (no scrollbar). `min_h` keeps a thumb for a
/// huge scrollback big enough to hit.
pub fn thumb(total: u64, offset: u64, len: u64, track_h: f32, min_h: f32) -> Option<Thumb> {
    if len == 0 || total <= len || track_h <= 0.0 {
        return None;
    }
    let height = (track_h * len as f32 / total as f32).clamp(min_h.min(track_h), track_h);
    let travel = track_h - height;
    let max_off = (total - len) as f32;
    let top = travel * (offset.min(total - len) as f32 / max_off);
    Some(Thumb { top, height })
}

/// The offset for a thumb whose top edge is at `y` on the track (the pointer position minus
/// where the thumb was grabbed).
pub fn offset_at(total: u64, len: u64, track_h: f32, min_h: f32, y: f32) -> u64 {
    let Some(t) = thumb(total, 0, len, track_h, min_h) else { return 0 };
    let travel = track_h - t.height;
    if travel <= 0.0 {
        return 0;
    }
    let max_off = (total - len) as f32;
    ((y / travel).clamp(0.0, 1.0) * max_off).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_thumb_when_everything_fits() {
        assert_eq!(thumb(30, 0, 30, 500.0, 20.0), None);
        assert_eq!(thumb(10, 0, 30, 500.0, 20.0), None);
        assert_eq!(thumb(100, 0, 0, 500.0, 20.0), None);
    }

    #[test]
    fn thumb_size_and_position() {
        let t = thumb(1000, 0, 100, 500.0, 20.0).unwrap();
        assert_eq!((t.top, t.height), (0.0, 50.0));
        let t = thumb(1000, 900, 100, 500.0, 20.0).unwrap();
        assert_eq!((t.top, t.height), (450.0, 50.0), "at the bottom the thumb touches the end");
        let t = thumb(1000, 450, 100, 500.0, 20.0).unwrap();
        assert!((t.top - 225.0).abs() < 0.01);
        // A huge scrollback keeps a usable thumb.
        let t = thumb(1_000_000, 500_000, 40, 500.0, 24.0).unwrap();
        assert_eq!(t.height, 24.0);
        assert!(t.top > 200.0 && t.top < 300.0);
        // An offset past the end is clamped.
        assert_eq!(thumb(1000, 5000, 100, 500.0, 20.0).unwrap().top, 450.0);
    }

    #[test]
    fn dragging_maps_back_to_an_offset() {
        assert_eq!(offset_at(1000, 100, 500.0, 20.0, 0.0), 0);
        assert_eq!(offset_at(1000, 100, 500.0, 20.0, 450.0), 900);
        assert_eq!(offset_at(1000, 100, 500.0, 20.0, 225.0), 450);
        assert_eq!(offset_at(1000, 100, 500.0, 20.0, -50.0), 0);
        assert_eq!(offset_at(1000, 100, 500.0, 20.0, 9999.0), 900);
        // Round trip.
        for off in [0u64, 1, 77, 450, 899, 900] {
            let t = thumb(1000, off, 100, 500.0, 20.0).unwrap();
            let back = offset_at(1000, 100, 500.0, 20.0, t.top);
            assert!(back.abs_diff(off) <= 1, "{off} -> {back}");
        }
    }
}
