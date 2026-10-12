//! The completion menu's rows, drawn just above the prompt box.
//!
//! ```text
//!   ▸ /model        [name]            pick the model (no name opens the picker)
//!     /mode         [plan|build|auto] show or set the approval mode
//!     /memory                         show the instruction files the agent reads
//! ```

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::{display_width, truncate};
use crate::app::App;
use crate::menu::{Kind, MAX_ROWS};

/// The rows to draw (none when no menu is open). A window of `MAX_ROWS` that follows the highlight.
pub fn lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let Some(menu) = &app.menu else { return Vec::new() };
    let width = width as usize;
    let dim = Style::new().fg(Color::DarkGray);
    let first = menu.selected.saturating_sub(MAX_ROWS - 1).min(menu.items.len().saturating_sub(MAX_ROWS));
    let shown = &menu.items[first..menu.items.len().min(first + MAX_ROWS)];
    let label_width = shown.iter().map(|i| display_width(&i.label)).max().unwrap_or(0).min(width / 2);
    let hint_width = shown.iter().map(|i| display_width(&i.hint)).max().unwrap_or(0).min(width / 3);

    let mut lines: Vec<Line<'static>> = shown
        .iter()
        .enumerate()
        .map(|(offset, item)| {
            let selected = first + offset == menu.selected;
            let marker = if selected { "▸ " } else { "  " };
            let label_style = match (selected, menu.kind) {
                (true, _) => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                (false, Kind::Command) => Style::new().fg(Color::Gray),
                (false, Kind::File) => Style::new().fg(Color::Gray),
            };
            let pad = |text: &str, to: usize| format!("{text}{}", " ".repeat(to.saturating_sub(display_width(text))));
            let detail_room = width.saturating_sub(4 + label_width + 2 + hint_width + 2);
            let mut detail = item.detail.clone();
            if !item.scope.is_empty() {
                detail = format!("{detail} ({})", item.scope);
            }
            let mut spans = vec![
                Span::styled(format!(" {marker}"), Style::new().fg(Color::Cyan)),
                Span::styled(pad(&truncate(&item.label, label_width.max(1)), label_width), label_style),
            ];
            if menu.kind == Kind::Command {
                spans.push(Span::styled(format!("  {}", pad(&truncate(&item.hint, hint_width.max(1)), hint_width)), dim));
                spans.push(Span::styled(
                    format!("  {}", truncate(&detail, detail_room.max(1))),
                    if selected { Style::new().fg(Color::Gray) } else { dim },
                ));
            }
            Line::from(spans)
        })
        .collect();

    // What the keys do, once, under the list.
    let hint = match menu.kind {
        Kind::Command => "  ↑↓ choose · tab complete · enter run · esc close",
        Kind::File => "  ↑↓ choose · tab attach · esc close — the file goes to the agent with your prompt",
    };
    lines.push(Line::styled(truncate(hint, width), dim));
    lines
}
