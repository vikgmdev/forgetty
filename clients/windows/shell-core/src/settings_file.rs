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

use toml_edit::{Array, DocumentMut, Item, Table, Value};

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
fn put(table: &mut Table, key: &str, v: Value) {
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
                        let table = item.as_table_mut().ok_or_else(|| format!("`{t}` in the file is not a table, so `{path}` cannot be set"))?;
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
                        let now_empty = match doc.as_table_mut().get_mut(t).and_then(Item::as_table_mut) {
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

/// Write `text` over `path` through a temporary file in the same folder. `expect` is what the
/// caller read; when the file differs now, somebody else changed it and nothing is written.
fn write_atomic(path: &Path, text: &str, expect: Option<&str>) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Not saved: {e}"))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("Not saved: {e}"))?;
    if let Some(expect) = expect {
        if read_current(path).ok().as_deref() != Some(expect) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("Not saved: {} changed while it was being saved. Try again.", name_of(path)));
        }
    }
    std::fs::rename(&tmp, path).map_err(|e| {
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

/// The TOML editor's save: `text` replaces the file when it loads. The old file is kept as
/// `config.toml.bak` (one copy, the one before this save).
pub fn save_text(path: &Path, text: &str) -> Result<Saved, String> {
    if let Some(why) = open_refusal(path) {
        return Err(why);
    }
    let warnings = check(text).map_err(|e| format!("Not saved: {e}"))?;
    if let Ok(old) = std::fs::read_to_string(path) {
        if old != text {
            let _ = std::fs::write(path.with_extension("toml.bak"), old);
        }
    }
    write_atomic(path, text, None)?;
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
        assert!(save_text(&p, "bell_mode = \"loud\"\n").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "bell_mode = \n");
        // Good text repairs, and the old one is kept beside it.
        save_text(&p, "bell_mode = \"audio\"\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "bell_mode = \"audio\"\n");
        assert_eq!(std::fs::read_to_string(p.with_extension("toml.bak")).unwrap(), "bell_mode = \n");
        assert!(!p.with_extension("toml.tmp").exists());
    }

    #[test]
    fn only_a_local_toml_path_is_written() {
        let d = dir();
        let p = d.path().join("config.txt");
        assert!(save_edits(&p, &[Edit::set("smart_copy", V::Bool(false))]).is_err());
        assert!(!p.exists());
        assert!(save_text(&p, "a = 1\n").is_err());
        assert!(read_for_editor(&p).is_err());
    }
}
