//! Tool calls as bordered cards: status glyph, input summary, collapsed
//! result. Untrusted results get a double-line magenta border so they're
//! impossible to mistake for the agent's own words.

use acp_core::ToolStatus;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::text::{display_width, pad_to, truncate, wrap};
use crate::app::ToolCard;

const COLLAPSED_RESULT_LINES: usize = 4;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn status_glyph(status: ToolStatus, spinner: usize) -> (&'static str, Color) {
    match status {
        ToolStatus::Pending => ("○", Color::Gray),
        ToolStatus::InProgress => (SPINNER[spinner % SPINNER.len()], Color::Yellow),
        ToolStatus::Completed => ("✓", Color::Green),
        ToolStatus::Failed => ("✗", Color::Red),
    }
}

fn input_summary(card: &ToolCard) -> Option<String> {
    let value = card.raw_input.as_ref()?;
    let text = match value {
        Value::Null => return None,
        Value::String(raw) if raw.is_empty() => return None,
        Value::String(raw) => raw.clone(),
        Value::Object(map) => {
            let command = map.get("command").and_then(Value::as_str);
            let code = map.get("code").and_then(Value::as_str);
            match (command, code) {
                (Some(command), _) => format!("$ {command}"),
                (_, Some(code)) => code.to_owned(),
                _ => value.to_string(),
            }
        }
        other => other.to_string(),
    };
    Some(crate::app::sanitize(&text))
}

pub fn render(
    card: &ToolCard,
    width: u16,
    expanded: bool,
    awaiting: bool,
    spinner: usize,
) -> Vec<Line<'static>> {
    let width = (width as usize).max(24);
    let inner = width - 4;
    let (tl, tr, bl, br, h, v) = if card.untrusted {
        ('╔', '╗', '╚', '╝', '═', '║')
    } else {
        ('╭', '╮', '╰', '╯', '─', '│')
    };
    let border = if card.untrusted {
        Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)
    } else if awaiting {
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else if card.status == ToolStatus::Failed {
        Style::new().fg(Color::Red)
    } else {
        Style::new().fg(Color::DarkGray)
    };

    let (glyph, glyph_color) = status_glyph(card.status, spinner);
    let title = format!(
        " {glyph} {} · {} ",
        truncate(&card.name, 30),
        card.kind.label()
    );
    let flag = if card.untrusted {
        " ⚠ UNTRUSTED CONTENT "
    } else if card.untrusted_source {
        " ⚠ may return untrusted content "
    } else {
        ""
    };
    let flag = if 3 + display_width(&title) + display_width(flag) + 1 > width {
        ""
    } else {
        flag
    };
    let fill = width.saturating_sub(2 + display_width(&title) + display_width(flag) + 1);

    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{tl}{h}"), border),
        Span::styled(
            title,
            Style::new().fg(glyph_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            flag,
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{}{tr}", h.to_string().repeat(fill)), border),
    ])];

    let body_line = |text: &str, style: Style| {
        Line::from(vec![
            Span::styled(format!("{v} "), border),
            Span::styled(pad_to(text, inner), style),
            Span::styled(format!(" {v}"), border),
        ])
    };

    let dim = Style::new().fg(Color::Gray);
    if let Some(summary) = input_summary(card) {
        let summary = truncate(&summary.replace('\n', " ⏎ "), 600);
        for row in wrap(&summary, inner) {
            lines.push(body_line(&row, dim));
        }
    }

    if !card.content.trim().is_empty() {
        let style = if card.untrusted {
            Style::new().fg(Color::Magenta)
        } else {
            Style::new()
        };
        let rows = wrap(card.content.trim_end(), inner);
        let shown = if expanded {
            rows.len()
        } else {
            rows.len().min(COLLAPSED_RESULT_LINES)
        };
        for row in &rows[..shown] {
            lines.push(body_line(row, style));
        }
        if shown < rows.len() {
            let more = format!("… +{} more lines (Ctrl+E expands)", rows.len() - shown);
            lines.push(body_line(&more, dim.add_modifier(Modifier::ITALIC)));
        }
    }

    lines.push(Line::from(Span::styled(
        format!("{bl}{}{br}", h.to_string().repeat(width - 2)),
        border,
    )));
    lines
}

#[cfg(test)]
mod tests {
    use acp_core::ToolKind;

    use super::*;

    fn card(content: &str, untrusted: bool) -> ToolCard {
        ToolCard {
            id: "t".into(),
            name: "shell".into(),
            kind: ToolKind::Execute,
            status: ToolStatus::Completed,
            raw_input: Some(serde_json::json!({"command": "echo hi"})),
            content: content.into(),
            untrusted,
            untrusted_source: false,
        }
    }

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn card_lines_all_have_equal_width() {
        let lines = plain(&render(&card("hello\nworld", false), 40, false, false, 0));
        assert!(lines.iter().all(|l| display_width(l) == 40), "{lines:#?}");
        assert!(lines[1].contains("$ echo hi"));
    }

    #[test]
    fn long_results_collapse_unless_expanded() {
        let long = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed = plain(&render(&card(&long, false), 40, false, false, 0));
        assert!(collapsed.iter().any(|l| l.contains("+6 more lines")));
        let expanded = plain(&render(&card(&long, false), 40, true, false, 0));
        assert!(expanded.iter().any(|l| l.contains("line9")));
    }

    #[test]
    fn untrusted_results_use_a_distinct_double_border_and_label() {
        let lines = plain(&render(&card("x", true), 50, false, false, 0));
        assert!(lines[0].starts_with('╔') && lines[0].contains("UNTRUSTED"));
        assert!(lines.last().unwrap().starts_with('╚'));
    }
}
