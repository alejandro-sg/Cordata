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
    /// Rows drawn under the list, outside it. A `List` scrolls to follow its
    /// selection, so anything that must stay readable — the selected entry's
    /// help and the key hints — cannot be a list item: it scrolls away as soon
    /// as the selection leaves the bottom of the panel.
    pub footer: Vec<Line<'static>>,
}

/// Fit `lines` into `rows`, keeping everything from `pinned_from` and the
/// first line, dropping from the middle with a marker.
fn fit_lines(
    mut lines: Vec<Line<'static>>,
    rows: usize,
    pinned_from: Option<usize>,
) -> Vec<Line<'static>> {
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

/// Plan rows shown before `d` shows the rest.
const MAX_PLAN_ROWS: usize = 22;

/// The plan card: the agent's `present_plan` call, drawn as the plan it carries and three answers. What the session
/// becomes is decided here, by the person — approve into build (changes still ask), approve into auto (routine calls
/// run), or keep planning.
fn plan_panel(app: &App, width: u16) -> Option<Panel> {
    let info = &app.pending.front()?.info;
    let inner_width = (width as usize).saturating_sub(4);
    let plan = info.raw_input.as_ref().and_then(|input| input.get("plan")).and_then(|plan| plan.as_str()).unwrap_or("(the agent sent an empty plan)");
    let rows = super::markdown::render(&sanitize(plan), inner_width.saturating_sub(1));
    let cap = if app.diff_expanded { 400 } else { MAX_PLAN_ROWS };
    let hidden = rows.len().saturating_sub(cap);
    let mut lines: Vec<Line<'static>> = rows
        .into_iter()
        .take(cap)
        .collect();
    if hidden > 0 {
        lines.push(Line::styled(format!("  … {hidden} more rows — d shows the whole plan"), Style::new().fg(Color::Gray)));
    } else if app.diff_expanded {
        lines.push(Line::styled("  — d shortens it again", Style::new().fg(Color::Gray)));
    }
    lines.push(Line::raw(""));
    let key = |text: &'static str, color: Color| Span::styled(text, Style::new().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD));
    let pinned_from = lines.len();
    if let Some(note) = note_lines(app, inner_width) {
        lines.extend(note);
    } else {
        let mut keys = vec![key(" b ", mode_color("build")), Span::raw(" build   ")];
        // An older backend offers no edit mode: then there is nothing to press.
        if info.option("approve_edit").is_some() {
            keys.extend([key(" e ", mode_color("edit")), Span::raw(" edit   ")]);
        }
        keys.extend([key(" a ", mode_color("auto")), Span::raw(" auto   "), key(" k ", mode_color("plan")), Span::raw(" keep planning (Esc)   "), key(" w ", Color::Gray), Span::raw(" say what to change")]);
        lines.push(Line::from(keys));
        lines.push(Line::styled(
            truncate("build asks before each change · edit runs file edits, asks for commands · auto runs routine calls without asking", inner_width),
            Style::new().fg(Color::DarkGray),
        ));
    }
    if app.pending.len() > 1 {
        lines.push(Line::styled(format!("{} more request(s) queued", app.pending.len() - 1), Style::new().fg(Color::Gray)));
    }
    Some(Panel {
        title: " ⌜ PLAN READY ⌝ — nothing has been changed yet ".into(),
        border: mode_color("plan"),
        lines,
        selected: None,
        pinned_from: Some(pinned_from),
        footer: Vec::new(),
    })
}

pub use cordata_view::view::ask_choices;

/// The note box under a card while the person is typing their own words (`w`), or `None` when it is closed.
fn note_lines(app: &App, inner_width: usize) -> Option<Vec<Line<'static>>> {
    let note = app.note.as_ref()?;
    let shown = truncate(&sanitize(note.text()), inner_width.saturating_sub(12));
    Some(vec![
        Line::from(vec![
            Span::styled(" say ", Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::raw(shown),
            Span::styled("▏", Style::new().fg(Color::Cyan)),
        ]),
        Line::styled("Enter sends it · Esc goes back to the choices", Style::new().fg(Color::DarkGray)),
    ])
}

/// A question from the agent (`ask_user`): its options numbered, one key each.
fn ask_panel(app: &App, width: u16) -> Option<Panel> {
    let info = &app.pending.front()?.info;
    let inner_width = (width as usize).saturating_sub(4);
    let question = info.raw_input.as_ref().and_then(|input| input.get("question")).and_then(|q| q.as_str()).unwrap_or("The agent has a question.");
    let mut lines: Vec<Line<'static>> = wrap(&sanitize(question), inner_width).into_iter().map(|row| Line::styled(row, Style::new().add_modifier(Modifier::BOLD))).collect();
    lines.push(Line::raw(""));
    let choices = ask_choices(info);
    for (index, choice) in choices.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", index + 1), Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::raw(truncate(&sanitize(&choice.name), inner_width.saturating_sub(6))),
        ]));
    }
    lines.push(Line::raw(""));
    let pinned_from = lines.len();
    if let Some(note) = note_lines(app, inner_width) {
        lines.extend(note);
    } else {
        let key = |text: &'static str, color: Color| Span::styled(text, Style::new().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD));
        let range = if choices.len() > 1 { format!(" 1–{} ", choices.len()) } else { " 1 ".to_owned() };
        lines.push(Line::from(vec![
            Span::styled(range, Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" choose   "),
            key(" w ", Color::Gray),
            Span::raw(" type your own   "),
            key(" s ", mode_color("plan")),
            Span::raw(" decide for me (Esc)"),
        ]));
    }
    if app.pending.len() > 1 {
        lines.push(Line::styled(format!("{} more request(s) queued", app.pending.len() - 1), Style::new().fg(Color::Gray)));
    }
    Some(Panel { title: " ⌜ QUESTION ⌝ — the agent needs a decision ".into(), border: Color::Cyan, lines, selected: None, pinned_from: Some(pinned_from), footer: Vec::new() })
}

fn permission_panel(app: &App, width: u16) -> Option<Panel> {
    let pending = app.pending.front()?;
    let info = &pending.info;
    if info.tool_name == "present_plan" {
        return plan_panel(app, width);
    }
    if info.tool_name == "ask_user" {
        return ask_panel(app, width);
    }
    let inner_width = (width as usize).saturating_sub(4);
    let label = Style::new().fg(Color::Gray);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("tool  ", label),
        Span::styled(
            sanitize(&info.tool_name),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  ({})", info.kind.label()), label),
        Span::styled("   mode ", label),
        Span::styled(app.mode.clone(), Style::new().fg(mode_color(&app.mode))),
    ]));
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
        .then(|| {
            info.raw_input
                .as_ref()?
                .get("patch")?
                .as_str()
                .map(str::to_owned)
        })
        .flatten();
    let cap = Some(if app.diff_expanded {
        400
    } else {
        MAX_DIFF_ROWS
    });
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
            Some(value) => {
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
            }
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
        Span::raw(" Reject (Esc)     "),
        key(" w ", Color::Gray),
        Span::raw(" Reject, saying why"),
    ];
    if shows_diff {
        keys.push(Span::raw("     "));
        keys.push(key(" d ", Color::Blue));
        keys.push(Span::raw(if app.diff_expanded {
            " shorter diff"
        } else {
            " whole diff"
        }));
    }
    let pinned_from = lines.len();
    if let Some(note) = note_lines(app, inner_width) {
        lines.extend(note);
    } else {
        lines.push(Line::from(keys));
    }
    if let Some(label) = info.always_label.as_ref().filter(|_| app.note.is_none()) {
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
        footer: Vec::new(),
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
        footer: Vec::new(),
    })
}

/// `/config`: one row per setting — its value, the file that set it, and where
/// the next change will be written.
fn config_panel(app: &App, width: u16) -> Option<Panel> {
    let panel = app.config.as_ref()?;
    let inner = (width as usize).saturating_sub(4);
    let dim = Style::new().fg(Color::DarkGray);
    let lines: Vec<Line<'static>> = SCHEMA
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
    // The selected entry's help goes in the *title*, not the list: the list
    // scrolls to follow the selection, so a help row at the end of it would be
    // unreadable for every entry above the last screenful. The title is drawn
    // by the block and never scrolls. "When it applies" comes before the help
    // text so that a narrow terminal cuts the help short, never the part that
    // says whether the change takes effect now.
    let entry = &SCHEMA[panel.selected.min(SCHEMA.len() - 1)];
    let when = match entry.applies {
        Applies::Now => "applies now",
        Applies::NextSession => "next session",
    };
    let status = if panel.status.is_empty() {
        format!(
            " ↑↓ select · Enter change · ⌫ reset · Tab file: {} · Esc close",
            panel.scope.name()
        )
    } else {
        format!(" {}", panel.status)
    };
    Some(Panel {
        title: truncate(
            &format!(
                " Settings → {} file · {when} · {} ",
                panel.scope.name(),
                entry.help
            ),
            width as usize,
        ),
        border: Color::Cyan,
        lines,
        selected: Some(panel.selected),
        pinned_from: None,
        footer: vec![Line::styled(
            truncate(&status, inner),
            Style::new().fg(Color::Gray),
        )],
    })
}

pub fn current(app: &App, width: u16) -> Option<Panel> {
    permission_panel(app, width)
        .or_else(|| picker_panel(app))
        .or_else(|| config_panel(app, width))
}

/// Total rows a panel needs, bordered, capped at `max`.
pub fn height(panel: &Panel, max: u16) -> u16 {
    (panel.lines.len() as u16 + panel.footer.len() as u16 + 2).min(max.max(3))
}

pub fn render(buffer: &mut Buffer, area: Rect, panel: Panel) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(panel.title)
        .border_style(Style::new().fg(panel.border).add_modifier(Modifier::BOLD));
    // The footer is carved out of the inner area before the scrolling body gets
    // it, so `footer.len()` rows stay put however the body scrolls.
    let inner = block.inner(area);
    let footer_rows = (panel.footer.len() as u16).min(inner.height);
    let body = Rect {
        height: inner.height - footer_rows,
        ..inner
    };
    block.render(area, buffer);
    match panel.selected {
        Some(selected) => {
            // Keep the highlighted row visible when the list is taller than the panel.
            let items: Vec<ListItem> = panel.lines.into_iter().map(ListItem::new).collect();
            let mut state = ListState::default().with_selected(Some(selected));
            StatefulWidget::render(
                List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
                body,
                buffer,
                &mut state,
            );
        }
        None => {
            let rows = body.height as usize;
            Paragraph::new(fit_lines(panel.lines, rows, panel.pinned_from)).render(body, buffer);
        }
    }
    for (row, line) in panel.footer.into_iter().enumerate() {
        let y = body.bottom() + row as u16;
        if y < inner.bottom() {
            buffer.set_line(inner.x, y, &line, inner.width);
        }
    }
}
