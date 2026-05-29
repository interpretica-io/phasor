//! Discovery of *all* running Claude Code CLI processes on the machine,
//! regardless of whether enxame launched them.
//!
//! The CLI shows up in `ps` with the exact command name `claude` (the desktop
//! app is `Claude` / `Claude Helper`, so a case-sensitive match excludes it).
//! macOS `ps` can't print a process cwd, so we resolve cwds in one batched
//! `lsof` call. Results are grouped by cwd — that cwd is the "project" node in
//! the dashboard, which also neatly handles several claudes in one directory.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

/// Map of working directory -> the claude PIDs currently running there.
pub fn running_claudes() -> BTreeMap<PathBuf, Vec<u32>> {
    let mut out: BTreeMap<PathBuf, Vec<u32>> = BTreeMap::new();
    let pids = claude_pids();
    if pids.is_empty() {
        return out;
    }
    for (pid, cwd) in cwds_for(&pids) {
        out.entry(cwd).or_default().push(pid);
    }
    out
}

/// PIDs of processes whose command name is exactly `claude`.
fn claude_pids() -> Vec<u32> {
    let Ok(o) = Command::new("ps").args(["-axo", "pid=,comm="]).output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&o.stdout);
    let mut pids = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((pid, comm)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        // comm is the basename of the executable, e.g. "claude".
        if comm.trim() == "claude" {
            if let Ok(p) = pid.trim().parse::<u32>() {
                pids.push(p);
            }
        }
    }
    pids
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
