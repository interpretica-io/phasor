//! Web dashboard mirroring the TUI, backed by the same [`crate::scan`].
//!
//! Built on **axum**: it serves the embedded page and a small JSON API, and
//! upgrades `/ws?w=@N` connections to a WebSocket bridged to a PTY running
//! `tmux attach` — so openable agents can be driven live in the browser via
//! xterm.js. The blocking scanner and PTY I/O run on dedicated OS threads and
//! talk to the async runtime over channels.

use crate::agent::{is_noise_folder, Agent, Status};
use crate::{config, scan};
use anyhow::{anyhow, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::{Arc, RwLock};
use std::thread;
use tokio::sync::mpsc;

#[derive(Serialize)]
struct AgentDto {
    label: String,
    id: String,
    cwd: String,
    openable: bool,
    wid: Option<String>,
    status: String,
    procs: usize,
    todos: Option<[usize; 2]>,
    phrase: Option<String>,
    folders: Vec<String>,
    load: u8,
    activity: Vec<u8>,
    seq: u64,
    pending: Option<String>,
    /// Project (from `~/.enxame/projects.json`) the agent's cwd falls under.
    project: Option<String>,
    pcolor: Option<String>,
}

#[derive(Serialize)]
struct Payload {
    openable: usize,
    external: usize,
    agents: Vec<AgentDto>,
}

/// Query string `?w=@N` shared by the WebSocket and instruct endpoints.
#[derive(Deserialize)]
struct WParam {
    w: Option<String>,
}

fn to_dto(a: &Agent) -> AgentDto {
    // Full (absolute) paths — clustering/overlap must compare canonical paths,
    // not basenames (two unrelated `src` dirs are not the same folder). The UI
    // derives the short label from the basename for display.
    // Open/working folders the agent has: its cwd (always), any `/add-dir`
    // dirs, and the subdirs it edits in. The cwd is kept so the list is never
    // empty (every agent has at least one open folder).
    let mut seen = HashSet::new();
    let folders = a
        .state
        .work_dirs
        .iter()
        .filter(|d| {
            d.file_name()
                .map(|n| !is_noise_folder(&n.to_string_lossy()))
                .unwrap_or(true)
        })
        .map(|d| d.to_string_lossy().into_owned())
        .filter(|p| p.starts_with('/') && !p.contains('"'))
        .filter(|p| seen.insert(p.clone()))
        .collect();
    AgentDto {
        id: a.id.clone(),
        label: a.label(),
        cwd: a.cwd.to_string_lossy().into_owned(),
        openable: a.openable(),
        wid: a.window_id.clone(),
        status: match a.state.status {
            Status::Working => "working",
            Status::Idle => "idle",
            Status::Unknown => "unknown",
        }
        .into(),
        procs: a.pids.len(),
        todos: a.state.todos.map(|(d, t)| [d, t]),
        phrase: a.state.last_phrases.back().cloned(),
        folders,
        load: a.load(),
        activity: a.activity.iter().copied().collect(),
        seq: a.completions,
        pending: a.pending.clone(),
        project: a.project_name.clone(),
        pcolor: a.project_color.clone(),
    }
}

type Shared = Arc<RwLock<Vec<Agent>>>;

/// Run the web dashboard until the process is killed.
pub fn serve(port: u16) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let latest: Shared = Arc::new(RwLock::new(scan::snapshot()));

        // The scanner streams snapshots over a *blocking* std channel; pump it
        // on a dedicated OS thread into the shared state the handlers read.
        {
            let latest = latest.clone();
            let rx = scan::spawn();
            thread::spawn(move || {
                for snap in rx {
                    if let Ok(mut w) = latest.write() {
                        *w = snap;
                    }
                }
            });
        }

        let app = Router::new()
            .route("/", get(index))
            .route("/index.html", get(index))
            .route("/api/agents", get(agents))
            .route("/api/projects", get(projects_get).post(projects_post))
            .route("/api/instruct", post(instruct))
            .route("/ws", get(ws_handler))
            .with_state(latest);

        let addr = format!("127.0.0.1:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| anyhow!("failed to bind {addr}: {e}"))?;
        println!("enxame web dashboard → http://{addr}");
        axum::serve(listener, app)
            .await
            .map_err(|e| anyhow!("server error: {e}"))
    })
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn agents(State(latest): State<Shared>) -> Json<Payload> {
    let agents = latest.read().unwrap();
    let openable = agents.iter().filter(|a| a.openable()).count();
    Json(Payload {
        openable,
        external: agents.len() - openable,
        agents: agents.iter().map(to_dto).collect(),
    })
}

async fn projects_get() -> Json<Vec<config::Project>> {
    Json(config::load())
}

/// Save the projects config (JSON array of `{name, prefix, color}`).
async fn projects_post(Json(projects): Json<Vec<config::Project>>) -> (StatusCode, String) {
    match config::save(&projects) {
        Ok(()) => (StatusCode::OK, "ok".into()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Queue an instruction to auto-send when an agent finishes its turn.
async fn instruct(Query(q): Query<WParam>, body: String) -> (StatusCode, &'static str) {
    let w = q.w.unwrap_or_default();
    if !valid_window(&w) {
        return (StatusCode::BAD_REQUEST, "bad window");
    }
    let text = body.trim();
    let _ = crate::tmux::set_window_pending(&w, text);
    (StatusCode::OK, "ok")
}

/// Upgrade to a WebSocket bridged to a PTY running `tmux attach`.
async fn ws_handler(ws: WebSocketUpgrade, Query(q): Query<WParam>) -> Response {
    // Window id is interpolated into a shell command — validate strictly.
    let win = q.w.unwrap_or_default();
    if !valid_window(&win) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    ws.on_upgrade(move |socket| bridge_pty(socket, win))
}

/// Bridge an upgraded WebSocket to a PTY running `tmux attach`.
///
/// portable-pty is blocking, so reads and writes live on their own OS threads
/// and talk to this async task over channels: PTY output → unbounded tokio
/// channel → WS sink; WS input → std channel → PTY writer thread. Window
/// resizes are applied inline (a quick ioctl).
async fn bridge_pty(socket: WebSocket, win: String) {
    let pty = native_pty_system();
    let Ok(pair) = pty.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) else {
        return;
    };

    // Select the requested window, then attach this client to the session.
    let (sock, sess) = (crate::tmux::socket(), crate::tmux::session());
    let script = format!(
        "tmux -L {sock} select-window -t {win} 2>/dev/null; exec tmux -L {sock} attach -t {sess}"
    );
    let mut cmd = CommandBuilder::new("sh");
    cmd.arg("-c");
    cmd.arg(&script);
    let Ok(mut child) = pair.slave.spawn_command(cmd) else {
        return;
    };
    drop(pair.slave);

    let Ok(mut reader) = pair.master.try_clone_reader() else {
        let _ = child.kill();
        return;
    };
    let Ok(mut writer) = pair.master.take_writer() else {
        let _ = child.kill();
        return;
    };
    let master = pair.master; // kept for resize

    // PTY -> async (blocking read thread).
    let (otx, mut orx) = mpsc::unbounded_channel::<Vec<u8>>();
    let read_thread = thread::spawn(move || {
        let mut buf = [0u8; 16384];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if otx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // async -> PTY (blocking write thread).
    let (itx, irx) = std::sync::mpsc::channel::<Vec<u8>>();
    let write_thread = thread::spawn(move || {
        while let Ok(bytes) = irx.recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let (mut sink, mut stream) = socket.split();

    // Forward PTY output to the browser as binary frames.
    let send_task = tokio::spawn(async move {
        while let Some(chunk) = orx.recv().await {
            if sink.send(Message::Binary(chunk.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    // Browser input: binary = keystrokes, text = resize JSON. Ping/Pong are
    // handled by axum automatically.
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Binary(b) => {
                if itx.send(b.to_vec()).is_err() {
                    break;
                }
            }
            Message::Text(t) => {
                if let Some((cols, rows)) = parse_resize(t.as_str()) {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Teardown: kill the attach client, stop the threads, stop forwarding.
    let _ = child.kill();
    drop(itx); // ends the write thread
    send_task.abort();
    let _ = write_thread.join();
    let _ = read_thread.join();
}

fn parse_resize(t: &str) -> Option<(u16, u16)> {
    let v: serde_json::Value = serde_json::from_str(t).ok()?;
    let r = v.get("resize")?;
    let cols = r.get("cols")?.as_u64()? as u16;
    let rows = r.get("rows")?.as_u64()? as u16;
    // Never resize the agent's terminal to a tiny size — Ink/React TUIs (like
    // claude) crash on 0/1-cell dimensions. Clamp to a sane floor.
    if cols < 10 || rows < 4 {
        return None;
    }
    Some((cols, rows))
}

/// Accept only `@<digits>` so the value is safe to put in a shell command.
fn valid_window(w: &str) -> bool {
    w.len() > 1 && w.starts_with('@') && w[1..].chars().all(|c| c.is_ascii_digit())
}

const INDEX_HTML: &str = include_str!("server_index.html");
