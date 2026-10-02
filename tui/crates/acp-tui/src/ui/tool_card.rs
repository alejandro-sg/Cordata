//! Tool calls as bordered cards: status glyph, input summary, collapsed
//! result. Untrusted results get a double-line magenta border so they're
//! impossible to mistake for the agent's own words.

use acp_core::{ToolStatus, unwrap_untrusted};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::team::{self, NodeState, TeamRow};
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
    summarize_input(card.raw_input.as_ref()?, team::is_team_tool(&card.name))
}

fn summarize_input(value: &Value, is_team: bool) -> Option<String> {
    let text = match value {
        Value::Null => return None,
        Value::String(raw) if raw.is_empty() => return None,
        // While a call streams in, its input is the JSON *text* so far. Once it
        // parses, summarise the parsed object; until then a team call is still
        // being written (its input is long), anything else shows the raw text.
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(parsed @ Value::Object(_)) => return summarize_input(&parsed, is_team),
            _ if is_team => return Some("(preparing the team…)".to_owned()),
            _ => raw.clone(),
        },
        // A team tool's input is a task plus every specialist's full system
        // prompt; the task is what a reader wants (the agents get their own rows).
        Value::Object(map) if map.get("agents").is_some() && map.get("task").is_some() => {
            format!(
                "task: {}",
                map.get("task").and_then(Value::as_str).unwrap_or_default()
            )
        }
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

/// "completed in 1.7s" from a team tool's result JSON, if it is one.
fn team_result_summary(body: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let status = parsed
        .get("status")?
        .as_str()?
        .trim_start_matches("Status.")
        .to_lowercase();
    let seconds = parsed
        .get("execution_time_ms")
        .and_then(Value::as_f64)
        .map(|ms| format!(" in {:.1}s", ms / 1000.0));
    Some(format!(
        "{status}{}  (Ctrl+E shows the full result)",
        seconds.unwrap_or_default()
    ))
}

/// One agent (or hand-off) of a team card, as bordered body line(s).
fn team_row_lines(
    row: &TeamRow,
    inner: usize,
    expanded: bool,
    spinner: usize,
    border: Style,
    v: char,
) -> Vec<Line<'static>> {
    let boxed = |spans: Vec<Span<'static>>| {
        let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
        let mut all = vec![Span::styled(format!("{v} "), border)];
        all.extend(spans);
        all.push(Span::styled(
            " ".repeat(inner.saturating_sub(used)),
            Style::new(),
        ));
        all.push(Span::styled(format!(" {v}"), border));
        Line::from(all)
    };
    let dim = Style::new().fg(Color::Gray);
    match row {
        TeamRow::Node(node) => {
            let (glyph, color) = match node.state {
                NodeState::Running => (SPINNER[spinner % SPINNER.len()], Color::Yellow),
                NodeState::Done => ("✓", Color::Green),
            };
            let name = truncate(&node.name, 18);
            let lead = 2 + display_width(&name) + 2;
            let avail = inner.saturating_sub(lead).max(8);
            let name_span = Span::styled(name, Style::new().add_modifier(Modifier::BOLD));
            let glyph_span = Span::styled(format!("{glyph} "), Style::new().fg(color));
            match node.state {
                NodeState::Running => {
                    let (text, style) = if node.thinking.is_empty() {
                        ("working…".to_owned(), dim.add_modifier(Modifier::ITALIC))
                    } else {
                        (
                            format!(
                                "thinking: {}",
                                truncate(&node.thinking, avail.saturating_sub(10))
                            ),
                            dim.add_modifier(Modifier::ITALIC),
                        )
                    };
                    vec![boxed(vec![
                        glyph_span,
                        name_span,
                        Span::raw("  "),
                        Span::styled(truncate(&text, avail), style),
                    ])]
                }
                NodeState::Done if expanded && display_width(&node.summary) > avail => {
                    let mut out = Vec::new();
                    for (index, piece) in wrap(&node.summary, avail).into_iter().enumerate() {
                        if index == 0 {
                            out.push(boxed(vec![
                                glyph_span.clone(),
                                name_span.clone(),
                                Span::raw("  "),
                                Span::styled(piece, dim),
                            ]));
                        } else {
                            out.push(boxed(vec![
                                Span::raw(" ".repeat(lead)),
                                Span::styled(piece, dim),
                            ]));
                        }
                    }
                    out
                }
                NodeState::Done => vec![boxed(vec![
                    glyph_span,
                    name_span,
                    Span::raw("  "),
                    Span::styled(truncate(&node.summary.replace('\n', " "), avail), dim),
                ])],
            }
        }
        TeamRow::Handoff { from, to, message } => {
            let route = format!("{from} → {to}");
            let avail = inner.saturating_sub(2 + display_width(&route) + 2).max(8);
            vec![boxed(vec![
                Span::styled("→ ", Style::new().fg(Color::Cyan)),
                Span::styled(route, Style::new().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(truncate(message, avail), dim),
            ])]
        }
    }
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
    let is_team = team::is_team_tool(&card.name);
    let team_rows = if is_team {
        team::parse(&card.progress)
    } else {
        Vec::new()
    };
    let agent_count = team_rows
        .iter()
        .filter(|row| matches!(row, TeamRow::Node(_)))
        .count();
    let title = if is_team {
        let agents = if agent_count > 0 {
            format!(" ({agent_count} agents)")
        } else {
            String::new()
        };
        format!(" {glyph} {} · team{agents} ", truncate(&card.name, 30))
    } else {
        format!(
            " {glyph} {} · {} ",
            truncate(&card.name, 30),
            card.kind.label()
        )
    };
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

    // Sub-agent team: one row per agent — running (with a live tail of what it
    // is thinking), done (with its summary), and the hand-offs between them.
    if is_team {
        if team_rows.is_empty() && card.status == ToolStatus::InProgress {
            lines.push(body_line(
                "starting the team…",
                dim.add_modifier(Modifier::ITALIC),
            ));
        }
        for row in &team_rows {
            lines.extend(team_row_lines(row, inner, expanded, spinner, border, v));
        }
    }

    if !card.content.trim().is_empty() {
        // The guard wrapper is the same boilerplate on every untrusted result;
        // show what matters about it (where it came from, whether the
        // classifier flagged it) and then the body, not nine words of
        // header before every result.
        let (body, source, flagged) = match unwrap_untrusted(&card.content) {
            Some(unwrapped) => (unwrapped.body, unwrapped.source, unwrapped.flagged),
            None => (card.content.clone(), None, false),
        };
        let style = if card.untrusted {
            Style::new().fg(Color::Magenta)
        } else {
            Style::new()
        };
        if is_team && !team_rows.is_empty() {
            lines.push(body_line("result", dim.add_modifier(Modifier::BOLD)));
        }
        if let Some(source) = source {
            let label = truncate(&format!("source: {source}"), inner);
            lines.push(body_line(&label, style.add_modifier(Modifier::ITALIC)));
        }
        if flagged {
            lines.push(body_line(
                "⚠ a local classifier flagged this as a likely injection attempt",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        // A team's result JSON repeats every agent's output, which the rows above
        // already show; collapsed, reduce it to the status line (Ctrl+E has the rest).
        let body = match (is_team && !expanded)
            .then(|| team_result_summary(&body))
            .flatten()
        {
            Some(summary) => summary,
            None => body,
        };
        let rows = wrap(body.trim_end(), inner);
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
            progress: String::new(),
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
    fn streamed_team_input_is_summarised_as_its_task_or_a_placeholder() {
        let complete = Value::String(
            r#"{"agents":[{"name":"a","system_prompt":"LONG"}],"task":"do it"}"#.into(),
        );
        assert_eq!(summarize_input(&complete, true).unwrap(), "task: do it");
        let partial = Value::String(r#"{"agents":[{"name":"a","system_pro"#.into());
        assert_eq!(
            summarize_input(&partial, true).unwrap(),
            "(preparing the team…)"
        );
        assert_eq!(
            summarize_input(&Value::String("echo hi".into()), false).unwrap(),
            "echo hi"
        );
    }

    #[test]
    fn team_results_collapse_to_a_status_line() {
        let body =
            r#"{"status": "Status.COMPLETED", "execution_time_ms": 1721, "results": {"a": "x"}}"#;
        assert_eq!(
            team_result_summary(body).unwrap(),
            "completed in 1.7s  (Ctrl+E shows the full result)"
        );
        assert_eq!(team_result_summary("not json"), None);
    }

    #[test]
    fn untrusted_results_use_a_distinct_double_border_and_label() {
        let lines = plain(&render(&card("x", true), 50, false, false, 0));
        assert!(lines[0].starts_with('╔') && lines[0].contains("UNTRUSTED"));
        assert!(lines.last().unwrap().starts_with('╚'));
    }
}
