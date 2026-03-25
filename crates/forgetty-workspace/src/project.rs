//! Project detection and configuration.
//!
//! Detects the type of project in a directory (e.g., Rust/Cargo, Node.js,
//! Python, Go) and applies project-specific terminal settings.

use std::path::{Path, PathBuf};

/// Known project types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectType {
    Rust,
    Node,
    Python,
    Go,
    Unknown,
}

/// Detect the project type by looking for well-known manifest files.
pub fn detect_project(dir: &Path) -> ProjectType {
    if dir.join("Cargo.toml").exists() {
        ProjectType::Rust
    } else if dir.join("package.json").exists() {
        ProjectType::Node
    } else if dir.join("pyproject.toml").exists()
        || dir.join("setup.py").exists()
        || dir.join("setup.cfg").exists()
    {
        ProjectType::Python
    } else if dir.join("go.mod").exists() {
        ProjectType::Go
    } else {
        ProjectType::Unknown
    }
}

/// Walk up from `start` looking for a project root (directory with a manifest).
///
/// Returns the first ancestor (including `start` itself) that contains a
/// recognised project file, or `None` if no root is found.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if detect_project(&dir) != ProjectType::Unknown {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Derive a human-readable project name from a root path.
///
/// Tries, in order:
/// 1. `Cargo.toml` — reads `[package] name`.
/// 2. `package.json` — reads `"name"`.
/// 3. Falls back to the directory name.
pub fn project_name(root: &Path) -> String {
    if let Some(name) = name_from_cargo_toml(root) {
        return name;
    }
    if let Some(name) = name_from_package_json(root) {
        return name;
    }
    root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "unnamed".into())
}

/// Try to read the package name from `Cargo.toml`.
fn name_from_cargo_toml(root: &Path) -> Option<String> {
    let path = root.join("Cargo.toml");
    let contents = std::fs::read_to_string(path).ok()?;
    let table: toml::Table = contents.parse().ok()?;
    table.get("package")?.get("name")?.as_str().map(String::from)
}

/// Try to read the name field from `package.json`.
fn name_from_package_json(root: &Path) -> Option<String> {
    let path = root.join("package.json");
    let contents = std::fs::read_to_string(path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&contents).ok()?;
    val.get("name")?.as_str().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detect_rust_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"foo\"\n").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Rust);
    }

    #[test]
    fn detect_node_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name":"bar"}"#).unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Node);
    }

    #[test]
    fn detect_unknown_project() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Unknown);
    }

    #[test]
    fn find_project_root_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"root\"\n").unwrap();
        let child = dir.path().join("src").join("bin");
        fs::create_dir_all(&child).unwrap();
        let root = find_project_root(&child).unwrap();
        assert_eq!(root, dir.path());
    }

    #[test]
    fn find_project_root_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        assert!(find_project_root(&deep).is_none());
    }

    #[test]
    fn project_name_from_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(project_name(dir.path()), "my-crate");
    }

    #[test]
    fn project_name_from_package_json() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name": "my-app", "version": "1.0.0"}"#)
            .unwrap();
        assert_eq!(project_name(dir.path()), "my-app");
    }

    #[test]
    fn project_name_falls_back_to_dir_name() {
        let dir = tempfile::tempdir().unwrap();
        let name = project_name(dir.path());
        // tempdir names are random, but should not be empty.
        assert!(!name.is_empty());
    }
}
