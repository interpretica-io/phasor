//! Thin wrapper around the tmux CLI.
//!
//! Enxame keeps all agent terminals inside a single dedicated tmux server
//! (its own socket, `-L enxame`) so it never collides with the user's own
//! tmux. One agent == one tmux window in the session `enxame`.

use anyhow::{Context, Result};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// tmux socket name. Override with `ENXAME_SOCKET` to run an isolated instance
/// (e.g. for tests) without touching the real `enxame` server.
pub fn socket() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| std::env::var("ENXAME_SOCKET").unwrap_or_else(|_| "enxame".into()))
}

/// tmux session name. Override with `ENXAME_SESSION`.
pub fn session() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| std::env::var("ENXAME_SESSION").unwrap_or_else(|_| "enxame".into()))
}

/// A tmux window backing a single agent. `id` is a stable tmux window id
/// (e.g. `@3`) that survives reordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub id: String,
    pub name: String,
}

fn tmux() -> Command {
    let mut c = Command::new("tmux");
    c.args(["-L", socket()]);
    c
}

/// Max time we'll wait for any tmux command. A wedged tmux server must never
/// freeze enxame — commands time out and surface an error instead.
const TMUX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// Spawn a command and wait at most `TMUX_TIMEOUT`; kill it if it hangs.
fn run_timed(c: &mut Command) -> Result<std::process::Output> {
    use std::time::Instant;
    c.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let mut child = c.spawn().context("failed to spawn tmux")?;
    let deadline = Instant::now() + TMUX_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("tmux command timed out (server unresponsive)");
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}

/// Run a tmux subcommand, capturing stdout. Errors include stderr.
fn run(args: &[&str]) -> Result<String> {
    let out = run_timed(tmux().args(args))?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// True if the enxame session already exists on our socket (timeout-bounded, so
/// a wedged server reports "no session" rather than hanging).
fn session_exists() -> bool {
    run_timed(tmux().args(["has-session", "-t", session()]))
        .map(|o| o.status.success())
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
            session(),
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
        // When a claude exits/dies, let tmux destroy its window so the dead
        // agent is cleaned up (rather than lingering as a dead pane).
        &["set-option", "-g", "remain-on-exit", "off"],
        // Drop bindings from earlier versions so they stop shadowing Claude.
        &["unbind-key", "-n", "M-o"],
        &["unbind-key", "-n", "F12"],
        // Keep our window names (set from the agent's task title) — don't let
        // the shell/program auto-rename them.
        &["set-option", "-g", "automatic-rename", "off"],
        &["set-option", "-g", "allow-rename", "off"],
        // A clean dashboard-style status bar.
        &["set-option", "-g", "status", "on"],
        &["set-option", "-g", "status-interval", "5"],
        &["set-option", "-g", "status-justify", "left"],
        &["set-option", "-g", "status-style", "bg=#11141d,fg=#8a92a6"],
        // brand chip on the left
        &[
            "set-option",
            "-g",
            "status-left",
            "#[fg=#0c0e14,bg=#6cb6ff,bold] ◍ enxame #[bg=#11141d,fg=#8a92a6]  ",
        ],
        &["set-option", "-g", "status-left-length", "24"],
        // window list: hide the `_enxame` placeholder; current window = green chip
        &[
            "set-option",
            "-g",
            "window-status-format",
            "#{?#{==:#{window_name},_enxame},,#[fg=#5b6275] #I #W }",
        ],
        &[
            "set-option",
            "-g",
            "window-status-current-format",
            // NB: no commas inside the #[...] here — commas are argument
            // separators inside the #{?...} conditional, so a comma in a style
            // block leaks (e.g. `bold]`). Use separate #[] blocks instead.
            "#{?#{==:#{window_name},_enxame},,#[fg=#0c0e14]#[bg=#5ce08a]#[bold] #W #[default]}",
        ],
        &["set-option", "-g", "window-status-separator", ""],
        // right: session name + the detach hint, with Ctrl-Q accented
        &[
            "set-option",
            "-g",
            "status-right",
            "#[fg=#5b6275]#S  #[fg=#8a92a6]#[fg=#6cb6ff,bold]Ctrl-Q#[fg=#8a92a6,nobold] detach ",
        ],
        &["set-option", "-g", "status-right-length", "40"],
        &["set-option", "-g", "message-style", "bg=#6cb6ff,fg=#0c0e14"],
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
    let target = format!("{}:", session());
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

/// Rename a window (used to label agent windows by their task title).
pub fn rename_window(window_id: &str, name: &str) -> Result<()> {
    run(&["rename-window", "-t", window_id, name])?;
    Ok(())
}

/// Generate a fresh v4 UUID for use as a claude `--session-id`.
pub fn new_session_id() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        // Fallback: derive from time + pid (good enough for uniqueness).
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mix = t ^ ((std::process::id() as u128) << 96);
        b.copy_from_slice(&mix.to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Tag a window with the claude session id it's running, so the scanner can
/// resolve that window's exact transcript file (not just the newest in the dir).
pub fn set_window_session(window_id: &str, session_id: &str) -> Result<()> {
    run(&["set-option", "-w", "-t", window_id, "@enxame_session", session_id])?;
    Ok(())
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
        session(),
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

/// An agent window with its active pane's cwd, pane process pid, and the claude
/// session id we tagged it with (if any).
pub struct WinInfo {
    pub id: String,
    pub cwd: std::path::PathBuf,
    pub pane_pid: u32,
    pub session_id: Option<String>,
}

/// List agent windows with the cwd and pane pid of their active pane. The
/// `_enxame` placeholder window is excluded.
pub fn list_windows_with_cwd() -> Result<Vec<WinInfo>> {
    if !session_exists() {
        return Ok(Vec::new());
    }
    let out = run(&[
        "list-windows",
        "-t",
        session(),
        "-F",
        "#{window_id}\t#{window_name}\t#{pane_pid}\t#{@enxame_session}\t#{pane_current_path}",
    ])?;
    let mut v = Vec::new();
    for line in out.lines() {
        let mut parts = line.splitn(5, '\t');
        let (Some(id), Some(name), Some(pid), Some(sid), Some(path)) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if name == "_enxame" {
            continue;
        }
        v.push(WinInfo {
            id: id.to_string(),
            cwd: std::path::PathBuf::from(path),
            pane_pid: pid.trim().parse().unwrap_or(0),
            session_id: (!sid.is_empty()).then(|| sid.to_string()),
        });
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
    c.args(["attach-session", "-t", session()]);
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
