use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, ConnState};

pub const MAX_INPUT_ROWS: usize = 6;

/// Hard-wrap (not word-wrap) so the cursor's row/column is exact.
pub fn layout(text: &str, cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let (mut col, mut cursor_pos) = (0usize, (0usize, 0usize));
    for (index, c) in text.char_indices() {
        if index == cursor {
            cursor_pos = (rows.len() - 1, col);
        }
        if c == '\n' {
            rows.push(String::new());
            col = 0;
            continue;
        }
        let w = c.width().unwrap_or(0);
        if col + w > width {
            rows.push(String::new());
            col = 0;
            if index == cursor {
                cursor_pos = (rows.len() - 1, 0);
            }
        }
        rows.last_mut().unwrap().push(c);
        col += w;
    }
    if cursor >= text.len() {
        if col >= width {
            rows.push(String::new());
            col = 0;
        }
        cursor_pos = (rows.len() - 1, col);
    }
    (rows, cursor_pos)
}

/// Messages typed while the agent was busy, waiting their turn, shown right above the prompt box.
pub fn queue_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let room = (width as usize).saturating_sub(12).max(8);
    app.queue
        .iter()
        .take(3)
        .map(|text| {
            let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
            Line::from(vec![
                ratatui::text::Span::styled(" ⏵ next  ", Style::new().fg(Color::Cyan)),
                ratatui::text::Span::styled(super::text::truncate(&one_line, room), dim),
            ])
        })
        .chain((app.queue.len() > 3).then(|| Line::styled(format!("   … and {} more", app.queue.len() - 3), dim)))
        .collect()
}

/// Columns before the text: a space, the `❯`, a space.
const LEAD: usize = 3;

pub fn height_for(app: &App, width: u16) -> u16 {
    let inner = (width as usize).saturating_sub(LEAD + 1);
    let (rows, _) = layout(app.input.text(), app.input.cursor(), inner);
    rows.len().clamp(1, MAX_INPUT_ROWS) as u16 + 2
}

/// The prompt: a rule above and a rule below, the text between them.
///
/// ```text
///  ━━ working — enter queues your next message ──────────────────────
///  ❯ then run the tests
///  ───────────────────────────────────────────────────────────────────
/// ```
///
/// The short heavy stroke on the top rule takes the mode's colour (plan green, build yellow, auto red), so the one
/// place the eyes are while typing says how much the agent may do on its own. Returns where the text cursor belongs
/// (column, row within `buffer`) when the box accepts input, else `None`.
pub fn render(buffer: &mut Buffer, area: Rect, app: &App) -> Option<(u16, u16)> {
    let enabled = app.can_type();
    let width = area.width as usize;
    let rule_style = Style::new().fg(Color::DarkGray);
    let accent = if enabled { super::footer::mode_color(&app.mode) } else { Color::DarkGray };
    let accent = if app.mode.is_empty() && enabled { Color::Cyan } else { accent };
    let (title, title_style) = if enabled && app.turn_active {
        // The agent is working; what is typed now waits its turn.
        ("working — enter queues your next message", rule_style)
    } else if enabled {
        ("", rule_style)
    } else {
        match &app.conn {
            ConnState::Closed { .. } => ("disconnected — Ctrl+Q to quit", Style::new().fg(Color::Red)),
            ConnState::Connecting => ("connecting…", rule_style),
            ConnState::Connected => ("working…", rule_style),
        }
    };

    // Top rule: ` ━━ title ───────…`
    let mut top = vec![Span::raw(" "), Span::styled("━━", Style::new().fg(accent).add_modifier(Modifier::BOLD))];
    let mut used = 3;
    if !title.is_empty() {
        let shown = super::text::truncate(title, width.saturating_sub(used + 6));
        used += 2 + super::text::display_width(&shown);
        top.push(Span::styled(format!(" {shown} "), title_style));
    }
    top.push(Span::styled("─".repeat(width.saturating_sub(used + usize::from(title.is_empty()))), rule_style));
    buffer.set_line(area.x, area.y, &Line::from(top), area.width);

    let body_rows = (area.height as usize).saturating_sub(2).max(1);
    let (rows, (cursor_row, cursor_col)) = layout(app.input.text(), app.input.cursor(), width.saturating_sub(LEAD + 1));
    let first = (cursor_row + 1).saturating_sub(body_rows);
    let lead_style = Style::new().fg(accent).add_modifier(Modifier::BOLD);
    let lines: Vec<Line> = if app.input.is_empty() && enabled {
        match (&app.suggestion, app.turn_active) {
            (Some(suggestion), false) => vec![Line::from(vec![
                Span::styled(" ❯ ", lead_style),
                Span::styled(super::text::truncate(suggestion, width.saturating_sub(LEAD + 12)), Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)),
                Span::styled("  tab ⇥", Style::new().fg(Color::DarkGray)),
            ])],
            (_, busy) => {
                let hint = if busy { "type ahead — it is sent when the agent is done" } else { "Ask anything — / commands · @ files · ! shell" };
                vec![Line::from(vec![Span::styled(" ❯ ", lead_style), Span::styled(hint, Style::new().fg(Color::DarkGray))])]
            }
        }
    } else {
        rows.iter()
            .skip(first)
            .take(body_rows)
            .enumerate()
            .map(|(i, row)| {
                // `❯` marks the first row of the prompt; wrapped rows indent under it.
                let lead = if first + i == 0 { " ❯ " } else { "   " };
                Line::from(vec![Span::styled(lead, lead_style), Span::raw(row.clone())])
            })
            .collect()
    };
    for (offset, line) in lines.iter().enumerate() {
        buffer.set_line(area.x, area.y + 1 + offset as u16, line, area.width);
    }
    let bottom = area.y + area.height.saturating_sub(1);
    buffer.set_line(area.x, bottom, &Line::styled("─".repeat(width.saturating_sub(1)), rule_style).patch_style(Style::new()), area.width);
    // A leading space keeps the rules aligned with the text above them.
    buffer.set_string(area.x, bottom, " ", rule_style);

    enabled.then(|| (area.x + (LEAD + cursor_col) as u16, area.y + 1 + (cursor_row - first) as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_wraps_and_tracks_cursor() {
        let (rows, cursor) = layout("abcdef", 6, 4);
        assert_eq!(rows, ["abcd", "ef"]);
        assert_eq!(cursor, (1, 2));

        let (rows, cursor) = layout("ab\ncd", 3, 10);
        assert_eq!(rows, ["ab", "cd"]);
        assert_eq!(cursor, (1, 0));

        let (rows, cursor) = layout("abcd", 4, 4);
        assert_eq!(rows, ["abcd", ""]);
        assert_eq!(cursor, (1, 0));
    }
}
