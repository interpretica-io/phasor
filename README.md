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

Agents are drawn as a **galaxy field**: each project is a star, and the folders
it has touched orbit it as a vertical column hanging off a line from the core.
Stars are laid out freely across the screen and never overlap.

```
◍ enxame  1 in tmux · 6 external

 ★ Analyze visao project overview      ✦ Full disassembly of Nethermind
   working  ▓▓▓▓░░ 5/8                   idle
   Build and test:…                      Готово — я разобрал hot-функции…
  │                                     │
  ├─ Visitors                           ├─ memory
  ├─ Parsing                            ╰─ bflat
  ╰─ VirtualMachine

n: new agent · ↑/↓: select · Enter: open · d: kill · q: quit
```

`★`/`☆` = openable agent in enxame's tmux (working / idle); `✦` (grey) = an
external claude you can only monitor. Only the beginning of the last phrase is
shown, and folders are listed by name, not full path.

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
| `↑/↓` `j/k`| move selection                                      |
| `Enter`    | open the selected agent's terminal (tmux attach)    |
| click      | select **and** open that agent                      |
| `d`        | kill the selected agent's window                    |
| `q`        | quit the dashboard (agents keep running in tmux)    |

Inside an agent's terminal, press **`Alt-o`** to jump back to the dashboard
(enxame binds this on its own tmux server, so no prefix is needed). The
classic tmux **prefix + d** (default `Ctrl-b` `d`) also works. A reminder is
shown in the tmux status bar at the bottom while you're attached. Agents
survive across dashboard restarts — they live in a dedicated tmux server
(`tmux -L enxame`).

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
