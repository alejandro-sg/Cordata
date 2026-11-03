//! The single line under the prompt: connection, model and directory on the
//! left; on the right, the key hints — or, while the agent works, how long and
//! how many tool calls so far.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::{display_width, truncate};
use crate::app::{App, ConnState, TabState};

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| display_width(&s.content)).sum()
}

/// `/Users/me/proj` → `~/proj`.
pub fn short_path(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

/// The right-hand text, longest first; the footer uses the longest that still
/// leaves room for the model. The working variants lead with the scanning light.
fn right_variants(app: &App) -> Vec<Vec<Span<'static>>> {
    if let Some(hint) = &app.hint {
        return vec![vec![Span::styled(hint.clone(), Style::new().fg(Color::Yellow))]];
    }
    if app.turn_active {
        let elapsed = app
            .turn_started
            .map_or(0, |started| started.elapsed().as_secs());
        let tools = match app.turn_tools {
            0 => String::new(),
            1 => " · 1 tool call".to_owned(),
            n => format!(" · {n} tool calls"),
        };
        let doing = activity(app).unwrap_or_else(|| "working".to_owned());
        let working = |rail: usize, text: String| {
            let mut spans = super::hud::scanner(app.spinner, rail);
            spans.push(Span::styled(text, Style::new().fg(Color::Cyan)));
            spans
        };
        return vec![
            working(12, format!(" {doing} {elapsed}s{tools} · Esc cancels")),
            working(10, format!(" working {elapsed}s{tools} · Esc cancels")),
            working(8, format!(" {elapsed}s · Esc cancels")),
            working(6, format!(" {elapsed}s")),
        ];
    }
    let hint = |text: &str| vec![Span::styled(text.to_owned(), Style::new().fg(Color::DarkGray))];
    vec![
        hint("? help · Ctrl+K palette · Ctrl+Q quit"),
        hint("? help · Ctrl+Q quit"),
        hint("? help"),
    ]
}

/// What the agent is doing right now, in a word, from the newest tool call still running.
fn activity(app: &App) -> Option<String> {
    use acp_core::{ToolKind, ToolStatus};
    app.messages.iter().rev().find_map(|message| match message {
        crate::app::Message::Tool(card) if matches!(card.status, ToolStatus::Pending | ToolStatus::InProgress) => Some(match card.kind {
            ToolKind::Execute => format!("running {}", card.name),
            ToolKind::Edit => format!("editing · {}", card.name),
            ToolKind::Read => format!("reading · {}", card.name),
            ToolKind::Search => format!("searching · {}", card.name),
            _ => card.name.clone(),
        }),
        _ => None,
    })
}

pub fn line(app: &App, width: u16) -> Line<'static> {
    let width = width as usize;
    let dim = Style::new().fg(Color::DarkGray);

    let (dot, dot_color) = match &app.conn {
        ConnState::Connecting => ("●", Color::Yellow),
        ConnState::Connected => ("●", Color::Green),
        ConnState::Closed { .. } => ("●", Color::Red),
    };
    let mut head = vec![Span::styled(format!(" {dot}"), Style::new().fg(dot_color))];
    // Several sessions: a tab strip — the shown one bracketed, the others with
    // what they are doing (⚠ = waiting for your approval).
    if app.tabs.len() > 1 {
        head.push(Span::raw(" "));
        for tab in &app.tabs {
            let (mark, color) = match tab.state {
                TabState::Connecting => ("…", Color::Yellow),
                TabState::Idle => ("", Color::Gray),
                TabState::Working => ("●", Color::Yellow),
                TabState::NeedsApproval => ("⚠", Color::Red),
                // Finished while you were elsewhere (Ctrl+G goes there).
                TabState::Done => ("✓", Color::Green),
                TabState::Closed => ("✗", Color::DarkGray),
            };
            let label = if tab.active {
                format!("[{}{mark}]", tab.number)
            } else {
                format!(" {}{mark} ", tab.number)
            };
            let style = if tab.active {
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(color)
            };
            head.push(Span::styled(label, style));
        }
    }
    if matches!(app.conn, ConnState::Connecting) {
        head.push(Span::styled(" connecting…", dim));
    }
    // Auto mode is in the head, so it shows at any width: calls the judge clears run without a card.
    if app.mode == acp_core::Mode::Auto {
        head.push(Span::raw(" "));
        head.push(Span::styled("auto", Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    }
    // Optional segments in the order they are worth keeping on a narrow terminal: the model, then where we are. Each is a few spans and its width.
    let mut segments: Vec<(Vec<Span<'static>>, usize)> = Vec::new();
    let mut add = |spans: Vec<Span<'static>>| {
        let w = spans.iter().map(|s| display_width(&s.content)).sum::<usize>();
        segments.push((spans, w));
    };
    if let Some(session) = &app.session
        && !session.model.is_empty()
    {
        add(vec![Span::styled(session.model.clone(), Style::new())]);
    }
    let cwd = app
        .session
        .as_ref()
        .map(|session| short_path(&session.cwd))
        .unwrap_or_default();
    if !cwd.is_empty() {
        let mut spans = vec![Span::styled(cwd, dim)];
        if let Some(branch) = &app.branch {
            spans.push(Span::styled(format!(" ⎇ {branch}"), Style::new().fg(Color::Gray)));
        }
        add(spans);
    }

    let variants = right_variants(app);
    let head_width: usize = head.iter().map(|s| display_width(&s.content)).sum();
    // The fullest arrangement that fits: all segments with the longest hint, then fewer segments, then shorter hints.
    let fits = |count: usize, variant: usize| {
        let used: usize = segments.iter().take(count).map(|(_, w)| w + 3).sum();
        head_width + used + spans_width(&variants[variant]) + 2 <= width
    };
    let (count, variant) = (0..=segments.len())
        .rev()
        .find_map(|count| (0..variants.len()).find(|&v| fits(count, v)).map(|v| (count, v)))
        .unwrap_or((0, variants.len() - 1));
    let right = variants[variant].clone();
    let right_width = spans_width(&right);

    let mut spans = head;
    let mut used = head_width;
    for (segment, w) in segments.into_iter().take(count) {
        used += w + 3;
        spans.push(Span::styled(" · ", dim));
        spans.extend(segment);
    }
    if used + right_width < width {
        spans.push(Span::raw(" ".repeat(width - used - right_width)));
        spans.extend(right);
    } else if used + 2 < width {
        spans.push(Span::raw("  "));
        let room = width.saturating_sub(used + 2);
        let flat: String = right.iter().map(|s| s.content.as_ref()).collect();
        let style = right.last().map_or_else(Style::new, |s| s.style);
        spans.push(Span::styled(truncate(&flat, room), style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_footer_shows_auto_only_in_auto_mode() {
        let mut app = App::new();
        app.conn = ConnState::Connected;
        assert!(!text(&line(&app, 80)).contains("auto"));
        app.mode = acp_core::Mode::Auto;
        assert!(text(&line(&app, 80)).contains(" auto"));
        assert!(text(&line(&app, 12)).contains("auto"), "kept on a narrow terminal");
    }
}
