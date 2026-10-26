//! Watches `config.toml` and reads it again when it changes.
//!
//! No polling: the operating system tells the watcher (inotify, ReadDirectoryChangesW) and a
//! worker thread sleeps until it does. The worker, not the UI thread, reads and parses the
//! file, so a slow disk or a big theme never stalls a frame. The UI end gets one
//! [`Reload`] per change on an async channel.
//!
//! The watch is on the directory, not the file: editors save by writing a temporary file and
//! renaming it over the original, which ends a watch on the old file.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crate::config::{Config, Loaded};

/// How long the file must be quiet before it is read. An editor save is often several events.
const QUIET: Duration = Duration::from_millis(150);

/// The outcome of one read.
#[derive(Debug)]
pub enum Reload {
    /// The file parsed (or does not exist: then it is the defaults, as at start).
    Loaded(Loaded),
    /// The file could not be read or parsed. The text says why and names the file.
    Failed(String),
}

/// Something wants the file read: the directory changed, or the user asked.
struct Msg;

/// Dropping it stops the watch and ends the worker thread.
pub struct ConfWatch {
    /// A [`Reload`] per change, or per [`ConfWatch::reload_now`].
    pub rx: async_channel::Receiver<Reload>,
    tx: mpsc::Sender<Msg>,
    _watcher: notify::RecommendedWatcher,
}

impl ConfWatch {
    /// Read the file again now (on the worker thread) and send the result.
    pub fn reload_now(&self) {
        let _ = self.tx.send(Msg);
    }
}

/// Read and parse `path`. A missing file is the defaults.
pub fn read(path: &Path) -> Reload {
    match std::fs::read_to_string(path) {
        Ok(text) => match Config::parse(&text) {
            Ok(l) => Reload::Loaded(l),
            Err(e) => Reload::Failed(format!("{}: {e}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Reload::Loaded(Loaded { config: Config::default(), warnings: Vec::new() })
        }
        Err(e) => Reload::Failed(format!("{}: {e}", path.display())),
    }
}

/// Start watching `path`. The directory is created when it is missing, so a file made later is
/// seen. `Err` when the operating system will not watch it.
pub fn watch(path: PathBuf) -> Result<ConfWatch, String> {
    use notify::{EventKind, RecursiveMode, Watcher};

    let dir = path.parent().map(Path::to_path_buf).filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dir);
    let name = path.file_name().map(|n| n.to_os_string()).ok_or("the config path has no file name")?;

    let (tx, worker_rx) = mpsc::channel::<Msg>();
    let (out_tx, rx) = async_channel::unbounded::<Reload>();

    let cb_tx = tx.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        if ev.paths.iter().any(|p| p.file_name() == Some(name.as_os_str())) {
            let _ = cb_tx.send(Msg);
        }
    })
    .map_err(|e| e.to_string())?;
    watcher.watch(&dir, RecursiveMode::NonRecursive).map_err(|e| format!("{}: {e}", dir.display()))?;

    std::thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            // Blocks until the OS reports a change. Ends when the watcher (and with it every
            // sender but the one in `ConfWatch`) is gone and `ConfWatch` dropped too.
            while worker_rx.recv().is_ok() {
                // Wait for quiet.
                loop {
                    match worker_rx.recv_timeout(QUIET) {
                        Ok(_) => {}
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                if out_tx.send_blocking(read(&path)).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(ConfWatch { rx, tx, _watcher: watcher })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!("forgetty-confwatch-{}-{tag}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn next(w: &ConfWatch, secs: u64) -> Option<Reload> {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if let Ok(r) = w.rx.try_recv() {
                return Some(r);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn a_missing_file_is_the_defaults_and_a_bad_one_is_a_failure() {
        let d = temp_dir("read");
        let p = d.join("config.toml");
        match read(&p) {
            Reload::Loaded(l) => assert_eq!(l.config, Config::default()),
            other => panic!("{other:?}"),
        }
        std::fs::write(&p, "bell_mode = \"loud\"").unwrap();
        match read(&p) {
            Reload::Failed(e) => assert!(e.contains("config.toml") && e.contains("loud"), "{e}"),
            other => panic!("{other:?}"),
        }
        std::fs::write(&p, "bell_mode = \"none\"").unwrap();
        match read(&p) {
            Reload::Loaded(l) => assert_eq!(l.config.bell_mode, crate::bell::BellMode::None),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_save_is_seen_once_and_idle_costs_nothing() {
        let d = temp_dir("watch");
        let p = d.join("config.toml");
        std::fs::write(&p, "bell_mode = \"none\"").unwrap();
        let w = watch(p.clone()).unwrap();
        // Nothing happens until the file does.
        assert!(next(&w, 1).is_none(), "an idle watcher must send nothing");

        // Several quick writes are one reload.
        for i in 0..4 {
            std::fs::write(&p, format!("font_size = {}", 10 + i)).unwrap();
        }
        match next(&w, 10) {
            Some(Reload::Loaded(l)) => assert_eq!(l.config.font_size_pt, 13.0),
            other => panic!("{other:?}"),
        }
        assert!(next(&w, 1).is_none(), "one burst, one reload");

        // An editor's save: write a temporary file, rename over the original.
        let tmp = d.join("config.toml.tmp");
        std::fs::write(&tmp, "font_size = 20").unwrap();
        std::fs::rename(&tmp, &p).unwrap();
        match next(&w, 10) {
            Some(Reload::Loaded(l)) => assert_eq!(l.config.font_size_pt, 20.0),
            other => panic!("{other:?}"),
        }

        // A bad file is a failure, then a good one loads again.
        std::fs::write(&p, "font_size = [").unwrap();
        assert!(matches!(next(&w, 10), Some(Reload::Failed(_))));
        std::fs::write(&p, "font_size = 9").unwrap();
        assert!(matches!(next(&w, 10), Some(Reload::Loaded(_))));

        // Another file in the directory is not our business.
        std::fs::write(d.join("machines.json"), "{}").unwrap();
        assert!(next(&w, 1).is_none());

        // The user asks.
        w.reload_now();
        assert!(matches!(next(&w, 10), Some(Reload::Loaded(_))));

        drop(w);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_file_made_later_in_a_missing_directory_is_seen() {
        let d = temp_dir("late");
        let dir = d.join("not-yet");
        let p = dir.join("config.toml");
        let w = watch(p.clone()).unwrap();
        assert!(dir.is_dir());
        std::fs::write(&p, "sidebar = true").unwrap();
        match next(&w, 10) {
            Some(Reload::Loaded(l)) => assert!(l.config.sidebar_open),
            other => panic!("{other:?}"),
        }
        drop(w);
        let _ = std::fs::remove_dir_all(d);
    }
}
