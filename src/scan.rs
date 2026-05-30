//! Shared agent discovery + transcript scanning.
//!
//! Both the TUI dashboard and the web server build their view from here. A
//! background thread (`spawn`) watches `~/.claude/projects` and only re-reads
//! transcripts that actually change, plus a cheap periodic discovery tick for
//! the process/window set. `snapshot` is a one-off synchronous scan.

use crate::agent::Agent;
use crate::{discover, transcript, tmux};
use notify::{RecursiveMode, Watcher};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How often the process/window set is re-checked (cheap: ps + lsof + tmux, no
/// transcript reads). Transcript content is refreshed by file-watch events.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(3);
/// How often agent activity (transcript growth) is sampled — a cheap `stat`.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Length of the activity history kept per agent (seconds, at 1 Hz).
const HISTORY: usize = 60;
/// Transcript bytes/second that map to 100% load.
const LOAD_FULL_BPS: f32 = 1200.0;

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

/// Locate and fully parse an agent's transcript (reads the file tail). If the
/// last completed-turn marker changed, record a completion event (but never on
/// the agent's very first parse, so pre-existing completions don't flash).
fn reparse(agent: &mut Agent) {
    let prev_marker = agent.state.final_marker.clone();
    agent.transcript = transcript::newest_session(&agent.cwd, SystemTime::UNIX_EPOCH);
    if let Some(t) = agent.transcript.clone() {
        if let Ok(state) = transcript::parse(&t, &agent.cwd) {
            let new_marker = state.final_marker.clone();
            agent.state = state;
            if prev_marker.is_some() && new_marker.is_some() && prev_marker != new_marker {
                agent.completed_at = Some(SystemTime::now());
                agent.completions = agent.completions.wrapping_add(1);
            }
        }
    }
}

/// One-off full scan, used to populate the first frame.
pub fn snapshot() -> Vec<Agent> {
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

/// Spawn the background scanner; returns a channel of fresh agent snapshots.
pub fn spawn() -> Receiver<Vec<Agent>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || run_scanner(tx));
    rx
}

/// Background scanner: watches `~/.claude/projects` for transcript changes and
/// re-parses only the affected file, plus a cheap periodic discovery tick.
fn run_scanner(tx: Sender<Vec<Agent>>) {
    let projects = dirs::home_dir().map(|h| h.join(".claude/projects"));

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
    let mut dir_to_cwd: HashMap<OsString, PathBuf> = HashMap::new();
    let mut last_discovery = Instant::now()
        .checked_sub(DISCOVERY_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut last_sample = Instant::now();

    loop {
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
                            None => *force = true,
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
                thread::sleep(Duration::from_millis(500));
            }
        }
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

        // Activity sampling: how fast each transcript is growing (cheap stat).
        if last_sample.elapsed() >= SAMPLE_INTERVAL {
            let secs = last_sample.elapsed().as_secs_f32().max(0.1);
            last_sample = Instant::now();
            for agent in agents.values_mut() {
                let len = agent
                    .transcript
                    .as_ref()
                    .and_then(|t| fs::metadata(t).ok())
                    .map(|m| m.len())
                    .unwrap_or(0);
                if agent.activity.is_empty() {
                    agent.last_len = len; // prime without a startup spike
                    agent.activity.push_back(0);
                } else {
                    let bps = len.saturating_sub(agent.last_len) as f32 / secs;
                    agent.last_len = len;
                    let load = ((bps / LOAD_FULL_BPS) * 100.0).clamp(0.0, 100.0) as u8;
                    agent.activity.push_back(load);
                }
                while agent.activity.len() > HISTORY {
                    agent.activity.pop_front();
                }
            }
            changed = true;
        }

        if changed && tx.send(agents.values().cloned().collect()).is_err() {
            break; // consumer gone
        }
    }
}
