//! Application state and input handling (UI-framework agnostic).
//!
//! Discovery (ps + lsof + transcript parsing) is comparatively slow, so it runs
//! on a background thread and streams fresh agent snapshots to the UI over a
//! channel. The main loop never blocks on it — keystrokes stay responsive.

use crate::agent::Agent;
use crate::{discover, transcript, tmux};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use notify::{RecursiveMode, Watcher};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How often the process/window set is re-checked (cheap: ps + lsof + tmux, no
/// transcript reads). Transcript content is refreshed by file-watch events, not
/// on this tick.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(3);

/// Interaction mode of the dashboard.
pub enum Mode {
    Normal,
    /// Entering a working directory for a new agent.
    NewAgent { input: String },
}

pub struct App {
    /// Agents sorted deterministically by cwd, refreshed from the scanner.
    pub agents: Vec<Agent>,
    pub selected: usize,
    pub mode: Mode,
    pub status: String,
    pub should_quit: bool,
    /// When set, the main loop should suspend the TUI and attach this window.
    pub attach_to: Option<String>,
    /// Selection is tracked by cwd so it stays put as the list is rebuilt.
    selected_cwd: Option<PathBuf>,
    /// Fresh agent snapshots from the background scanner.
    rx: Receiver<Vec<Agent>>,
}

impl App {
    pub fn new() -> Self {
        // One synchronous scan so the first frame isn't empty, then a worker
        // thread takes over: it watches the transcript files and only re-reads
        // the ones that actually change.
        let agents = initial_scan();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || run_scanner(tx));

        Self {
            agents,
            selected: 0,
            mode: Mode::Normal,
            status: "n: new · 1-9: jump · ←↑↓→/hjkl: move · Enter: open · d: kill · q: quit".into(),
            should_quit: false,
            attach_to: None,
            selected_cwd: None,
            rx,
        }
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
            // Grid-aware navigation: up/down jump a row, left/right a column.
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-(self.grid_cols() as isize)),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(self.grid_cols() as isize),
            KeyCode::Left | KeyCode::Char('h') => self.move_selection(-1),
            KeyCode::Right | KeyCode::Char('l') => self.move_selection(1),
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
                    // Optimistic local removal; the scanner will confirm.
                    self.agents.remove(self.selected);
                    self.reconcile_selection();
                    self.status = "agent window killed".into();
                }
            }
            Some(_) => {
                self.status = "external claude — enxame won't kill processes it didn't start".into();
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
        tmux::new_window(&name, &canon.to_string_lossy(), "claude")?;
        self.selected_cwd = Some(canon);
        self.status = "agent started — claude launching".into();
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

/// The cwd -> (pids, tmux window id) map from a process/window discovery pass.
fn discover_cwds() -> BTreeMap<PathBuf, (Vec<u32>, Option<String>)> {
    let mut cwds: BTreeMap<PathBuf, (Vec<u32>, Option<String>)> = BTreeMap::new();
    for (cwd, pids) in discover::running_claudes() {
        cwds.entry(cwd).or_default().0 = pids;
    }
    for (win, cwd) in tmux::list_windows_with_cwd().unwrap_or_default() {
        cwds.entry(cwd).or_default().1 = Some(win.id);
    }
    cwds
}

/// Locate and fully parse an agent's transcript (reads the file tail).
fn reparse(agent: &mut Agent) {
    agent.transcript = transcript::newest_session(&agent.cwd, SystemTime::UNIX_EPOCH);
    if let Some(t) = agent.transcript.clone() {
        if let Ok(state) = transcript::parse(&t, &agent.cwd) {
            agent.state = state;
        }
    }
}

/// One-off full scan used to populate the very first frame.
fn initial_scan() -> Vec<Agent> {
    discover_cwds()
        .into_iter()
        .map(|(cwd, (pids, window_id))| {
            let mut a = Agent::new(cwd);
            a.pids = pids;
            a.window_id = window_id;
            reparse(&mut a);
            a
        })
        .collect()
}

/// Background scanner: watches `~/.claude/projects` for transcript changes and
/// re-parses only the affected file, plus a cheap periodic discovery tick for
/// the process/window set. Streams full snapshots to the UI.
fn run_scanner(tx: Sender<Vec<Agent>>) {
    let projects = dirs::home_dir().map(|h| h.join(".claude/projects"));

    // Set up the filesystem watcher (best effort).
    let (fs_tx, fs_rx) = mpsc::channel();
    let watcher = projects.as_ref().and_then(|p| {
        let mut w = notify::recommended_watcher(move |res| {
            let _ = fs_tx.send(res);
        })
        .ok()?;
        w.watch(p, RecursiveMode::Recursive).ok()?;
        Some(w)
    });
    let _watcher = watcher; // keep alive for the loop's lifetime

    let mut agents: BTreeMap<PathBuf, Agent> = BTreeMap::new();
    // Encoded project-dir name (the `-Users-...` folder) -> cwd, for mapping
    // file events back to an agent without fragile path-prefix comparisons.
    let mut dir_to_cwd: HashMap<OsString, PathBuf> = HashMap::new();
    let mut last_discovery = Instant::now()
        .checked_sub(DISCOVERY_INTERVAL)
        .unwrap_or_else(Instant::now); // force an immediate discovery

    loop {
        // Block until a file event arrives or it's time for a discovery tick.
        let first = fs_rx.recv_timeout(Duration::from_millis(500));
        let mut dirty: HashSet<PathBuf> = HashSet::new();
        let mut force_discovery = false;

        let handle = |res: notify::Result<notify::Event>,
                      dirty: &mut HashSet<PathBuf>,
                      force: &mut bool| {
            if let Ok(ev) = res {
                for path in ev.paths {
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    match path.parent().and_then(|p| p.file_name()).map(|n| n.to_os_string()) {
                        Some(name) => match dir_to_cwd.get(&name) {
                            Some(cwd) => {
                                dirty.insert(cwd.clone());
                            }
                            None => *force = true, // unknown project — rediscover
                        },
                        None => {}
                    }
                }
            }
        };

        match first {
            Ok(res) => handle(res, &mut dirty, &mut force_discovery),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // No watcher available — fall back to periodic discovery only.
                thread::sleep(Duration::from_millis(500));
            }
        }
        // Coalesce any further queued events into this batch.
        while let Ok(res) = fs_rx.try_recv() {
            handle(res, &mut dirty, &mut force_discovery);
        }

        let mut changed = false;

        if force_discovery || last_discovery.elapsed() >= DISCOVERY_INTERVAL {
            let cwds = discover_cwds();
            agents.retain(|cwd, _| cwds.contains_key(cwd));
            for (cwd, (pids, window_id)) in cwds {
                let entry = agents.entry(cwd.clone()).or_insert_with(|| {
                    let mut a = Agent::new(cwd.clone());
                    reparse(&mut a);
                    a
                });
                entry.pids = pids;
                entry.window_id = window_id;
                // Refresh working/idle cheaply from mtime (no content read).
                if let Some(t) = &entry.transcript {
                    entry.state.status = transcript::status_from_mtime(t);
                }
            }
            dir_to_cwd = agents
                .keys()
                .filter_map(|cwd| {
                    transcript::project_dir(cwd)
                        .and_then(|d| d.file_name().map(|n| (n.to_os_string(), cwd.clone())))
                })
                .collect();
            last_discovery = Instant::now();
            changed = true;
        }

        for cwd in dirty {
            if let Some(agent) = agents.get_mut(&cwd) {
                reparse(agent);
                changed = true;
            }
        }

        if changed && tx.send(agents.values().cloned().collect()).is_err() {
            break; // UI gone
        }
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
