use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};
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

pub fn height_for(app: &App, width: u16) -> u16 {
    let inner = width.saturating_sub(4) as usize; // borders + the "› " lead
    let (rows, _) = layout(app.input.text(), app.input.cursor(), inner);
    rows.len().clamp(1, MAX_INPUT_ROWS) as u16 + 2
}

/// The prompt box. Returns where the text cursor belongs (column, row within
/// `buffer`) when the box accepts input, else `None`.
pub fn render(buffer: &mut Buffer, area: Rect, app: &App) -> Option<(u16, u16)> {
    let enabled = app.input_enabled();
    let (title, border) = if enabled {
        ("", Color::Cyan)
    } else {
        match &app.conn {
            ConnState::Closed { .. } => (" disconnected — Ctrl+Q to quit ", Color::Red),
            ConnState::Connecting => (" connecting… ", Color::DarkGray),
            ConnState::Connected => (" working… ", Color::DarkGray),
        }
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::new().fg(border));
    let inner = block.inner(area);

    let (rows, (cursor_row, cursor_col)) = layout(
        app.input.text(),
        app.input.cursor(),
        inner.width.saturating_sub(2) as usize,
    );
    let visible = inner.height as usize;
    let first = (cursor_row + 1).saturating_sub(visible);
    let lines: Vec<Line> = if app.input.is_empty() && enabled {
        vec![Line::from(vec![
            ratatui::text::Span::styled("› ", Style::new().fg(border)),
            ratatui::text::Span::styled(
                "Ask a question, request code edits, or type / for commands...",
                Style::new().fg(Color::DarkGray),
            ),
        ])]
    } else {
        rows.iter()
            .skip(first)
            .take(visible)
            .enumerate()
            .map(|(i, row)| {
                // `› ` marks the first row of the prompt; wrapped rows indent under it.
                let lead = if first + i == 0 { "› " } else { "  " };
                Line::from(vec![
                    ratatui::text::Span::styled(lead, Style::new().fg(border)),
                    ratatui::text::Span::raw(row.clone()),
                ])
            })
            .collect()
    };

    Paragraph::new(lines).block(block).render(area, buffer);
    enabled.then(|| {
        (
            inner.x + 2 + cursor_col as u16,
            inner.y + (cursor_row - first) as u16,
        )
    })
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
