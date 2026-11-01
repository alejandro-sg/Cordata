//! `/help`: keys and commands, written into the transcript like any message
//! (so it scrolls with the conversation and can be copied).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::truncate;
use crate::commands::HELP;

const KEYS: &[(&str, &str)] = &[
    (
        "Enter",
        "send  (Ctrl+J / Alt+Enter / pasted text keeps newlines)",
    ),
    (
        "Esc / Ctrl+C",
        "cancel the running turn; clear the box when idle",
    ),
    (
        "a / r / s",
        "allow / reject / always-allow (saves a rule) in the approval box",
    ),
    (
        "Ctrl+R",
        "expand or collapse reasoning (applies to new output)",
    ),
    (
        "Ctrl+E",
        "expand or collapse tool results (applies to new output)",
    ),
    ("Up / Down", "earlier / later prompts"),
    ("? / F1", "this help"),
    ("Ctrl+Q", "quit"),
];

pub fn lines(width: u16) -> Vec<Line<'static>> {
    let width = width as usize;
    let key_style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::Gray);
    let bold = Style::new().add_modifier(Modifier::BOLD);

    let mut lines = vec![Line::styled("Keys", bold)];
    for (key, what) in KEYS {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<17}"), key_style),
            Span::raw(truncate(what, width.saturating_sub(19))),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Commands", bold));
    for (usage, what) in HELP {
        lines.push(Line::from(vec![
            Span::styled(format!("  {usage:<36}"), key_style),
            Span::raw(truncate(what, width.saturating_sub(38))),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Scroll with your terminal (mouse wheel, trackpad, Shift+PageUp) — finished output is ordinary scrollback, so selecting and copying work too.",
        dim,
    ));
    lines
}
