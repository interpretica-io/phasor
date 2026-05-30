//! Shared agent discovery + transcript scanning.
//!
//! Both the TUI dashboard and the web server build their view from here. A
//! background thread (`spawn`) watches `~/.claude/projects` and only re-reads
//! transcripts that actually change, plus a cheap periodic discovery tick for
//! the process/window set. `snapshot` is a one-off synchronous scan.

use crate::agent::{Agent, Status};
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

/// A discovered node: one tmux window OR one external claude process. Agents
/// are NOT grouped by folder.
struct Node {
    id: String,
    cwd: PathBuf,
    window_id: Option<String>,
    pids: Vec<u32>,
    session_id: Option<String>,
    pending: Option<String>,
}

/// Discover all nodes: each enxame tmux window is its own node; each external
/// claude (one not running inside an enxame window) is its own node.
fn discover_nodes() -> Vec<Node> {
    let windows = tmux::list_windows_with_cwd().unwrap_or_default();
    let pane_pids: HashSet<u32> = windows.iter().map(|w| w.pane_pid).collect();
    let claudes = discover::running_claudes();

    let mut nodes: Vec<Node> = Vec::new();
    // One node per tmux window; attach the claude pid running inside it.
    for w in &windows {
        let pids = claudes
            .iter()
            .filter(|p| p.pid == w.pane_pid || p.ppid == w.pane_pid)
            .map(|p| p.pid)
            .collect();
        nodes.push(Node {
            id: w.id.clone(),
            cwd: w.cwd.clone(),
            window_id: Some(w.id.clone()),
            pids,
            session_id: w.session_id.clone(),
            pending: w.pending.clone(),
        });
    }
    // One node per external claude (not inside any enxame window).
    for p in &claudes {
        if pane_pids.contains(&p.pid) || pane_pids.contains(&p.ppid) {
            continue;
        }
        nodes.push(Node {
            id: format!("pid:{}", p.pid),
            cwd: p.cwd.clone(),
            window_id: None,
            pids: vec![p.pid],
            session_id: None,
            pending: None,
        });
    }
    // Multiple external claudes in the same folder share one project dir but
    // each has its OWN session file. Give each a distinct session (the N
    // newest, paired in a stable pid order) so the nodes aren't identical.
    let mut by_cwd: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if n.window_id.is_none() {
            by_cwd.entry(n.cwd.clone()).or_default().push(i);
        }
    }
    for (cwd, mut idxs) in by_cwd {
        if idxs.len() < 2 {
            continue; // single claude → newest-session fallback is fine
        }
        idxs.sort_by_key(|&i| nodes[i].pids.first().copied().unwrap_or(0));
        let files = transcript::sessions(&cwd);
        for (k, &i) in idxs.iter().enumerate() {
            if let Some(stem) = files.get(k).and_then(|p| p.file_stem()).and_then(|s| s.to_str()) {
                nodes[i].session_id = Some(stem.to_string());
            }
        }
    }

    // Stable display order: by cwd, then id.
    nodes.sort_by(|a, b| a.cwd.cmp(&b.cwd).then(a.id.cmp(&b.id)));
    nodes
}

fn node_to_agent(n: Node) -> Agent {
    let mut a = Agent::new(n.id, n.cwd);
    a.pids = n.pids;
    a.window_id = n.window_id;
    a.session_id = n.session_id;
    a.pending = n.pending;
    a
}

/// Locate and fully parse an agent's transcript (reads the file tail). If the
/// last completed-turn marker changed, record a completion event (but never on
/// the agent's very first parse, so pre-existing completions don't flash).
/// Returns true if the agent just finished a turn (a fresh completion).
fn reparse(agent: &mut Agent) -> bool {
    let prev_marker = agent.state.final_marker.clone();
    let prev_dirs = agent.state.work_dirs.clone();
    // Prefer this agent's own session file (when enxame launched it with a known
    // session id); otherwise fall back to the newest session in the cwd.
    agent.transcript = agent
        .session_id
        .as_ref()
        .and_then(|sid| transcript::project_dir(&agent.cwd).map(|d| d.join(format!("{sid}.jsonl"))))
        .filter(|p| p.exists())
        .or_else(|| transcript::newest_session(&agent.cwd, SystemTime::UNIX_EPOCH));
    if let Some(t) = agent.transcript.clone() {
        if let Ok(mut state) = transcript::parse(&t, &agent.cwd) {
            // Accumulate touched folders across polls: each parse only sees the
            // tail, so without this the set would collapse whenever recent
            // activity has no file ops. Union keeps everything once seen.
            for d in &prev_dirs {
                if !state.work_dirs.contains(d) {
                    state.work_dirs.push(d.clone());
                }
            }
            state.work_dirs.sort();
            let new_marker = state.final_marker.clone();
            agent.state = state;
            if prev_marker.is_some() && new_marker.is_some() && prev_marker != new_marker {
                agent.completed_at = Some(SystemTime::now());
                agent.completions = agent.completions.wrapping_add(1);
                return true;
            }
        }
    }
    false
}

/// If the agent has a queued instruction and has finished a turn we haven't
/// acted on yet, either send the (repeating) instruction or — if it declared
/// it's fully done — stop. Idempotent per turn via the `@enxame_sent` marker.
fn try_autosend(agent: &mut Agent) {
    let (Some(pending), Some(wid), Some(marker)) = (
        agent.pending.clone(),
        agent.window_id.clone(),
        agent.state.final_marker.clone(),
    ) else {
        return;
    };
    if tmux::get_window_sent(&wid).as_deref() == Some(marker.as_str()) {
        return; // already handled this turn
    }
    if agent.state.final_says_done {
        let _ = tmux::set_window_pending(&wid, ""); // stop the repeat
        agent.pending = None;
    } else {
        let msg =
            format!("{pending} but if you really finished the task, write 'FINISHED COMPLETELY'");
        let _ = tmux::send_text(&wid, &msg);
    }
    tmux::set_window_sent(&wid, &marker);
}

/// One-off full scan, used to populate the first frame.
pub fn snapshot() -> Vec<Agent> {
    discover_nodes()
        .into_iter()
        .map(|n| {
            let mut a = node_to_agent(n);
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

    let mut agents: BTreeMap<String, Agent> = BTreeMap::new(); // keyed by node id
    let mut dir_to_cwd: HashMap<OsString, PathBuf> = HashMap::new();
    let mut last_discovery = Instant::now()
        .checked_sub(DISCOVERY_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut last_sample = Instant::now();
    let mut named: HashMap<String, String> = HashMap::new(); // window_id -> name we set

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
            let nodes = discover_nodes();
            let ids: HashSet<String> = nodes.iter().map(|n| n.id.clone()).collect();
            agents.retain(|id, _| ids.contains(id));
            for n in nodes {
                let entry = agents.entry(n.id.clone()).or_insert_with(|| {
                    let mut a = node_to_agent(Node {
                        id: n.id.clone(),
                        cwd: n.cwd.clone(),
                        window_id: n.window_id.clone(),
                        pids: vec![],
                        session_id: n.session_id.clone(),
                        pending: n.pending.clone(),
                    });
                    reparse(&mut a);
                    a
                });
                entry.cwd = n.cwd;
                entry.pids = n.pids;
                entry.window_id = n.window_id;
                entry.session_id = n.session_id;
                entry.pending = n.pending;
                if let Some(t) = &entry.transcript {
                    entry.state.status = transcript::status_from_mtime(t);
                }
            }
            // Map encoded project-dir name -> cwd (several nodes may share a cwd).
            dir_to_cwd = agents
                .values()
                .filter_map(|a| {
                    transcript::project_dir(&a.cwd)
                        .and_then(|d| d.file_name().map(|n| (n.to_os_string(), a.cwd.clone())))
                })
                .collect();
            last_discovery = Instant::now();
            changed = true;
        }

        // A changed transcript may belong to several nodes (same cwd) — reparse
        // all of them. When one finishes a turn and has a queued instruction,
        // auto-send it.
        for cwd in &dirty {
            for agent in agents.values_mut().filter(|a| &a.cwd == cwd) {
                reparse(agent);
                changed = true;
                // Just finished a turn → act on a queued instruction immediately.
                try_autosend(agent);
            }
        }

        // Also catch agents that already finished/idle when the instruction was
        // queued (no fresh completion event to ride on).
        for agent in agents.values_mut() {
            if agent.pending.is_some() && agent.state.status == Status::Idle {
                try_autosend(agent);
            }
        }

        // Label each agent's tmux window with its task title (so the tmux
        // status bar shows meaningful names, not "1 enxame 2 enxame"). Only
        // rename when it actually changes.
        for agent in agents.values() {
            if let (Some(wid), Some(title)) = (&agent.window_id, agent.state.title.as_ref()) {
                let name: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
                let name: String = name.chars().take(28).collect();
                if name.is_empty() {
                    continue;
                }
                if named.get(wid).map(|n| n != &name).unwrap_or(true) {
                    let _ = tmux::rename_window(wid, &name);
                    named.insert(wid.clone(), name);
                }
            }
        }
        named.retain(|wid, _| agents.values().any(|a| a.window_id.as_deref() == Some(wid)));

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

        if changed {
            let mut snap: Vec<Agent> = agents.values().cloned().collect();
            snap.sort_by(|a, b| a.cwd.cmp(&b.cwd).then(a.id.cmp(&b.id)));
            if tx.send(snap).is_err() {
                break; // consumer gone
            }
        }
    }
}
