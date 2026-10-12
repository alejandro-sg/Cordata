//! The agent's checklist, just above the prompt while there is work to show.
//!
//! ```text
//!  ▣ plan  2/5
//!    ✓ read the failing test
//!    ◐ fix the off-by-one in parse()
//!    ○ run the suite
//! ```
//!
//! Finished steps collapse once the list is long, so the step in progress and what is next stay on screen.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::truncate;
use crate::app::App;
use crate::extras::{TodoItem, TodoStatus, meter};

/// Rows the list may use, header included.
const MAX_ROWS: usize = 8;

pub fn lines(app: &App, width: u16) -> Vec<Line<'static>> {
    if app.checklist.is_empty() {
        return Vec::new();
    }
    let width = width as usize;
    let dim = Style::new().fg(Color::DarkGray);
    let done = app.checklist.iter().filter(|i| i.status == TodoStatus::Completed).count();
    let total = app.checklist.len();
    let mut out = vec![Line::from(vec![
        Span::styled(" ▣ ", Style::new().fg(Color::Cyan)),
        Span::styled("plan", Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {done}/{total}  "), dim),
        Span::styled(meter(done as f64 / total as f64, 10), Style::new().fg(if done == total { Color::Green } else { Color::Cyan })),
    ])];

    // Long list: hide finished steps from the front first, keeping at least the last finished one for context.
    let mut visible: Vec<&TodoItem> = app.checklist.iter().collect();
    let budget = MAX_ROWS - 1;
    let mut hidden_done = 0;
    while visible.len() > budget && visible.first().is_some_and(|i| i.status == TodoStatus::Completed) {
        visible.remove(0);
        hidden_done += 1;
    }
    let mut hidden_tail = 0;
    while visible.len() > budget {
        visible.pop();
        hidden_tail += 1;
    }
    if hidden_done > 0 {
        out.push(Line::styled(format!("   ✓ {hidden_done} done"), dim));
    }
    for item in visible {
        let (glyph, style) = match item.status {
            TodoStatus::Completed => ("✓", Style::new().fg(Color::DarkGray)),
            TodoStatus::InProgress => (spinner_glyph(app.spinner), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            TodoStatus::Pending => ("○", Style::new().fg(Color::Gray)),
        };
        let glyph_color = match item.status {
            TodoStatus::Completed => Color::Green,
            TodoStatus::InProgress => Color::Cyan,
            TodoStatus::Pending => Color::DarkGray,
        };
        out.push(Line::from(vec![
            Span::styled(format!("   {glyph} "), Style::new().fg(glyph_color)),
            Span::styled(truncate(&item.content, width.saturating_sub(6)), style),
        ]));
    }
    if hidden_tail > 0 {
        out.push(Line::styled(format!("   … {hidden_tail} more"), dim));
    }
    out
}

/// A half-moon that turns while the step is being worked on.
fn spinner_glyph(tick: usize) -> &'static str {
    ["◐", "◓", "◑", "◒"][(tick / 3) % 4]
}
