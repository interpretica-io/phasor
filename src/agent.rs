//! Agent model: a Claude Code "project" node, keyed by working directory.
//!
//! An agent is keyed by its cwd (the project root shown in the block diagram).
//! It may be backed by one or more running `claude` processes (`pids`) and, if
//! phasor launched it in its own tmux window, by an openable `window_id`.
//! Agents without a tmux window are external claudes — monitored, dimmed, and
//! not openable.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::SystemTime;

/// Liveness of an agent, derived from transcript activity recency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    /// Produced output very recently — actively working.
    Working,
    /// Quiet for a while — likely waiting for input or done.
    Idle,
    /// No transcript located yet (just launched, or claude not started).
    #[default]
    Unknown,
}

/// State parsed from a Claude Code transcript for one agent.
#[derive(Debug, Clone, Default)]
pub struct AgentState {
    /// Auto-generated session title (`ai-title`), our display name.
    pub title: Option<String>,
    /// Most recent assistant text snippets ("last phrases"), newest last.
    pub last_phrases: VecDeque<String>,
    /// Directories the agent has touched (root cwd + tool paths).
    pub work_dirs: Vec<PathBuf>,
    /// Todo progress as (completed, total) from the latest TodoWrite.
    pub todos: Option<(usize, usize)>,
    /// Timestamp of the last transcript activity.
    pub last_activity: Option<SystemTime>,
    /// Liveness derived from how recently the transcript changed.
    pub status: Status,
    /// Identifier (uuid) of the most recent completed turn (`stop_reason ==
    /// "end_turn"`). A change in this marks a freshly completed task.
    pub final_marker: Option<String>,
    /// Whether the last finished turn's answer contained the
    /// "FINISHED COMPLETELY" sentinel (stops a repeating auto-instruction).
    pub final_says_done: bool,
}

/// Folders that are Claude's own scaffolding rather than the agent's work —
/// excluded from the displayed folder lists.
pub fn is_noise_folder(name: &str) -> bool {
    matches!(name, "memory" | ".claude")
}

/// One agent node — a single tmux window or a single external claude process
/// (agents are NOT grouped by folder).
#[derive(Debug, Clone)]
pub struct Agent {
    /// Stable node identity: the tmux window id (e.g. `@3`) for managed agents,
    /// or `pid:<n>` for external claude processes.
    pub id: String,
    /// Working directory.
    pub cwd: PathBuf,
    /// Stable tmux window id, if this agent lives in phasor's tmux session.
    /// `Some` => openable; `None` => external claude (dimmed, monitor-only).
    pub window_id: Option<String>,
    /// Running `claude` PIDs detected at this cwd (may be empty right after a
    /// managed launch, before claude has fully started).
    pub pids: Vec<u32>,
    /// claude session id this agent runs (set when phasor launched it with
    /// `--session-id`), used to resolve its exact transcript file.
    pub session_id: Option<String>,
    /// Instruction queued to auto-send when this agent next finishes a turn.
    pub pending: Option<String>,
    /// Project (from `~/.phasor/projects.json`) this agent's cwd falls under.
    pub project_name: Option<String>,
    /// The matched project's group colour (hex), if any.
    pub project_color: Option<String>,
    /// Resolved transcript file, once located.
    pub transcript: Option<PathBuf>,
    /// State parsed from the transcript (title, phrases, todos, status, …).
    pub state: AgentState,
    /// Recent activity load samples (0-100%), newest last. Derived from how
    /// fast the transcript is growing.
    pub activity: VecDeque<u8>,
    /// Last observed transcript byte length (scanner bookkeeping for deltas).
    pub last_len: u64,
    /// When the agent most recently completed a task (finished a turn).
    pub completed_at: Option<SystemTime>,
    /// Count of completed tasks — increments on each completion so the web UI
    /// can fire its animation exactly once per event.
    pub completions: u64,
}

impl Agent {
    /// Create a bare agent with the given node id and cwd; all derived state
    /// (transcript, activity, project, …) is filled in later by the scanner.
    pub fn new(id: String, cwd: PathBuf) -> Self {
        Self {
            id,
            cwd,
            window_id: None,
            pids: Vec::new(),
            session_id: None,
            pending: None,
            project_name: None,
            project_color: None,
            transcript: None,
            state: AgentState::default(),
            activity: VecDeque::new(),
            last_len: 0,
            completed_at: None,
            completions: 0,
        }
    }

    /// Whether the agent completed a task within the last `secs` seconds.
    pub fn just_completed(&self, secs: u64) -> bool {
        self.completed_at
            .and_then(|t| t.elapsed().ok())
            .map(|e| e.as_secs() < secs)
            .unwrap_or(false)
    }

    /// Current activity load (0-100%), the latest sample.
    pub fn load(&self) -> u8 {
        self.activity.back().copied().unwrap_or(0)
    }

    /// Whether this agent's terminal can be opened (it's in phasor's tmux).
    pub fn openable(&self) -> bool {
        self.window_id.is_some()
    }

    /// Display label: session title if known, else the directory name.
    pub fn label(&self) -> String {
        if let Some(t) = &self.state.title {
            return t.clone();
        }
        self.cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.cwd.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn agent(id: &str, cwd: &str) -> Agent {
        Agent::new(id.into(), PathBuf::from(cwd))
    }

    #[test]
    fn noise_folders() {
        assert!(is_noise_folder("memory"));
        assert!(is_noise_folder(".claude"));
        assert!(!is_noise_folder("src"));
        assert!(!is_noise_folder("Memory")); // case-sensitive
    }

    #[test]
    fn new_defaults() {
        let a = agent("@1", "/x");
        assert_eq!(a.id, "@1");
        assert_eq!(a.cwd, PathBuf::from("/x"));
        assert!(a.window_id.is_none());
        assert!(a.pids.is_empty());
        assert!(a.project_name.is_none());
        assert_eq!(a.completions, 0);
        assert_eq!(a.state.status, Status::Unknown);
    }

    #[test]
    fn openable_iff_window() {
        let mut a = agent("pid:5", "/x");
        assert!(!a.openable());
        a.window_id = Some("@3".into());
        assert!(a.openable());
    }

    #[test]
    fn label_prefers_title() {
        let mut a = agent("@1", "/home/u/proj");
        assert_eq!(a.label(), "proj");
        a.state.title = Some("Build the thing".into());
        assert_eq!(a.label(), "Build the thing");
    }

    #[test]
    fn label_falls_back_to_basename_then_full() {
        assert_eq!(agent("@1", "/home/u/myproj").label(), "myproj");
        // root has no file_name → full path
        assert_eq!(agent("@1", "/").label(), "/");
    }

    #[test]
    fn load_is_latest_sample() {
        let mut a = agent("@1", "/x");
        assert_eq!(a.load(), 0);
        a.activity.extend([5u8, 20, 88]);
        assert_eq!(a.load(), 88);
    }

    #[test]
    fn just_completed_window() {
        let mut a = agent("@1", "/x");
        assert!(!a.just_completed(20)); // none
        a.completed_at = Some(SystemTime::now());
        assert!(a.just_completed(20));
        a.completed_at = SystemTime::now().checked_sub(Duration::from_secs(100));
        assert!(!a.just_completed(20));
    }
}
