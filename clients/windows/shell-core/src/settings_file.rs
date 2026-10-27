//! Changing `config.toml` from the Settings window without hurting the rest of the file.
//!
//! An edit touches one key: comments, the order of the keys, the keys this client does not know
//! and the values of the keys it does not change all stay as they were (the text is edited, never
//! written again from a parsed struct). A file that does not load is never written over by a
//! settings change; the TOML editor is the way to repair it (`save_text`). Every write goes to a
//! temporary file and is renamed over the real one, so the file watcher never sees half a file.
//!
//! Pure, apart from `save_edits` and `save_text`, which do the disk work on the caller's thread:
//! call those from a worker.

use std::path::Path;

use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

use crate::config::{open_refusal, Config};

/// A value to write.
#[derive(Debug, Clone, PartialEq)]
pub enum V {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// Written as a string when it has one entry, else as an array.
    List(Vec<String>),
}

/// One change to the file. Paths are `key` or `table.key`.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    Set(String, V),
    Remove(String),
}

impl Edit {
    pub fn set(path: &str, v: V) -> Edit {
        Edit::Set(path.to_string(), v)
    }
    pub fn remove(path: &str) -> Edit {
        Edit::Remove(path.to_string())
    }
}

fn to_value(v: &V) -> Value {
    match v {
        V::Str(s) => Value::from(s.as_str()),
        V::Int(i) => Value::from(*i),
        V::Float(f) => Value::from(*f),
        V::Bool(b) => Value::from(*b),
        V::List(l) if l.len() == 1 => Value::from(l[0].as_str()),
        V::List(l) => {
            let mut a = Array::new();
            for s in l {
                a.push(s.as_str());
            }
            Value::from(a)
        }
    }
}

fn split(path: &str) -> (Option<&str>, &str) {
    match path.split_once('.') {
        Some((t, k)) => (Some(t), k),
        None => (None, path),
    }
}

/// Set `key` in `table`, keeping the comment around an existing value.
fn put(table: &mut dyn TableLike, key: &str, v: Value) {
    if let Some(old) = table.get_mut(key).and_then(Item::as_value_mut) {
        let decor = old.decor().clone();
        let mut v = v;
        *v.decor_mut() = decor;
        *old = v;
    } else {
        table.insert(key, Item::Value(v));
    }
}

/// `text` with `edits` applied. An error says what could not be done; nothing is half applied.
pub fn apply(text: &str, edits: &[Edit]) -> Result<String, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.to_string().lines().next().unwrap_or("").to_string())?;
    for e in edits {
        match e {
            Edit::Set(path, v) => {
                let (tbl, key) = split(path);
                match tbl {
                    None => put(doc.as_table_mut(), key, to_value(v)),
                    Some(t) => {
                        if !doc.as_table().contains_key(t) {
                            let mut nt = Table::new();
                            nt.set_implicit(false);
                            doc.as_table_mut().insert(t, Item::Table(nt));
                        }
                        let item = doc.as_table_mut().get_mut(t).ok_or("table vanished")?;
                        // A table written inline (`keybindings = { a = "x" }`) is a table too.
                        let table = item.as_table_like_mut().ok_or_else(|| format!("`{t}` in the file is not a table, so `{path}` cannot be set"))?;
                        put(table, key, to_value(v));
                    }
                }
            }
            Edit::Remove(path) => {
                let (tbl, key) = split(path);
                match tbl {
                    None => {
                        doc.as_table_mut().remove(key);
                    }
                    Some(t) => {
                        let now_empty = match doc.as_table_mut().get_mut(t).and_then(Item::as_table_like_mut) {
                            Some(table) => {
                                table.remove(key);
                                table.is_empty()
                            }
                            None => false,
                        };
                        // A table the user did not keep anything in is not left behind, with
                        // one exception: a table with a comment inside is a table with content.
                        if now_empty && !has_comment(doc.as_table().get(t)) {
                            doc.as_table_mut().remove(t);
                        }
                    }
                }
            }
        }
    }
    Ok(doc.to_string())
}

fn has_comment(item: Option<&Item>) -> bool {
    item.and_then(Item::as_table).is_some_and(|t| {
        t.decor().prefix().and_then(|p| p.as_str()).is_some_and(|s| s.contains('#')) || t.decor().suffix().and_then(|p| p.as_str()).is_some_and(|s| s.contains('#'))
    })
}

/// The first line of why `text` does not load, or the warnings when it does.
pub fn check(text: &str) -> Result<Vec<String>, String> {
    Config::parse(text).map(|l| l.warnings).map_err(|e| e.lines().next().unwrap_or("").to_string())
}

fn read_current(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("Not saved: cannot read the file ({e})")),
    }
}

fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "the file".into())
}

/// One write at a time in this process: two saves (a row and the editor, a second window) would
/// otherwise read the same file and the second would drop the first one's change.
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A temporary file of ours that is older than this is left over from a crash.
const STALE_TMP: std::time::Duration = std::time::Duration::from_secs(600);

/// Remove the `config.toml.<pid>.<n>.tmp` files a crashed save left behind.
fn sweep_stale(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().map(|n| n.to_string_lossy().into_owned())) else { return };
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let f = e.file_name().to_string_lossy().into_owned();
        if f.starts_with(&format!("{name}.")) && f.ends_with(".tmp") {
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > STALE_TMP);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Create `dest` new with `bytes`. When `model` exists, `dest` gets its access rules before the
/// first byte is written. The data is on disk when this returns.
fn write_new_like(model: &Path, dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(dest)?;
    let r = (|| {
        if model.exists() {
            crate::fileperm::copy_security(model, dest)?;
        }
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if r.is_err() {
        drop(f);
        let _ = std::fs::remove_file(dest);
    }
    r
}

/// Rename over the target. Windows refuses for a moment while a scanner or an indexer has the
/// file open; that passes, so the call is repeated for about a second.
fn rename_over(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if tries < 200 && (matches!(e.kind(), std::io::ErrorKind::PermissionDenied) || matches!(e.raw_os_error(), Some(5 | 32 | 33))) => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Write `text` over `path` through a temporary file in the same folder (a name that is ours alone:
/// process id and a counter). `expect` is what the caller read; when the file differs now,
/// somebody else changed it and nothing is written.
fn write_atomic(path: &Path, text: &str, expect: Option<&str>) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Not saved: {e}"))?;
    }
    sweep_stale(path);
    let n = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp_name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    tmp_name.push(format!(".{}.{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let _ = std::fs::remove_file(&tmp);
    write_new_like(path, &tmp, text.as_bytes()).map_err(|e| format!("Not saved: {e}"))?;
    if let Some(expect) = expect {
        if read_current(path).ok().as_deref() != Some(expect) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("Not saved: {} changed while it was being saved. Try again.", name_of(path)));
        }
    }
    rename_over(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Not saved: {e}")
    })
}

/// What a save did.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    /// The new text of the file.
    pub text: String,
    /// Things worth saying about the new file (unknown keys and the like).
    pub warnings: Vec<String>,
}

/// Apply `edits` to the file at `path`. Refuses a path that is not a local `.toml` file, a file
/// that does not load now (the TOML editor repairs those), and a result that does not load.
pub fn save_edits(path: &Path, edits: &[Edit]) -> Result<Saved, String> {
    save_with(path, |_| Ok(edits.to_vec()))
}

/// [`save_edits`] where the edits depend on the text of the file (`plan` gets it): a font set in
/// a `[font]` table is changed there, not beside it.
pub fn save_with(path: &Path, plan: impl FnOnce(&str) -> Result<Vec<Edit>, String>) -> Result<Saved, String> {
    if let Some(why) = open_refusal(path) {
        return Err(why);
    }
    let _guard = write_lock();
    let cur = read_current(path)?;
    if !cur.trim().is_empty() {
        if let Err(e) = check(&cur) {
            return Err(format!("Not saved: {} does not load ({e}). Repair it in the TOML editor first.", name_of(path)));
        }
    }
    let edits = plan(&cur)?;
    let new = apply(&cur, &edits).map_err(|e| format!("Not saved: {e}"))?;
    let warnings = check(&new).map_err(|e| format!("Not saved: the change would make the file invalid ({e})"))?;
    if new == cur {
        return Ok(Saved { text: new, warnings });
    }
    write_atomic(path, &new, Some(&cur))?;
    Ok(Saved { text: new, warnings })
}

/// The TOML editor's save: `text` replaces the file when it loads and when the file is still
/// what the editor read (`expect`): a file that changed on disk since is never overwritten.
/// The old file is kept as `config.toml.bak` (one copy, the one before this save); a backup that
/// cannot be written is said in the warnings, the save goes on.
pub fn save_text(path: &Path, text: &str, expect: &str) -> Result<Saved, String> {
    if let Some(why) = open_refusal(path) {
        return Err(why);
    }
    let mut warnings = check(text).map_err(|e| format!("Not saved: {e}"))?;
    let _guard = write_lock();
    let cur = read_current(path)?;
    if cur != expect {
        return Err(format!("Not saved: {} changed on disk after the editor read it. Choose Reload or Keep in the editor, then save again.", name_of(path)));
    }
    if cur != text && path.exists() {
        let bak = path.with_extension("toml.bak");
        let _ = std::fs::remove_file(&bak);
        if let Err(e) = write_new_like(path, &bak, cur.as_bytes()) {
            eprintln!("forgetty-win: backup {} not written: {e}", bak.display());
            warnings.insert(0, format!("The backup {} was not written ({e})", bak.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        }
    }
    write_atomic(path, text, Some(expect))?;
    Ok(Saved { text: text.to_string(), warnings })
}

/// The text of the file for the editor: what is on disk, or empty when there is no file.
pub fn read_for_editor(path: &Path) -> Result<String, String> {
    if let Some(why) = open_refusal(path) {
        return Err(why);
    }
    read_current(path).map_err(|e| e.replacen("Not saved: ", "", 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# my settings\nfuture_key = 1   # keep me\nbell_mode = \"visual\"  # how loud\n\n[keybindings]\n# the tabs\nnew_tab = [\"ctrl+shift+t\", \"ctrl+alt+t\"]\n\n[[profiles]]\nname = \"A\"\ncommand = \"cmd.exe\"\n";

    #[test]
    fn a_change_keeps_comments_unknown_keys_and_the_rest() {
        let out = apply(SAMPLE, &[Edit::set("bell_mode", V::Str("both".into()))]).unwrap();
        assert!(out.contains("bell_mode = \"both\"  # how loud"), "{out}");
        assert!(out.contains("# my settings\nfuture_key = 1   # keep me"));
        assert!(out.contains("# the tabs\nnew_tab = [\"ctrl+shift+t\", \"ctrl+alt+t\"]"));
        assert!(out.contains("[[profiles]]\nname = \"A\""));
        assert!(Config::parse(&out).is_ok());
    }

    #[test]
    fn a_new_top_level_key_goes_before_the_tables_and_a_new_table_is_made() {
        let out = apply(SAMPLE, &[Edit::set("smart_copy", V::Bool(false)), Edit::set("font.family", V::Str("Consolas".into()))]).unwrap();
        let top = out.find("smart_copy = false").unwrap();
        assert!(top < out.find("[keybindings]").unwrap(), "{out}");
        assert!(out.contains("[font]\nfamily = \"Consolas\""), "{out}");
        let c = Config::parse(&out).unwrap().config;
        assert!(!c.smart_copy);
        assert_eq!(c.font_family, "Consolas");
    }

    #[test]
    fn keybinding_values_are_a_string_for_one_chord_and_an_array_for_more() {
        let out = apply("", &[Edit::set("keybindings.search", V::List(vec!["ctrl+alt+s".into()])), Edit::set("keybindings.new_tab", V::List(vec!["ctrl+t".into(), "ctrl+alt+t".into()]))]).unwrap();
        assert!(out.contains("search = \"ctrl+alt+s\""), "{out}");
        assert!(out.contains("new_tab = [\"ctrl+t\", \"ctrl+alt+t\"]"), "{out}");
        let c = Config::parse(&out).unwrap().config;
        assert_eq!(c.keybindings["new_tab"], vec!["ctrl+t".to_string(), "ctrl+alt+t".to_string()]);
        assert_eq!(c.keybindings["search"], vec!["ctrl+alt+s".to_string()]);
    }

    #[test]
    fn removing_the_last_key_removes_a_table_without_comments_only() {
        let out = apply("[keybindings]\nsearch = \"ctrl+alt+s\"\n", &[Edit::remove("keybindings.search")]).unwrap();
        assert!(!out.contains("keybindings"), "{out}");
        let kept = apply("# mine\n[keybindings]\nsearch = \"ctrl+alt+s\"\n", &[Edit::remove("keybindings.search")]).unwrap();
        assert!(kept.contains("[keybindings]") && kept.contains("# mine"), "{kept}");
        // A key that is not there is not an error.
        assert_eq!(apply("a = 1\n", &[Edit::remove("b"), Edit::remove("t.k")]).unwrap(), "a = 1\n");
    }

    #[test]
    fn a_table_where_a_key_is_needed_is_an_error_not_a_loss() {
        let e = apply("font = \"x\"\n", &[Edit::set("font.family", V::Str("y".into()))]).unwrap_err();
        assert!(e.contains("font"), "{e}");
        assert!(apply("a = = 1", &[]).is_err());
    }

    #[test]
    fn numbers_and_text_are_written_as_toml() {
        let out = apply("", &[Edit::set("font_size", V::Float(12.0)), Edit::set("paste_warn_size", V::Int(2048)), Edit::set("shell", V::Str("C:\\a b\\p.exe -x".into()))]).unwrap();
        assert!(out.contains("font_size = 12.0") && out.contains("paste_warn_size = 2048"), "{out}");
        assert_eq!(Config::parse(&out).map(|l| l.config.paste_warn_size).unwrap(), 2048);
        assert!(toml::from_str::<toml::Table>(&out).unwrap()["shell"].as_str() == Some("C:\\a b\\p.exe -x"));
    }

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_save_edits_the_file_and_a_broken_file_is_never_written_over() {
        let d = dir();
        let p = d.path().join("config.toml");
        std::fs::write(&p, SAMPLE).unwrap();
        let s = save_edits(&p, &[Edit::set("bell_mode", V::Str("none".into()))]).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), s.text);
        assert!(s.text.contains("# my settings"));
        // A file that does not load: refused, bytes unchanged.
        std::fs::write(&p, "bell_mode = \n").unwrap();
        let e = save_edits(&p, &[Edit::set("smart_copy", V::Bool(false))]).unwrap_err();
        assert!(e.contains("does not load") && e.contains("TOML editor"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "bell_mode = \n");
        // A change that makes the file invalid is refused too.
        std::fs::write(&p, "a = 1\n").unwrap();
        let e = save_edits(&p, &[Edit::set("keymap", V::Str("emacs".into()))]).unwrap_err();
        assert!(e.contains("invalid") && e.contains("emacs"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a = 1\n");
    }

    #[test]
    fn a_missing_file_is_made_and_the_editor_repairs_a_broken_one() {
        let d = dir();
        let p = d.path().join("sub").join("config.toml");
        let s = save_edits(&p, &[Edit::set("smart_copy", V::Bool(false))]).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), s.text);
        std::fs::write(&p, "bell_mode = \n").unwrap();
        // Bad text: refused, file unchanged, no backup.
        assert!(save_text(&p, "bell_mode = \"loud\"\n", "bell_mode = \n").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "bell_mode = \n");
        // Good text repairs, and the old one is kept beside it.
        save_text(&p, "bell_mode = \"audio\"\n", "bell_mode = \n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "bell_mode = \"audio\"\n");
        assert_eq!(std::fs::read_to_string(p.with_extension("toml.bak")).unwrap(), "bell_mode = \n");
        let left: Vec<_> = std::fs::read_dir(d.path().join("sub")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
        assert!(left.is_empty(), "temporary files left: {left:?}");
    }

    #[test]
    fn the_editor_never_overwrites_a_file_that_changed_on_disk() {
        let d = dir();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "smart_copy = true\n").unwrap();
        // The editor read "smart_copy = true", a row toggle then changed the file.
        save_edits(&p, &[Edit::set("smart_copy", V::Bool(false))]).unwrap();
        let e = save_text(&p, "bell_mode = \"audio\"\n", "smart_copy = true\n").unwrap_err();
        assert!(e.contains("changed on disk"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "smart_copy = false\n");
        // With the text it read now, the save goes through.
        save_text(&p, "bell_mode = \"audio\"\n", "smart_copy = false\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "bell_mode = \"audio\"\n");
    }

    #[test]
    fn two_writers_in_a_row_both_land_and_a_stale_tmp_is_swept() {
        let d = dir();
        let p = d.path().join("config.toml");
        let stale = d.path().join("config.toml.1.1.tmp");
        std::fs::write(&stale, "junk").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&stale).unwrap().set_modified(old).unwrap();
        let fresh = d.path().join("config.toml.2.2.tmp");
        std::fs::write(&fresh, "other writer").unwrap();
        let hs: Vec<_> = (0..6)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    let key = ["smart_copy", "copy_on_select", "cursor_blink"][i % 3];
                    save_edits(&p, &[Edit::set(key, V::Bool(i % 2 == 0))]).unwrap();
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let text = std::fs::read_to_string(&p).unwrap();
        for k in ["smart_copy", "copy_on_select", "cursor_blink"] {
            assert!(text.contains(k), "{k} lost: {text}");
        }
        assert!(!stale.exists());
        assert!(fresh.exists(), "a recent tmp may belong to a running save");
    }

    #[test]
    fn inline_tables_take_set_and_remove() {
        let d = dir();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "keybindings = { new_tab = \"ctrl+k\" }\n").unwrap();
        let s = save_edits(&p, &[Edit::set("keybindings.close_tab", V::Str("ctrl+j".into()))]).unwrap();
        assert!(s.text.contains("new_tab") && s.text.contains("close_tab"), "{}", s.text);
        let s = save_edits(&p, &[Edit::remove("keybindings.new_tab")]).unwrap();
        assert!(!s.text.contains("new_tab") && s.text.contains("close_tab"), "{}", s.text);
    }

    #[test]
    fn only_a_local_toml_path_is_written() {
        let d = dir();
        let p = d.path().join("config.txt");
        assert!(save_edits(&p, &[Edit::set("smart_copy", V::Bool(false))]).is_err());
        assert!(!p.exists());
        assert!(save_text(&p, "a = 1\n", "").is_err());
        assert!(read_for_editor(&p).is_err());
    }
}
