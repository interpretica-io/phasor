//! Node-field rendering: each agent is a compact rounded card, spread across
//! the canvas, with solid line-drawn arrows fanning out to the folders it has
//! touched. External claudes (not in enxame's tmux) are dimmed.

use super::HitBox;
use crate::agent::{Agent, Status};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Widget};

const PHRASE_LEN: usize = 60;
/// How long the "task completed" highlight lingers.
const COMPLETE_FLASH_SECS: u64 = 3;

/// Large, glanceable 3-row "seven-segment" digits for the quick-jump number.
const BIG_DIGITS: [[&str; 3]; 10] = [
    ["┏━┓", "┃ ┃", "┗━┛"], // 0
    [" ╻ ", " ┃ ", " ╹ "], // 1
    ["┏━┓", "┏━┛", "┗━┛"], // 2
    ["┏━┓", " ━┫", "┗━┛"], // 3
    ["┃ ┃", "┗━┫", "  ╹"], // 4
    ["┏━┓", "┗━┓", "┗━┛"], // 5
    ["┏━┓", "┣━┓", "┗━┛"], // 6
    ["┏━┓", "  ┃", "  ╹"], // 7
    ["┏━┓", "┣━┫", "┗━┛"], // 8
    ["┏━┓", "┗━┫", "┗━┛"], // 9
];

const C_BAR_FILL: Color = Color::Rgb(110, 200, 150);
const C_BAR_EMPTY: Color = Color::Rgb(70, 75, 95);

const C_ARROW: Color = Color::Rgb(95, 125, 165);
const C_ARROW_SEL: Color = Color::Rgb(120, 200, 255);
const C_FOLDER: Color = Color::Rgb(150, 180, 220);
const C_BORDER_SEL: Color = Color::Rgb(120, 205, 255);
const C_BORDER_TMUX: Color = Color::Rgb(80, 135, 175); // openable (in tmux)
const C_BORDER_EXT: Color = Color::Rgb(64, 66, 82); // external (monitor only)
const C_TAG_TMUX: Color = Color::Rgb(110, 200, 245);

/// Draw all agents as a node field within `area`. Returns clickable cards.
pub fn draw(buf: &mut Buffer, area: Rect, agents: &[Agent], selected: usize) -> Vec<HitBox> {
    let mut hits = Vec::new();
    let n = agents.len();
    if n == 0 || area.width < 6 || area.height < 4 {
        return hits;
    }

    let cols = (n as f32).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let cell_w = area.width / cols as u16;
    let cell_h = area.height / rows as u16;

    for (i, agent) in agents.iter().enumerate() {
        let r = (i / cols) as u16;
        let c = (i % cols) as u16;
        let region = Rect {
            x: area.x + c * cell_w,
            y: area.y + r * cell_h,
            width: cell_w,
            height: cell_h,
        };
        if let Some(hit) = draw_node(buf, region, agent, i, i == selected) {
            hits.push(hit);
        }
    }
    hits
}

fn draw_node(
    buf: &mut Buffer,
    region: Rect,
    agent: &Agent,
    idx: usize,
    selected: bool,
) -> Option<HitBox> {
    if region.height < 4 || region.width < 14 {
        return None;
    }
    let external = !agent.openable();
    let dim = Style::new().fg(Color::DarkGray);

    let inner = Rect {
        x: region.x + 1,
        y: region.y + 1,
        width: region.width.saturating_sub(2),
        height: region.height.saturating_sub(2),
    };

    // Folders (names only, deduped).
    let mut seen = std::collections::HashSet::new();
    let folders: Vec<String> = agent
        .state
        .work_dirs
        .iter()
        .filter_map(|d| d.file_name().map(|s| s.to_string_lossy().into_owned()))
        .filter(|name| !crate::agent::is_noise_folder(name))
        .filter(|name| seen.insert(name.clone()))
        .collect();

    // Card on top (full width), arrows + folders below.
    let card_h = 5u16.min(inner.height.saturating_sub(2)).max(3);
    let card = Rect { x: inner.x, y: inner.y, width: inner.width, height: card_h };
    draw_card(buf, card, agent, idx, selected, external);

    if !folders.is_empty() && inner.bottom() > card.bottom() {
        let arrow_color = if external { Color::Rgb(80, 85, 100) } else if selected { C_ARROW_SEL } else { C_ARROW };
        let folder_style = if external { dim } else { Style::new().fg(C_FOLDER) };

        // A solid "bus" drops from the card's bottom border and fans out one
        // arrow per folder.
        let bus_x = inner.x as i32 + 4;
        let arrow_st = Style::new().fg(arrow_color);

        // Tee on the card's bottom border, then one connector row of bus.
        put_str(buf, inner, bus_x, card.bottom() as i32 - 1, "┬", arrow_st);
        let mut fy = card.bottom() as i32;

        // Show every folder (no truncation); rows past the cell are clipped by
        // put_str's bounds check.
        let n = folders.len();
        for (k, name) in folders.iter().enumerate() {
            let last = k + 1 == n;
            let branch = if last { "╰──▶ " } else { "├──▶ " };
            put_str(buf, inner, bus_x, fy, branch, arrow_st);
            let name_x = bus_x + branch.chars().count() as i32;
            put_str(buf, inner, name_x, fy, name, folder_style);
            fy += 1;
        }
    }

    // Just finished a task: a red stripe down the right edge of the cell for 3s.
    if agent.just_completed(COMPLETE_FLASH_SECS) {
        let red = Style::new().fg(Color::Rgb(240, 70, 70)).add_modifier(Modifier::BOLD);
        let x = region.right() as i32 - 1;
        for y in region.y..region.bottom() {
            put_str(buf, region, x, y as i32, "▌", red);
        }
    }

    Some(HitBox {
        idx,
        left: card.x,
        right: card.right(),
        top: card.y,
        bottom: card.bottom(),
    })
}

fn draw_card(
    buf: &mut Buffer,
    card: Rect,
    agent: &Agent,
    idx: usize,
    selected: bool,
    external: bool,
) {
    let (dot, dot_color) = match agent.state.status {
        Status::Working => ("●", if external { Color::Rgb(120, 120, 130) } else { Color::Rgb(120, 230, 140) }),
        Status::Idle => ("○", if external { Color::Rgb(120, 120, 130) } else { Color::Rgb(235, 205, 110) }),
        Status::Unknown => ("·", Color::Rgb(150, 150, 160)),
    };

    // Border + corner tag make the tmux/external distinction obvious.
    let border_style = if selected {
        Style::new().fg(C_BORDER_SEL).add_modifier(Modifier::BOLD)
    } else if external {
        Style::new().fg(C_BORDER_EXT)
    } else {
        Style::new().fg(C_BORDER_TMUX)
    };
    let tag = if external {
        Span::styled(" ext ", Style::new().fg(Color::Rgb(120, 120, 135)))
    } else {
        Span::styled(" ⧉ tmux ", Style::new().fg(C_TAG_TMUX).add_modifier(Modifier::BOLD))
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(Line::from(tag).right_aligned());
    let in_card = block.inner(card);
    block.render(card, buf);

    let cx = in_card.x as i32;
    let cy = in_card.y as i32;

    // --- big quick-jump number on the left (spans the 3 inner rows) ---
    let digits: Vec<usize> = {
        let num = idx + 1;
        num.to_string().chars().map(|c| c as usize - '0' as usize).collect()
    };
    let num_style = if selected {
        Style::new().fg(C_ARROW_SEL).add_modifier(Modifier::BOLD)
    } else if external {
        Style::new().fg(Color::Rgb(95, 100, 120)).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Rgb(165, 185, 220)).add_modifier(Modifier::BOLD)
    };
    for row in 0..3 {
        let mut line = String::new();
        for d in &digits {
            line.push_str(BIG_DIGITS[*d][row]);
            line.push(' ');
        }
        put_str(buf, in_card, cx, cy + row as i32, &line, num_style);
    }

    // info column to the right of the number
    let info_x = cx + digits.len() as i32 * 4 + 1;
    let info_w = (in_card.right() as i32 - info_x).max(0) as usize;

    // row 0: status dot + name (+ ↻ when a repeating auto-instruction is set)
    put_str(buf, in_card, info_x, cy, dot, Style::new().fg(dot_color).add_modifier(Modifier::BOLD));
    let pending = agent.pending.is_some();
    let name = clip(&agent.label(), info_w.saturating_sub(if pending { 4 } else { 2 }));
    // The session title is always shown in full colour, even for external
    // agents — only the rest of an external card is dimmed.
    let name_style = if selected {
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Rgb(205, 210, 225)).add_modifier(Modifier::BOLD)
    };
    put_str(buf, in_card, info_x + 2, cy, &name, name_style);
    if pending {
        put_str(buf, in_card, in_card.right() as i32 - 1, cy, "↻",
            Style::new().fg(Color::Rgb(235, 205, 110)).add_modifier(Modifier::BOLD));
    }

    // row 1: progress bar (always present) + activity load %, right-aligned
    if in_card.height >= 2 {
        draw_progress(buf, in_card, info_x, cy + 1, info_w, agent.state.todos, external);
        let load = agent.load();
        let s = format!("⚡{load}%");
        // ⚡ renders 2 cells wide; account for that when right-aligning.
        let disp = 2 + format!("{load}%").chars().count() as i32;
        let lx = in_card.right() as i32 - disp;
        let lc = if external {
            Color::Rgb(90, 95, 110)
        } else if load >= 66 {
            Color::Rgb(240, 150, 90)
        } else if load >= 25 {
            Color::Rgb(225, 200, 120)
        } else {
            Color::Rgb(110, 120, 140)
        };
        put_str(buf, in_card, lx, cy + 1, &s, Style::new().fg(lc));
    }

    // row 2: beginning of last phrase
    if in_card.height >= 3 {
        if let Some(p) = agent.state.last_phrases.back() {
            let phrase = clip_phrase(p, PHRASE_LEN.min(info_w));
            let pstyle = Style::new()
                .fg(if external { Color::DarkGray } else { Color::Rgb(120, 125, 140) })
                .add_modifier(Modifier::ITALIC);
            put_str(buf, in_card, info_x, cy + 2, &phrase, pstyle);
        }
    }
}

/// Render a progress bar; always draws a bar, even when the todo count is
/// unknown (a dim, empty track).
fn draw_progress(
    buf: &mut Buffer,
    region: Rect,
    x: i32,
    y: i32,
    avail: usize,
    todos: Option<(usize, usize)>,
    external: bool,
) {
    let w = avail.saturating_sub(6).clamp(3, 18);
    let empty = Style::new().fg(C_BAR_EMPTY);
    match todos {
        Some((done, total)) if total > 0 => {
            let filled = ((done * w) / total).min(w);
            let fcol = if external { Style::new().fg(Color::Rgb(90, 110, 100)) } else { Style::new().fg(C_BAR_FILL) };
            put_str(buf, region, x, y, &"━".repeat(filled), fcol);
            put_str(buf, region, x + filled as i32, y, &"─".repeat(w - filled), empty);
            put_str(
                buf,
                region,
                x + w as i32 + 1,
                y,
                &format!("{done}/{total}"),
                Style::new().fg(Color::Rgb(140, 150, 170)),
            );
        }
        _ => {
            // Unknown progress: a slim, quiet rule rather than chunky blocks.
            put_str(buf, region, x, y, &"─".repeat(w), empty);
        }
    }
}

/// Write a string at (x,y), clipped to both the region and the buffer bounds.
fn put_str(buf: &mut Buffer, region: Rect, x: i32, y: i32, s: &str, style: Style) {
    if y < region.y as i32 || y >= region.bottom() as i32 {
        return;
    }
    let start = x.max(region.x as i32);
    if start >= region.right() as i32 {
        return;
    }
    let skip = (start - x).max(0) as usize;
    let avail = (region.right() as i32 - start) as usize;
    let shown: String = s.chars().skip(skip).take(avail).collect();
    if shown.is_empty() {
        return;
    }
    buf.set_string(start as u16, y as u16, shown, style);
}

fn clip(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    s.chars().take(max).collect()
}

fn clip_phrase(s: &str, max: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if max == 0 {
        return String::new();
    }
    if one.chars().count() <= max {
        one
    } else {
        let head: String = one.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}
