//! Application state and input handling (UI-framework agnostic).

use crate::agent::Agent;
use crate::{discover, transcript, tmux};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Interaction mode of the dashboard.
pub enum Mode {
    Normal,
    /// Entering a working directory for a new agent.
    NewAgent { input: String },
}

pub struct App {
    /// Agents sorted deterministically by cwd, rebuilt each poll.
    pub agents: Vec<Agent>,
    pub selected: usize,
    pub mode: Mode,
    pub status: String,
    pub should_quit: bool,
    /// When set, the main loop should suspend the TUI and attach this window.
    pub attach_to: Option<String>,
    /// Selection is tracked by cwd so it stays put as the list is rebuilt.
    selected_cwd: Option<PathBuf>,
    last_poll: SystemTime,
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            agents: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            status: "n: new · 1-9: jump · ↑/↓: select · Enter: open · d: kill · q: quit".into(),
            should_quit: false,
            attach_to: None,
            selected_cwd: None,
            last_poll: SystemTime::UNIX_EPOCH,
        };
        app.poll(); // populate immediately so the first frame isn't empty
        app
    }

    /// Handle a key event.
    pub fn on_key(&mut self, key: KeyEvent) {
        match &mut self.mode {
            Mode::Normal => self.on_key_normal(key),
            Mode::NewAgent { input } => match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => {
                    let path = input.clone();
                    self.mode = Mode::Normal;
                    if let Err(e) = self.spawn_agent(&path) {
                        self.status = format!("error: {e}");
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            },
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('n') => {
                let prefill = std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.mode = Mode::NewAgent { input: prefill };
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            // Quick-jump: 1-9 select agents 1..9, 0 selects the 10th. Selects
            // only — never opens.
            KeyCode::Char(c @ '0'..='9') => {
                let d = c.to_digit(10).unwrap() as usize;
                let idx = if d == 0 { 9 } else { d - 1 };
                self.select(idx);
            }
            KeyCode::Enter => self.open_selected(),
            KeyCode::Char('d') => self.kill_selected(),
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let n = self.agents.len();
        if n == 0 {
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, n as isize - 1) as usize;
        self.selected = next;
        self.selected_cwd = self.agents.get(next).map(|a| a.cwd.clone());
    }

    /// Select an agent by index (used by mouse clicks).
    pub fn select(&mut self, idx: usize) {
        if idx < self.agents.len() {
            self.selected = idx;
            self.selected_cwd = Some(self.agents[idx].cwd.clone());
        }
    }

    /// Open the selected agent's terminal, if it lives in enxame's tmux.
    pub fn open_selected(&mut self) {
        match self.agents.get(self.selected) {
            Some(a) if a.openable() => {
                self.attach_to = a.window_id.clone();
            }
            Some(_) => {
                self.status =
                    "external claude (not in an enxame tmux window) — monitor only".into();
            }
            None => {}
        }
    }

    fn kill_selected(&mut self) {
        match self.agents.get(self.selected) {
            Some(a) if a.openable() => {
                if let Some(id) = a.window_id.clone() {
                    let _ = tmux::kill_window(&id);
                    self.status = "agent window killed".into();
                    self.poll();
                }
            }
            Some(_) => {
                self.status = "external claude — enxame won't kill processes it didn't start".into();
            }
            None => {}
        }
    }

    /// Create a tmux window and launch claude in it. Discovery picks it up.
    fn spawn_agent(&mut self, raw_path: &str) -> Result<()> {
        let path = expand_path(raw_path);
        let canon = std::fs::canonicalize(&path)
            .map_err(|_| anyhow::anyhow!("not a directory: {}", path.display()))?;
        if !canon.is_dir() {
            anyhow::bail!("not a directory: {}", canon.display());
        }
        let name = canon
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "agent".into());
        tmux::new_window(&name, &canon.to_string_lossy(), "claude")?;
        self.selected_cwd = Some(canon);
        self.status = "agent started — claude launching".into();
        self.poll();
        Ok(())
    }

    /// Periodic refresh (rate-limited).
    pub fn maybe_poll(&mut self) {
        if self.last_poll.elapsed().unwrap_or(POLL_INTERVAL) < POLL_INTERVAL {
            return;
        }
        self.poll();
    }

    /// Discover all running claudes + enxame tmux windows, rebuild the agent
    /// list keyed by cwd, and refresh transcript-derived state.
    fn poll(&mut self) {
        self.last_poll = SystemTime::now();

        // Carry over cached state (transcript path, parsed state) by cwd.
        let mut prev: BTreeMap<PathBuf, Agent> =
            self.agents.drain(..).map(|a| (a.cwd.clone(), a)).collect();

        let discovered = discover::running_claudes();
        let windows = tmux::list_windows_with_cwd().unwrap_or_default();

        // Union of every cwd we know about: running claudes + enxame windows.
        let mut cwds: BTreeMap<PathBuf, (Vec<u32>, Option<String>)> = BTreeMap::new();
        for (cwd, pids) in discovered {
            cwds.entry(cwd).or_default().0 = pids;
        }
        for (win, cwd) in windows {
            cwds.entry(cwd).or_default().1 = Some(win.id);
        }

        let mut agents: Vec<Agent> = Vec::new();
        for (cwd, (pids, window_id)) in cwds {
            let mut agent = prev.remove(&cwd).unwrap_or_else(|| Agent::new(cwd.clone()));
            agent.pids = pids;
            agent.window_id = window_id;
            if agent.transcript.is_none() {
                agent.transcript = transcript::newest_session(&cwd, SystemTime::UNIX_EPOCH);
            }
            if let Some(t) = agent.transcript.clone() {
                match transcript::parse(&t, &cwd) {
                    Ok(state) => agent.state = state,
                    Err(_) => {}
                }
            }
            agents.push(agent);
        }

        self.agents = agents; // already sorted: BTreeMap iterates by cwd
        self.reconcile_selection();
    }

    /// Keep the selection pinned to the same cwd across rebuilds.
    fn reconcile_selection(&mut self) {
        if let Some(cwd) = &self.selected_cwd {
            if let Some(idx) = self.agents.iter().position(|a| &a.cwd == cwd) {
                self.selected = idx;
                return;
            }
        }
        if self.selected >= self.agents.len() {
            self.selected = self.agents.len().saturating_sub(1);
        }
        self.selected_cwd = self.agents.get(self.selected).map(|a| a.cwd.clone());
    }
}

/// Expand a leading `~` and make the path absolute against the process cwd.
fn expand_path(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix('~') {
        if let Some(home) = dirs::home_dir() {
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}
