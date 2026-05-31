//! Project config at `~/.phasor/projects.json`.
//!
//! A project maps a directory **prefix** to a display **name** and group
//! **color**. An agent whose cwd is under a project's prefix is shown with that
//! project's name and color (longest-prefix match wins). Editable from both the
//! TUI (opens `$EDITOR`) and the web (JSON editor → save).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One configured project: a directory prefix tagged with a display name and
/// a group colour.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
    /// Display name shown on cards / contours.
    pub name: String,
    /// Absolute directory prefix (agents under it belong to this project).
    pub prefix: String,
    /// Group color, any CSS/hex string (e.g. `#ff8c42`).
    #[serde(default)]
    pub color: String,
}

/// Path to the projects config file.
pub fn path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".phasor/projects.json"))
}

/// Load the configured projects (empty list if missing/invalid).
pub fn load() -> Vec<Project> {
    match path() {
        Some(p) => load_from(&p),
        None => Vec::new(),
    }
}

/// Load projects from a specific file (empty on missing/invalid JSON).
fn load_from(p: &Path) -> Vec<Project> {
    let Ok(text) = std::fs::read_to_string(p) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Save projects, creating `~/.phasor` if needed.
pub fn save(projects: &[Project]) -> Result<()> {
    let p = path().context("no home dir")?;
    save_to(&p, projects)
}

/// Save projects to a specific file, creating parent dirs as needed.
fn save_to(p: &Path, projects: &[Project]) -> Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let text = serde_json::to_string_pretty(projects)?;
    std::fs::write(p, text).context("writing projects.json")?;
    Ok(())
}

/// Resolve the project a cwd belongs to (longest matching prefix).
pub fn resolve<'a>(projects: &'a [Project], cwd: &Path) -> Option<&'a Project> {
    projects
        .iter()
        .filter(|pr| !pr.prefix.is_empty() && cwd.starts_with(Path::new(&pr.prefix)))
        .max_by_key(|pr| pr.prefix.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        p.push(format!("phasor-cfg-{}-{}", std::process::id(), n));
        p
    }

    fn proj(name: &str, prefix: &str, color: &str) -> Project {
        Project {
            name: name.into(),
            prefix: prefix.into(),
            color: color.into(),
        }
    }

    #[test]
    fn resolve_none_when_empty() {
        assert!(resolve(&[], Path::new("/a/b")).is_none());
    }

    #[test]
    fn resolve_matches_simple_prefix() {
        let ps = [proj("P", "/home/u/src", "#fff000")];
        let m = resolve(&ps, Path::new("/home/u/src/app")).unwrap();
        assert_eq!(m.name, "P");
    }

    #[test]
    fn resolve_exact_prefix_equal_to_cwd() {
        let ps = [proj("P", "/home/u/src", "")];
        assert!(resolve(&ps, Path::new("/home/u/src")).is_some());
    }

    #[test]
    fn resolve_longest_prefix_wins() {
        let ps = [
            proj("Outer", "/home/u", ""),
            proj("Inner", "/home/u/src/app", ""),
            proj("Mid", "/home/u/src", ""),
        ];
        let m = resolve(&ps, Path::new("/home/u/src/app/x")).unwrap();
        assert_eq!(m.name, "Inner");
    }

    #[test]
    fn resolve_is_component_wise_not_string_prefix() {
        // "/a/foobar" must NOT match prefix "/a/foo" (different component).
        let ps = [proj("P", "/a/foo", "")];
        assert!(resolve(&ps, Path::new("/a/foobar")).is_none());
    }

    #[test]
    fn resolve_handles_trailing_slash_prefix() {
        let ps = [proj("P", "/a/foo/", "")];
        assert!(resolve(&ps, Path::new("/a/foo/bar")).is_some());
    }

    #[test]
    fn resolve_ignores_empty_prefix() {
        let ps = [proj("Empty", "", "#fff")];
        assert!(resolve(&ps, Path::new("/anything")).is_none());
    }

    #[test]
    fn resolve_no_match_for_sibling() {
        let ps = [proj("P", "/home/a", "")];
        assert!(resolve(&ps, Path::new("/home/b")).is_none());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tmp();
        let file = dir.join("projects.json");
        let ps = vec![
            proj("Visao", "/u/visao", "#0400fa"),
            proj("Mid", "/u/mid", "#10ad22"),
        ];
        save_to(&file, &ps).unwrap();
        assert!(file.exists());
        let back = load_from(&file);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "Visao");
        assert_eq!(back[1].prefix, "/u/mid");
        assert_eq!(back[0].color, "#0400fa");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_pretty_json() {
        let dir = tmp();
        let file = dir.join("projects.json");
        save_to(&file, &[proj("P", "/p", "#abc123")]).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains('\n'), "pretty JSON should be multi-line");
        assert!(text.contains("\"name\": \"P\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let file = tmp().join("nope.json");
        assert!(load_from(&file).is_empty());
    }

    #[test]
    fn load_invalid_json_is_empty() {
        let dir = tmp();
        let file = dir.join("projects.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "not json at all").unwrap();
        assert!(load_from(&file).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn color_defaults_when_absent() {
        let ps: Vec<Project> = serde_json::from_str(r#"[{"name":"P","prefix":"/p"}]"#).unwrap();
        assert_eq!(ps[0].color, "");
    }
}
