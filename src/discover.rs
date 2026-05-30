//! Discovery of *all* running Claude Code CLI processes on the machine,
//! regardless of whether phasor launched them.
//!
//! The CLI shows up in `ps` with the exact command name `claude` (the desktop
//! app is `Claude` / `Claude Helper`, so a case-sensitive match excludes it).
//! macOS `ps` can't print a process cwd, so we resolve cwds in one batched
//! `lsof` call. Results are grouped by cwd — that cwd is the "project" node in
//! the dashboard, which also neatly handles several claudes in one directory.

use std::path::PathBuf;
use std::process::Command;

/// A running Claude Code CLI process.
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub cwd: PathBuf,
}

/// All running claude CLI processes with their pid, parent pid and cwd.
pub fn running_claudes() -> Vec<Proc> {
    let procs = claude_pids(); // (pid, ppid)
    if procs.is_empty() {
        return Vec::new();
    }
    let pids: Vec<u32> = procs.iter().map(|(p, _)| *p).collect();
    let cwds: std::collections::HashMap<u32, PathBuf> = cwds_for(&pids).into_iter().collect();
    procs
        .into_iter()
        .filter_map(|(pid, ppid)| cwds.get(&pid).map(|cwd| Proc { pid, ppid, cwd: cwd.clone() }))
        .collect()
}

/// (pid, ppid) of processes whose command name is exactly `claude`.
fn claude_pids() -> Vec<(u32, u32)> {
    let Ok(o) = Command::new("ps").args(["-axo", "pid=,ppid=,comm="]).output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&o.stdout);
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next(), it.next()) else { continue };
        let comm = it.collect::<Vec<_>>().join(" ");
        if comm == "claude" {
            if let (Ok(p), Ok(pp)) = (pid.parse(), ppid.parse()) {
                out.push((p, pp));
            }
        }
    }
    out
}

/// Resolve the cwd of each pid via a single `lsof` call.
fn cwds_for(pids: &[u32]) -> Vec<(u32, PathBuf)> {
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let Ok(o) = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-p", &list, "-Fpn"])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&o.stdout);
    // lsof -F output: lines prefixed with field id; `p<pid>` then `n<path>`.
    let mut result = Vec::new();
    let mut cur: Option<u32> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            cur = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = line.strip_prefix('n') {
            if let Some(pid) = cur {
                result.push((pid, PathBuf::from(rest)));
            }
        }
    }
    result
}
