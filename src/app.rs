//! Application state and input handling (UI-framework agnostic).
//!
//! Discovery runs on a background thread (see [`crate::scan`]) and streams
//! fresh agent snapshots over a channel; the main loop never blocks on it.

use crate::agent::Agent;
use crate::{scan, tmux};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Instant;

/// Interaction mode of the dashboard.
pub enum Mode {
    Normal,
    /// Entering a working directory for a new agent.
    NewAgent { input: String },
    /// Entering an instruction to auto-send when the agent next finishes.
    Instruct { input: String },
}

pub struct App {
    /// Agents sorted deterministically by cwd, refreshed from the scanner.
    pub agents: Vec<Agent>,
    pub selected: usize,
    pub mode: Mode,
    /// Transient status message (shown briefly, then the bar reverts to the
    /// selected-agent context). Set via [`App::note`].
    pub status: String,
    pub status_at: Instant,
    pub should_quit: bool,
    /// When set, the main loop should suspend the TUI and attach this window.
    pub attach_to: Option<String>,
    /// When set, the main loop should suspend the TUI and open the projects
    /// config (`~/.phasor/projects.json`) in `$EDITOR`.
    pub edit_projects: bool,
    /// Selection is tracked by node id so it stays put as the list is rebuilt.
    selected_id: Option<String>,
    /// Fresh agent snapshots from the background scanner.
    rx: Receiver<Vec<Agent>>,
}

impl App {
    pub fn new() -> Self {
        let agents = scan::snapshot();
        let rx = scan::spawn();
        Self {
            agents,
            selected: 0,
            mode: Mode::Normal,
            status: String::new(),
            status_at: Instant::now(),
            should_quit: false,
            attach_to: None,
            edit_projects: false,
            selected_id: None,
            rx,
        }
    }

    /// Show a transient status message in the bottom bar.
    pub fn note(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_at = Instant::now();
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
                        self.note(format!("error: {e}"));
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            },
            Mode::Instruct { input } => match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => {
                    let text = input.trim().to_string();
                    self.mode = Mode::Normal;
                    if !text.is_empty() {
                        if let Some(wid) = self
                            .agents
                            .get(self.selected)
                            .and_then(|a| a.window_id.clone())
                        {
                            let _ = tmux::set_window_pending(&wid, &text);
                            self.note("instruction queued — auto-sends when the agent finishes");
                        }
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
            // Grid-aware navigation: up/down jump a row, left/right a column.
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-(self.grid_cols() as isize)),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(self.grid_cols() as isize),
            KeyCode::Left | KeyCode::Char('h') => self.move_selection(-1),
            KeyCode::Right | KeyCode::Char('l') => self.move_selection(1),
            // Quick-jump: 1-9 select agents 1..9, 0 selects the 10th.
            KeyCode::Char(c @ '0'..='9') => {
                let d = c.to_digit(10).unwrap() as usize;
                let idx = if d == 0 { 9 } else { d - 1 };
                self.select(idx);
            }
            KeyCode::Enter => self.open_selected(),
            KeyCode::Char('d') => self.kill_selected(),
            // Edit the projects config (~/.phasor/projects.json) in $EDITOR.
            KeyCode::Char('p') => self.edit_projects = true,
            // Queue an instruction to auto-send when this agent finishes.
            KeyCode::Char('i') => match self.agents.get(self.selected) {
                Some(a) if a.openable() => self.mode = Mode::Instruct { input: String::new() },
                Some(_) => self.note("can't instruct an external claude (not in phasor's tmux)"),
                None => {}
            },
            _ => {}
        }
    }

    /// Columns in the display grid — must match the UI's layout.
    fn grid_cols(&self) -> usize {
        let n = self.agents.len();
        if n == 0 {
            1
        } else {
            (n as f32).sqrt().ceil() as usize
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let n = self.agents.len();
        if n == 0 {
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, n as isize - 1) as usize;
        self.selected = next;
        self.selected_id = self.agents.get(next).map(|a| a.id.clone());
    }

    /// Select an agent by index (used by mouse clicks).
    pub fn select(&mut self, idx: usize) {
        if idx < self.agents.len() {
            self.selected = idx;
            self.selected_id = Some(self.agents[idx].id.clone());
        }
    }

    /// Open the selected agent's terminal, if it lives in phasor's tmux.
    pub fn open_selected(&mut self) {
        match self.agents.get(self.selected) {
            Some(a) if a.openable() => {
                self.attach_to = a.window_id.clone();
            }
            Some(_) => {
                self.note("external claude (not in an phasor tmux window) — monitor only");
            }
            None => {}
        }
    }

    fn kill_selected(&mut self) {
        match self.agents.get(self.selected) {
            Some(a) if a.openable() => {
                if let Some(id) = a.window_id.clone() {
                    let _ = tmux::kill_window(&id);
                    self.agents.remove(self.selected);
                    self.reconcile_selection();
                    self.note("agent window killed");
                }
            }
            Some(_) => {
                self.note("external claude — phasor won't kill processes it didn't start");
            }
            None => {}
        }
    }

    /// Create a tmux window and launch claude in it. The scanner picks it up.
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
        // Give claude a known session id so we can resolve this window's exact
        // transcript (not just the newest in the folder).
        let sid = tmux::new_session_id();
        let cmd = format!("claude --session-id {sid}");
        let win = tmux::new_window(&name, &canon.to_string_lossy(), &cmd)?;
        let _ = tmux::set_window_session(&win.id, &sid);
        // Select the new window once discovery picks it up (node id == win id).
        self.selected_id = Some(win.id);
        self.note("agent started — claude launching");
        Ok(())
    }

    /// Apply any agent snapshots the scanner has produced (non-blocking).
    pub fn drain_updates(&mut self) {
        let mut latest = None;
        while let Ok(agents) = self.rx.try_recv() {
            latest = Some(agents);
        }
        if let Some(agents) = latest {
            self.agents = agents;
            self.reconcile_selection();
        }
    }

    /// Keep the selection pinned to the same node across rebuilds.
    fn reconcile_selection(&mut self) {
        if let Some(id) = &self.selected_id {
            if let Some(idx) = self.agents.iter().position(|a| &a.id == id) {
                self.selected = idx;
                return;
            }
        }
        if self.selected >= self.agents.len() {
            self.selected = self.agents.len().saturating_sub(1);
        }
        self.selected_id = self.agents.get(self.selected).map(|a| a.id.clone());
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
