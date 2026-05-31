//! phasor — a terminal dashboard that monitors and orchestrates every running
//! Claude Code agent on the machine.
//!
//! Each agent is one tmux window (managed by phasor) or one external `claude`
//! process (discovered, monitor-only). The dashboard draws them as a field of
//! live cards (TUI) and, via [`server`], a force-directed graph in the browser.
//!
//! # Architecture
//!
//! - [`discover`] finds running `claude` processes (`ps` + `lsof`).
//! - [`transcript`] resolves and tails each agent's Claude Code transcript
//!   (`~/.claude/projects/<encoded-cwd>/<session>.jsonl`) for title, todos,
//!   phrases, touched folders, status and task-completion markers.
//! - [`tmux`] wraps the `tmux` CLI on phasor's own socket/session.
//! - [`scan`] fuses the three into a stream of [`agent::Agent`] snapshots on a
//!   background thread; both front-ends consume it without blocking.
//! - [`config`] holds the projects config (`~/.phasor/projects.json`).
//! - [`app`] + [`ui`] are the ratatui TUI; [`server`] is the axum web app.
//!
//! # Commands
//!
//! `phasor` (TUI) · `serve [port]` (web) · `exec`/`start CMD…` (spawn a window)
//! · `save`/`restore [file]` (snapshot & recreate sessions) · `doctor [cwd]`
//! · `render [WxH]`.

// Documentation coverage is enforced (warn-only) for every item, public or not.
#![warn(clippy::missing_docs_in_private_items)]

mod agent;
mod app;
mod config;
mod discover;
mod scan;
mod server;
mod session;
mod tmux;
mod transcript;
mod ui;

use anyhow::{Context, Result};
use app::App;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::time::Duration;
use ui::HitBox;

/// The concrete ratatui terminal type used throughout the dashboard.
type Term = Terminal<CrosstermBackend<Stdout>>;

fn main() -> Result<()> {
    if !tmux::available() {
        eprintln!("phasor requires tmux on PATH.");
        std::process::exit(1);
    }
    match std::env::args().nth(1).as_deref() {
        Some("doctor") => return doctor(),
        Some("render") => return render_once(),
        Some("exec") => return exec_window(),
        Some("start") => return start_window(),
        Some("save") => return save_sessions(),
        Some("restore") => return restore_sessions(),
        Some("serve") => {
            let port = std::env::args()
                .nth(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(7878);
            return server::serve(port);
        }
        // No args → the dashboard TUI.
        None => {}
        // An unrecognized command (e.g. `server` instead of `serve`) — don't
        // silently launch the TUI; show usage.
        Some(other) => {
            eprintln!(
                "phasor: unknown command '{other}'\n\
                 usage:\n  phasor                 dashboard (TUI)\n  \
                 phasor serve [port]    web dashboard (default 7878)\n  \
                 phasor start CMD…      run CMD in a new window and open it\n  \
                 phasor exec  CMD…      run CMD in a new window (background)\n  \
                 phasor save  [file]    snapshot managed sessions (cwd + id)\n  \
                 phasor restore [file]  recreate the saved sessions\n  \
                 phasor doctor [cwd]    diagnostics"
            );
            std::process::exit(2);
        }
    }
    run_dashboard(None)
}

/// Launch the dashboard TUI. If `initial_attach` is set, the dashboard opens
/// straight into that tmux window (used by `start`); detaching collapses
/// back into the dashboard.
fn run_dashboard(initial_attach: Option<String>) -> Result<()> {
    tmux::ensure_session().context("failed to create phasor tmux session")?;

    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal, initial_attach);
    restore_terminal(&mut terminal)?;
    res
}

/// Enter raw mode + the alternate screen and return a ready ratatui terminal.
fn setup_terminal() -> Result<Term> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

/// Restore the terminal on panic — otherwise a render panic leaves it in raw
/// mode with mouse capture on, and the user "can't type anything".
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default(info);
    }));
}

/// Leave the alternate screen and raw mode, restoring the normal terminal.
fn restore_terminal(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// The dashboard main loop: drain scanner updates, render, and handle input —
/// suspending to attach a tmux window or to edit the projects config on demand.
fn run(terminal: &mut Term, initial_attach: Option<String>) -> Result<()> {
    let mut app = App::new();
    app.attach_to = initial_attach;
    let mut hits: Vec<HitBox> = Vec::new();

    while !app.should_quit {
        // A requested attach suspends the TUI and hands the screen to tmux.
        // Handled first so `start` opens straight into the window with no
        // dashboard flash; detaching (Alt-o / prefix+d) drops back here.
        // A failure must NOT kill the dashboard — show it in the status line.
        if let Some(window_id) = app.attach_to.take() {
            if let Err(e) = attach(terminal, &window_id) {
                app.note(format!("could not open terminal: {e}"));
            }
            continue;
        }

        // Edit projects config in $EDITOR — also suspends the TUI.
        if std::mem::take(&mut app.edit_projects) {
            match edit_projects(terminal) {
                Ok(()) => app.note("projects saved — colors update within a few seconds"),
                Err(e) => app.note(format!("could not edit projects: {e}")),
            }
            continue;
        }

        app.drain_updates();
        terminal.draw(|f| {
            hits = ui::render(f, &app);
        })?;

        // Wait briefly for input, then drain every queued event this iteration
        // so bursts of keystrokes are all handled (no dropped/laggy keys).
        if event::poll(Duration::from_millis(100))? {
            loop {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                    Event::Mouse(m) => {
                        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                            if let Some(h) = hits.iter().find(|h| h.contains(m.column, m.row)) {
                                app.select(h.idx);
                                app.open_selected();
                            }
                        }
                    }
                    _ => {}
                }
                if app.should_quit || app.attach_to.is_some() || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Start the command (everything after the subcommand) in a new tmux window of
/// the phasor session, in the current directory. Returns the window plus a
/// human-readable command string.
fn spawn_exec_window(subcmd: &str) -> Result<(tmux::Window, String)> {
    let cmd: Vec<String> = std::env::args().skip(2).collect();
    if cmd.is_empty() {
        anyhow::bail!("usage: phasor {subcmd} <command> [args...]");
    }
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    let name = cwd
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "agent".into());

    // If launching claude, give it a known session id so the window maps to its
    // exact transcript. (Skip for arbitrary commands.)
    let mut cmd = cmd;
    let is_claude = cmd
        .first()
        .map(|c| c == "claude" || c.ends_with("/claude"))
        .unwrap_or(false);
    let sid = if is_claude && !cmd.iter().any(|a| a == "--session-id") {
        let sid = tmux::new_session_id();
        cmd.insert(1, "--session-id".into());
        cmd.insert(2, sid.clone());
        Some(sid)
    } else {
        None
    };

    // tmux runs the window command through a shell, so shell-quote each argv
    // element to preserve boundaries (e.g. `sh -c "a; b"`).
    let joined = cmd
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let win = tmux::new_window(&name, &cwd.to_string_lossy(), &joined)
        .context("failed to create tmux window")?;
    if let Some(sid) = sid {
        let _ = tmux::set_window_session(&win.id, &sid);
    }
    Ok((win, cmd.join(" ")))
}

/// `phasor exec <command...>`: spawn the command in a new phasor tmux window,
/// then exit. Lets external scripts seed phasor-managed agents (they show up as
/// openable in the dashboard).
fn exec_window() -> Result<()> {
    let (win, shown) = spawn_exec_window("exec")?;
    println!(
        "phasor: launched [{}] in tmux window {} ({})",
        shown, win.id, win.name
    );
    Ok(())
}

/// `phasor start <command...>`: like `exec`, but launches the dashboard opened
/// straight into the new window. Detaching (Ctrl-Q / prefix+d) collapses back
/// into the dashboard, where the agent is a card you can re-open (Enter).
fn start_window() -> Result<()> {
    let (win, _shown) = spawn_exec_window("start")?;
    run_dashboard(Some(win.id))
}

/// Resolve the snapshot file: a positional arg overrides `~/.phasor/session.json`.
fn session_arg_path() -> Result<std::path::PathBuf> {
    match std::env::args().nth(2) {
        Some(a) => Ok(std::path::PathBuf::from(a)),
        None => session::path().context("no home directory"),
    }
}

/// `phasor save [file]`: snapshot every agent (cwd + claude session id) so it
/// can be recreated later. Sources the full discovery, so **external** (non-tmux)
/// claudes are saved too: the session id comes from the `@phasor_session` option
/// for managed agents, or from the resolved transcript filename for external
/// ones. Agents with no resolvable session (no transcript yet) are skipped.
fn save_sessions() -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    let agents: Vec<session::SavedAgent> = scan::snapshot()
        .into_iter()
        .filter_map(|a| {
            let session_id = a.session_id.clone().or_else(|| {
                a.transcript
                    .as_ref()
                    .and_then(|p| p.file_stem())
                    .map(|s| s.to_string_lossy().into_owned())
            })?;
            if !seen.insert(session_id.clone()) {
                return None; // de-dup identical sessions
            }
            Some(session::SavedAgent {
                cwd: a.cwd.clone(),
                session_id,
                title: Some(a.label()),
                managed: a.openable(),
            })
        })
        .collect();
    let path = session_arg_path()?;
    session::save_to(&path, &agents)?;
    let managed = agents.iter().filter(|a| a.managed).count();
    println!(
        "phasor: saved {} session(s) — {} managed, {} external — to {}",
        agents.len(),
        managed,
        agents.len() - managed,
        path.display()
    );
    Ok(())
}

/// `phasor restore [file]`: recreate the saved agents — one tmux window each,
/// resuming the claude session when its transcript still exists, else starting
/// a fresh session pinned to the same id. Sessions already open are skipped.
fn restore_sessions() -> Result<()> {
    let path = session_arg_path()?;
    let saved = session::load_from(&path)?;
    if saved.is_empty() {
        println!("phasor: nothing to restore ({} is empty)", path.display());
        return Ok(());
    }
    tmux::ensure_session().context("failed to create phasor tmux session")?;

    // Skip sessions that are already open so restore is idempotent.
    let open: std::collections::HashSet<String> = tmux::list_windows_with_cwd()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|w| w.session_id)
        .collect();

    let (mut restored, mut skipped, mut missing) = (0u32, 0u32, 0u32);
    for a in &saved {
        if open.contains(&a.session_id) {
            skipped += 1;
            continue;
        }
        if !a.cwd.is_dir() {
            eprintln!("  skip (directory gone): {}", a.cwd.display());
            missing += 1;
            continue;
        }
        let name = a.title.clone().unwrap_or_else(|| {
            a.cwd
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "agent".into())
        });
        // Resume the existing conversation if its transcript is still on disk;
        // otherwise launch a fresh session pinned to the same id.
        let has_transcript = transcript::project_dir(&a.cwd)
            .map(|d| d.join(format!("{}.jsonl", a.session_id)).exists())
            .unwrap_or(false);
        let flag = if has_transcript {
            "--resume"
        } else {
            "--session-id"
        };
        let cmd = format!("claude {flag} {}", shell_quote(&a.session_id));
        match tmux::new_window(&name, &a.cwd.to_string_lossy(), &cmd) {
            Ok(win) => {
                let _ = tmux::set_window_session(&win.id, &a.session_id);
                restored += 1;
            }
            Err(e) => eprintln!("  failed to restore {}: {e}", a.cwd.display()),
        }
    }
    println!(
        "phasor: restored {restored} session(s) — {skipped} already open, {missing} missing dir(s) — from {}",
        path.display()
    );
    Ok(())
}

/// Wrap an argument in single quotes for safe shell execution.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Render a single dashboard frame to an off-screen buffer and print it as
/// plain text. Lets you eyeball the galaxy layout without a TTY.
/// Usage: `phasor render [WIDTHxHEIGHT]`.
fn render_once() -> Result<()> {
    use ratatui::backend::TestBackend;

    let (w, h) = std::env::args()
        .nth(2)
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((120u16, 40u16));

    let app = App::new();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|f| {
        let _ = ui::render(f, &app);
    })?;

    let buf = terminal.backend().buffer();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

/// Non-TUI self-check: prints tmux windows and parses the most recent
/// transcript under a given cwd. Usage: `phasor doctor [cwd]`.
fn doctor() -> Result<()> {
    use std::path::PathBuf;
    use std::time::SystemTime;

    println!("tmux available: yes");
    match tmux::list_windows() {
        Ok(ws) => {
            println!("phasor windows: {}", ws.len());
            for w in ws {
                println!("  {} {}", w.id, w.name);
            }
        }
        Err(e) => println!("list_windows error: {e}"),
    }

    println!("\ndiscovered running claudes:");
    for p in discover::running_claudes() {
        println!("  pid {} (ppid {})  {}", p.pid, p.ppid, p.cwd.display());
    }

    let cwd = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap();
    println!("\nresolving transcript for: {}", cwd.display());
    match transcript::project_dir(&cwd) {
        Some(d) => println!("project dir: {} (exists: {})", d.display(), d.exists()),
        None => println!("no project dir"),
    }
    match transcript::newest_session(&cwd, SystemTime::UNIX_EPOCH) {
        Some(path) => {
            println!("session: {}", path.display());
            let st = transcript::parse(&path, &cwd)?;
            println!("  title:   {:?}", st.title);
            println!("  status:  {:?}", st.status);
            println!("  todos:   {:?}", st.todos);
            println!("  dirs:    {:?}", st.work_dirs);
            println!("  phrases:");
            for p in &st.last_phrases {
                println!("    “{}”", p);
            }
        }
        None => println!("no session file found"),
    }
    Ok(())
}

/// Suspend the dashboard and open the projects config in `$EDITOR`. Seeds a
/// commented example if the file doesn't exist yet, so the user sees the
/// expected shape. Restores the TUI afterward.
fn edit_projects(terminal: &mut Term) -> Result<()> {
    let path = config::path().context("no home directory")?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let example = serde_json::to_string_pretty(&vec![config::Project {
            name: "example".into(),
            prefix: dirs::home_dir()
                .map(|h| h.join("src").to_string_lossy().into_owned())
                .unwrap_or_else(|| "/path/to/projects".into()),
            color: "#7aa2f7".into(),
        }])?;
        std::fs::write(&path, example).ok();
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());

    restore_terminal(terminal)?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} {}", shell_quote(&path.to_string_lossy())))
        .status();
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;
    status.context("failed to launch editor")?;
    Ok(())
}

/// Suspend the dashboard, attach the real terminal to a tmux window, then
/// restore the dashboard when the user detaches (prefix + d).
fn attach(terminal: &mut Term, window_id: &str) -> Result<()> {
    restore_terminal(terminal)?;
    let status = tmux::attach_command(window_id).status();
    // Re-enter the TUI regardless of how the attach ended.
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;
    status.context("failed to attach to tmux window")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_wraps_in_single_quotes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a b c"), "'a b c'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_escapes_embedded_quotes() {
        // a'b → 'a'\''b'  (close, escaped quote, reopen)
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("''"), "''\\'''\\'''");
    }
}
