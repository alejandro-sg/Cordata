//! The conversation's blocks: the person's message as a right-aligned bubble, the assistant's markdown under a
//! `◆ CORDATA` label, consecutive tool calls as one bordered group with a row per call (status, verb, target, meta;
//! a click opens the output), reasoning, notices and errors. Every string goes through `cordata_view::shown`
//! (escapes stripped, credentials masked) before it is laid out.

use acp_core::ToolStatus;
use cordata_theme::tokens as t;
use cordata_view::app::{Message, ToolCard};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, FontWeight, IntoElement, ParentElement, SharedString, Styled, div, px, relative};
use gpui_component::text::{TextView, TextViewStyle};

use crate::hud;
use crate::theme::{amber, c, line, mono, teal, tk};
use crate::ui;

/// Rows of a tool's result shown when it is opened, before "… N more lines".
const RESULT_PREVIEW_LINES: usize = 40;
/// A line longer than this is cut when drawn: a 2 MB single-line diff must not be laid out whole.
const MAX_LINE_CHARS: usize = 400;

/// Where a tool row sits in its group (consecutive tool calls share one border).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupEdge {
    pub first: bool,
    pub last: bool,
}

pub fn block(message: &Message, ix: usize, edge: GroupEdge, expanded: bool, show_thoughts: bool, cx: &App) -> AnyElement {
    match message {
        Message::User(text) => user(text, cx),
        Message::Agent(text) => assistant(text, ix, cx),
        Message::Thought(text) => thought(text, show_thoughts, cx),
        Message::Tool(card) => tool(card, edge, expanded, cx),
        Message::Info(text) => notice(text, tk(cx).text_muted, cx),
        Message::Error(text) => notice(text, tk(cx).danger, cx),
        Message::Divider(label) => div().py(px(8.)).child(hud::divider(&shown(label), cx)).into_any_element(),
        Message::Diff(text) => div().py(px(6.)).child(ui::code_block(SharedString::from(cut_lines(text)), cx)).into_any_element(),
        Message::Welcome(_) => div().into_any_element(),
        Message::Help => notice("⌘K palette · / commands · @ files · ! shell · Esc stops a turn", tk(cx).text_muted, cx),
        Message::Laya(decision) => {
            let route = decision.route.as_deref().unwrap_or("—");
            notice(&format!("◆ laya → {route}  ({:.0}%)", decision.route_confidence * 100.0), tk(cx).info, cx)
        }
        Message::LayaOutcome(_) => div().into_any_element(),
        Message::Heal(info) => notice(&format!("self-heal · {}{}", info.phase, info.root_cause.as_deref().map(|c| format!(" — {c}")).unwrap_or_default()), tk(cx).amber, cx),
    }
}

/// Whether a message is part of a conversation (the empty state shows while there are none).
pub fn is_conversation(message: &Message) -> bool {
    matches!(message, Message::User(_) | Message::Agent(_) | Message::Tool(_) | Message::Thought(_) | Message::Diff(_))
}

fn user(text: &str, cx: &App) -> AnyElement {
    let tk = tk(cx);
    div()
        .w_full()
        .flex()
        .justify_end()
        .py(px(10.))
        .child(
            div()
                .max_w(relative(0.8))
                .px(px(14.))
                .py(px(10.))
                .rounded(px(t::RADIUS_POPOVER))
                .bg(line(cx, t::FILL_FAINT * 1.6))
                .border_1()
                .border_color(line(cx, t::LINE_DEFAULT))
                .text_size(px(t::TEXT_BODY))
                .line_height(relative(t::LINE_HEIGHT_BODY))
                .text_color(c(tk.text_body))
                .child(SharedString::from(shown(text))),
        )
        .into_any_element()
}

fn assistant(text: &str, ix: usize, cx: &App) -> AnyElement {
    let tk = tk(cx);
    div()
        .py(px(10.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(ui::diamond(cx))
                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_LABEL)).font_weight(FontWeight::MEDIUM).text_color(c(tk.amber)).child(SharedString::from(t::tracked_text("cordata", t::Tracking::Label, true)))),
        )
        .child(div().text_size(px(t::TEXT_BODY)).line_height(relative(t::LINE_HEIGHT_BODY)).text_color(c(tk.text_body)).child(TextView::markdown(("agent", ix), SharedString::from(shown(text))).selectable(true).style(TextViewStyle::default().inline_code(gpui::HighlightStyle { color: Some(c(tk.amber_soft)), background_color: Some(amber(cx, t::AMBER_FILL_MID)), ..Default::default() }))))
        .into_any_element()
}

fn thought(text: &str, expanded: bool, cx: &App) -> AnyElement {
    let text = shown(text);
    let first = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").to_owned();
    div()
        .py(px(4.))
        .text_size(px(t::TEXT_UI))
        .italic()
        .text_color(c(tk(cx).text_muted))
        .child(SharedString::from(if expanded { format!("∴ {text}") } else { format!("∴ {first}") }))
        .into_any_element()
}

fn notice(text: &str, ink: cordata_theme::Rgb, _cx: &App) -> AnyElement {
    div().py(px(4.)).text_size(px(t::TEXT_UI)).text_color(c(ink)).child(SharedString::from(shown(text))).into_any_element()
}

/// The verb a tool row shows (`read` · `edit` · `run` · …), fixed width so the targets line up.
pub fn verb(name: &str) -> &'static str {
    let name = name.rsplit("__").next().unwrap_or(name).to_ascii_lowercase();
    match name.as_str() {
        n if n.contains("read") || n == "view" || n == "cat" => "read",
        n if n.contains("edit") || n.contains("write") || n.contains("patch") || n.contains("replace") => "edit",
        n if n.contains("shell") || n.contains("bash") || n.contains("exec") || n.contains("run") || n.contains("command") => "run",
        n if n.contains("search") || n.contains("grep") || n.contains("glob") || n.contains("find") => "search",
        n if n.contains("fetch") || n.contains("http") || n.contains("web") || n.contains("browse") => "fetch",
        n if n.contains("todo") || n.contains("plan") || n.contains("task") => "plan",
        _ => "call",
    }
}

/// The thing a tool acted on: a path, a command, a URL — from its input; else the tool's own name.
pub fn target(card: &ToolCard) -> String {
    let input = card.raw_input.as_ref();
    let field = |k: &str| input.and_then(|i| i.get(k)).and_then(|v| v.as_str()).map(str::to_owned);
    field("path")
        .or_else(|| field("file_path"))
        .or_else(|| field("command"))
        .or_else(|| field("url"))
        .or_else(|| field("pattern"))
        .or_else(|| field("query"))
        .or_else(|| card.raw_input.as_ref().and_then(|i| cordata_view::view::summarize_input(i, false)))
        .unwrap_or_else(|| attribution(&card.name).1.to_owned())
}

/// What a row says on the right: `+12 −3`, a line count, a test result, the file count.
fn meta_of(card: &ToolCard) -> String {
    if !card.diffs.is_empty() {
        let (mut add, mut del) = (0usize, 0usize);
        for diff in &card.diffs {
            let old = diff.old_text.as_deref().map_or(0, |t| t.lines().count());
            let new = diff.new_text.lines().count();
            if new >= old {
                add += new - old;
            } else {
                del += old - new;
            }
        }
        return if card.diffs.len() > 1 { format!("{} files · +{add} −{del}", card.diffs.len()) } else { format!("+{add} −{del}") };
    }
    let content = &card.content;
    let lower = content.to_ascii_lowercase();
    if let Some(line) = content.lines().rev().find(|l| {
        let l = l.to_ascii_lowercase();
        (l.contains("passed") || l.contains("failed")) && l.len() < 80
    }) {
        return line.trim().trim_matches('=').trim().to_owned();
    }
    if lower.is_empty() {
        return String::new();
    }
    let n = content.lines().count();
    format!("{n} line{}", if n == 1 { "" } else { "s" })
}

fn tool(card: &ToolCard, edge: GroupEdge, expanded: bool, cx: &App) -> AnyElement {
    let tk = tk(cx);
    let (mark, ink) = match card.status {
        ToolStatus::Completed => ("✓", tk.teal),
        ToolStatus::Failed => ("✕", tk.danger),
        ToolStatus::InProgress => ("◆", tk.amber),
        ToolStatus::Pending => ("◆", tk.amber_dim),
    };
    let untrusted = card.untrusted || card.untrusted_source;
    let (server, _) = attribution(&card.name);
    let radius = px(t::RADIUS_CARD);
    let body = expanded.then(|| {
        let result = shown(&card.content);
        let lines: Vec<&str> = result.lines().collect();
        let shown_lines = lines.len().min(RESULT_PREVIEW_LINES);
        let more = lines.len().saturating_sub(shown_lines);
        let text = lines[..shown_lines].iter().map(|line| cut(line)).collect::<Vec<_>>().join("\n");
        let progress = (!card.progress.is_empty() && matches!(card.status, ToolStatus::InProgress) && !cordata_view::team::is_team_tool(&card.name)).then(|| shown(&card.progress));
        div()
            .px(px(12.))
            .pb(px(10.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .when_some(progress, |d, p| d.child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.teal)).child(SharedString::from(p))))
            .when(!text.is_empty(), |d| d.child(ui::code_block(SharedString::from(text), cx)))
            .when(more > 0, |d| d.child(ui::meta(SharedString::from(format!("… {more} more lines (Ctrl+E shows everything)")), cx)))
    });
    // A sub-agent call (`subagent`, the old teams): one row per agent — live while it runs, the reasoning tail and the
    // tools it called — shown without expanding the card, because watching it is the point.
    let team = cordata_view::team::is_team_tool(&card.name).then(|| {
        let mut rows = cordata_view::team::parse(&card.progress);
        if !matches!(card.status, ToolStatus::InProgress | ToolStatus::Pending) {
            cordata_view::team::settle(&mut rows);
        }
        rows
    });
    let team = team.filter(|rows| !rows.is_empty() && (expanded || matches!(card.status, ToolStatus::InProgress)));
    let team = team.map(|rows| team_rows(&rows, cx));
    div()
        .when(edge.first, |d| d.mt(px(6.)))
        .when(edge.last, |d| d.mb(px(6.)))
        .border_l_1()
        .border_r_1()
        .border_color(if untrusted { teal(cx, t::TEAL_BORDER) } else { line(cx, t::LINE_DEFAULT) })
        .when(edge.first, |d| d.border_t_1().rounded_tl(radius).rounded_tr(radius))
        .when(edge.last, |d| d.border_b_1().rounded_bl(radius).rounded_br(radius))
        .when(!edge.first, |d| d.border_t_1().border_color(line(cx, t::LINE_SUBTLE)))
        .bg(line(cx, t::FILL_FAINT))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .h(px(32.))
                .px(px(12.))
                .font_family(mono(cx))
                .text_size(px(t::TEXT_SMALL))
                .child(div().w(px(12.)).flex_shrink_0().text_color(c(ink)).child(mark))
                .child(div().w(px(48.)).flex_shrink_0().text_color(c(tk.text_muted)).child(verb(&card.name)))
                .when_some(server, |d, server| d.child(ui::badge(server, ui::Tone::Teal, cx)))
                .child(div().flex_1().min_w_0().truncate().text_color(c(tk.text_body)).child(SharedString::from(cut(&shown(&target(card))))))
                .when(untrusted, |d| d.child(ui::badge("untrusted", ui::Tone::Teal, cx)))
                .child(div().flex_shrink_0().whitespace_nowrap().text_color(c(if card.status == ToolStatus::Failed { tk.danger } else { tk.text_faint })).child(SharedString::from(meta_of(card))))
                .child(div().flex_shrink_0().text_color(c(tk.text_faint)).child(if expanded { "⌃" } else { "⌄" })),
        )
        .children(team)
        .children(body)
        .into_any_element()
}

/// The agents of a sub-agent call, one block each: ◆ running (amber) or ✓ done, its name, what it is reasoning about
/// now, and the tools it called.
fn team_rows(rows: &[cordata_view::team::TeamRow], cx: &App) -> AnyElement {
    use cordata_view::team::{NodeState, TeamRow};
    let tk = tk(cx);
    div()
        .px(px(12.))
        .pb(px(8.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .font_family(mono(cx))
        .text_size(px(t::TEXT_META))
        .children(rows.iter().map(|row| match row {
            TeamRow::Node(node) => {
                let running = node.state == NodeState::Running;
                let (mark, ink) = if running { ("◆", tk.amber) } else { ("✓", tk.teal) };
                let detail = if running {
                    if node.thinking.is_empty() { "working…".to_owned() } else { format!("💭 {}", cut(&node.thinking)) }
                } else if node.summary.is_empty() {
                    format!("done after {} tool call{}", node.tools.len(), if node.tools.len() == 1 { "" } else { "s" })
                } else {
                    cut(&node.summary)
                };
                let tools = (!node.tools.is_empty()).then(|| {
                    let shown: Vec<&str> = node.tools.iter().rev().take(6).rev().map(String::as_str).collect();
                    let more = node.tools.len().saturating_sub(shown.len());
                    let prefix = if more > 0 { format!("↳ +{more} · ") } else { "↳ ".to_owned() };
                    div().pl(px(22.)).text_color(c(tk.text_faint)).child(SharedString::from(format!("{prefix}{}", shown.join(" · "))))
                });
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .gap(px(10.))
                            .child(div().w(px(12.)).flex_shrink_0().text_color(c(ink)).child(mark))
                            .child(div().flex_shrink_0().text_color(c(tk.text_body)).child(SharedString::from(node.name.clone())))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .italic()
                                    .text_color(c(if running { tk.text_muted } else { tk.text_faint }))
                                    .child(SharedString::from(detail)),
                            ),
                    )
                    .children(tools)
                    .into_any_element()
            }
            TeamRow::Handoff { from, to, message } => div()
                .text_color(c(tk.teal))
                .child(SharedString::from(format!("→ {from} → {to}  {}", cut(message))))
                .into_any_element(),
        }))
        .into_any_element()
}

/// `mcp__github__create_issue` → (`github`, `create_issue`): which server a tool came from. A built-in has none.
fn attribution(name: &str) -> (Option<&str>, &str) {
    if let Some(rest) = name.strip_prefix("mcp__")
        && let Some((server, tool)) = rest.split_once("__")
    {
        return (Some(server), tool);
    }
    (None, name)
}

fn cut(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        line.to_owned()
    } else {
        let mut out: String = line.chars().take(MAX_LINE_CHARS).collect();
        out.push_str(" …");
        out
    }
}

fn cut_lines(text: &str) -> String {
    shown(text).lines().map(cut).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tools_are_attributed_to_their_server() {
        assert_eq!(attribution("mcp__github__create_issue"), (Some("github"), "create_issue"));
        assert_eq!(attribution("shell"), (None, "shell"));
        assert_eq!(attribution("mcp__odd"), (None, "mcp__odd"));
    }

    #[test]
    fn a_huge_line_is_cut_before_layout() {
        let line = "x".repeat(2_000_000);
        let out = cut(&line);
        assert!(out.chars().count() <= MAX_LINE_CHARS + 2);
    }

    #[test]
    fn verbs_read_at_a_glance() {
        assert_eq!(verb("file_read"), "read");
        assert_eq!(verb("edit_file"), "edit");
        assert_eq!(verb("shell"), "run");
        assert_eq!(verb("mcp__github__search_code"), "search");
        assert_eq!(verb("frobnicate"), "call");
    }
}
