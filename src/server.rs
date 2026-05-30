//! A small read-only web dashboard mirroring the TUI, backed by the same
//! [`crate::scan`] discovery. Serves an embedded HTML page plus a JSON API.

use crate::agent::{Agent, Status};
use crate::scan;
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::thread;
use tiny_http::{Header, Response, Server};

#[derive(Serialize)]
struct AgentDto {
    label: String,
    cwd: String,
    openable: bool,
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

/// Run the web dashboard until the process is killed.
pub fn serve(port: u16) -> Result<()> {
    // Keep a shared latest snapshot, updated by the background scanner.
    let latest = Arc::new(RwLock::new(scan::snapshot()));
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
    let server = Server::http(&addr).map_err(|e| anyhow!("failed to bind {addr}: {e}"))?;
    println!("enxame web dashboard → http://{addr}");

    for req in server.incoming_requests() {
        match req.url() {
            "/" | "/index.html" => {
                let _ = req.respond(Response::from_string(INDEX_HTML).with_header(header(
                    "Content-Type",
                    "text/html; charset=utf-8",
                )));
            }
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
                let _ = req.respond(
                    Response::from_string(body).with_header(header("Content-Type", "application/json")),
                );
            }
            _ => {
                let _ = req.respond(Response::from_string("not found").with_status_code(404));
            }
        }
    }
    Ok(())
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("valid header")
}

const INDEX_HTML: &str = r####"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>◍ enxame</title>
<style>
  :root{
    --bg:#0c0e14; --card:#161a26; --card2:#1b2030;
    --line:#262c3d; --txt:#dfe4f0; --dim:#8a92a6; --faint:#5b6275;
    --accent:#6cb6ff; --tmux:#6cb6ff; --green:#7ee787; --amber:#e3b341;
    --grey:#79839a; --barbg:#222838;
  }
  *{box-sizing:border-box}
  html,body{margin:0;height:100%}
  body{
    background:radial-gradient(1200px 700px at 80% -10%, #16203a 0%, var(--bg) 60%) ,var(--bg);
    color:var(--txt); font:14px/1.45 -apple-system,BlinkMacSystemFont,"SF Pro Text",Inter,Segoe UI,Roboto,sans-serif;
    -webkit-font-smoothing:antialiased;
  }
  header{
    position:sticky;top:0;z-index:5;display:flex;align-items:center;gap:14px;
    padding:16px 24px;background:linear-gradient(180deg,rgba(12,14,20,.92),rgba(12,14,20,.6));
    backdrop-filter:blur(8px);border-bottom:1px solid var(--line);
  }
  header .logo{font-size:20px;font-weight:700;letter-spacing:.3px}
  header .logo .dot{color:var(--accent)}
  header .counts{color:var(--dim);font-size:13px}
  header .counts b{color:var(--txt);font-weight:600}
  header .live{margin-left:auto;display:flex;align-items:center;gap:7px;color:var(--dim);font-size:12px}
  header .live .pulse{width:8px;height:8px;border-radius:50%;background:var(--green);box-shadow:0 0 0 0 rgba(126,231,135,.6);animation:pulse 2s infinite}
  @keyframes pulse{0%{box-shadow:0 0 0 0 rgba(126,231,135,.5)}70%{box-shadow:0 0 0 7px rgba(126,231,135,0)}100%{box-shadow:0 0 0 0 rgba(126,231,135,0)}}

  main{padding:22px;display:grid;gap:18px;grid-template-columns:repeat(auto-fill,minmax(340px,1fr));max-width:1500px;margin:0 auto}

  .card{
    position:relative;background:linear-gradient(180deg,var(--card2),var(--card));
    border:1px solid var(--line);border-radius:16px;padding:16px 18px;
    box-shadow:0 8px 30px rgba(0,0,0,.25);transition:transform .15s ease,border-color .15s ease,box-shadow .15s;
    overflow:hidden;
  }
  .card:hover{transform:translateY(-3px);box-shadow:0 14px 40px rgba(0,0,0,.35)}
  .card.tmux{border-color:#2c476b}
  .card.tmux::before{content:"";position:absolute;left:0;top:0;bottom:0;width:3px;background:linear-gradient(var(--tmux),#3b6ea5)}
  .card.ext{opacity:.82}

  .top{display:flex;align-items:flex-start;gap:14px}
  .num{
    font-family:"SF Mono",ui-monospace,Menlo,monospace;font-size:30px;font-weight:800;line-height:1;
    min-width:38px;text-align:center;color:#9fb4d6;text-shadow:0 0 18px rgba(108,182,255,.25)
  }
  .card.ext .num{color:#5d667c;text-shadow:none}
  .head{flex:1;min-width:0}
  .name{display:flex;align-items:center;gap:8px;font-weight:650;font-size:15px}
  .name .title{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
  .dotst{width:9px;height:9px;border-radius:50%;flex:none}
  .st-working{background:var(--green);box-shadow:0 0 8px rgba(126,231,135,.6)}
  .st-idle{background:var(--amber)}
  .st-unknown,.st-ext{background:var(--grey)}
  .pill{font-size:11px;padding:2px 8px;border-radius:999px;border:1px solid var(--line);color:var(--dim);white-space:nowrap}
  .pill.tmux{color:var(--tmux);border-color:#33527d;background:rgba(108,182,255,.08)}
  .cwd{color:var(--faint);font-size:11.5px;margin-top:3px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;font-family:"SF Mono",ui-monospace,monospace}

  .bar{height:6px;border-radius:999px;background:var(--barbg);margin:13px 0 6px;overflow:hidden}
  .bar>i{display:block;height:100%;border-radius:999px;background:linear-gradient(90deg,#46d089,#8be0b0);transition:width .4s ease}
  .barmeta{display:flex;justify-content:space-between;color:var(--dim);font-size:11.5px}

  .phrase{color:var(--dim);font-style:italic;font-size:12.5px;margin:10px 0 2px;
    display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}
  .folders{display:flex;flex-wrap:wrap;gap:6px;margin-top:12px}
  .folder{display:inline-flex;align-items:center;gap:5px;font-size:11.5px;color:#a9c2e6;
    background:rgba(108,182,255,.07);border:1px solid #243a59;border-radius:8px;padding:3px 8px;
    font-family:"SF Mono",ui-monospace,monospace}
  .card.ext .folder{color:#8a93a8;background:rgba(255,255,255,.03);border-color:var(--line)}
  .folder .arr{color:var(--faint)}

  .empty{grid-column:1/-1;text-align:center;color:var(--dim);padding:60px 0}
</style>
</head>
<body>
<header>
  <div class="logo"><span class="dot">◍</span> enxame</div>
  <div class="counts" id="counts">…</div>
  <div class="live"><span class="pulse"></span> live</div>
</header>
<main id="grid"></main>
<script>
const esc = s => (s||"").replace(/[&<>"]/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"}[c]));
function card(a, i){
  const cls = a.openable ? "tmux" : "ext";
  const st = a.openable ? a.status : (a.status==="working"?"working":"ext");
  const stcls = "st-"+(a.openable? a.status : "ext");
  const pill = a.openable ? '<span class="pill tmux">⧉ tmux</span>' : '<span class="pill">ext'+(a.procs>1?' ×'+a.procs:'')+'</span>';
  let bar = "";
  if(a.todos){ const [d,t]=a.todos; const p=t?Math.round(d*100/t):0;
    bar = `<div class="bar"><i style="width:${p}%"></i></div><div class="barmeta"><span>${a.status}</span><span>${d}/${t}</span></div>`;
  } else {
    bar = `<div class="bar"><i style="width:0%"></i></div><div class="barmeta"><span>${a.status}</span><span></span></div>`;
  }
  const folders = (a.folders||[]).map(f=>`<span class="folder"><span class="arr">→</span>${esc(f)}</span>`).join("");
  const phrase = a.phrase ? `<div class="phrase">${esc(a.phrase)}</div>` : "";
  return `<div class="card ${cls}">
    <div class="top">
      <div class="num">${i+1}</div>
      <div class="head">
        <div class="name"><span class="dotst ${stcls}"></span><span class="title">${esc(a.label)}</span>${pill}</div>
        <div class="cwd">${esc(a.cwd)}</div>
      </div>
    </div>
    ${bar}
    ${phrase}
    <div class="folders">${folders}</div>
  </div>`;
}
async function tick(){
  try{
    const r = await fetch("/api/agents",{cache:"no-store"});
    const d = await r.json();
    document.getElementById("counts").innerHTML =
      `<b>${d.openable}</b> in tmux · <b>${d.external}</b> external`;
    const grid = document.getElementById("grid");
    grid.innerHTML = d.agents.length ? d.agents.map(card).join("")
      : '<div class="empty">No claude agents found.</div>';
  }catch(e){ /* keep last view */ }
}
tick(); setInterval(tick, 1500);
</script>
</body>
</html>
"####;
