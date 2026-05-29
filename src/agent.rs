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
}

/// One Claude project node.
#[derive(Debug, Clone)]
pub struct Agent {
    /// Working directory — the project root and stable identity key.
    pub cwd: PathBuf,
    /// Stable tmux window id, if this agent lives in enxame's tmux session.
    /// `Some` => openable; `None` => external claude (dimmed, monitor-only).
    pub window_id: Option<String>,
    /// Running `claude` PIDs detected at this cwd (may be empty right after a
    /// managed launch, before claude has fully started).
    pub pids: Vec<u32>,
    /// Resolved transcript file, once located.
    pub transcript: Option<PathBuf>,
    pub state: AgentState,
}

impl Agent {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            window_id: None,
            pids: Vec::new(),
            transcript: None,
            state: AgentState::default(),
        }
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
