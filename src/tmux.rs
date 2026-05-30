//! Thin wrapper around the tmux CLI.
//!
//! Enxame keeps all agent terminals inside a single dedicated tmux server
//! (its own socket, `-L enxame`) so it never collides with the user's own
//! tmux. One agent == one tmux window in the session `enxame`.

use anyhow::{Context, Result};
use std::process::{Command, Stdio};

const SOCKET: &str = "enxame";
const SESSION: &str = "enxame";

/// A tmux window backing a single agent. `id` is a stable tmux window id
/// (e.g. `@3`) that survives reordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub id: String,
    pub name: String,
}

fn tmux() -> Command {
    let mut c = Command::new("tmux");
    c.args(["-L", SOCKET]);
    c
}

/// Run a tmux subcommand, capturing stdout. Errors include stderr.
fn run(args: &[&str]) -> Result<String> {
    let out = tmux()
        .args(args)
        .output()
        .context("failed to spawn tmux")?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// True if the enxame session already exists on our socket.
fn session_exists() -> bool {
    tmux()
        .args(["has-session", "-t", SESSION])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ensure the enxame session exists. The session is created detached with a
/// throwaway placeholder window; real agents get their own windows.
pub fn ensure_session() -> Result<()> {
    if !session_exists() {
        run(&[
            "new-session",
            "-d",
            "-s",
            SESSION,
            "-n",
            "_enxame",
            "-x",
            "200",
            "-y",
            "50",
        ])?;
    }
    configure();
    Ok(())
}

/// Best-effort configuration of the enxame tmux server: a single no-prefix
/// key to jump back to the dashboard, plus a visible hint in the status bar.
/// All commands are idempotent; failures are ignored so they never block the
/// app.
fn configure() {
    let cmds: &[&[&str]] = &[
        // Ctrl-Q detaches (collapses back to the dashboard) without the tmux
        // prefix. We avoid Alt/Fn keys: Claude Code uses some Alt keys (e.g.
        // Alt-o) and Fn keys are awkward on macOS — a no-prefix binding would
        // swallow whatever it's bound to before Claude sees it.
        &["bind-key", "-n", "C-q", "detach-client"],
        // Drop bindings from earlier versions so they stop shadowing Claude.
        &["unbind-key", "-n", "M-o"],
        &["unbind-key", "-n", "F12"],
        &["set-option", "-g", "status", "on"],
        &["set-option", "-g", "status-style", "bg=colour237,fg=colour250"],
        &["set-option", "-g", "status-left", "#[bold] ◍ enxame #[default]"],
        &[
            "set-option",
            "-g",
            "status-right",
            " ⟵  Ctrl-Q  (or prefix d)  back to dashboard ",
        ],
        &["set-option", "-g", "status-right-length", "60"],
    ];
    for c in cmds {
        let _ = run(c);
    }
}

/// Create a new window running `cmd` in `cwd`, returning its stable id.
pub fn new_window(name: &str, cwd: &str, cmd: &str) -> Result<Window> {
    ensure_session()?;
    // Target the session with a trailing colon (`enxame:`) so tmux appends at
    // the next free index. A bare `enxame` is parsed as a target *window* and
    // would collide with a window that happens to be named `enxame`.
    let target = format!("{SESSION}:");
    // -P prints info about the new window; -F gives us id + name.
    let out = run(&[
        "new-window",
        "-t",
        &target,
        "-c",
        cwd,
        "-n",
        name,
        "-P",
        "-F",
        "#{window_id}\t#{window_name}",
        cmd,
    ])?;
    let line = out.lines().next().unwrap_or_default();
    let (id, wname) = line.split_once('\t').context("unexpected new-window output")?;
    Ok(Window {
        id: id.to_string(),
        name: wname.to_string(),
    })
}

/// List all agent windows in the enxame session (excludes nothing; the
/// caller filters the `_enxame` placeholder if desired).
pub fn list_windows() -> Result<Vec<Window>> {
    if !session_exists() {
        return Ok(Vec::new());
    }
    let out = run(&[
        "list-windows",
        "-t",
        SESSION,
        "-F",
        "#{window_id}\t#{window_name}",
    ])?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(id, name)| Window {
            id: id.to_string(),
            name: name.to_string(),
        })
        .collect())
}

/// List agent windows together with the current path of their active pane.
/// The `_enxame` placeholder window is excluded.
pub fn list_windows_with_cwd() -> Result<Vec<(Window, std::path::PathBuf)>> {
    if !session_exists() {
        return Ok(Vec::new());
    }
    let out = run(&[
        "list-windows",
        "-t",
        SESSION,
        "-F",
        "#{window_id}\t#{window_name}\t#{pane_current_path}",
    ])?;
    let mut v = Vec::new();
    for line in out.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(id), Some(name), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if name == "_enxame" {
            continue;
        }
        v.push((
            Window {
                id: id.to_string(),
                name: name.to_string(),
            },
            std::path::PathBuf::from(path),
        ));
    }
    Ok(v)
}

/// Send a line of keys (followed by Enter) to a window.
#[allow(dead_code)] // part of the tmux API surface; used by upcoming features
pub fn send_line(window_id: &str, line: &str) -> Result<()> {
    run(&["send-keys", "-t", window_id, line, "Enter"])?;
    Ok(())
}

/// Kill a window by id.
pub fn kill_window(window_id: &str) -> Result<()> {
    run(&["kill-window", "-t", window_id])?;
    Ok(())
}

/// Capture the visible contents of a window's active pane as plain text.
#[allow(dead_code)] // reserved for the in-panel terminal preview
pub fn capture_pane(window_id: &str) -> Result<String> {
    run(&["capture-pane", "-p", "-t", window_id])
}

/// Build the command to attach the current terminal to a specific window.
/// The caller runs this as a foreground child, inheriting stdio, so tmux
/// takes over the screen until the user detaches (Alt-o / prefix + d).
pub fn attach_command(window_id: &str) -> Command {
    // Point the session at the target window first (best-effort) so a plain
    // `attach` lands there.
    let _ = run(&["select-window", "-t", window_id]);
    let mut c = tmux();
    // Crucial: if enxame itself runs inside a tmux session, `$TMUX` is set and
    // tmux refuses to attach ("sessions should be nested with care"). We attach
    // on our own dedicated socket, so clearing it is safe and correct.
    c.env_remove("TMUX");
    c.args(["attach-session", "-t", SESSION]);
    c
}

/// True if a tmux binary is reachable at all.
pub fn available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
