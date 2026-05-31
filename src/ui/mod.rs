//! Rendering the dashboard with ratatui.
//!
//! Agents are drawn as a node field: each agent is a compact rounded card laid
//! out on a grid, with solid line-drawn arrows fanning out to the folders it
//! has touched. A card matched to a configured project is tinted in the
//! project's colour and tagged with its name. External claudes (not in phasor's
//! tmux) are dimmed. Below the field sits a status bar; popups handle the
//! new-agent and instruction prompts.

mod galaxy;

use crate::agent::Status;
use crate::app::{App, Mode};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::time::Duration;

/// A clickable region (an agent's card) mapping screen cells to an agent index.
#[derive(Clone, Copy)]
pub struct HitBox {
    /// Index of the agent this region belongs to.
    pub idx: usize,
    /// Left column (inclusive).
    pub left: u16,
    /// Right column (exclusive).
    pub right: u16,
    /// Top row (inclusive).
    pub top: u16,
    /// Bottom row (exclusive).
    pub bottom: u16,
}

impl HitBox {
    /// Whether the cell at `(col, row)` falls inside this region.
    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.left && col < self.right && row >= self.top && row < self.bottom
    }
}

/// Render the whole UI. Returns hit boxes for mouse selection.
pub fn render(f: &mut Frame, app: &App) -> Vec<HitBox> {
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(1),    // galaxy field
        Constraint::Length(1), // status/help
    ])
    .split(f.area());

    render_header(f, chunks[0], app);
    let hits = if app.agents.is_empty() {
        render_empty(f, chunks[1]);
        Vec::new()
    } else {
        let area = chunks[1];
        galaxy::draw(f.buffer_mut(), area, &app.agents, app.selected)
    };
    render_status(f, chunks[2], app);

    match &app.mode {
        Mode::NewAgent { input } => render_input_popup(
            f,
            f.area(),
            " working directory for new agent (Enter / Esc) ",
            input,
        ),
        Mode::Instruct { input } => render_input_popup(
            f,
            f.area(),
            " instruction to auto-send when the agent finishes (Enter / Esc) ",
            input,
        ),
        Mode::Normal => {}
    }
    hits
}

/// Draw the top header: the brand and the in-tmux / external agent counts.
fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let external = app.agents.iter().filter(|a| !a.openable()).count();
    let openable = app.agents.len() - external;
    let title = Line::from(vec![
        Span::styled("◍ phasor", Style::new().fg(Color::Cyan).bold()),
        Span::raw("  "),
        Span::styled(
            format!("{openable} in tmux · {external} external"),
            Style::new().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(title), area);
}

/// A proper bottom status bar: a filled strip with the selected agent's
/// context on the left (or a transient message), and compact key hints on the
/// right.
fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let bar_bg = Style::new().bg(Color::Rgb(24, 28, 40)).fg(Color::Gray);
    f.buffer_mut().set_style(area, bar_bg);

    // LEFT: a fresh transient message, else the selected agent's context.
    let fresh = !app.status.is_empty() && app.status_at.elapsed() < Duration::from_secs(5);
    let left = if fresh {
        Line::from(vec![
            Span::styled(
                " ● ",
                Style::new()
                    .fg(Color::Rgb(240, 190, 90))
                    .bg(Color::Rgb(24, 28, 40)),
            ),
            Span::styled(
                app.status.clone(),
                Style::new()
                    .fg(Color::Rgb(240, 220, 180))
                    .bg(Color::Rgb(24, 28, 40)),
            ),
        ])
    } else if let Some(a) = app.agents.get(app.selected) {
        let (dot, dc) = match a.state.status {
            Status::Working => ("●", Color::Rgb(120, 230, 140)),
            Status::Idle => ("○", Color::Rgb(235, 205, 110)),
            Status::Unknown => ("·", Color::Rgb(150, 150, 160)),
        };
        let tag = if a.openable() { "tmux" } else { "external" };
        let folders = a
            .state
            .work_dirs
            .iter()
            .filter(|d| **d != a.cwd)
            .filter(|d| {
                d.file_name()
                    .map(|n| !crate::agent::is_noise_folder(&n.to_string_lossy()))
                    .unwrap_or(true)
            })
            .count();
        let bg = Color::Rgb(24, 28, 40);
        let mut spans = vec![
            Span::styled(
                format!(" {}/{} ", app.selected + 1, app.agents.len()),
                Style::new().fg(Color::Rgb(120, 150, 190)).bg(bg),
            ),
            Span::styled("▸ ", Style::new().fg(Color::Rgb(120, 200, 255)).bg(bg)),
            Span::styled(a.label(), Style::new().fg(Color::White).bg(bg).bold()),
            Span::styled("  ", Style::new().bg(bg)),
            Span::styled(format!("{dot} "), Style::new().fg(dc).bg(bg)),
            Span::styled(
                format!("⚡{}%  ", a.load()),
                Style::new().fg(Color::Rgb(180, 170, 150)).bg(bg),
            ),
            Span::styled(
                format!("[{tag}] "),
                Style::new()
                    .fg(if a.openable() {
                        Color::Rgb(110, 180, 240)
                    } else {
                        Color::DarkGray
                    })
                    .bg(bg),
            ),
            Span::styled(
                format!("· {folders} dir{}", if folders == 1 { "" } else { "s" }),
                Style::new().fg(Color::DarkGray).bg(bg),
            ),
        ];
        if let Some(name) = &a.project_name {
            spans.push(Span::styled(
                format!("  ◆ {name}"),
                Style::new().fg(Color::Rgb(150, 200, 240)).bg(bg),
            ));
        }
        if a.pending.is_some() {
            spans.push(Span::styled(
                "  ↻ auto-instruct",
                Style::new().fg(Color::Rgb(235, 205, 110)).bg(bg),
            ));
        }
        Line::from(spans)
    } else {
        Line::from(Span::styled(" no agents", bar_bg))
    };

    let keys = " n new · i instruct · p projects · 1-9 jump · ↵ open · d kill · q quit ";
    let right = Line::from(Span::styled(
        keys,
        Style::new()
            .fg(Color::Rgb(110, 120, 140))
            .bg(Color::Rgb(24, 28, 40)),
    ));

    f.render_widget(Paragraph::new(left), area);
    f.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

/// Draw the placeholder shown when no agents have been discovered yet.
fn render_empty(f: &mut Frame, area: Rect) {
    let msg = Paragraph::new(vec![
        Line::raw(""),
        Line::from(Span::styled(
            "No claude agents found.",
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "Press 'n' to launch one in a new tmux window.",
            Style::new().fg(Color::DarkGray),
        )),
    ])
    .alignment(Alignment::Center);
    f.render_widget(msg, area);
}

/// Draw a centered single-line input popup (used for new-agent / instruct).
fn render_input_popup(f: &mut Frame, area: Rect, title: &str, input: &str) {
    let w = area.width.saturating_sub(8).min(90);
    let h = 3;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + area.height / 3;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(title.to_string());
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(input),
            Span::styled("▏", Style::new().fg(Color::Cyan)),
        ])),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hb() -> HitBox {
        HitBox {
            idx: 0,
            left: 10,
            right: 20,
            top: 5,
            bottom: 8,
        }
    }

    #[test]
    fn contains_inside() {
        assert!(hb().contains(15, 6));
    }

    #[test]
    fn contains_edges_left_top_inclusive() {
        let h = hb();
        assert!(h.contains(10, 5)); // top-left corner inclusive
        assert!(h.contains(19, 7)); // last inside cell
    }

    #[test]
    fn contains_right_bottom_exclusive() {
        let h = hb();
        assert!(!h.contains(20, 6)); // right is exclusive
        assert!(!h.contains(15, 8)); // bottom is exclusive
    }

    #[test]
    fn contains_outside() {
        let h = hb();
        assert!(!h.contains(9, 6));
        assert!(!h.contains(15, 4));
        assert!(!h.contains(0, 0));
    }
}
