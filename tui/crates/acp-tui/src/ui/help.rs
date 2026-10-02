use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::permission_modal::centered;
use crate::app::App;
use crate::commands::HELP;

const KEYS: &[(&str, &str)] = &[
    ("Enter", "send  (Ctrl+J / Alt+Enter: newline)"),
    (
        "Esc / Ctrl+C",
        "cancel the running turn  (not while a permission is open)",
    ),
    ("a / r", "allow / reject a tool call in the approval box"),
    (
        "Tab / Shift+Tab",
        "cycle mode: plan (safe) → build (asks) → auto (never asks)",
    ),
    ("Ctrl+O / Ctrl+T", "model / temperature picker"),
    ("Ctrl+R", "expand or collapse reasoning"),
    ("Ctrl+E", "expand or collapse tool results"),
    ("Up / Down", "earlier / later prompts"),
    ("PgUp / PgDn", "scroll"),
    ("? / F1", "this help"),
    ("Ctrl+Q", "quit"),
];

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    if !app.help_open {
        return;
    }
    let key_style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::Gray);
    let mut lines = vec![Line::styled(
        "Keys",
        Style::new().add_modifier(Modifier::BOLD),
    )];
    for (key, what) in KEYS {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<17}"), key_style),
            Span::raw(*what),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Commands (type in the input box)",
        Style::new().add_modifier(Modifier::BOLD),
    ));
    for (usage, what) in HELP {
        lines.push(Line::from(vec![
            Span::styled(format!("  {usage:<36}"), key_style),
            Span::raw(*what),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Reasoning is collapsed to a one-line preview; sub-agent teams show one row per agent.",
        dim,
    ));
    lines.push(Line::styled(
        "◆ laya = the fast classifier's live decision under your prompt, then whether the LLM agreed (/laya off hides it).",
        dim,
    ));
    lines.push(Line::styled("Any key closes this.", dim));

    let height = lines.len() as u16 + 2;
    let rect = centered(area, 96.min(area.width), height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Help ")
                .border_style(Style::new().fg(Color::Cyan)),
        ),
        rect,
    );
}
