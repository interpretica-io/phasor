//! Rendering the dashboard with ratatui.
//!
//! Agents are drawn as a free-form "galaxy" field: each project is a star
//! (the core) placed somewhere on the canvas, with the directories it touches
//! orbiting around it as satellites connected by faint spokes. External
//! claudes (not in enxame's tmux) are dimmed.

mod galaxy;

use crate::app::{App, Mode};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// A clickable region (the core of a galaxy) mapping screen cells to an agent.
#[derive(Clone, Copy)]
pub struct HitBox {
    pub idx: usize,
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
}

impl HitBox {
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

    if let Mode::NewAgent { input } = &app.mode {
        render_input_popup(f, f.area(), input);
    }
    hits
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let external = app.agents.iter().filter(|a| !a.openable()).count();
    let openable = app.agents.len() - external;
    let title = Line::from(vec![
        Span::styled("◍ enxame", Style::new().fg(Color::Cyan).bold()),
        Span::raw("  "),
        Span::styled(
            format!("{openable} in tmux · {external} external"),
            Style::new().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(title), area);
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            app.status.clone(),
            Style::new().fg(Color::DarkGray),
        ))),
        area,
    );
}

fn render_empty(f: &mut Frame, area: Rect) {
    let msg = Paragraph::new(vec![
        Line::raw(""),
        Line::from(Span::styled("No claude agents found.", Style::new().fg(Color::DarkGray))),
        Line::from(Span::styled(
            "Press 'n' to launch one in a new tmux window.",
            Style::new().fg(Color::DarkGray),
        )),
    ])
    .alignment(Alignment::Center);
    f.render_widget(msg, area);
}

fn render_input_popup(f: &mut Frame, area: Rect, input: &str) {
    let w = area.width.saturating_sub(8).min(80);
    let h = 3;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + area.height / 3;
    let popup = Rect { x, y, width: w, height: h };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" working directory for new agent (Enter / Esc) ");
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
