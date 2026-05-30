//! Project config at `~/.phasor/projects.json`.
//!
//! A project maps a directory **prefix** to a display **name** and group
//! **color**. An agent whose cwd is under a project's prefix is shown with that
//! project's name and color (longest-prefix match wins). Editable from both the
//! TUI (opens `$EDITOR`) and the web (JSON editor → save).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
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
    let Some(p) = path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Save projects, creating `~/.phasor` if needed.
pub fn save(projects: &[Project]) -> Result<()> {
    let p = path().context("no home dir")?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let text = serde_json::to_string_pretty(projects)?;
    std::fs::write(&p, text).context("writing projects.json")?;
    Ok(())
}

/// Resolve the project a cwd belongs to (longest matching prefix).
pub fn resolve<'a>(projects: &'a [Project], cwd: &Path) -> Option<&'a Project> {
    projects
        .iter()
        .filter(|pr| !pr.prefix.is_empty() && cwd.starts_with(Path::new(&pr.prefix)))
        .max_by_key(|pr| pr.prefix.len())
}
