//! Free-form "galaxy" rendering.
//!
//! Each agent is a star (the project core) placed in its own region of a grid
//! — free placement that never overlaps. From the star a line descends into a
//! vertical column of the folders the agent has touched (names only), drawn as
//! tree branches. External claudes (not in enxame's tmux) are dimmed.

use super::HitBox;
use crate::agent::{Agent, Status};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// How many leading chars of the last phrase to show.
const PHRASE_LEN: usize = 40;

/// Draw all agents as a galaxy field within `area`. Returns clickable cores.
pub fn draw(buf: &mut Buffer, area: Rect, agents: &[Agent], selected: usize) -> Vec<HitBox> {
    let mut hits = Vec::new();
    let n = agents.len();
    if n == 0 || area.width < 6 || area.height < 4 {
        return hits;
    }

    // Lay galaxies on a roughly square grid of regions.
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
        if let Some(hit) = draw_galaxy(buf, region, agent, i, i == selected) {
            hits.push(hit);
        }
    }
    hits
}

fn draw_galaxy(
    buf: &mut Buffer,
    region: Rect,
    agent: &Agent,
    idx: usize,
    selected: bool,
) -> Option<HitBox> {
    if region.height < 2 || region.width < 6 {
        return None;
    }
    let external = !agent.openable();
    let dim = Style::new().fg(Color::DarkGray);

    let x0 = region.x as i32 + 1;
    let text_x = x0 + 2; // indent text under the star
    let inner_w = region.width.saturating_sub(3) as usize;
    let mut y = region.y as i32 + 1;

    // --- core: star + project name ---
    let (glyph, color) = match agent.state.status {
        _ if external => ("✦", Color::DarkGray),
        Status::Working => ("★", Color::Green),
        Status::Idle => ("☆", Color::Yellow),
        Status::Unknown => ("✦", Color::DarkGray),
    };
    // Quick-jump number badge (1-based). Press the digit to select this agent.
    let num_label = format!("{}", idx + 1);
    let num_style = if selected {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    };
    put_str(buf, region, x0, y, &num_label, num_style);

    let star_x = x0 + num_label.chars().count() as i32 + 1;
    let star_style = if selected {
        Style::new().fg(color).add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::new().fg(color).add_modifier(Modifier::BOLD)
    };
    put_str(buf, region, star_x, y, glyph, star_style);

    let name_x = star_x + 2;
    let name_avail = (region.right() as i32 - name_x).max(0) as usize;
    let name = clip(&agent.label(), name_avail);
    let name_style = if external {
        dim.add_modifier(Modifier::BOLD)
    } else if selected {
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD)
    };
    put_str(buf, region, name_x, y, &name, name_style);
    let core_row = y as u16;
    y += 1;

    // --- meta: status + progress ---
    let meta = meta_line(agent);
    let meta_style = if external { dim } else { Style::new().fg(Color::Magenta) };
    put_str(buf, region, text_x, y, &clip(&meta, inner_w), meta_style);
    y += 1;

    // --- phrase: only the beginning ---
    if let Some(p) = agent.state.last_phrases.back() {
        let phrase = clip_phrase(p, PHRASE_LEN.min(inner_w));
        let pstyle = if external {
            dim.add_modifier(Modifier::ITALIC)
        } else {
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
        };
        put_str(buf, region, text_x, y, &phrase, pstyle);
        y += 1;
    }

    // --- folder column: a line descending into a vertical list of folders ---
    let mut seen = std::collections::HashSet::new();
    let folders: Vec<String> = agent
        .state
        .work_dirs
        .iter()
        .filter(|d| *d != &agent.cwd)
        .filter_map(|d| d.file_name().map(|s| s.to_string_lossy().into_owned()))
        .filter(|name| seen.insert(name.clone()))
        .collect();

    if !folders.is_empty() {
        let gx = x0 + 1; // guide column (the descending line)
        let guide_style = dim;
        let folder_style = if external { dim } else { Style::new().fg(Color::Blue) };

        // The line leaving the star.
        put_str(buf, region, gx, y, "│", guide_style);
        y += 1;

        let bottom = region.bottom() as i32;
        let avail = (bottom - y).max(0) as usize;
        // Reserve one row for a "+N" note if the list won't fit.
        let (show, overflow) = if folders.len() <= avail {
            (folders.len(), 0)
        } else {
            (avail.saturating_sub(1), folders.len() - avail.saturating_sub(1))
        };

        for (k, name) in folders.iter().take(show).enumerate() {
            let last = k + 1 == show && overflow == 0;
            let branch = if last { "╰─ " } else { "├─ " };
            let label = clip(name, inner_w.saturating_sub(3));
            put_str(buf, region, gx, y, branch, guide_style);
            put_str(buf, region, gx + 3, y, &label, folder_style);
            y += 1;
        }
        if overflow > 0 {
            put_str(buf, region, gx, y, &format!("╰─ +{overflow} more"), guide_style);
        }
    }

    // Clickable region: the core line (star + name).
    Some(HitBox {
        idx,
        left: region.x,
        right: region.right(),
        top: core_row,
        bottom: core_row + 1,
    })
}

/// Build the "working  ▓▓▓░ 3/7" style meta line.
fn meta_line(agent: &Agent) -> String {
    let status = match agent.state.status {
        Status::Working => "working",
        Status::Idle => "idle",
        Status::Unknown => "·",
    };
    match agent.state.todos {
        Some((done, total)) if total > 0 => {
            let w = 6usize;
            let filled = (done * w) / total;
            let bar: String = "▓".repeat(filled) + &"░".repeat(w - filled);
            format!("{status}  {bar} {done}/{total}")
        }
        _ => status.to_string(),
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

/// Clip a label to `max` chars (hard cut, no ellipsis — for folder names).
fn clip(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    s.chars().take(max).collect()
}

/// Clip a phrase to its first `max` chars, collapsing whitespace, with an
/// ellipsis if truncated — we only want the beginning.
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
