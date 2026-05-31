//! Saving and restoring the set of managed agent sessions.
//!
//! `phasor save` snapshots each managed tmux window's cwd + claude session id
//! to `~/.phasor/session.json`; `phasor restore` recreates a window per saved
//! entry, resuming the claude session (the transcript on disk is reused). This
//! lets a workspace survive a reboot or `tmux kill-server`, which the live tmux
//! server otherwise would not.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One restorable agent: where it ran and which claude session to resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedAgent {
    /// Working directory the agent ran in.
    pub cwd: PathBuf,
    /// claude session id (uuid) — used to resume the conversation.
    pub session_id: String,
    /// Window title at save time (cosmetic; re-derived from the transcript).
    #[serde(default)]
    pub title: Option<String>,
    /// Whether phasor managed this agent (it lived in phasor's tmux) at save
    /// time. External (monitor-only) agents are saved too, and `restore` brings
    /// every entry back as a managed window regardless.
    #[serde(default = "default_true")]
    pub managed: bool,
}

/// Serde default for [`SavedAgent::managed`]: snapshots written before this
/// field existed only ever contained managed agents.
fn default_true() -> bool {
    true
}

/// Path to the saved-session file (`~/.phasor/session.json`).
pub fn path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".phasor/session.json"))
}

/// Write a snapshot to `p`, creating parent dirs as needed.
pub fn save_to(p: &Path, agents: &[SavedAgent]) -> Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let text = serde_json::to_string_pretty(agents)?;
    std::fs::write(p, text).context("writing session.json")?;
    Ok(())
}

/// Read a snapshot from `p` (errors if missing or invalid).
pub fn load_from(p: &Path) -> Result<Vec<SavedAgent>> {
    let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    serde_json::from_str(&text).context("parsing session.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        p.push(format!("phasor-sess-{}-{}", std::process::id(), n));
        p
    }

    fn agent(cwd: &str, sid: &str, title: Option<&str>) -> SavedAgent {
        SavedAgent {
            cwd: PathBuf::from(cwd),
            session_id: sid.into(),
            title: title.map(|s| s.into()),
            managed: true,
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tmp();
        let file = dir.join("session.json");
        let agents = vec![
            agent(
                "/home/u/web",
                "11111111-1111-4111-8111-111111111111",
                Some("Web"),
            ),
            agent("/home/u/api", "22222222-2222-4222-8222-222222222222", None),
        ];
        save_to(&file, &agents).unwrap();
        let back = load_from(&file).unwrap();
        assert_eq!(back, agents);
        assert_eq!(back[0].title.as_deref(), Some("Web"));
        assert!(back[1].title.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_pretty_json() {
        let dir = tmp();
        let file = dir.join("session.json");
        save_to(&file, &[agent("/p", "sid", None)]).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains('\n'));
        assert!(text.contains("\"session_id\": \"sid\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_errors() {
        assert!(load_from(&tmp().join("nope.json")).is_err());
    }

    #[test]
    fn title_defaults_to_none_when_absent() {
        let a: Vec<SavedAgent> =
            serde_json::from_str(r#"[{"cwd":"/p","session_id":"x"}]"#).unwrap();
        assert!(a[0].title.is_none());
    }

    #[test]
    fn managed_defaults_true_but_parses_false() {
        // Old snapshots (no `managed` field) were all managed.
        let old: Vec<SavedAgent> =
            serde_json::from_str(r#"[{"cwd":"/p","session_id":"x"}]"#).unwrap();
        assert!(old[0].managed);
        // External agents are saved with managed = false.
        let ext: Vec<SavedAgent> =
            serde_json::from_str(r#"[{"cwd":"/p","session_id":"x","managed":false}]"#).unwrap();
        assert!(!ext[0].managed);
    }
}
