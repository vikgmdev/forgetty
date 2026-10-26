//! Watches `config.toml` and the `themes` folder next to it, and reads them again when they change.
//!
//! No polling: the operating system tells the watcher (inotify, ReadDirectoryChangesW) and a
//! worker thread sleeps until it does. The worker, not the UI thread, reads and parses the
//! file and builds the theme list, so a slow disk or a big theme never stalls a frame. The UI
//! end gets one [`Event`] per change on an async channel.
//!
//! The watch is on the directories, not the file: editors save by writing a temporary file and
//! renaming it over the original, which ends a watch on the old file. The themes folder may
//! not exist yet: the config folder is watched for it to appear.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::config::{Config, Loaded, ThemeIndex};
use crate::resolve::Host;

/// How long the files must be quiet before they are read. An editor save is often several events.
const QUIET: Duration = Duration::from_millis(150);

/// The outcome of one read.
#[derive(Debug)]
pub enum Reload {
    /// The file parsed.
    Loaded(Loaded),
    /// There is no file (it was deleted, or never made). The caller keeps what it has: at start
    /// that is the defaults, while running it is the last good config.
    Missing,
    /// The file could not be read or parsed. The text says why and names the file.
    Failed(String),
}

/// What the worker sends.
#[derive(Debug)]
pub enum Event {
    /// The config file was read.
    Config(Reload),
    /// The theme list: first thing at start, then after every change in the themes folder.
    /// It comes before the [`Event::Config`] that the same change causes.
    Themes(Arc<ThemeIndex>),
}

/// Something wants a read.
enum Msg {
    /// The config file changed, or the user asked.
    Config,
    /// Something in the themes folder changed. `rewatch`: the folder itself came or went.
    Themes { rewatch: bool },
    /// The `ConfWatch` is gone.
    Stop,
}

/// Dropping it stops the watch and ends the worker thread.
pub struct ConfWatch {
    /// An [`Event`] per change, or per [`ConfWatch::reload_now`].
    pub rx: async_channel::Receiver<Event>,
    tx: mpsc::Sender<Msg>,
}

impl ConfWatch {
    /// Read the file again now (on the worker thread) and send the result.
    pub fn reload_now(&self) {
        let _ = self.tx.send(Msg::Config);
    }
}

impl Drop for ConfWatch {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// The custom themes folder for a config file: `themes` next to it. This is also the folder
/// when `FORGETTY_CONFIG` names the file.
pub fn themes_dir_for(config_path: &Path) -> PathBuf {
    config_path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")).join("themes")
}

/// The file's name, for a message. The folder would eat the room of a short notice.
fn shown(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// Read and parse `path`; the custom themes are the `themes` folder next to it.
pub fn read(path: &Path) -> Reload {
    read_with(path, &Host::real(Some(themes_dir_for(path))))
}

/// [`read`] against `host`.
pub fn read_with(path: &Path, host: &Host) -> Reload {
    match std::fs::read_to_string(path) {
        Ok(text) => match Config::parse_with(&text, host) {
            Ok(l) => Reload::Loaded(l),
            Err(e) => Reload::Failed(format!("{}: {e}", shown(path))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Reload::Missing,
        Err(e) => Reload::Failed(format!("{}: {e}", shown(path))),
    }
}

/// Start watching `path` and the themes folder next to it. The config directory is created when
/// it is missing, so a file made later is seen. `Err` when the operating system will not watch it.
pub fn watch(path: PathBuf) -> Result<ConfWatch, String> {
    use notify::{EventKind, RecursiveMode, Watcher};

    let dir = path.parent().map(Path::to_path_buf).filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dir);
    let name = path.file_name().map(|n| n.to_os_string()).ok_or("the config path has no file name")?;
    let themes = themes_dir_for(&path);

    let (tx, worker_rx) = mpsc::channel::<Msg>();
    let (out_tx, rx) = async_channel::unbounded::<Event>();

    let cb_tx = tx.clone();
    let cb_themes = themes.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        for p in &ev.paths {
            if p.starts_with(&cb_themes) {
                // The folder itself (made, removed or renamed) needs a new watch.
                let _ = cb_tx.send(Msg::Themes { rewatch: *p == cb_themes });
            } else if p.file_name() == Some(name.as_os_str()) {
                let _ = cb_tx.send(Msg::Config);
            }
        }
    })
    .map_err(|e| e.to_string())?;
    watcher.watch(&dir, RecursiveMode::NonRecursive).map_err(|e| format!("{}: {e}", dir.display()))?;

    std::thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            // The themes folder may be missing: the config folder's own events say when it comes.
            let mut watching_themes = watcher.watch(&themes, RecursiveMode::NonRecursive).is_ok();
            let send_themes = |out: &async_channel::Sender<Event>| {
                out.send_blocking(Event::Themes(Arc::new(ThemeIndex::build(&themes)))).is_ok()
            };
            if !send_themes(&out_tx) {
                return;
            }
            // Blocks until the OS reports a change. The watcher lives here, so it ends with us.
            while let Ok(first) = worker_rx.recv() {
                let (mut want_config, mut want_themes, mut rewatch) = (false, false, false);
                let mut note = |m: Msg| match m {
                    Msg::Config => want_config = true,
                    Msg::Themes { rewatch: r } => {
                        want_themes = true;
                        rewatch |= r;
                    }
                    Msg::Stop => {}
                };
                if matches!(first, Msg::Stop) {
                    return;
                }
                note(first);
                // Wait for quiet.
                loop {
                    match worker_rx.recv_timeout(QUIET) {
                        Ok(Msg::Stop) => return,
                        Ok(m) => note(m),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                if want_themes {
                    // A folder made or replaced is a new thing to watch. The folder may also have
                    // appeared without an event for itself, so try when it is not watched.
                    if rewatch || !watching_themes {
                        let _ = watcher.unwatch(&themes);
                        watching_themes = watcher.watch(&themes, RecursiveMode::NonRecursive).is_ok();
                    }
                    if !send_themes(&out_tx) {
                        return;
                    }
                    // The config may name a theme that changed.
                    want_config = true;
                }
                if want_config && out_tx.send_blocking(Event::Config(read(&path))).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(ConfWatch { rx, tx })
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

    /// The next config read. Theme lists on the way are kept in `themes`.
    fn next(w: &ConfWatch, secs: u64) -> Option<Reload> {
        match next_event(w, secs)? {
            Event::Config(r) => Some(r),
            Event::Themes(_) => next(w, secs),
        }
    }

    fn next_event(w: &ConfWatch, secs: u64) -> Option<Event> {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if let Ok(r) = w.rx.try_recv() {
                return Some(r);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn next_themes(w: &ConfWatch, secs: u64) -> Option<Arc<ThemeIndex>> {
        match next_event(w, secs)? {
            Event::Themes(t) => Some(t),
            Event::Config(_) => next_themes(w, secs),
        }
    }

    #[test]
    fn a_missing_file_is_reported_as_missing_and_a_bad_one_is_a_failure() {
        let d = temp_dir("read");
        let p = d.join("config.toml");
        assert!(matches!(read(&p), Reload::Missing));
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
        // The theme list comes once at start, then nothing happens until a file does.
        assert!(next_themes(&w, 10).is_some_and(|t| t.names.len() > 400));
        assert!(next_event(&w, 1).is_none(), "an idle watcher must send nothing");

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

        // The file is deleted: the caller is told, and keeps what it has.
        std::fs::remove_file(&p).unwrap();
        assert!(matches!(next(&w, 10), Some(Reload::Missing)));

        // The user asks.
        w.reload_now();
        assert!(matches!(next(&w, 10), Some(Reload::Missing)));

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

    #[test]
    fn a_theme_file_added_edited_and_a_folder_made_later_are_seen() {
        let d = temp_dir("themes");
        let p = d.join("config.toml");
        std::fs::write(&p, "theme = \"My Theme\"").unwrap();
        let w = watch(p.clone()).unwrap();
        // The themes folder does not exist yet; the config names a theme that is not there.
        let t = next_themes(&w, 10).unwrap();
        assert!(!t.names.iter().any(|n| n == "My Theme"));
        let theme = |bg: &str| format!("name = \"My Theme\"\n[colors]\nforeground = \"#102030\"\nbackground = \"{bg}\"\n");

        // The folder appears with a theme in it: a new list, then the config read again with the colors.
        let tdir = d.join("themes");
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join("mine.toml"), theme("#405060")).unwrap();
        let t = next_themes(&w, 10).unwrap();
        assert_eq!(t.colors("My Theme").unwrap().background, 0x405060);
        match next(&w, 10) {
            Some(Reload::Loaded(l)) => assert_eq!(l.config.background, 0x405060),
            other => panic!("{other:?}"),
        }

        // Editing it is seen too (the folder is watched now).
        std::fs::write(tdir.join("mine.toml"), theme("#0a0b0c")).unwrap();
        let t = next_themes(&w, 10).unwrap();
        assert_eq!(t.colors("My Theme").unwrap().background, 0x0a0b0c);
        match next(&w, 10) {
            Some(Reload::Loaded(l)) => assert_eq!(l.config.background, 0x0a0b0c),
            other => panic!("{other:?}"),
        }
        drop(w);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_themes_folder_is_next_to_the_file_whatever_its_name() {
        assert_eq!(themes_dir_for(Path::new("C:\\x\\y\\config.toml")).file_name().unwrap(), "themes");
        assert_eq!(themes_dir_for(Path::new("/tmp/a/other.toml")), Path::new("/tmp/a/themes"));
        assert_eq!(themes_dir_for(Path::new("config.toml")), Path::new("./themes"));
    }
}
