use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::text::truncate;
use crate::app::{App, ConnState};

/// Risk-ordered, not menu-ordered: plan runs nothing (green), build gates
/// dangerous tools behind approval (yellow), auto gates nothing (red).
pub fn mode_color(mode: &str) -> Color {
    match mode {
        "plan" => Color::Green,
        "build" => Color::Yellow,
        "auto" => Color::Red,
        _ => Color::Gray,
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let sep = Span::styled(" │ ", Style::new().fg(Color::DarkGray));
    let (dot, dot_color, conn_label) = match &app.conn {
        ConnState::Connecting => ("●", Color::Yellow, "connecting"),
        ConnState::Connected => ("●", Color::Green, "connected"),
        ConnState::Closed { .. } => ("●", Color::Red, "disconnected"),
    };

    let mut spans = vec![
        Span::styled(format!(" {dot} "), Style::new().fg(dot_color)),
        Span::raw(conn_label),
    ];

    if let Some(session) = &app.session {
        let short: String = session.session_id.chars().take(8).collect();
        spans.push(sep.clone());
        spans.push(Span::styled(
            format!("session {short}"),
            Style::new().fg(Color::Gray),
        ));
    }

    if !app.mode.is_empty() {
        spans.push(sep.clone());
        spans.push(Span::styled(
            format!(" {} ", app.mode.to_uppercase()),
            Style::new()
                .fg(Color::Black)
                .bg(mode_color(&app.mode))
                .add_modifier(Modifier::BOLD),
        ));
        if let Some(pending) = app
            .pending_mode
            .as_deref()
            .filter(|pending| *pending != app.mode)
        {
            spans.push(Span::styled(
                format!(" → {pending}…"),
                Style::new().fg(mode_color(pending)),
            ));
        }
    }

    let model = app.config("model").map(|o| o.current_name().to_owned());
    let temperature = app
        .config("temperature")
        .map(|o| o.current_name().to_owned());
    if model.is_some() || temperature.is_some() {
        spans.push(sep.clone());
        let label = [model, temperature]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        spans.push(Span::raw(label));
    }

    if let Some(session) = &app.session {
        spans.push(sep.clone());
        spans.push(Span::styled(
            session.cwd.clone(),
            Style::new().fg(Color::Gray),
        ));
    }

    if let Some(title) = &app.title {
        spans.push(sep);
        spans.push(Span::styled(
            truncate(title, 40),
            Style::new().add_modifier(Modifier::ITALIC),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(Color::Rgb(30, 30, 38))),
        area,
    );
}
