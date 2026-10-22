//! The crash handler writes a line to stderr before the process dies.

#![cfg(unix)]

use std::process::Command;

const CHILD: &str = "FORGETTY_CRASH_TEST_CHILD";

#[test]
fn fatal_signal_leaves_a_line_in_the_log() {
    if let Ok(which) = std::env::var(CHILD) {
        forgetty_server::crash::install();
        let sig = if which == "segv" { libc::SIGSEGV } else { libc::SIGABRT };
        // SAFETY: raising a signal in the child process on purpose.
        unsafe { libc::raise(sig) };
        std::process::exit(0); // not reached
    }
    for (which, num, name) in [("abort", 6, "SIGABRT"), ("segv", 11, "SIGSEGV")] {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fatal_signal_leaves_a_line_in_the_log", "--nocapture"])
            .env(CHILD, which)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(&format!("forgetty-server: fatal signal {num} ({name}) pid ")),
            "no crash line for {which}: {err}"
        );
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(out.status.signal(), Some(num), "the process must still die of the signal");
    }
}
