# ◍ enxame

A terminal dashboard that monitors and orchestrates **every** running Claude
Code agent on your machine. Enxame auto-discovers all `claude` CLI processes
(grouped by project directory) and shows them as a live block diagram — project
node, arrows to the directories each agent is touching, todo progress, status,
and its most recent phrases.

Agents you launch from enxame run in its own tmux session and can be **opened**
(the dashboard hands the screen over to that agent's terminal). Claudes started
elsewhere (a plain iTerm tab, etc.) are still discovered and monitored, but
shown **dimmed/grey** and marked `[external]` — they aren't in tmux, so they
can't be opened, only watched.

Each agent is a compact rounded card, spread across the screen, with solid
line-drawn arrows fanning out to the folders it has touched (names only).

```
◍ enxame  1 in tmux · 6 external

 ╭────────────────────────────────────╮
 │┏━┓  ● Analyze visao project overview│
 │┗━┓  ▰▰▰▰▰▱ 5/8                       │
 │┗━┛  Build and test:…                │
 ╰───┬────────────────────────────────╯
     ├──▶ Visitors
     ├──▶ Parsing
     ╰──▶ VirtualMachine
```

The big seven-segment number on the left is the **quick-jump key** (press it to
select). `●` (green) = openable agent working in enxame's tmux, `○` (amber) =
idle; an external claude you can only monitor is dimmed grey. Each card has a
progress bar (filled from the agent's todo list, empty when unknown) and an
**activity load** `⚡N%` — how hard the agent is working, derived from how fast
its transcript is growing (sampled every second). Arrows point to folder
**names**, never full paths. The selected card is highlighted.

The web dashboard shows the same load as a live **sparkline graph** per agent.

**Task-completion highlight.** When an agent finishes a task (a final
`end_turn` answer), the TUI marks it with a red stripe down the right edge for
3 s, and the web card celebrates with a pop + a gradient shimmer tinted to the
hue of that last answer, then settles back.

## Requirements

- `tmux` on `PATH`
- `claude` (Claude Code CLI) on `PATH`
- Rust toolchain (to build)

## Build & run

```sh
cargo build --release
./target/release/enxame
```

### Keys

| key        | action                                              |
|------------|-----------------------------------------------------|
| `n`        | new agent — prompts for a working directory         |
| `1`-`9`,`0`| jump to (select) that numbered agent — does not open |
| `←↑↓→` / `hjkl` | move selection around the grid                  |
| `Enter`    | open the selected agent's terminal (tmux attach)    |
| click      | select **and** open that agent                      |
| `d`        | kill the selected agent's window                    |
| `q`        | quit the dashboard (agents keep running in tmux)    |

Inside an agent's terminal, press **`Ctrl-Q`** to jump back to the dashboard
(enxame binds this on its own tmux server, so no prefix is needed). The
classic tmux **prefix + d** (default `Ctrl-b` `d`) also works. A reminder is
shown in the tmux status bar at the bottom while you're attached. Agents
survive across dashboard restarts — they live in a dedicated tmux server
(`tmux -L enxame`).

### Web dashboard

```sh
enxame serve            # http://127.0.0.1:7878
enxame serve 9000       # custom port
```

A parallel **graphical dashboard in the browser**, backed by the exact same
discovery as the TUI (shared `scan` module). Built for a big screen / wall
display: a live **force-directed graph** (D3.js) — every agent is a glowing
node whose size grows with its activity load, colour shows status (green
working / amber idle / grey external), with a progress ring and a breathing
pulse while working; the folders it touches float around it as linked satellite
nodes. Agents that **share working folders** are pulled into clusters and linked by
thin lines (brighter/thicker the more folders they share), each cluster tinted
its own colour — so you can see at a glance who's working on overlapping code.
Drag to rearrange, scroll to zoom, hover for details, and click a
`⧉ tmux` node to open its live terminal. Finishing a task makes the node burst.
A slim header shows live working/idle/external counters. Adaptive light/dark.
Auto-refreshes every 1.5 s; JSON API at `/api/agents`.

**Adaptive theme.** The page follows your OS light/dark preference by default;
the header button cycles auto → light → dark and remembers your choice. The
in-browser terminal recolors to match.

**Live terminals in the browser.** Click an openable `⧉ tmux` agent and its
terminal opens right in the page via [xterm.js](https://xtermjs.org) over a
WebSocket bridged to a PTY running `tmux attach`. Type, resize, the lot. Close
the overlay (or `Ctrl-Q` / prefix + d inside) to detach; the agent keeps
running. The server binds `127.0.0.1` only — a browser terminal is full shell
access, so it stays local.

### Spawning agents from outside

```sh
cd /path/to/project
enxame --exec claude --dangerously-skip-permissions
```

Everything after `--exec` is run as a command in a **new tmux window** of the
enxame session, in the current directory, and then the CLI exits. The window
shows up in the dashboard as an openable `⧉ tmux` agent. Use it from scripts or
other tools to seed enxame-managed terminals. Arguments are preserved, so
compound commands work too: `enxame --exec bash -lc 'cd sub && claude'`.

`--start` launches the **dashboard opened straight into the new window**, so
you watch the command immediately:

```sh
cd /path/to/project
enxame --start claude
```

Press **`Ctrl-Q`** (or tmux prefix + d) to **collapse** the terminal — you drop
into the enxame dashboard, where the command is now a card. Select it and press
`Enter` (or click) to re-open ("maximize") it. `q` quits the dashboard; the
command keeps running in the enxame session.

### Diagnostics

```sh
enxame doctor [cwd]
```

Prints the current enxame tmux windows and parses the most recent Claude
transcript for `cwd`, showing the title, status, todos, detected directories
and recent phrases — handy for debugging transcript resolution.

## How it works

| concern            | mechanism                                                            |
|--------------------|----------------------------------------------------------------------|
| discovery          | `ps` for processes named `claude`, cwds resolved via one `lsof` call |
| project node       | agents are keyed by cwd; several claudes in one dir collapse to one  |
| openable vs external | a cwd backed by an enxame tmux window is openable; others are dimmed |
| agent terminals    | one tmux window each, in the `enxame` session on socket `-L enxame`|
| live terminal view | the dashboard `tmux attach`es; detach (prefix+d) returns to it       |
| state / progress   | tails `~/.claude/projects/<encoded-cwd>/<session>.jsonl`             |
| working folders    | auto-detected from `cwd` + file paths in `tool_use` records          |
| progress           | latest `TodoWrite` → completed / total                              |
| last phrases       | most recent assistant `text` blocks                                  |
| status             | `working` if the transcript changed in the last 20s, else `idle`     |

### Module map

```
src/
├── main.rs        event loop, terminal setup, attach, `doctor`
├── app.rs         app state, key/mouse handling, discovery-based polling
├── agent.rs       Agent (keyed by cwd) + AgentState + Status models
├── discover.rs    ps + lsof discovery of all running claude processes
├── transcript.rs  cwd→dir encoding, newest-session resolution, JSONL parsing
├── tmux.rs        tmux CLI wrapper (session/window/attach/capture)
└── ui/
    ├── mod.rs     ratatui rendering: header, status, input popup, hit-testing
    └── galaxy.rs  the galaxy field: star cores + vertical folder columns
```

## Status / roadmap

Working today: launch agents, auto-detected work-dir block diagram, progress,
phrases, status, click-to-open, kill.

Planned: persist the agent↔cwd map across restarts, in-panel terminal preview
(`capture-pane`), sending a prompt to an agent without attaching, adopting
pre-existing enxame windows on startup.
