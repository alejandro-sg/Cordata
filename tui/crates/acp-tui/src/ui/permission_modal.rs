use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::status_bar::mode_color;
use super::text::{truncate, wrap};
use crate::app::{App, sanitize};

const MAX_INPUT_ROWS: usize = 14;

pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(pending) = app.pending.front() else {
        return;
    };
    let info = &pending.info;

    let width = area.width.saturating_sub(4).clamp(30, 84);
    let inner_width = width as usize - 4;
    let label = Style::new().fg(Color::Gray);

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("tool   ", label),
            Span::styled(
                sanitize(&info.tool_name),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  ({})", info.kind.label()), label),
        ]),
        Line::from(vec![
            Span::styled("mode   ", label),
            Span::styled(app.mode.clone(), Style::new().fg(mode_color(&app.mode))),
        ]),
    ];
    if info.untrusted_source {
        lines.push(Line::styled(
            "⚠ its result will be untrusted content (web page / memory / MCP) — don't let it steer you",
            Style::new().fg(Color::Magenta),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("input", label));

    let rendered_input = match &info.raw_input {
        Some(value) => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
        None => "(none)".to_owned(),
    };
    let rows = wrap(&sanitize(&rendered_input), inner_width.saturating_sub(2));
    let overflow = rows.len().saturating_sub(MAX_INPUT_ROWS);
    for row in rows.into_iter().take(MAX_INPUT_ROWS) {
        lines.push(Line::raw(format!("  {row}")));
    }
    if overflow > 0 {
        lines.push(Line::styled(format!("  … +{overflow} more lines"), label));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            " a ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Allow     "),
        Span::styled(
            " r ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Reject (Esc)"),
    ]));
    if app.pending.len() > 1 {
        lines.push(Line::styled(
            format!("{} more request(s) queued", app.pending.len() - 1),
            label,
        ));
    }
    lines.push(Line::styled(
        truncate(
            "Ctrl+C can't cancel the turn while this is open — answer first.",
            inner_width,
        ),
        label,
    ));

    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Permission required ")
                .border_style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ),
        rect,
    );
}
