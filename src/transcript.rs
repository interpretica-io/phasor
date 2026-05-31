//! Reading Claude Code session transcripts.
//!
//! Claude Code writes one JSONL file per session under
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. The encoded cwd is
//! the absolute path with `/` and `.` replaced by `-`. We locate the session
//! belonging to an agent (newest file in that dir), then parse the tail of it
//! to derive title, recent phrases, todo progress and touched directories.

use crate::agent::{AgentState, Status};
use anyhow::Result;
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How many recent assistant phrases to retain per agent.
const MAX_PHRASES: usize = 4;
/// How much of the (potentially huge) transcript tail to parse per poll.
/// (Larger than it used to be so more touched folders are seen; the scanner
/// also accumulates folders across polls so they never silently disappear.)
const TAIL_BYTES: u64 = 2 * 1024 * 1024;
/// Activity newer than this counts as "working".
const WORKING_WINDOW: Duration = Duration::from_secs(20);

/// Encode an absolute cwd the way Claude Code names its project directory.
fn encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    s.chars()
        .map(|c| {
            if c == '/' || c == '.' || c == '_' {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// Path to the per-project transcript directory for a cwd.
pub fn project_dir(cwd: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".claude/projects").join(encode_cwd(cwd)))
}

/// Find the newest `*.jsonl` session file for a cwd, modified at or after
/// `not_before` (the agent's launch time, minus a small grace).
pub fn newest_session(cwd: &Path, not_before: SystemTime) -> Option<PathBuf> {
    let dir = project_dir(cwd)?;
    let grace = not_before
        .checked_sub(Duration::from_secs(5))
        .unwrap_or(not_before);
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < grace {
            continue;
        }
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

/// All session `*.jsonl` files for a cwd, newest-modified first.
pub fn sessions(cwd: &Path) -> Vec<PathBuf> {
    let Some(dir) = project_dir(cwd) else {
        return Vec::new();
    };
    let mut v: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Ok(t) = entry.metadata().and_then(|m| m.modified()) {
            v.push((t, path));
        }
    }
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter().map(|(_, p)| p).collect()
}

/// Cheap liveness check from the file's mtime alone (a `stat`, no read).
pub fn status_from_mtime(path: &Path) -> Status {
    match fs::metadata(path).and_then(|m| m.modified()) {
        Ok(mtime) => match mtime.elapsed() {
            Ok(e) if e <= WORKING_WINDOW => Status::Working,
            _ => Status::Idle,
        },
        Err(_) => Status::Unknown,
    }
}

/// Read the last `TAIL_BYTES` of a file as a string (dropping a possibly
/// partial first line).
fn read_tail(path: &Path) -> Result<(String, SystemTime)> {
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mtime = f.metadata()?.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        if let Some(nl) = text.find('\n') {
            text = text[nl + 1..].to_string();
        }
    }
    Ok((text, mtime))
}

/// Parse the transcript tail into an [`AgentState`].
pub fn parse(path: &Path, root: &Path) -> Result<AgentState> {
    let (text, mtime) = read_tail(path)?;
    let mut state = AgentState::default();
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    // Always anchor the root directory.
    dirs.insert(root.to_path_buf());

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                    state.title = Some(t.to_string());
                }
            }
            Some("assistant") => {
                // A finished turn (final answer) marks a completed task.
                if v.get("message")
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|s| s.as_str())
                    == Some("end_turn")
                {
                    if let Some(u) = v.get("uuid").and_then(|u| u.as_str()) {
                        state.final_marker = Some(u.to_string());
                    }
                    // Did the agent declare it's fully done? Check the full
                    // answer text (not the length-clipped phrase).
                    let full: String = v
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                        .map(|blocks| {
                            blocks
                                .iter()
                                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    state.final_says_done = full.contains("FINISHED COMPLETELY");
                }
                let content = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array());
                let Some(content) = content else { continue };
                for block in content {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(txt) = block.get("text").and_then(|t| t.as_str()) {
                                let txt = txt.trim();
                                if !txt.is_empty() {
                                    push_phrase(&mut state, txt);
                                }
                            }
                        }
                        Some("tool_use") => {
                            harvest_tool_use(block, &mut state, &mut dirs);
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                // `/add-dir <path>` declares an extra working directory — but
                // only when the *human* typed it. Claude/SDK sometimes emit
                // messages that merely contain the command text; accept only:
                //   - userType "external" (the human, not an internal/agent msg)
                //   - not a sidechain (subagent) message
                //   - interactive entrypoint (not the programmatic "sdk-cli")
                //   - content that actually STARTS with the command tag
                let external = v.get("userType").and_then(|u| u.as_str()) == Some("external");
                let sidechain = v
                    .get("isSidechain")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                let sdk = v
                    .get("entrypoint")
                    .and_then(|e| e.as_str())
                    .map(|e| e.contains("sdk"))
                    .unwrap_or(false);
                if external && !sidechain && !sdk {
                    if let Some(c) = v
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_str())
                    {
                        if c.trim_start()
                            .starts_with("<command-name>/add-dir</command-name>")
                        {
                            if let Some(arg) = between(c, "<command-args>", "</command-args>") {
                                if let Some(p) = resolve_dir(arg.trim(), root) {
                                    dirs.insert(p);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Guard against parse junk ever surfacing as a folder.
    state.work_dirs = dirs
        .into_iter()
        .filter(|d| d.is_absolute() && !d.to_string_lossy().contains('"'))
        .collect();
    state.last_activity = Some(mtime);
    state.status = match mtime.elapsed() {
        Ok(e) if e <= WORKING_WINDOW => Status::Working,
        _ => Status::Idle,
    };
    Ok(state)
}

/// Slice between two delimiters (first occurrence), if present.
fn between<'a>(s: &'a str, a: &str, b: &str) -> Option<&'a str> {
    let start = s.find(a)? + a.len();
    let end = s[start..].find(b)? + start;
    Some(&s[start..end])
}

/// Resolve a `/add-dir` argument (which may use `~`, be absolute, or relative
/// to the session root) to an absolute directory path.
fn resolve_dir(arg: &str, root: &Path) -> Option<PathBuf> {
    if arg.is_empty() {
        return None;
    }
    let p = if let Some(rest) = arg.strip_prefix('~') {
        let home = dirs::home_dir()?;
        home.join(rest.trim_start_matches('/'))
    } else if arg.starts_with('/') {
        PathBuf::from(arg)
    } else {
        root.join(arg)
    };
    Some(p)
}

/// Append an assistant phrase (whitespace-collapsed, length-clamped) to the
/// agent's ring of recent phrases, evicting the oldest past [`MAX_PHRASES`].
fn push_phrase(state: &mut AgentState, txt: &str) {
    // Collapse whitespace and clamp length for display.
    let one_line: String = txt.split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped: String = one_line.chars().take(200).collect();
    state.last_phrases.push_back(clipped);
    while state.last_phrases.len() > MAX_PHRASES {
        state.last_phrases.pop_front();
    }
}

/// Pull todo progress and **working directories** out of a tool_use block.
///
/// A working directory is one the agent actually *changes files in* — so only
/// mutating tools count. Reads, greps, globs and shell commands are exploration
/// (just "folders it used"), not where it works, and are ignored.
fn harvest_tool_use(
    block: &serde_json::Value,
    state: &mut AgentState,
    dirs: &mut BTreeSet<PathBuf>,
) {
    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let input = block.get("input");
    let Some(input) = input else { return };

    if name == "TodoWrite" {
        if let Some(todos) = input.get("todos").and_then(|t| t.as_array()) {
            let total = todos.len();
            let done = todos
                .iter()
                .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("completed"))
                .count();
            if total > 0 {
                state.todos = Some((done, total));
            }
        }
        return;
    }

    if matches!(
        name,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Update"
    ) {
        for key in ["file_path", "notebook_path", "path"] {
            if let Some(p) = input.get(key).and_then(|p| p.as_str()) {
                if let Some(parent) = PathBuf::from(p).parent() {
                    if parent.is_absolute() {
                        dirs.insert(parent.to_path_buf());
                    }
                }
            }
        }
    }
}
