use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{text::wrap, tool_card};
use crate::app::{App, Message};

fn prefixed(
    text: &str,
    width: usize,
    first: &str,
    rest: &str,
    style: Style,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    let body_width = width.saturating_sub(first.chars().count()).max(1);
    wrap(text, body_width)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            Line::from(vec![
                Span::styled(
                    if i == 0 {
                        first.to_owned()
                    } else {
                        rest.to_owned()
                    },
                    prefix_style,
                ),
                Span::styled(row, style),
            ])
        })
        .collect()
}

pub fn build_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let w = width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut previous_was_tool = false;

    for message in &app.messages {
        let is_tool = matches!(message, Message::Tool(_));
        if !lines.is_empty() && !(is_tool && previous_was_tool) {
            lines.push(Line::raw(""));
        }
        previous_was_tool = is_tool;

        match message {
            Message::User(text) => lines.extend(prefixed(
                text,
                w,
                "› ",
                "  ",
                Style::new().add_modifier(Modifier::BOLD),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            Message::Agent(text) => {
                lines.extend(prefixed(text, w, "", "", Style::new(), Style::new()))
            }
            Message::Thought(text) => {
                if app.show_thoughts {
                    lines.extend(prefixed(
                        text,
                        w,
                        "· ",
                        "  ",
                        Style::new()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                        Style::new().fg(Color::DarkGray),
                    ));
                } else {
                    lines.push(Line::styled(
                        "· thinking hidden (Ctrl+R)",
                        Style::new().fg(Color::DarkGray),
                    ));
                }
            }
            Message::Tool(card) => lines.extend(tool_card::render(
                card,
                width,
                app.expand_tools,
                app.awaiting_approval(&card.id),
                app.spinner,
            )),
            Message::Divider(text) => {
                let label = format!(" {text} ");
                let fill = w.saturating_sub(label.chars().count());
                let (left, right) = (fill / 2, fill - fill / 2);
                lines.push(Line::styled(
                    format!("{}{label}{}", "─".repeat(left), "─".repeat(right)),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            Message::Info(text) => lines.extend(prefixed(
                text,
                w,
                "ℹ ",
                "  ",
                Style::new().fg(Color::Gray),
                Style::new().fg(Color::Blue),
            )),
            Message::Error(text) => lines.extend(prefixed(
                text,
                w,
                "✗ ",
                "  ",
                Style::new().fg(Color::Red),
                Style::new().fg(Color::Red),
            )),
        }
    }
    lines
}

fn welcome() -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::Gray);
    vec![
        Line::styled(
            "sierralab-agents · ACP terminal client",
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(
            "Type a prompt and press Enter. Dangerous tools ask first (build mode).",
            dim,
        ),
        Line::styled(
            "Tab cycles plan → build → auto · Ctrl+O model · Ctrl+T temperature",
            dim,
        ),
    ]
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let mut lines = build_lines(app, area.width);
    if lines.is_empty() {
        lines = welcome();
    }
    let total = lines.len();
    let height = area.height as usize;

    // Keep the viewport anchored while scrolled up and output keeps arriving.
    if app.scroll > 0 && total > app.last_total_lines {
        app.scroll += total - app.last_total_lines;
    }
    app.last_total_lines = total;
    app.scroll = app.scroll.min(total.saturating_sub(height));

    let start = total.saturating_sub(height).saturating_sub(app.scroll);
    let end = (start + height).min(total);
    frame.render_widget(Paragraph::new(lines[start..end].to_vec()), area);

    if app.scroll > 0 {
        let label = format!(" ↓ {} lines below (PgDn) ", app.scroll);
        let w = label.chars().count() as u16;
        if area.width > w {
            let rect = Rect::new(
                area.x + area.width - w,
                area.y + area.height.saturating_sub(1),
                w,
                1,
            );
            frame.render_widget(
                Paragraph::new(label).style(Style::new().fg(Color::Black).bg(Color::Gray)),
                rect,
            );
        }
    }
}
