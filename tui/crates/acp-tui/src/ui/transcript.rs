//! The conversation as terminal output: which messages are *final* (written once
//! into the terminal's scrollback) and which are still *live* (redrawn in the
//! region at the bottom until they settle).
//!
//! Scrollback is immutable, so a message may only be written once it can no
//! longer change:
//!
//! * a user line, notice, divider, error, help block, Laya line — immediately;
//! * a tool card — once it has completed or failed (while it runs it is live,
//!   spinner and sub-agent rows included);
//! * reasoning — once something else follows it (while it streams it is a live
//!   two-line tail), then as a one-line summary;
//! * the reply — *progressively*: every complete line of streamed text is written
//!   as soon as it ends, so a long reply flows into scrollback as it arrives
//!   instead of piling up in the live region; only the unfinished last line is live.
//!
//! The same rules decide spacing, so a message looks the same whether it is
//! committed or still live.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::{heal, help, laya, markdown, text::wrap, tool_card};
use crate::app::{App, Message};
use acp_core::ToolStatus;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    User,
    Laya,
    LayaOutcome,
    Agent,
    Thought,
    Tool,
    Divider,
    Info,
    Error,
    Help,
    Welcome,
    Heal,
}

impl Message {
    pub fn kind(&self) -> Kind {
        match self {
            Message::User(_) => Kind::User,
            Message::Laya(_) => Kind::Laya,
            Message::LayaOutcome(_) => Kind::LayaOutcome,
            Message::Agent(_) => Kind::Agent,
            Message::Thought(_) => Kind::Thought,
            Message::Tool(_) => Kind::Tool,
            Message::Divider(_) => Kind::Divider,
            Message::Info(_) => Kind::Info,
            Message::Error(_) => Kind::Error,
            Message::Help => Kind::Help,
            Message::Heal(_) => Kind::Heal,
            Message::Welcome(_) => Kind::Welcome,
        }
    }
}

/// A blank line goes between messages — except where two belong together.
fn blank_between(previous: Option<Kind>, next: Kind) -> bool {
    !matches!(
        (previous, next),
        (None, _)
            | (Some(Kind::Tool), Kind::Tool)
            | (Some(Kind::User), Kind::Laya)
            | (Some(Kind::Laya), Kind::LayaOutcome)
            | (Some(Kind::Heal), Kind::Heal)
    )
}

pub fn prefixed(
    text: &str,
    width: usize,
    first: &str,
    rest: &str,
    style: Style,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    let body_width = width.saturating_sub(first.chars().count()).max(1);
    wrap(text, body_width)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            Line::from(vec![
                Span::styled(
                    if i == 0 {
                        first.to_owned()
                    } else {
                        rest.to_owned()
                    },
                    prefix_style,
                ),
                Span::styled(row, style),
            ])
        })
        .collect()
}

/// Reasoning: a live two-line tail while it streams; once settled, one summary
/// line — or the whole text if reasoning is expanded (Ctrl+R / `/thinking`).
fn thought_lines(text: &str, width: usize, expanded: bool, streaming: bool) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let italic = dim.add_modifier(Modifier::ITALIC);
    if expanded && !streaming {
        return prefixed(text, width, "· ", "  ", italic, dim);
    }
    if streaming {
        let rows = wrap(
            &text.split_whitespace().collect::<Vec<_>>().join(" "),
            width.saturating_sub(2).max(1),
        );
        let tail = &rows[rows.len().saturating_sub(2)..];
        let mut lines = vec![Line::styled("· thinking…", dim)];
        lines.extend(
            tail.iter()
                .map(|row| Line::styled(format!("  {row}"), italic)),
        );
        return lines;
    }
    let words = text.split_whitespace().count();
    vec![Line::styled(format!("· reasoned ({words} words)"), dim)]
}

/// One message as lines, exactly as it would be written (no spacing).
fn message_lines(app: &App, index: usize, width: u16) -> Vec<Line<'static>> {
    let w = width as usize;
    let message = &app.messages[index];
    let streaming = app.turn_active && index + 1 == app.messages.len();
    match message {
        Message::User(text) => prefixed(
            text,
            w,
            "› ",
            "  ",
            Style::new().add_modifier(Modifier::BOLD),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Message::Agent(text) => markdown::render(text, w),
        Message::Thought(text) => thought_lines(text, w, app.thoughts_expanded, streaming),
        Message::Tool(card) => tool_card::render_in(
            card,
            width,
            app.expand_tools,
            app.awaiting_approval(&card.id),
            app.spinner,
            app.session.as_ref().map_or("", |s| s.cwd.as_str()),
        ),
        Message::Divider(text) => {
            let label = format!(" {text} ");
            let fill = w.saturating_sub(label.chars().count());
            let (left, right) = (fill / 2, fill - fill / 2);
            vec![Line::styled(
                format!("{}{label}{}", "─".repeat(left), "─".repeat(right)),
                Style::new().fg(Color::DarkGray),
            )]
        }
        Message::Info(text) => prefixed(
            text,
            w,
            "  ✦ ",
            "    ",
            Style::new().fg(Color::Gray),
            Style::new().fg(Color::Cyan),
        ),
        Message::Error(text) => prefixed(
            text,
            w,
            "✗ ",
            "  ",
            Style::new().fg(Color::Red),
            Style::new().fg(Color::Red),
        ),
        Message::Laya(decision) if app.show_laya => laya::decision_lines(decision, width),
        Message::LayaOutcome(outcome) if app.show_laya => laya::outcome_lines(outcome, width),
        Message::Laya(_) | Message::LayaOutcome(_) => Vec::new(),
        Message::Help => help::lines(width),
        Message::Heal(info) => heal::lines(info, width, app.expand_tools),
        Message::Welcome(info) => crate::ui::welcome_lines(info, width),
    }
}

/// Can this message no longer change?
fn is_final(app: &App, index: usize) -> bool {
    let later_exists = index + 1 < app.messages.len();
    match &app.messages[index] {
        Message::Agent(_) | Message::Thought(_) => later_exists || !app.turn_active,
        Message::Tool(card) => {
            // A card that never got a result (a cancelled turn) must still settle.
            !matches!(card.status, ToolStatus::Pending | ToolStatus::InProgress) || !app.turn_active
        }
        _ => true,
    }
}

/// How many wrapped rows of a streaming reply are certain: those of its
/// *complete* lines (everything up to the last newline). Rendering the complete
/// prefix on its own yields exactly the leading rows of the full render, because
/// each logical line is wrapped independently and fence state only flows forward.
fn stable_rows(text: &str, width: u16) -> usize {
    match text.rfind('\n') {
        Some(end) => markdown::render(&text[..end], width as usize).len(),
        None => 0,
    }
}

/// Everything newly final since the last call, as lines to write into the
/// scrollback. Advances the commit cursor.
pub fn take_commits(app: &mut App, width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    while app.committed < app.messages.len() {
        let index = app.committed;
        let kind = app.messages[index].kind();
        let lines = message_lines(app, index, width);
        if is_final(app, index) {
            let skip = app.committed_rows.min(lines.len());
            let rest = &lines[skip..];
            if !rest.is_empty() {
                if skip == 0 && blank_between(app.last_committed, kind) {
                    out.push(Line::raw(""));
                }
                out.extend(rest.iter().cloned());
                app.last_committed = Some(kind);
            }
            app.committed += 1;
            app.committed_rows = 0;
            continue;
        }
        // A reply still streaming: write the lines that are complete, keep the tail live.
        if let Message::Agent(text) = &app.messages[index] {
            let stable = stable_rows(text, width).min(lines.len());
            if stable > app.committed_rows {
                if app.committed_rows == 0 && blank_between(app.last_committed, kind) {
                    out.push(Line::raw(""));
                }
                out.extend(lines[app.committed_rows..stable].iter().cloned());
                app.committed_rows = stable;
                app.last_committed = Some(kind);
            }
        }
        break;
    }
    out
}

/// The not-yet-final remainder, redrawn every frame.
pub fn live_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut previous = app.last_committed;
    for index in app.committed..app.messages.len() {
        let kind = app.messages[index].kind();
        let mut lines = message_lines(app, index, width);
        let mut starting = true;
        if index == app.committed && app.committed_rows > 0 {
            lines.drain(..app.committed_rows.min(lines.len()));
            starting = false; // continuing a reply already begun in scrollback
        }
        if lines.is_empty() {
            continue;
        }
        if starting && blank_between(previous, kind) {
            out.push(Line::raw(""));
        }
        out.extend(lines);
        previous = Some(kind);
    }
    out
}
