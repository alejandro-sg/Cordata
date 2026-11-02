//! Tool calls as bordered cards: status glyph, input summary, collapsed
//! result. Untrusted results get a double-line magenta border so they're
//! impossible to mistake for the agent's own words.

use acp_core::ToolStatus;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::{display_width, pad_to, truncate, wrap};
use crate::app::ToolCard;

const COLLAPSED_RESULT_LINES: usize = 4;
/// Diff rows shown per file while the card is collapsed (Ctrl+E shows all).
const COLLAPSED_DIFF_ROWS: usize = 12;

fn is_edit_tool(name: &str) -> bool {
    name == "edit_file"
}

/// Spans inside the card's side borders, padded to the inner width.
fn boxed(spans: Vec<Span<'static>>, inner: usize, border: Style, v: char) -> Line<'static> {
    let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
    let mut all = vec![Span::styled(format!("{v} "), border)];
    all.extend(spans);
    all.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
    all.push(Span::styled(format!(" {v}"), border));
    Line::from(all)
}

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
    summarize_input(card.raw_input.as_ref()?)
}

use cordata_view::view::summarize_input;

#[cfg_attr(not(test), allow(dead_code))]
pub fn render(
    card: &ToolCard,
    width: u16,
    expanded: bool,
    awaiting: bool,
    spinner: usize,
) -> Vec<Line<'static>> {
    render_in(card, width, expanded, awaiting, spinner, "")
}

/// `cwd` shortens diff paths to the session's project.
pub fn render_in(
    card: &ToolCard,
    width: u16,
    expanded: bool,
    awaiting: bool,
    spinner: usize,
    cwd: &str,
) -> Vec<Line<'static>> {
    let width = (width as usize).max(24);
    let inner = width - 4;
    let (tl, tr, bl, br, h, v) = ('╭', '╮', '╰', '╯', '─', '│');
    let border = if awaiting {
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
    let fill = width.saturating_sub(2 + display_width(&title) + 1);

    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{tl}{h}"), border),
        Span::styled(
            title,
            Style::new().fg(glyph_color).add_modifier(Modifier::BOLD),
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
    // Edits: the diff viewer instead of the raw input.
    let cap = (!expanded).then_some(COLLAPSED_DIFF_ROWS);
    if is_edit_tool(&card.name) && !card.diffs.is_empty() && awaiting {
        // The approval panel right below shows this diff in full; one line here.
        for file in &card.diffs {
            let lines_ =
                super::diff::line_diff(file.old_text.as_deref().unwrap_or(""), &file.new_text);
            let (added, removed) = super::diff::stats(&lines_);
            let path = super::diff::short(&file.path, cwd);
            lines.push(boxed(
                vec![
                    Span::styled("✎ ", Style::new().fg(Color::Cyan)),
                    Span::styled(
                        truncate(&path, inner.saturating_sub(34)),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  +{added}"), Style::new().fg(Color::Green)),
                    Span::styled(format!(" −{removed}"), Style::new().fg(Color::Red)),
                    Span::styled("  — review it below", dim.add_modifier(Modifier::ITALIC)),
                ],
                inner,
                border,
                v,
            ));
        }
    } else if is_edit_tool(&card.name) && !card.diffs.is_empty() {
        for file in &card.diffs {
            for row in super::diff::file_rows(file, cwd, inner, cap) {
                lines.push(boxed(row, inner, border, v));
            }
        }
    } else if let Some(summary) = input_summary(card) {
        let summary = truncate(&summary.replace('\n', " ⏎ "), 600);
        for row in wrap(&summary, inner) {
            lines.push(body_line(&row, dim));
        }
    }

    if !card.content.trim().is_empty() {
        let body = card.content.clone();
        let style = Style::new();
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

    fn card(content: &str) -> ToolCard {
        ToolCard {
            id: "t".into(),
            name: "run_command".into(),
            kind: ToolKind::Execute,
            status: ToolStatus::Completed,
            raw_input: Some(serde_json::json!({"cmd": "echo hi"})),
            content: content.into(),
            diffs: vec![],
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
        let lines = plain(&render(&card("hello\nworld"), 40, false, false, 0));
        assert!(lines.iter().all(|l| display_width(l) == 40), "{lines:#?}");
        assert!(lines[1].contains("$ echo hi"));
    }

    #[test]
    fn long_results_collapse_unless_expanded() {
        let long = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed = plain(&render(&card(&long), 40, false, false, 0));
        assert!(collapsed.iter().any(|l| l.contains("+6 more lines")));
        let expanded = plain(&render(&card(&long), 40, true, false, 0));
        assert!(expanded.iter().any(|l| l.contains("line9")));
    }

    #[test]
    fn edit_cards_show_the_diff_with_equal_width_rows() {
        let mut edit = card("patched");
        edit.name = "edit_file".into();
        edit.kind = ToolKind::Edit;
        edit.diffs = vec![acp_core::FileDiff {
            path: "/proj/src/a.rs".into(),
            old_text: Some("let a = 1;\nlet b = 2;\n".into()),
            new_text: "let a = 1;\nlet b = 3;\n".into(),
        }];
        let lines = plain(&render_in(&edit, 50, false, false, 0, "/proj"));
        assert!(lines.iter().all(|l| display_width(l) == 50), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("✎ src/a.rs")));
        assert!(lines.iter().any(|l| l.contains("   2 − let b = 2;")));
        assert!(lines.iter().any(|l| l.contains("   2 + let b = 3;")));
        assert!(
            !lines.iter().any(|l| l.contains("old_str")),
            "the raw input is replaced by the diff"
        );
    }

    #[test]
    fn while_awaiting_approval_the_card_defers_the_diff_to_the_panel() {
        let mut edit = card("");
        edit.name = "edit_file".into();
        edit.diffs = vec![acp_core::FileDiff {
            path: "/p/a.txt".into(),
            old_text: None,
            new_text: "x\ny\n".into(),
        }];
        let lines = plain(&render_in(&edit, 60, false, true, 0, "/p"));
        assert_eq!(lines.len(), 3, "{lines:#?}");
        assert!(lines[1].contains("✎ a.txt  +2 −0  — review it below"));
    }
}
