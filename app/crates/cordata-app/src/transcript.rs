//! The conversation's blocks: the person's message as a right-aligned bubble (with clean attachment pills), the
//! assistant's markdown under a `◆ CORDATA` header, collapsible thought disclosures, consecutive tool calls as one
//! bordered group with inline unified diffs and subagent progress, and routing/self-heal notices. Every string goes
//! through `cordata_view::shown` (escapes stripped, credentials masked) before it is laid out.

use acp_core::{FileDiff, ToolStatus};
use cordata_theme::tokens as t;
use cordata_view::app::{Message, ToolCard};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, FontWeight, InteractiveElement as _, IntoElement, ParentElement, SharedString, Styled, div, px, relative};
use gpui_component::text::{TextView, TextViewStyle};

use crate::hud;
use crate::theme::{amber, c, ca, line, mono, teal, tk};
use crate::ui::{self, Tone};

/// Rows of a tool's result shown when it is opened, before "… N more lines".
const RESULT_PREVIEW_LINES: usize = 40;
/// Maximum diff lines rendered inline per edited file before "… N more changed lines".
const DIFF_PREVIEW_LINES: usize = 36;
/// A line longer than this is cut when drawn: a 2 MB single-line diff must not be laid out whole.
const MAX_LINE_CHARS: usize = 400;

/// Where a tool row sits in its group (consecutive tool calls share one border).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupEdge {
    pub first: bool,
    pub last: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLineKind {
    Header,
    Add,
    Del,
    Context,
}

pub fn block(message: &Message, ix: usize, edge: GroupEdge, expanded: bool, show_thoughts: bool, cx: &App) -> AnyElement {
    match message {
        Message::User(text) => user(text, cx),
        Message::Agent(text) => assistant(text, ix, cx),
        Message::Thought(text) => thought(text, show_thoughts, cx),
        Message::Tool(card) => tool(card, edge, expanded, cx),
        Message::Info(text) => notice(text, tk(cx).text_muted, cx),
        Message::Error(text) => error_banner(text, cx),
        Message::Divider(label) => div().py(px(8.)).child(hud::divider(&shown(label), cx)).into_any_element(),
        Message::Diff(text) => div().py(px(6.)).child(unified_diff_block(text, cx)).into_any_element(),
        Message::Welcome(_) => div().into_any_element(),
        Message::Help => notice("⌘K palette · ⌘B sidebar · ⌘\\ inspector · / commands · @ files · ! shell · ⇧Tab mode · Esc stops", tk(cx).text_muted, cx),
        Message::Laya(decision) => {
            let route = decision.route.as_deref().unwrap_or("direct");
            pill_notice("⚡", &format!("Auto-routed → {route} ({:.0}% confidence)", decision.route_confidence * 100.0), tk(cx).info, cx)
        }
        Message::LayaOutcome(_) => div().into_any_element(),
        Message::Heal(info) => {
            let detail = info.root_cause.as_deref().map(|cause| format!(" — {cause}")).unwrap_or_default();
            pill_notice("🛡", &format!("Self-heal · {}{detail}", info.phase), tk(cx).amber, cx)
        }
    }
}

/// Whether a message is part of a conversation (the empty state shows while there are none).
pub fn is_conversation(message: &Message) -> bool {
    matches!(message, Message::User(_) | Message::Agent(_) | Message::Tool(_) | Message::Thought(_) | Message::Diff(_))
}

/// Splits trailing or inline `[attached: a.rs, b.png]` tags out of a user prompt for clean pill rendering.
pub fn split_attachments(text: &str) -> (String, Vec<String>) {
    let mut attachments = Vec::new();
    let mut kept_lines = Vec::new();
    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if let Some(inner) = trimmed.strip_prefix("[attached:").and_then(|s| s.strip_suffix(']')) {
            for item in inner.split(',') {
                let item = item.trim();
                if !item.is_empty() {
                    attachments.push(item.to_owned());
                }
            }
        } else {
            kept_lines.push(raw_line);
        }
    }
    let mut body = kept_lines.join("\n").trim().to_owned();
    // Also handle inline suffix `... [attached: foo.diff]` on the same line.
    while let Some(start) = body.rfind("[attached:") {
        if body.ends_with(']') {
            let inner = &body[start + "[attached:".len()..body.len() - 1];
            for item in inner.split(',') {
                let item = item.trim();
                if !item.is_empty() {
                    attachments.push(item.to_owned());
                }
            }
            body = body[..start].trim_end().to_owned();
        } else {
            break;
        }
    }
    (body, attachments)
}

fn user(text: &str, cx: &App) -> AnyElement {
    let tk = tk(cx);
    let clean = shown(text);
    let (body, attachments) = split_attachments(&clean);
    let pills: Vec<AnyElement> = attachments
        .into_iter()
        .map(|name| {
            div()
                .flex()
                .items_center()
                .gap(px(5.))
                .px(px(7.))
                .py(px(2.))
                .rounded(px(t::RADIUS_BADGE))
                .bg(amber(cx, t::AMBER_FILL))
                .border_1()
                .border_color(amber(cx, t::AMBER_BORDER))
                .font_family(mono(cx))
                .text_size(px(t::TEXT_META))
                .text_color(c(tk.amber_soft))
                .child("▤")
                .child(SharedString::from(name))
                .into_any_element()
        })
        .collect();

    div()
        .w_full()
        .flex()
        .justify_end()
        .py(px(10.))
        .child(
            div()
                .max_w(relative(0.82))
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(14.))
                .py(px(10.))
                .rounded(px(t::RADIUS_POPOVER))
                .bg(line(cx, t::HOVER))
                .border_1()
                .border_color(line(cx, t::LINE_CONTROL))
                .when(!pills.is_empty(), |card| card.child(div().flex().flex_wrap().gap(px(6.)).children(pills)))
                .when(!body.is_empty(), |card| {
                    card.child(
                        div()
                            .text_size(px(t::TEXT_BODY))
                            .line_height(relative(t::LINE_HEIGHT_BODY))
                            .text_color(c(tk.text_primary))
                            .child(SharedString::from(body)),
                    )
                }),
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
                .child(
                    div()
                        .font_family(mono(cx))
                        .text_size(px(t::TEXT_LABEL))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c(tk.amber))
                        .child(SharedString::from(t::tracked_text("cordata", t::Tracking::Label, true))),
                ),
        )
        .child(
            div()
                .text_size(px(t::TEXT_BODY))
                .line_height(relative(t::LINE_HEIGHT_BODY))
                .text_color(c(tk.text_body))
                .child(
                    TextView::markdown(("agent", ix), SharedString::from(shown(text)))
                        .selectable(true)
                        .style(TextViewStyle::default().inline_code(gpui::HighlightStyle {
                            color: Some(c(tk.amber_soft)),
                            background_color: Some(amber(cx, t::AMBER_FILL_MID)),
                            ..Default::default()
                        })),
                ),
        )
        .into_any_element()
}

fn thought(text: &str, expanded: bool, cx: &App) -> AnyElement {
    let tk = tk(cx);
    let text = shown(text);
    let line_count = text.lines().filter(|l| !l.trim().is_empty()).count().max(1);
    let first = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim().to_owned();
    let hover = line(cx, t::HOVER);
    div()
        .py(px(4.))
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(px(t::RADIUS_CARD))
                .bg(line(cx, t::FILL_FAINT))
                .border_1()
                .border_color(line(cx, t::LINE_SUBTLE))
                .hover(move |s| s.bg(hover))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .h(px(28.))
                        .px(px(10.))
                        .text_size(px(t::TEXT_SMALL))
                        .child(div().text_color(c(tk.amber_soft)).child("∴"))
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_secondary)).child("Thought process"))
                        .when(!expanded && !first.is_empty(), |row| {
                            row.child(div().text_color(c(tk.text_faint)).child("·"))
                                .child(div().flex_1().min_w_0().truncate().italic().text_color(c(tk.text_muted)).child(SharedString::from(cut(&first))))
                        })
                        .when(expanded || first.is_empty(), |row| {
                            row.child(
                                div()
                                    .flex_1()
                                    .font_family(mono(cx))
                                    .text_size(px(t::TEXT_META))
                                    .text_color(c(tk.text_faint))
                                    .child(SharedString::from(format!("· {line_count} line{}", if line_count == 1 { "" } else { "s" }))),
                            )
                        })
                        .child(div().flex_shrink_0().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(if expanded { "⌃" } else { "⌄" })),
                )
                .when(expanded, |card| {
                    card.child(
                        div()
                            .mx(px(10.))
                            .mb(px(8.))
                            .pl(px(10.))
                            .border_l_2()
                            .border_color(amber(cx, t::AMBER_BORDER))
                            .text_size(px(t::TEXT_UI))
                            .line_height(relative(1.5))
                            .text_color(c(tk.text_secondary))
                            .child(SharedString::from(cut_lines(&text))),
                    )
                }),
        )
        .into_any_element()
}

fn notice(text: &str, ink: cordata_theme::Rgb, _cx: &App) -> AnyElement {
    div().py(px(4.)).text_size(px(t::TEXT_UI)).text_color(c(ink)).child(SharedString::from(shown(text))).into_any_element()
}

fn pill_notice(icon: &'static str, text: &str, ink: cordata_theme::Rgb, cx: &App) -> AnyElement {
    div()
        .py(px(4.))
        .flex()
        .items_center()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(10.))
                .py(px(4.))
                .rounded(px(t::RADIUS_CARD))
                .bg(line(cx, t::FILL_FAINT))
                .border_1()
                .border_color(line(cx, t::LINE_SUBTLE))
                .font_family(mono(cx))
                .text_size(px(t::TEXT_META))
                .text_color(c(ink))
                .child(icon)
                .child(SharedString::from(shown(text))),
        )
        .into_any_element()
}

fn error_banner(text: &str, cx: &App) -> AnyElement {
    let tk = tk(cx);
    div()
        .py(px(6.))
        .child(
            div()
                .flex()
                .items_start()
                .gap(px(8.))
                .px(px(12.))
                .py(px(8.))
                .rounded(px(t::RADIUS_CARD))
                .bg(ca(tk.danger, 0.08))
                .border_1()
                .border_color(ca(tk.danger, 0.28))
                .text_size(px(t::TEXT_UI))
                .line_height(relative(1.45))
                .text_color(c(tk.danger))
                .child(div().flex_shrink_0().font_weight(FontWeight::BOLD).child("✕"))
                .child(div().flex_1().min_w_0().child(SharedString::from(shown(text)))),
        )
        .into_any_element()
}

/// The canonical verb token a tool row maps to (`read` · `edit` · `run` · …).
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

/// Human-friendly Title Case action label shown in tool call rows.
pub fn verb_display(name: &str) -> &'static str {
    if cordata_view::team::is_team_tool(name) {
        return "Agent";
    }
    match verb(name) {
        "read" => "Read",
        "edit" => "Edited",
        "run" => "Ran",
        "search" => "Searched",
        "fetch" => "Fetched",
        "plan" => "Planned",
        _ => "Called",
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
        .or_else(|| card.raw_input.as_ref().and_then(|i| cordata_view::view::summarize_input(i, cordata_view::team::is_team_tool(&card.name))))
        .unwrap_or_else(|| attribution(&card.name).1.to_owned())
}

fn diff_counts(diffs: &[FileDiff]) -> (usize, usize) {
    let (mut add, mut del) = (0usize, 0usize);
    for diff in diffs {
        let old_lines: Vec<&str> = diff.old_text.as_deref().unwrap_or("").lines().collect();
        let new_lines: Vec<&str> = diff.new_text.lines().collect();
        let mut prefix = 0usize;
        while prefix < old_lines.len() && prefix < new_lines.len() && old_lines[prefix] == new_lines[prefix] {
            prefix += 1;
        }
        let mut old_end = old_lines.len();
        let mut new_end = new_lines.len();
        while old_end > prefix && new_end > prefix && old_lines[old_end - 1] == new_lines[new_end - 1] {
            old_end -= 1;
            new_end -= 1;
        }
        del += old_end - prefix;
        add += new_end - prefix;
    }
    (add, del)
}

/// Computes compact changed-hunk preview lines (`(DiffLineKind, String)`) for a `FileDiff` in O(N) time.
pub fn preview_file_diff(diff: &FileDiff, max_lines: usize) -> (Vec<(DiffLineKind, String)>, usize) {
    let old_lines: Vec<&str> = diff.old_text.as_deref().unwrap_or("").lines().collect();
    let new_lines: Vec<&str> = diff.new_text.lines().collect();
    let mut prefix = 0usize;
    while prefix < old_lines.len() && prefix < new_lines.len() && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }
    let mut old_end = old_lines.len();
    let mut new_end = new_lines.len();
    while old_end > prefix && new_end > prefix && old_lines[old_end - 1] == new_lines[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }
    let ctx_before = prefix.saturating_sub(2);
    let ctx_after_end = (new_end + 2).min(new_lines.len());
    let mut all = Vec::new();
    for line in &old_lines[ctx_before..prefix] {
        all.push((DiffLineKind::Context, format!("  {}", cut(&shown(line)))));
    }
    for line in &old_lines[prefix..old_end] {
        all.push((DiffLineKind::Del, format!("− {}", cut(&shown(line)))));
    }
    for line in &new_lines[prefix..new_end] {
        all.push((DiffLineKind::Add, format!("+ {}", cut(&shown(line)))));
    }
    for line in &new_lines[new_end..ctx_after_end] {
        all.push((DiffLineKind::Context, format!("  {}", cut(&shown(line)))));
    }
    let total = all.len();
    let shown_count = total.min(max_lines);
    all.truncate(shown_count);
    (all, total.saturating_sub(shown_count))
}

fn render_diff_rows(rows: Vec<(DiffLineKind, String)>, more: usize, header: Option<&str>, cx: &App) -> AnyElement {
    let tk = tk(cx);
    let elements = rows.into_iter().map(|(kind, text)| {
        let (bg, ink) = match kind {
            DiffLineKind::Header => (Some(amber(cx, t::AMBER_FILL)), tk.amber),
            DiffLineKind::Add => (Some(teal(cx, 0.10)), tk.teal),
            DiffLineKind::Del => (Some(ca(tk.danger, 0.10)), tk.danger),
            DiffLineKind::Context => (None, tk.text_secondary),
        };
        div()
            .w_full()
            .px(px(10.))
            .py(px(1.))
            .when_some(bg, |d, bg| d.bg(bg))
            .font_family(mono(cx))
            .text_size(px(t::TEXT_SMALL))
            .line_height(relative(1.5))
            .text_color(c(ink))
            .child(SharedString::from(text))
            .into_any_element()
    });
    div()
        .w_full()
        .overflow_hidden()
        .rounded(px(t::RADIUS_CARD))
        .bg(c(tk.bg_window))
        .border_1()
        .border_color(line(cx, t::LINE_DEFAULT))
        .when_some(header, |card, path| {
            card.child(
                div()
                    .px(px(10.))
                    .py(px(5.))
                    .border_b_1()
                    .border_color(line(cx, t::LINE_SUBTLE))
                    .font_family(mono(cx))
                    .text_size(px(t::TEXT_META))
                    .text_color(c(tk.text_secondary))
                    .child(SharedString::from(shown(path))),
            )
        })
        .child(div().py(px(4.)).flex().flex_col().children(elements))
        .when(more > 0, |card| {
            card.child(
                div()
                    .px(px(10.))
                    .py(px(4.))
                    .border_t_1()
                    .border_color(line(cx, t::LINE_SUBTLE))
                    .child(ui::meta(SharedString::from(format!("… {more} more diff lines (inspect full file in Changes ⌘⌥4)")), cx)),
            )
        })
        .into_any_element()
}

fn unified_diff_block(raw: &str, cx: &App) -> AnyElement {
    let cleaned = shown(raw);
    let all_lines: Vec<&str> = cleaned.lines().collect();
    let limit = 80usize;
    let shown_count = all_lines.len().min(limit);
    let more = all_lines.len().saturating_sub(shown_count);
    let rows: Vec<(DiffLineKind, String)> = all_lines[..shown_count]
        .iter()
        .map(|line| {
            let kind = if line.starts_with("@@") || line.starts_with("diff ") || line.starts_with("---") || line.starts_with("+++") {
                DiffLineKind::Header
            } else if line.starts_with('+') {
                DiffLineKind::Add
            } else if line.starts_with('-') {
                DiffLineKind::Del
            } else {
                DiffLineKind::Context
            };
            (kind, cut(line))
        })
        .collect();
    render_diff_rows(rows, more, None, cx)
}

/// What a row says on the right when there are no structured diffs: a line count or test summary.
fn meta_of(card: &ToolCard) -> String {
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
    let row_hover = line(cx, t::HOVER);

    let right_meta: AnyElement = if !card.diffs.is_empty() {
        let (add, del) = diff_counts(&card.diffs);
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.))
            .when(card.diffs.len() > 1, |r| r.child(div().text_color(c(tk.text_faint)).child(SharedString::from(format!("{} files", card.diffs.len())))))
            .when(add > 0 || del == 0, |r| r.child(div().text_color(c(tk.teal)).child(SharedString::from(format!("+{add}")))))
            .when(del > 0, |r| r.child(div().text_color(c(tk.danger)).child(SharedString::from(format!("−{del}")))))
            .into_any_element()
    } else {
        div()
            .flex_shrink_0()
            .whitespace_nowrap()
            .text_color(c(if card.status == ToolStatus::Failed { tk.danger } else { tk.text_faint }))
            .child(SharedString::from(meta_of(card)))
            .into_any_element()
    };

    let body = expanded.then(|| {
        let diff_blocks: Vec<AnyElement> = card
            .diffs
            .iter()
            .take(4)
            .map(|diff| {
                let (rows, more) = preview_file_diff(diff, DIFF_PREVIEW_LINES);
                let header = (card.diffs.len() > 1).then_some(diff.path.as_str());
                render_diff_rows(rows, more, header, cx)
            })
            .collect();
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
            .gap(px(6.))
            .when_some(progress, |d, p| d.child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.teal)).child(SharedString::from(p))))
            .children(diff_blocks)
            .when(!text.is_empty() && (card.diffs.is_empty() || card.status == ToolStatus::Failed), |d| d.child(ui::code_block(SharedString::from(text), cx)))
            .when(more > 0 && card.diffs.is_empty(), |d| d.child(ui::meta(SharedString::from(format!("… {more} more lines (Ctrl+E expands all)")), cx)))
    });

    // A sub-agent call (`subagent`, teams): live rows shown without expanding the card.
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
                .hover(move |s| s.bg(row_hover))
                .font_family(mono(cx))
                .text_size(px(t::TEXT_SMALL))
                .child(div().w(px(12.)).flex_shrink_0().text_color(c(ink)).child(mark))
                .child(div().w(px(62.)).flex_shrink_0().font_weight(FontWeight::MEDIUM).text_color(c(tk.text_secondary)).child(verb_display(&card.name)))
                .when_some(server, |d, server| d.child(ui::badge(server, Tone::Teal, cx)))
                .child(div().flex_1().min_w_0().truncate().text_color(c(tk.text_body)).child(SharedString::from(cut(&shown(&target(card))))))
                .when(untrusted, |d| d.child(ui::badge("untrusted", Tone::Teal, cx)))
                .child(right_meta)
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
                            .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).text_color(c(tk.text_body)).child(SharedString::from(node.name.clone())))
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
        assert_eq!(verb_display("edit_file"), "Edited");
        assert_eq!(verb_display("shell"), "Ran");
    }

    #[test]
    fn attachments_split_cleanly_from_user_messages() {
        let (body, files) = split_attachments("Review the uncommitted changes\n[attached: uncommitted.diff]");
        assert_eq!(body, "Review the uncommitted changes");
        assert_eq!(files, vec!["uncommitted.diff"]);

        let (body2, files2) = split_attachments("Check these [attached: a.png, b.rs]");
        assert_eq!(body2, "Check these");
        assert_eq!(files2, vec!["a.png", "b.rs"]);
    }

    #[test]
    fn file_diff_preview_highlights_changed_region() {
        let diff = FileDiff {
            path: "src/lib.rs".into(),
            old_text: Some("fn a() {\n    old();\n}\n".into()),
            new_text: "fn a() {\n    new_one();\n    new_two();\n}\n".into(),
        };
        let (rows, more) = preview_file_diff(&diff, 20);
        assert_eq!(more, 0);
        assert!(rows.iter().any(|(k, l)| *k == DiffLineKind::Del && l.contains("old();")));
        assert!(rows.iter().any(|(k, l)| *k == DiffLineKind::Add && l.contains("new_one();")));
        assert_eq!(diff_counts(&[diff]), (2, 1));
    }
}
