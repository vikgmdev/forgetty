//! The crash handler writes a line to stderr before the process dies.

#![cfg(unix)]

use std::process::Command;

const CHILD: &str = "FORGETTY_CRASH_TEST_CHILD";

extern "C" fn plain_handler(sig: libc::c_int) {
    let msg = b"old plain handler ran\n";
    // SAFETY: write(2) on fd 2 with a valid buffer.
    unsafe { libc::write(2, msg.as_ptr().cast(), msg.len()) };
    let _ = sig;
}

#[test]
fn fatal_signal_leaves_a_line_in_the_log() {
    if let Ok(which) = std::env::var(CHILD) {
        if which == "plain" {
            // An old handler of the plain `sa_handler` kind, set before ours.
            // SAFETY: sigaction with a zeroed struct and an `extern "C"` fn.
            unsafe {
                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = plain_handler as *const () as usize;
                libc::sigemptyset(&mut act.sa_mask);
                libc::sigaction(libc::SIGABRT, &act, std::ptr::null_mut());
            }
        }
        forgetty_server::crash::install();
        forgetty_server::crash::install(); // twice: must not chain to itself
        let sig = if which == "segv" { libc::SIGSEGV } else { libc::SIGABRT };
        // SAFETY: raising a signal in the child process on purpose.
        unsafe { libc::raise(sig) };
        std::process::exit(0); // not reached
    }
    for (which, num, name) in
        [("abort", 6, "SIGABRT"), ("segv", 11, "SIGSEGV"), ("plain", 6, "SIGABRT")]
    {
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
        if which == "plain" {
            assert!(err.contains("old plain handler ran"), "old handler not chained: {err}");
        }
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(out.status.signal(), Some(num), "the process must still die of the signal");
    }
}
