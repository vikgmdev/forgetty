//! `forgetty skill setup|status|show|remove`: install a short skill file that
//! teaches a coding agent to use the CLI.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::cli::CliError;

type Result<T> = std::result::Result<T, CliError>;

/// The skill text, the same for every agent.
pub const SKILL: &str = include_str!("skill.md");

/// Directory name of the skill.
const NAME: &str = "forgetty";
/// File name of the skill.
const FILE: &str = "SKILL.md";
/// First lines of the skill file. `remove` deletes only files that start so.
const HEADER: &str = "---\nname: forgetty\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    Claude,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Scope {
    User,
    Project,
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// Where the skill file goes: a base directory and the components below it.
///
/// - Claude Code: `~/.claude/skills/forgetty/SKILL.md` (user),
///   `.claude/skills/forgetty/SKILL.md` in the current directory (project).
/// - Codex: `$CODEX_HOME/skills/forgetty/SKILL.md` (default `~/.codex`) for user,
///   `.agents/skills/forgetty/SKILL.md` in the current directory for project.
///
/// The base (home, cwd or `CODEX_HOME`) may be a symlink. No component below it
/// may be one: a cloned repository could plant `.claude -> /somewhere` and make
/// a write land outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub base: PathBuf,
    pub rel: Vec<&'static str>,
}

impl Target {
    pub fn path(&self) -> PathBuf {
        self.rel.iter().fold(self.base.clone(), |p, c| p.join(c))
    }
}

pub fn target(agent: Agent, scope: Scope, home: &Path, cwd: &Path) -> Target {
    match (agent, scope) {
        (Agent::Claude, Scope::User) => {
            Target { base: home.to_path_buf(), rel: vec![".claude", "skills", NAME, FILE] }
        }
        (Agent::Claude, Scope::Project) => {
            Target { base: cwd.to_path_buf(), rel: vec![".claude", "skills", NAME, FILE] }
        }
        (Agent::Codex, Scope::User) => match std::env::var_os("CODEX_HOME") {
            Some(h) if !h.is_empty() => {
                Target { base: PathBuf::from(h), rel: vec!["skills", NAME, FILE] }
            }
            _ => Target { base: home.to_path_buf(), rel: vec![".codex", "skills", NAME, FILE] },
        },
        (Agent::Codex, Scope::Project) => {
            Target { base: cwd.to_path_buf(), rel: vec![".agents", "skills", NAME, FILE] }
        }
    }
}

#[cfg(test)]
pub fn skill_path(agent: Agent, scope: Scope, home: &Path, cwd: &Path) -> PathBuf {
    target(agent, scope, home, cwd).path()
}

fn locations() -> Result<(PathBuf, PathBuf)> {
    let home = dirs::home_dir()
        .ok_or_else(|| CliError::Failed("cannot find the home directory".to_string()))?;
    let cwd = std::env::current_dir().map_err(|e| CliError::Failed(e.to_string()))?;
    Ok((home, cwd))
}

fn target_for(agent: Agent, scope: Scope) -> Result<Target> {
    let (home, cwd) = locations()?;
    Ok(target(agent, scope, &home, &cwd))
}

fn refuse(path: &Path, why: &str) -> CliError {
    CliError::Failed(format!("refusing to use {}: {why}", path.display()))
}

/// Walk each component below the base. Any existing component that is a
/// symlink, or a non-directory where a directory belongs, is refused.
/// Missing components are fine.
fn check_no_symlinks(t: &Target) -> Result<()> {
    let mut cur = t.base.clone();
    for (i, comp) in t.rel.iter().enumerate() {
        cur = cur.join(comp);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return Err(refuse(&cur, "it is a symlink")),
            Ok(m) if i + 1 < t.rel.len() && !m.is_dir() => {
                return Err(refuse(&cur, "it is not a directory"))
            }
            Ok(m) if i + 1 == t.rel.len() && !m.is_file() => {
                return Err(refuse(&cur, "it is not a regular file"))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(CliError::Failed(format!("stat {}: {e}", cur.display()))),
        }
    }
    Ok(())
}

/// Create the directories below the base one by one (never through a link),
/// then check that the result is still under the canonical base.
fn make_dirs(t: &Target) -> Result<PathBuf> {
    let mut cur = t.base.clone();
    for comp in &t.rel[..t.rel.len() - 1] {
        cur = cur.join(comp);
        match std::fs::create_dir(&cur) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(CliError::Failed(format!("create {}: {e}", cur.display()))),
        }
        // A link planted after the first check is caught here.
        let m = std::fs::symlink_metadata(&cur)
            .map_err(|e| CliError::Failed(format!("stat {}: {e}", cur.display())))?;
        if m.file_type().is_symlink() || !m.is_dir() {
            return Err(refuse(&cur, "it is not a plain directory"));
        }
    }
    let base = std::fs::canonicalize(&t.base)
        .map_err(|e| CliError::Failed(format!("resolve {}: {e}", t.base.display())))?;
    let dir = std::fs::canonicalize(&cur)
        .map_err(|e| CliError::Failed(format!("resolve {}: {e}", cur.display())))?;
    if !dir.starts_with(&base) {
        return Err(refuse(&cur, "it resolves outside the base directory"));
    }
    Ok(cur)
}

/// Write `SKILL` through a new temp file in the same directory (`create_new`,
/// `O_NOFOLLOW`), then rename it into place.
fn write_atomic(dir: &Path, name: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .map_err(|e| CliError::Failed(format!("create {}: {e}", tmp.display())))?;
    let res = f.write_all(SKILL.as_bytes()).and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(CliError::Failed(format!("write {}: {e}", tmp.display())));
    }
    std::fs::rename(&tmp, dir.join(name)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        CliError::Failed(format!("rename into {}: {e}", dir.display()))
    })
}

/// Write the skill file. Returns whether it changed.
pub fn setup_at(t: &Target) -> Result<bool> {
    check_no_symlinks(t)?;
    let path = t.path();
    if std::fs::read_to_string(&path).is_ok_and(|have| have == SKILL) {
        return Ok(false);
    }
    let dir = make_dirs(t)?;
    write_atomic(&dir, FILE)?;
    Ok(true)
}

/// `setup`: write the skill file.
pub fn setup(agent: Agent, scope: Scope) -> Result<()> {
    let t = target_for(agent, scope)?;
    let changed = setup_at(&t)?;
    println!("{} {}", if changed { "wrote" } else { "up to date:" }, t.path().display());
    Ok(())
}

/// State of one skill file.
fn state(t: &Target) -> &'static str {
    if check_no_symlinks(t).is_err() {
        return "unsafe";
    }
    match std::fs::read_to_string(t.path()) {
        Ok(have) if have == SKILL => "current",
        Ok(_) => "outdated",
        Err(_) => "missing",
    }
}

/// `status`: show every agent and scope, or the ones asked for.
pub fn status(agent: Option<Agent>, scope: Option<Scope>, json_out: bool) -> Result<()> {
    let (home, cwd) = locations()?;
    let agents = agent.map(|a| vec![a]).unwrap_or_else(|| vec![Agent::Claude, Agent::Codex]);
    let scopes = scope.map(|s| vec![s]).unwrap_or_else(|| vec![Scope::User, Scope::Project]);
    let mut rows = Vec::new();
    for a in &agents {
        for s in &scopes {
            let t = target(*a, *s, &home, &cwd);
            rows.push(json!({
                "agent": a.as_str(),
                "scope": s.as_str(),
                "path": t.path().display().to_string(),
                "state": state(&t),
            }));
        }
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&Value::Array(rows)).unwrap_or_default());
        return Ok(());
    }
    for r in &rows {
        println!(
            "{:<7} {:<8} {:<9} {}",
            r["agent"].as_str().unwrap_or(""),
            r["scope"].as_str().unwrap_or(""),
            r["state"].as_str().unwrap_or(""),
            r["path"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

/// Remove the skill file. Only a regular file that starts with our header goes.
/// Returns whether a file was removed.
pub fn remove_at(t: &Target) -> Result<bool> {
    check_no_symlinks(t)?;
    let path = t.path();
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(CliError::Failed(format!("stat {}: {e}", path.display()))),
        Ok(m) if !m.is_file() => return Err(refuse(&path, "it is not a regular file")),
        Ok(_) => {}
    }
    let head = std::fs::read(&path)
        .map_err(|e| CliError::Failed(format!("read {}: {e}", path.display())))?;
    if !head.starts_with(HEADER.as_bytes()) {
        return Err(refuse(&path, "it was not written by `forgetty skill setup`"));
    }
    std::fs::remove_file(&path)
        .map_err(|e| CliError::Failed(format!("remove {}: {e}", path.display())))?;
    if let Some(dir) = path.parent() {
        // Only an empty directory goes. Anything else in it is not ours.
        let _ = std::fs::remove_dir(dir);
    }
    Ok(true)
}

/// `show`: print the skill text.
pub fn show() {
    print!("{SKILL}");
}

/// `remove`: delete the skill file and its directory.
pub fn remove(agent: Agent, scope: Scope) -> Result<()> {
    let t = target_for(agent, scope)?;
    if remove_at(&t)? {
        println!("removed {}", t.path().display());
    } else {
        println!("not installed: {}", t.path().display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let home = Path::new("/h");
        let cwd = Path::new("/p");
        assert_eq!(
            skill_path(Agent::Claude, Scope::User, home, cwd),
            Path::new("/h/.claude/skills/forgetty/SKILL.md")
        );
        assert_eq!(
            skill_path(Agent::Claude, Scope::Project, home, cwd),
            Path::new("/p/.claude/skills/forgetty/SKILL.md")
        );
        assert_eq!(
            skill_path(Agent::Codex, Scope::Project, home, cwd),
            Path::new("/p/.agents/skills/forgetty/SKILL.md")
        );
    }

    fn dirs() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    fn outside_is_untouched(outside: &Path, names: &[&str]) {
        let mut seen: Vec<String> = std::fs::read_dir(outside)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        seen.sort();
        let mut want: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(seen, want, "files outside changed");
    }

    #[test]
    fn setup_and_remove_work_in_a_plain_tree() {
        let (proj, _) = dirs();
        let t = target(Agent::Claude, Scope::Project, Path::new("/nope"), proj.path());
        assert!(setup_at(&t).unwrap());
        assert!(!setup_at(&t).unwrap(), "second setup changes nothing");
        assert_eq!(std::fs::read_to_string(t.path()).unwrap(), SKILL);
        assert!(remove_at(&t).unwrap());
        assert!(!t.path().exists());
        assert!(!remove_at(&t).unwrap());
    }

    #[test]
    fn symlinked_dot_claude_is_refused() {
        let (proj, outside) = dirs();
        std::fs::write(outside.path().join("precious"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.path(), proj.path().join(".claude")).unwrap();
        let t = target(Agent::Claude, Scope::Project, Path::new("/nope"), proj.path());
        assert!(setup_at(&t).is_err());
        assert!(remove_at(&t).is_err());
        assert_eq!(state(&t), "unsafe");
        outside_is_untouched(outside.path(), &["precious"]);
    }

    #[test]
    fn symlinked_skills_dir_is_refused() {
        let (proj, outside) = dirs();
        std::fs::write(outside.path().join("precious"), "keep").unwrap();
        std::fs::create_dir(proj.path().join(".claude")).unwrap();
        std::os::unix::fs::symlink(outside.path(), proj.path().join(".claude/skills")).unwrap();
        let t = target(Agent::Claude, Scope::Project, Path::new("/nope"), proj.path());
        assert!(setup_at(&t).is_err());
        assert!(remove_at(&t).is_err());
        outside_is_untouched(outside.path(), &["precious"]);
    }

    #[test]
    fn symlinked_target_file_is_refused_and_not_overwritten_or_deleted() {
        let (proj, outside) = dirs();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, format!("{HEADER}precious")).unwrap();
        let dir = proj.path().join(".claude/skills/forgetty");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("SKILL.md")).unwrap();
        let t = target(Agent::Claude, Scope::Project, Path::new("/nope"), proj.path());
        assert!(setup_at(&t).is_err());
        assert!(remove_at(&t).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), format!("{HEADER}precious"));
        outside_is_untouched(outside.path(), &["victim"]);
    }

    #[test]
    fn symlinked_forgetty_dir_is_refused() {
        let (proj, outside) = dirs();
        std::fs::create_dir_all(proj.path().join(".claude/skills")).unwrap();
        std::os::unix::fs::symlink(outside.path(), proj.path().join(".claude/skills/forgetty"))
            .unwrap();
        let t = target(Agent::Claude, Scope::Project, Path::new("/nope"), proj.path());
        assert!(setup_at(&t).is_err());
        outside_is_untouched(outside.path(), &[]);
    }

    #[test]
    fn a_symlinked_base_is_allowed() {
        // /home or the project directory itself may be a link.
        let (real, links) = dirs();
        let link = links.path().join("proj");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let t = target(Agent::Claude, Scope::Project, Path::new("/nope"), &link);
        assert!(setup_at(&t).unwrap());
        assert!(real.path().join(".claude/skills/forgetty/SKILL.md").is_file());
    }

    #[test]
    fn remove_deletes_only_our_files() {
        let (proj, _) = dirs();
        let t = target(Agent::Claude, Scope::Project, Path::new("/nope"), proj.path());
        let dir = proj.path().join(".claude/skills/forgetty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "someone else's skill").unwrap();
        assert!(remove_at(&t).is_err());
        assert!(dir.join("SKILL.md").exists());
        // A directory where the file belongs is refused too.
        std::fs::remove_file(dir.join("SKILL.md")).unwrap();
        std::fs::create_dir(dir.join("SKILL.md")).unwrap();
        assert!(remove_at(&t).is_err());
        assert!(setup_at(&t).is_err());
    }

    #[test]
    fn setup_leaves_no_temp_file() {
        let (proj, _) = dirs();
        let t = target(Agent::Claude, Scope::Project, Path::new("/nope"), proj.path());
        setup_at(&t).unwrap();
        let names: Vec<_> = std::fs::read_dir(t.path().parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["SKILL.md".to_string()]);
    }

    #[test]
    fn skill_text_is_short_and_has_frontmatter() {
        assert!(SKILL.starts_with("---\nname: forgetty\ndescription: "));
        assert!(SKILL.lines().count() < 60, "keep the skill short");
        for cmd in [
            "ls",
            "new",
            "run",
            "send ",
            "send-key",
            "capture",
            "wait",
            "events",
            "label",
            "api describe",
        ] {
            assert!(SKILL.contains(&format!("forgetty {cmd}")), "skill lacks `forgetty {cmd}`");
        }
    }

    #[test]
    fn every_command_in_the_skill_exists() {
        // Each `forgetty X` the skill names must be a real subcommand.
        use clap::CommandFactory;
        let cmd = crate::Cli::command();
        let names: Vec<&str> = cmd.get_subcommands().map(|c| c.get_name()).collect();
        for word in SKILL.split("`forgetty ").skip(1) {
            let first = word.split([' ', '`', ',', ')']).next().unwrap_or("");
            if first.is_empty() || first.starts_with('-') {
                continue;
            }
            assert!(names.contains(&first), "skill names unknown command {first:?}");
        }
    }
}
