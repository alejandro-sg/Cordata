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

use super::text::{truncate, wrap};
use crate::app::{App, sanitize};

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

fn permission_panel(app: &App, width: u16) -> Option<Panel> {
    let pending = app.pending.front()?;
    let info = &pending.info;
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

pub fn current(app: &App, width: u16) -> Option<Panel> {
    permission_panel(app, width)
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
