//! Interactive panels that replace the prompt box while they are open: the
//! tool-approval request, and the model / temperature picker. In an inline UI
//! they sit directly above the footer (there is no screen to float over).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, List, ListItem, ListState, Paragraph, StatefulWidget, Widget,
};

use super::footer::mode_color;
use super::text::{truncate, wrap};
use crate::app::{App, sanitize};
use crate::config_ui::default_value;
use crate::settings::{Applies, SCHEMA};

const MAX_INPUT_ROWS: usize = 12;
/// Diff rows in the approval panel before `d` shows the rest.
const MAX_DIFF_ROWS: usize = 16;

/// What a panel needs to be drawn: its lines, border colour, and title.
pub struct Panel {
    pub title: String,
    pub border: Color,
    pub lines: Vec<Line<'static>>,
    /// A picker highlights one of its lines.
    pub selected: Option<usize>,
    /// Lines from this index on are never cut when the panel doesn't fit (the
    /// key hints); rows just above them are dropped instead.
    pub pinned_from: Option<usize>,
}

/// Fit `lines` into `rows`, keeping everything from `pinned_from` and the
/// first line, dropping from the middle with a marker.
fn fit_lines(mut lines: Vec<Line<'static>>, rows: usize, pinned_from: Option<usize>) -> Vec<Line<'static>> {
    let Some(pinned) = pinned_from.filter(|p| *p <= lines.len()) else {
        return lines;
    };
    if lines.len() <= rows || rows < pinned.min(1) + (lines.len() - pinned) + 1 {
        return lines;
    }
    let tail = lines.split_off(pinned);
    let keep = rows - tail.len() - 1;
    let hidden = lines.len() - keep;
    lines.truncate(keep);
    lines.push(Line::styled(
        format!("  … {hidden} more rows (enlarge the terminal, or d for the whole diff)"),
        Style::new().fg(Color::Gray),
    ));
    lines.extend(tail);
    lines
}

fn permission_panel(app: &App, width: u16) -> Option<Panel> {
    let pending = app.pending.front()?;
    let info = &pending.info;
    let inner_width = (width as usize).saturating_sub(4);
    let label = Style::new().fg(Color::Gray);

    let mut lines: Vec<Line<'static>> = vec![Line::from(vec![
        Span::styled("tool  ", label),
        Span::styled(
            sanitize(&info.tool_name),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  ({})", info.kind.label()), label),
        Span::styled("   mode ", label),
        Span::styled(app.mode.clone(), Style::new().fg(mode_color(&app.mode))),
    ])];
    if info.untrusted_source {
        lines.push(Line::styled(
            "⚠ its result will be untrusted content (web page / memory / MCP) — don't let it steer you",
            Style::new().fg(Color::Magenta),
        ));
    }
    if let Some(reason) = info.title.split_once(" — ").map(|(_, why)| why.to_owned()) {
        lines.push(Line::styled(
            truncate(&format!("⚠ {}", sanitize(&reason)), inner_width),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    // An edit: show what it will change (read from disk before it runs).
    let card = app.card(&info.tool_call_id);
    let diffs = card.map(|c| c.diffs.as_slice()).unwrap_or_default();
    let patch = (info.tool_name == "apply_patch")
        .then(|| info.raw_input.as_ref()?.get("patch")?.as_str().map(str::to_owned))
        .flatten();
    let cap = Some(if app.diff_expanded { 400 } else { MAX_DIFF_ROWS });
    let cwd = app.session.as_ref().map_or("", |s| s.cwd.as_str());
    let shows_diff = !diffs.is_empty() || patch.is_some();
    if !diffs.is_empty() {
        for file in diffs {
            for row in super::diff::file_rows(file, cwd, inner_width.saturating_sub(2), cap) {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(row);
                lines.push(Line::from(spans));
            }
        }
    } else if let Some(patch) = patch {
        for row in super::diff::patch_rows(&patch, inner_width.saturating_sub(2), cap) {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(row);
            lines.push(Line::from(spans));
        }
    } else {
        let rendered_input = match &info.raw_input {
            Some(value) => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
            None => "(none)".to_owned(),
        };
        let rows = wrap(&sanitize(&rendered_input), inner_width.saturating_sub(2));
        let overflow = rows.len().saturating_sub(MAX_INPUT_ROWS);
        for row in rows.into_iter().take(MAX_INPUT_ROWS) {
            lines.push(Line::raw(format!("  {row}")));
        }
        if overflow > 0 {
            lines.push(Line::styled(format!("  … +{overflow} more lines"), label));
        }
    }
    let key = |text: &'static str, color: Color| {
        Span::styled(
            text,
            Style::new()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )
    };
    let mut keys = vec![
        key(" a ", Color::Green),
        Span::raw(" Allow     "),
        key(" r ", Color::Red),
        Span::raw(" Reject (Esc)"),
    ];
    if shows_diff {
        keys.push(Span::raw("     "));
        keys.push(key(" d ", Color::Blue));
        keys.push(Span::raw(if app.diff_expanded { " shorter diff" } else { " whole diff" }));
    }
    let pinned_from = lines.len();
    lines.push(Line::from(keys));
    if let Some(label) = &info.always_label {
        lines.push(Line::from(vec![
            key(" s ", Color::Cyan),
            Span::raw(format!(
                " {}",
                truncate(&sanitize(label), inner_width.saturating_sub(5))
            )),
        ]));
    }
    if app.pending.len() > 1 {
        lines.push(Line::styled(
            format!("{} more request(s) queued", app.pending.len() - 1),
            label,
        ));
    }
    lines.push(Line::styled(
        truncate(
            "Ctrl+C can't cancel the turn while this is open — answer first.",
            inner_width,
        ),
        label,
    ));
    Some(Panel {
        title: " Permission required ".into(),
        border: Color::Yellow,
        lines,
        selected: None,
        pinned_from: Some(pinned_from),
    })
}

fn picker_panel(app: &App) -> Option<Panel> {
    let picker = app.picker.as_ref()?;
    let current = app
        .config(&picker.config_id)
        .map(|o| o.current_value.as_str());
    let lines = picker
        .choices
        .iter()
        .map(|choice| {
            let marker = if Some(choice.value.as_str()) == current {
                "● "
            } else {
                "  "
            };
            Line::raw(format!("{marker}{}", choice.name))
        })
        .collect();
    Some(Panel {
        title: format!(" {} — ↑/↓ Enter · Esc ", picker.title),
        border: Color::Cyan,
        lines,
        selected: Some(picker.selected),
        pinned_from: None,
    })
}

/// `/config`: one row per setting — its value, the file that set it, and where
/// the next change will be written.
fn config_panel(app: &App, width: u16) -> Option<Panel> {
    let panel = app.config.as_ref()?;
    let inner = (width as usize).saturating_sub(4);
    let dim = Style::new().fg(Color::DarkGray);
    let mut lines: Vec<Line<'static>> = SCHEMA
        .iter()
        .map(|entry| {
            let (value, source) = match app.settings.get(entry.key) {
                Some(_) => (
                    app.settings.display(entry.key),
                    app.settings
                        .sources
                        .get(entry.key)
                        .map_or("", |scope| scope.name()),
                ),
                None => {
                    // Not set in any file: what is in effect anyway.
                    let live = matches!(entry.key, "model" | "temperature")
                        .then(|| {
                            app.config(entry.key)
                                .map(|option| option.current_name().to_owned())
                        })
                        .flatten();
                    match (live, default_value(entry)) {
                        (Some(name), _) => (name, "session"),
                        (None, serde_json::Value::Null) => ("—".to_owned(), ""),
                        (None, value) => (
                            value
                                .as_str()
                                .map_or_else(|| value.to_string(), str::to_owned),
                            "default",
                        ),
                    }
                }
            };
            Line::from(vec![
                Span::raw(format!(" {:<24}", entry.key)),
                Span::styled(
                    format!("{:<16}", truncate(&value, 15)),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::styled(source.to_owned(), dim),
            ])
        })
        .collect();
    let entry = &SCHEMA[panel.selected.min(SCHEMA.len() - 1)];
    let when = match entry.applies {
        Applies::Now => "applies now",
        Applies::NextSession => "applies to the next session",
    };
    lines.push(Line::styled(
        truncate(&format!(" {} — {when}", entry.help), inner),
        dim,
    ));
    let status = if panel.status.is_empty() {
        format!(
            " ↑↓ select · Enter change · ⌫ reset · Tab file: {} · Esc close",
            panel.scope.name()
        )
    } else {
        format!(" {}", panel.status)
    };
    lines.push(Line::styled(
        truncate(&status, inner),
        Style::new().fg(Color::Gray),
    ));
    Some(Panel {
        title: format!(" Settings → {} file ", panel.scope.name()),
        border: Color::Cyan,
        lines,
        selected: Some(panel.selected),
        pinned_from: None,
    })
}

pub fn current(app: &App, width: u16) -> Option<Panel> {
    permission_panel(app, width)
        .or_else(|| picker_panel(app))
        .or_else(|| config_panel(app, width))
}

/// Total rows a panel needs, bordered, capped at `max`.
pub fn height(panel: &Panel, max: u16) -> u16 {
    (panel.lines.len() as u16 + 2).min(max.max(3))
}

pub fn render(buffer: &mut Buffer, area: Rect, panel: Panel) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(panel.title)
        .border_style(Style::new().fg(panel.border).add_modifier(Modifier::BOLD));
    match panel.selected {
        Some(selected) => {
            // Keep the highlighted row visible when the list is taller than the panel.
            let items: Vec<ListItem> = panel.lines.into_iter().map(ListItem::new).collect();
            let mut state = ListState::default().with_selected(Some(selected));
            StatefulWidget::render(
                List::new(items)
                    .block(block)
                    .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
                area,
                buffer,
                &mut state,
            );
        }
        None => {
            let rows = area.height.saturating_sub(2) as usize;
            Paragraph::new(fit_lines(panel.lines, rows, panel.pinned_from))
                .block(block)
                .render(area, buffer)
        }
    }
}
