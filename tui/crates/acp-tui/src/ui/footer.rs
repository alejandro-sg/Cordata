//! The single line under the prompt: connection, mode, model and directory on the
//! left; on the right, the key hints — or, while the agent works, how long and
//! how many tool calls so far.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::{display_width, truncate};
use crate::app::{App, ConnState, TabState};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
/// leaves room for the model.
fn right_variants(app: &App) -> (Vec<String>, Style) {
    if let Some(hint) = &app.hint {
        return (vec![hint.clone()], Style::new().fg(Color::Yellow));
    }
    if app.turn_active {
        let spinner = SPINNER[app.spinner % SPINNER.len()];
        let elapsed = app
            .turn_started
            .map_or(0, |started| started.elapsed().as_secs());
        let tools = match app.turn_tools {
            0 => String::new(),
            1 => " · 1 tool call".to_owned(),
            n => format!(" · {n} tool calls"),
        };
        return (
            vec![
                format!("{spinner} working {elapsed}s{tools} · Esc cancels"),
                format!("{spinner} {elapsed}s · Esc cancels"),
                format!("{spinner} {elapsed}s"),
            ],
            Style::new().fg(Color::Yellow),
        );
    }
    (
        vec![
            "? help · Tab mode · Ctrl+O model · Ctrl+Q quit".to_owned(),
            "? help · Ctrl+Q quit".to_owned(),
            "? help".to_owned(),
        ],
        Style::new().fg(Color::DarkGray),
    )
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
    if !app.mode.is_empty() {
        head.push(Span::raw(" "));
        head.push(Span::styled(
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
            head.push(Span::styled(
                format!(" → {pending}…"),
                Style::new().fg(mode_color(pending)),
            ));
        }
    }

    // Optional parts, dropped from the least important when the line is tight.
    let model = [app.config("model"), app.config("temperature")]
        .into_iter()
        .flatten()
        .map(|option| option.current_name().to_owned())
        .collect::<Vec<_>>()
        .join(" · ");
    let cwd = app
        .session
        .as_ref()
        .map(|session| short_path(&session.cwd))
        .unwrap_or_default();
    let (variants, right_style) = right_variants(app);

    let head_width: usize = head.iter().map(|s| display_width(&s.content)).sum();
    let width_of = |text: &str| {
        if text.is_empty() {
            0
        } else {
            display_width(text) + 3
        }
    };
    // Most informative combination that fits: model and cwd with the full hint,
    // then model with progressively shorter hints, then just the hint.
    let mut plans: Vec<(bool, bool, usize)> = vec![(true, true, 0), (true, false, 0)];
    plans.extend((1..variants.len()).map(|v| (true, false, v)));
    plans.extend((0..variants.len()).map(|v| (false, false, v)));
    let (with_model, with_cwd, variant) = plans
        .into_iter()
        .find(|&(m, c, v)| {
            let optional =
                if m { width_of(&model) } else { 0 } + if c { width_of(&cwd) } else { 0 };
            head_width + optional + display_width(&variants[v]) + 2 <= width
        })
        .unwrap_or((false, false, variants.len() - 1));
    let right = variants[variant].clone();
    let right_width = display_width(&right);

    let mut spans = head;
    let mut used = head_width;
    for (include, text, style) in [(with_model, model, Style::new()), (with_cwd, cwd, dim)] {
        if include && !text.is_empty() {
            used += width_of(&text);
            spans.push(Span::styled(" · ", dim));
            spans.push(Span::styled(text, style));
        }
    }
    if used + right_width < width {
        spans.push(Span::raw(" ".repeat(width - used - right_width)));
        spans.push(Span::styled(right, right_style));
    } else if used + 2 < width {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            truncate(&right, width.saturating_sub(used + 2)),
            right_style,
        ));
    }
    Line::from(spans)
}
