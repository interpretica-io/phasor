//! Web dashboard mirroring the TUI, backed by the same [`crate::scan`].
//!
//! A tiny hand-rolled HTTP/1.1 server (one thread per connection) serves the
//! embedded page and a JSON API, and upgrades `/ws?w=@N` connections to a
//! WebSocket bridged to a PTY running `tmux attach` — so openable agents can be
//! driven live in the browser via xterm.js.

use crate::agent::{is_noise_folder, Agent, Status};
use crate::scan;
use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use tungstenite::handshake::derive_accept_key;


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
    load: u8,
    activity: Vec<u8>,
    seq: u64,
}

#[derive(Serialize)]
struct Payload {
    openable: usize,
    external: usize,
    agents: Vec<AgentDto>,
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

    // Interactive terminal: disable Nagle so keystrokes/output aren't batched.
    stream.set_nodelay(true).ok();
    bridge_pty(stream, &win)
}

/// Bridge a WebSocket (post-handshake `stream`) to a PTY running `tmux attach`.
///
/// Read and write use independent clones of the socket so the blocking PTY/WS
/// reads never stall the output path. Writes (frames + pongs) are serialized by
/// a mutex; that mutex is only ever held for the duration of a quick write.
fn bridge_pty(stream: TcpStream, win: &str) -> Result<()> {
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })?;

    // Select the requested window, then attach this client to the session.
    let (sock, sess) = (crate::tmux::socket(), crate::tmux::session());
    let script = format!(
        "tmux -L {sock} select-window -t {win} 2>/dev/null; exec tmux -L {sock} attach -t {sess}"
    );
    let mut cmd = CommandBuilder::new("sh");
    cmd.arg("-c");
    cmd.arg(&script);
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut pty_reader = pair.master.try_clone_reader()?;
    let mut pty_writer = pair.master.take_writer()?;
    let master = pair.master; // used only here, for resize

    let mut ws_read = stream.try_clone()?;
    let ws_write = Arc::new(Mutex::new(stream));

    // PTY -> WebSocket (binary frames). Its own write clone; no lock contention
    // with the input loop's blocking reads.
    let out = ws_write.clone();
    let pump = thread::spawn(move || {
        let mut buf = [0u8; 16384];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut w = out.lock().unwrap();
                    if write_frame(&mut *w, 0x2, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = write_frame(&mut *out.lock().unwrap(), 0x8, &[]); // close
    });

    // WebSocket -> PTY (binary = input bytes, text = resize JSON, ping -> pong).
    loop {
        match read_frame(&mut ws_read) {
            Ok(Some((0x2, payload))) => {
                let _ = pty_writer.write_all(&payload);
                let _ = pty_writer.flush();
            }
            Ok(Some((0x1, payload))) => {
                let t = String::from_utf8_lossy(&payload);
                if let Some((cols, rows)) = parse_resize(&t) {
                    let _ = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                }
            }
            Ok(Some((0x9, payload))) => {
                let _ = write_frame(&mut *ws_write.lock().unwrap(), 0xA, &payload);
            }
            Ok(Some((0x8, _))) | Ok(None) | Err(_) => break,
            Ok(Some(_)) => {}
        }
    }

    let _ = child.kill();
    let _ = ws_read.shutdown(std::net::Shutdown::Both);
    let _ = pump.join();
    Ok(())
}

/// Write a single (unmasked, FIN) WebSocket server frame.
fn write_frame(w: &mut impl Write, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(10);
    head.push(0x80 | (opcode & 0x0f));
    let n = payload.len();
    if n < 126 {
        head.push(n as u8);
    } else if n <= 0xffff {
        head.push(126);
        head.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        head.push(127);
        head.extend_from_slice(&(n as u64).to_be_bytes());
    }
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one WebSocket frame from a client, unmasking. Returns `(opcode,
/// payload)`, or `None` on EOF. Assumes unfragmented frames (true for the tiny
/// input/resize/ping frames browsers send here).
fn read_frame(r: &mut impl Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut h = [0u8; 2];
    if r.read_exact(&mut h).is_err() {
        return Ok(None);
    }
    let opcode = h[0] & 0x0f;
    let masked = h[1] & 0x80 != 0;
    let mut len = (h[1] & 0x7f) as usize;
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b)?;
        len = u16::from_be_bytes(b) as usize;
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b)?;
        len = u64::from_be_bytes(b) as usize;
    }
    let mask = if masked {
        let mut m = [0u8; 4];
        r.read_exact(&mut m)?;
        Some(m)
    } else {
        None
    };
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Ok(Some((opcode, payload)))
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
