//! The conversation's blocks (docs/app.md §4 "Center"): the person, the assistant's markdown, reasoning, tool cards,
//! notices, errors. One function per `Message` kind; every string goes through `cordata_view::shown` (escapes
//! stripped, credentials masked) before it is laid out. Teal marks what an agent or a tool produced; amber is ours.

use acp_core::ToolStatus;
use cordata_theme::{Role, tracked};
use cordata_view::app::{Message, ToolCard};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, IntoElement, ParentElement, SharedString, Styled, div, px};
use gpui_component::text::TextView;

use crate::hud;
use crate::theme::{color, tint};

/// Rows of a tool's result shown before "… N more lines".
const RESULT_PREVIEW_LINES: usize = 8;
/// A line longer than this is cut when drawn: a 2 MB single-line diff must not be laid out whole.
const MAX_LINE_CHARS: usize = 400;

pub fn block(message: &Message, ix: usize, expand_tools: bool, show_thoughts: bool, cx: &App) -> AnyElement {
    match message {
        Message::User(text) => user(text, cx),
        Message::Agent(text) => assistant(text, ix, cx),
        Message::Thought(text) => thought(text, show_thoughts, cx),
        Message::Tool(card) => tool(card, expand_tools, cx),
        Message::Info(text) => notice(text, Role::TextSecondary, cx),
        Message::Error(text) => notice(text, Role::Danger, cx),
        Message::Divider(label) => hud::divider(&shown(label), cx).into_any_element(),
        Message::Diff(text) => code(text, Some(Role::Agent), cx),
        Message::Welcome(info) => welcome(info, cx),
        Message::Help => notice("⌘K commands · / slash commands · @ files · ! shell · Esc stops a turn", Role::TextSecondary, cx),
        Message::Laya(decision) => {
            let route = decision.route.as_deref().unwrap_or("—");
            notice(&format!("◆ laya → {route}  ({:.0}%)", decision.route_confidence * 100.0), Role::Info, cx)
        }
        Message::LayaOutcome(_) => div().into_any_element(),
        Message::Heal(info) => notice(&format!("self-heal · {}{}", info.phase, info.root_cause.as_deref().map(|c| format!(" — {c}")).unwrap_or_default()), Role::Attention, cx),
    }
}

fn user(text: &str, cx: &App) -> AnyElement {
    div()
        .flex()
        .gap(px(10.))
        .py(px(6.))
        .child(div().w(px(2.)).flex_shrink_0().bg(color(cx, Role::Accent)))
        .child(div().flex_1().text_color(color(cx, Role::Text)).child(SharedString::from(shown(text))))
        .into_any_element()
}

fn assistant(text: &str, ix: usize, cx: &App) -> AnyElement {
    div()
        .py(px(4.))
        .text_color(color(cx, Role::Text))
        .child(TextView::markdown(("agent", ix), SharedString::from(shown(text))).selectable(true))
        .into_any_element()
}

fn thought(text: &str, expanded: bool, cx: &App) -> AnyElement {
    let text = shown(text);
    let first = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").to_owned();
    div()
        .py(px(2.))
        .text_sm()
        .italic()
        .text_color(color(cx, Role::TextDim))
        .child(SharedString::from(if expanded { format!("∴ {text}") } else { format!("∴ {first}") }))
        .into_any_element()
}

fn notice(text: &str, role: Role, cx: &App) -> AnyElement {
    div().py(px(3.)).text_sm().text_color(color(cx, role)).child(SharedString::from(shown(text))).into_any_element()
}

fn code(text: &str, edge: Option<Role>, cx: &App) -> AnyElement {
    let body: String = shown(text).lines().map(cut).collect::<Vec<_>>().join("\n");
    div()
        .my(px(4.))
        .p(px(8.))
        .rounded(px(3.))
        .bg(color(cx, Role::Surface))
        .border_l_2()
        .border_color(color(cx, edge.unwrap_or(Role::Hairline)))
        .font_family(cordata_theme::fonts::MONO)
        .text_xs()
        .text_color(color(cx, Role::TextSecondary))
        .child(SharedString::from(body))
        .into_any_element()
}

fn welcome(info: &acp_core::SessionInfo, cx: &App) -> AnyElement {
    div()
        .py(px(8.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(hud::bracket_title("Cordata", cx))
        .child(
            div()
                .text_xs()
                .text_color(color(cx, Role::TextDim))
                .child(SharedString::from(format!("{} · {} · session {}", shown(&info.cwd), info.transport, info.session_id.chars().take(8).collect::<String>()))),
        )
        .into_any_element()
}

fn status(status: ToolStatus) -> (&'static str, Role) {
    match status {
        ToolStatus::Pending => ("○", Role::TextDim),
        ToolStatus::InProgress => ("◐", Role::Accent),
        ToolStatus::Completed => ("●", Role::Ok),
        ToolStatus::Failed => ("✕", Role::Danger),
    }
}

/// A tool card: the attribution header (what ran, from which tool or MCP server), what it was asked to do, and what
/// came back. Output the backend marked untrusted is edged in teal and labelled — it is data, not instructions.
fn tool(card: &ToolCard, expand: bool, cx: &App) -> AnyElement {
    let (glyph, role) = status(card.status);
    let summary = card.raw_input.as_ref().and_then(|input| cordata_view::view::summarize_input(input, matches!(card.name.as_str(), "local_swarm" | "local_graph")));
    let (server, name) = attribution(&card.name);
    let result = shown(&card.content);
    let lines: Vec<&str> = result.lines().collect();
    let shown_lines = if expand { lines.len() } else { lines.len().min(RESULT_PREVIEW_LINES) };
    let more = lines.len().saturating_sub(shown_lines);
    let untrusted = card.untrusted || card.untrusted_source;
    let edge = if untrusted { Role::Agent } else { Role::Hairline };

    div()
        .my(px(4.))
        .rounded(px(3.))
        .border_1()
        .border_color(tint(cx, edge, if untrusted { 0.55 } else { 1.0 }))
        .bg(color(cx, Role::Surface))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .py(px(5.))
                .border_b_1()
                .border_color(color(cx, Role::Hairline))
                .child(div().text_color(color(cx, role)).child(glyph))
                .when_some(server, |this, server| this.child(div().text_xs().text_color(color(cx, Role::Agent)).child(SharedString::from(tracked(server)))))
                .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).text_color(color(cx, Role::Text)).child(SharedString::from(name.to_owned())))
                .when(untrusted, |this| this.child(hud::pill("untrusted", Role::Agent, cx)))
                .when(!card.diffs.is_empty(), |this| this.child(div().text_xs().text_color(color(cx, Role::Attention)).child(SharedString::from(format!("{} file{}", card.diffs.len(), if card.diffs.len() == 1 { "" } else { "s" })))))
                .child(div().flex_1())
                .when_some(card.p_ok, |this, p| this.child(div().text_xs().text_color(color(cx, Role::TextDim)).child(SharedString::from(format!("p(ok) {:.0}%", p * 100.0))))),
        )
        .when_some(summary, |this, summary| {
            this.child(div().px(px(10.)).py(px(4.)).font_family(cordata_theme::fonts::MONO).text_xs().text_color(color(cx, Role::TextSecondary)).child(SharedString::from(cut(&summary))))
        })
        .when(!card.progress.is_empty() && matches!(card.status, ToolStatus::InProgress), |this| {
            let tail: Vec<&str> = card.progress.lines().rev().take(3).collect();
            this.child(div().px(px(10.)).pb(px(4.)).text_xs().text_color(color(cx, Role::Agent)).child(SharedString::from(shown(&tail.into_iter().rev().collect::<Vec<_>>().join("\n")))))
        })
        .when(shown_lines > 0, |this| {
            let body = lines[..shown_lines].iter().map(|line| cut(line)).collect::<Vec<_>>().join("\n");
            this.child(
                div()
                    .px(px(10.))
                    .py(px(6.))
                    .border_t_1()
                    .border_color(color(cx, Role::Hairline))
                    .font_family(cordata_theme::fonts::MONO)
                    .text_xs()
                    .text_color(color(cx, Role::TextSecondary))
                    .child(SharedString::from(body))
                    .when(more > 0, |this| this.child(div().pt(px(2.)).text_color(color(cx, Role::TextDim)).child(SharedString::from(format!("… {more} more lines"))))),
            )
        })
        .into_any_element()
}

/// `mcp__github__create_issue` → (`github`, `create_issue`): which server a tool came from, shown on its card (the
/// Gemini "tool attribution header"). A built-in tool has no server.
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
}
