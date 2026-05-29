//! enxame — a terminal dashboard that orchestrates multiple Claude Code
//! agents, each running in its own tmux window, shown as a live block diagram.

mod agent;
mod app;
mod discover;
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
        _ => {}
    }
    tmux::ensure_session().context("failed to create enxame tmux session")?;

    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal);
    restore_terminal(&mut terminal)?;
    res
}

fn setup_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
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

fn run(terminal: &mut Term) -> Result<()> {
    let mut app = App::new();
    let mut hits: Vec<HitBox> = Vec::new();

    while !app.should_quit {
        app.maybe_poll();
        terminal.draw(|f| {
            hits = ui::render(f, &app);
        })?;

        if event::poll(Duration::from_millis(200))? {
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
        }

        // A requested attach suspends the TUI and hands the screen to tmux.
        // A failure here must NOT kill the dashboard — surface it in the
        // status line and carry on.
        if let Some(window_id) = app.attach_to.take() {
            if let Err(e) = attach(terminal, &window_id) {
                app.status = format!("could not open terminal: {e}");
            }
        }
    }
    Ok(())
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

    println!("\ndiscovered running claudes (by cwd):");
    for (cwd, pids) in discover::running_claudes() {
        println!("  {:?}  {}", pids, cwd.display());
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
