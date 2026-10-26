//! A last line in the log when the server dies, on Windows.
//!
//! A panic goes through the tracing log, so it shows in the same file as the rest. The Linux
//! module (`crash.rs`) also reports fatal signals from native or Zig code. Windows has no signals
//! for that: a fault is a structured exception, and the process dies with a Windows Error
//! Reporting entry in the Event Log. A handler for it is not part of the first release. Only the
//! panic hook is installed.

static INSTALLED: std::sync::Once = std::sync::Once::new();

/// Install the panic hook, once.
pub fn install() {
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            tracing::error!(thread = thread.name().unwrap_or("<unnamed>"), "server panic: {info}");
            previous(info);
        }));
    });
}
