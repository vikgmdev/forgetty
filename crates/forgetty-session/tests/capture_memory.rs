//! Capture of a full ring must not grow the process by a scrollback.
//!
//! This test has its own process, so the high-water mark of the resident set
//! (`VmHWM`) belongs to it alone. Before the fix capture kept the default 10
//! MiB scrollback and parsed the whole ring: about 19 MiB more than the ring.

#![cfg(target_os = "linux")]

use forgetty_session::capture::capture_text;

fn hwm_kib() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = s.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn capture_of_a_full_ring_of_short_lines_stays_small() {
    let mut ring = Vec::with_capacity(8 * 1024 * 1024 + 16);
    let mut n = 0u64;
    while ring.len() < 8 * 1024 * 1024 {
        ring.extend_from_slice(format!("{n}\r\n").as_bytes());
        n += 1;
    }
    let before = hwm_kib();
    let out = capture_text(&ring, 24, 80, true, Some(3));
    let grown = hwm_kib().saturating_sub(before);
    assert_eq!(out.lines().count(), 3);
    assert!(grown < 8 * 1024, "capture raised the peak by {grown} KiB");
}
