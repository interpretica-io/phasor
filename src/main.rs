//! enxame — a terminal dashboard that orchestrates multiple Claude Code
//! agents, each running in its own tmux window, shown as a live block diagram.

mod agent;
mod app;
mod discover;
mod scan;
mod server;
mod transcript;
mod tmux;
mod ui;

use anyhow::{Context, Result};
use app::App;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton,
    MouseEventKind,
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

type Term = Terminal<CrosstermBackend<Stdout>>;

fn main() -> Result<()> {
    if !tmux::available() {
        eprintln!("enxame requires tmux on PATH.");
        std::process::exit(1);
    }
    match std::env::args().nth(1).as_deref() {
        Some("doctor") => return doctor(),
        Some("render") => return render_once(),
        Some("--exec") => return exec_window(),
        Some("--start") => return start_window(),
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
                "enxame: unknown command '{other}'\n\
                 usage:\n  enxame                 dashboard (TUI)\n  \
                 enxame serve [port]    web dashboard (default 7878)\n  \
                 enxame --start CMD…    run CMD in a new window and open it\n  \
                 enxame --exec  CMD…    run CMD in a new window (background)\n  \
                 enxame doctor [cwd]    diagnostics"
            );
            std::process::exit(2);
        }
    }
    run_dashboard(None)
}

/// Launch the dashboard TUI. If `initial_attach` is set, the dashboard opens
/// straight into that tmux window (used by `--start`); detaching collapses
/// back into the dashboard.
fn run_dashboard(initial_attach: Option<String>) -> Result<()> {
    tmux::ensure_session().context("failed to create enxame tmux session")?;

    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal, initial_attach);
    restore_terminal(&mut terminal)?;
    res
}

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

fn run(terminal: &mut Term, initial_attach: Option<String>) -> Result<()> {
    let mut app = App::new();
    app.attach_to = initial_attach;
    let mut hits: Vec<HitBox> = Vec::new();

    while !app.should_quit {
        // A requested attach suspends the TUI and hands the screen to tmux.
        // Handled first so `--start` opens straight into the window with no
        // dashboard flash; detaching (Alt-o / prefix+d) drops back here.
        // A failure must NOT kill the dashboard — show it in the status line.
        if let Some(window_id) = app.attach_to.take() {
            if let Err(e) = attach(terminal, &window_id) {
                app.note(format!("could not open terminal: {e}"));
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

/// Start the command (everything after the flag) in a new tmux window of the
/// enxame session, in the current directory. Returns the window plus a
/// human-readable command string.
fn spawn_exec_window(flag: &str) -> Result<(tmux::Window, String)> {
    let cmd: Vec<String> = std::env::args().skip(2).collect();
    if cmd.is_empty() {
        anyhow::bail!("usage: enxame {flag} <command> [args...]");
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
    let joined = cmd.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    let win = tmux::new_window(&name, &cwd.to_string_lossy(), &joined)
        .context("failed to create tmux window")?;
    if let Some(sid) = sid {
        let _ = tmux::set_window_session(&win.id, &sid);
    }
    Ok((win, cmd.join(" ")))
}

/// `enxame --exec <command...>`: spawn the command in a new enxame tmux window,
/// then exit. Lets external scripts seed enxame-managed agents (they show up as
/// openable in the dashboard).
fn exec_window() -> Result<()> {
    let (win, shown) = spawn_exec_window("--exec")?;
    println!("enxame: launched [{}] in tmux window {} ({})", shown, win.id, win.name);
    Ok(())
}

/// `enxame --start <command...>`: like `--exec`, but launches the dashboard
/// opened straight into the new window. Detaching (Alt-o / prefix+d) collapses
/// back into the dashboard, where the agent is a card you can re-open (Enter).
fn start_window() -> Result<()> {
    let (win, _shown) = spawn_exec_window("--start")?;
    run_dashboard(Some(win.id))
}

/// Wrap an argument in single quotes for safe shell execution.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Render a single dashboard frame to an off-screen buffer and print it as
/// plain text. Lets you eyeball the galaxy layout without a TTY.
/// Usage: `enxame render [WIDTHxHEIGHT]`.
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
/// transcript under a given cwd. Usage: `enxame doctor [cwd]`.
fn doctor() -> Result<()> {
    use std::path::PathBuf;
    use std::time::SystemTime;

    println!("tmux available: yes");
    match tmux::list_windows() {
        Ok(ws) => {
            println!("enxame windows: {}", ws.len());
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
