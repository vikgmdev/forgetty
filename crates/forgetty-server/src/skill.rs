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

/// Where the skill file goes.
///
/// - Claude Code: `~/.claude/skills/forgetty/SKILL.md` (user),
///   `.claude/skills/forgetty/SKILL.md` in the current directory (project).
/// - Codex: `$CODEX_HOME/skills/forgetty/SKILL.md` (default `~/.codex`) for user,
///   `.agents/skills/forgetty/SKILL.md` in the current directory for project.
pub fn skill_path(agent: Agent, scope: Scope, home: &Path, cwd: &Path) -> PathBuf {
    let base = match (agent, scope) {
        (Agent::Claude, Scope::User) => home.join(".claude").join("skills"),
        (Agent::Claude, Scope::Project) => cwd.join(".claude").join("skills"),
        (Agent::Codex, Scope::User) => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"))
            .join("skills"),
        (Agent::Codex, Scope::Project) => cwd.join(".agents").join("skills"),
    };
    base.join(NAME).join("SKILL.md")
}

fn locations() -> Result<(PathBuf, PathBuf)> {
    let home = dirs::home_dir()
        .ok_or_else(|| CliError::Failed("cannot find the home directory".to_string()))?;
    let cwd = std::env::current_dir().map_err(|e| CliError::Failed(e.to_string()))?;
    Ok((home, cwd))
}

fn path_for(agent: Agent, scope: Scope) -> Result<PathBuf> {
    let (home, cwd) = locations()?;
    Ok(skill_path(agent, scope, &home, &cwd))
}

/// `setup`: write the skill file. Returns what happened.
pub fn setup(agent: Agent, scope: Scope) -> Result<()> {
    let path = path_for(agent, scope)?;
    if std::fs::read_to_string(&path).is_ok_and(|have| have == SKILL) {
        println!("up to date: {}", path.display());
        return Ok(());
    }
    let dir = path.parent().ok_or_else(|| CliError::Failed("bad skill path".into()))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| CliError::Failed(format!("create {}: {e}", dir.display())))?;
    std::fs::write(&path, SKILL)
        .map_err(|e| CliError::Failed(format!("write {}: {e}", path.display())))?;
    println!("wrote {}", path.display());
    Ok(())
}

/// State of one skill file.
fn state(path: &Path) -> &'static str {
    match std::fs::read_to_string(path) {
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
            let path = skill_path(*a, *s, &home, &cwd);
            rows.push(json!({
                "agent": a.as_str(),
                "scope": s.as_str(),
                "path": path.display().to_string(),
                "state": state(&path),
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

/// `show`: print the skill text.
pub fn show() {
    print!("{SKILL}");
}

/// `remove`: delete the skill file and its directory.
pub fn remove(agent: Agent, scope: Scope) -> Result<()> {
    let path = path_for(agent, scope)?;
    if !path.exists() {
        println!("not installed: {}", path.display());
        return Ok(());
    }
    std::fs::remove_file(&path)
        .map_err(|e| CliError::Failed(format!("remove {}: {e}", path.display())))?;
    if let Some(dir) = path.parent() {
        // Only an empty directory goes. Anything else in it is not ours.
        let _ = std::fs::remove_dir(dir);
    }
    println!("removed {}", path.display());
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
