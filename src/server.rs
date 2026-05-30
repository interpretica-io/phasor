//! Web dashboard mirroring the TUI, backed by the same [`crate::scan`].
//!
//! A tiny hand-rolled HTTP/1.1 server (one thread per connection) serves the
//! embedded page and a JSON API, and upgrades `/ws?w=@N` connections to a
//! WebSocket bridged to a PTY running `tmux attach` — so openable agents can be
//! driven live in the browser via xterm.js.

use crate::agent::{Agent, Status};
use crate::scan;
use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::{Role, WebSocket};
use tungstenite::Message;

const SESSION: &str = "enxame";
const SOCKET: &str = "enxame";

#[derive(Serialize)]
struct AgentDto {
    label: String,
    cwd: String,
    openable: bool,
    wid: Option<String>,
    status: String,
    procs: usize,
    todos: Option<[usize; 2]>,
    phrase: Option<String>,
    folders: Vec<String>,
}

#[derive(Serialize)]
struct Payload {
    openable: usize,
    external: usize,
    agents: Vec<AgentDto>,
}

fn to_dto(a: &Agent) -> AgentDto {
    let mut seen = HashSet::new();
    let folders = a
        .state
        .work_dirs
        .iter()
        .filter(|d| **d != a.cwd)
        .filter_map(|d| d.file_name().map(|s| s.to_string_lossy().into_owned()))
        .filter(|n| seen.insert(n.clone()))
        .collect();
    AgentDto {
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
    }
}

type Shared = Arc<RwLock<Vec<Agent>>>;

/// Run the web dashboard until the process is killed.
pub fn serve(port: u16) -> Result<()> {
    let latest: Shared = Arc::new(RwLock::new(scan::snapshot()));
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

    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr).map_err(|e| anyhow!("failed to bind {addr}: {e}"))?;
    println!("enxame web dashboard → http://{addr}");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let latest = latest.clone();
        thread::spawn(move || {
            let _ = handle(stream, latest);
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, latest: Shared) -> Result<()> {
    let head = read_request_head(&mut stream)?;
    let (target, headers) = parse_head(&head);
    let path = target.split('?').next().unwrap_or("/");

    let is_ws = headers
        .get("upgrade")
        .map(|u| u.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if is_ws && path == "/ws" {
        return handle_ws(stream, &target, &headers);
    }

    match path {
        "/" | "/index.html" => write_http(stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        "/api/agents" => {
            let body = {
                let agents = latest.read().unwrap();
                let openable = agents.iter().filter(|a| a.openable()).count();
                let payload = Payload {
                    openable,
                    external: agents.len() - openable,
                    agents: agents.iter().map(to_dto).collect(),
                };
                serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
            };
            write_http(stream, "200 OK", "application/json", body.as_bytes())
        }
        _ => write_http(stream, "404 Not Found", "text/plain", b"not found"),
    }
    Ok(())
}

/// Upgrade to a WebSocket and bridge it to a PTY running `tmux attach`.
fn handle_ws(mut stream: TcpStream, target: &str, headers: &HashMap<String, String>) -> Result<()> {
    let Some(key) = headers.get("sec-websocket-key") else {
        return Ok(());
    };
    let accept = derive_accept_key(key.as_bytes());
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(resp.as_bytes())?;

    // Window id from the query, strictly validated (it is interpolated into a
    // shell command).
    let win = query_param(target, "w").unwrap_or_default();
    if !valid_window(&win) {
        return Ok(());
    }

    // A short read timeout lets the input loop release the WebSocket lock so the
    // output thread can write — a simple full-duplex over one socket.
    stream.set_read_timeout(Some(Duration::from_millis(20))).ok();
    let ws = WebSocket::from_raw_socket(stream, Role::Server, None);
    bridge_pty(ws, &win)
}

fn bridge_pty(ws: WebSocket<TcpStream>, win: &str) -> Result<()> {
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })?;

    // Select the requested window, then attach this client to the session.
    let script = format!(
        "tmux -L {SOCKET} select-window -t {win} 2>/dev/null; exec tmux -L {SOCKET} attach -t {SESSION}"
    );
    let mut cmd = CommandBuilder::new("sh");
    cmd.arg("-c");
    cmd.arg(&script);
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let master = Arc::new(Mutex::new(pair.master));

    let ws = Arc::new(Mutex::new(ws));

    // PTY -> WebSocket
    let ws_out = ws.clone();
    let pump = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut g = ws_out.lock().unwrap();
                    if g.send(Message::Binary(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ws_out.lock().unwrap().close(None);
    });

    // WebSocket -> PTY (and resize control messages)
    loop {
        let msg = { ws.lock().unwrap().read() };
        match msg {
            Ok(Message::Binary(b)) => {
                let _ = writer.write_all(&b);
                let _ = writer.flush();
            }
            Ok(Message::Text(t)) => {
                if let Some((cols, rows)) = parse_resize(&t) {
                    let _ = master.lock().unwrap().resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // No input this slice; let the output thread breathe.
                continue;
            }
            Err(_) => break,
        }
    }

    let _ = child.kill();
    let _ = pump.join();
    Ok(())
}

fn parse_resize(t: &str) -> Option<(u16, u16)> {
    let v: serde_json::Value = serde_json::from_str(t).ok()?;
    let r = v.get("resize")?;
    let cols = r.get("cols")?.as_u64()? as u16;
    let rows = r.get("rows")?.as_u64()? as u16;
    Some((cols.max(1), rows.max(1)))
}

/// Accept only `@<digits>` so the value is safe to put in a shell command.
fn valid_window(w: &str) -> bool {
    w.len() > 1 && w.starts_with('@') && w[1..].chars().all(|c| c.is_ascii_digit())
}

fn query_param(target: &str, key: &str) -> Option<String> {
    let q = target.split_once('?')?.1;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read an HTTP request head byte-by-byte (so the stream isn't over-read past
/// the blank line — important for the WebSocket upgrade that follows).
fn read_request_head(s: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::new();
    let mut one = [0u8; 1];
    loop {
        let n = s.read(&mut one)?;
        if n == 0 {
            break;
        }
        buf.push(one[0]);
        if buf.ends_with(b"\r\n\r\n") || buf.len() > 16384 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn parse_head(head: &str) -> (String, HashMap<String, String>) {
    let mut lines = head.split("\r\n");
    let target = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    (target, headers)
}

fn write_http(mut s: TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
    let _ = s.flush();
}

const INDEX_HTML: &str = include_str!("server_index.html");
