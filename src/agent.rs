//! Agent model: a Claude Code "project" node, keyed by working directory.
//!
//! An agent is keyed by its cwd (the project root shown in the block diagram).
//! It may be backed by one or more running `claude` processes (`pids`) and, if
//! enxame launched it in its own tmux window, by an openable `window_id`.
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
    /// Stable tmux window id, if this agent lives in enxame's tmux session.
    /// `Some` => openable; `None` => external claude (dimmed, monitor-only).
    pub window_id: Option<String>,
    /// Running `claude` PIDs detected at this cwd (may be empty right after a
    /// managed launch, before claude has fully started).
    pub pids: Vec<u32>,
    /// claude session id this agent runs (set when enxame launched it with
    /// `--session-id`), used to resolve its exact transcript file.
    pub session_id: Option<String>,
    /// Instruction queued to auto-send when this agent next finishes a turn.
    pub pending: Option<String>,
    /// Project (from `~/.enxame/projects.json`) this agent's cwd falls under.
    pub project_name: Option<String>,
    pub project_color: Option<String>,
    /// Resolved transcript file, once located.
    pub transcript: Option<PathBuf>,
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

    /// Whether this agent's terminal can be opened (it's in enxame's tmux).
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
