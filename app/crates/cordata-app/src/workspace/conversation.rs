//! The conversation column: the header (title, a word only when something is wrong, the dock toggle) whose bottom
//! edge is the live line; the empty state (the project and three starters, as text); the approval (an attention rule,
//! the question, the answers); the after-reply links; the `/` and `@` menu; and the composer — a command line under
//! a hairline, with the `›` hanging in the margin so your words share one edge with the transcript's.

use acp_core::{ApprovalDecision, PermissionInfo};
use cordata_theme::tokens as t;
use cordata_view::app::{ConnState, Message};
use cordata_view::shown;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{Animation, AnimationExt as _, ease_in_out};
use gpui::{AnyElement, App, BoxShadow, ClickEvent, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, StyledImage as _, Window, div, point, px, relative};
use gpui_component::input::Textarea;

use super::{DockTab, Overlay, TogglePanel, ToggleSidebar, Workspace, tilde};
use crate::theme::{amber, c, inset_ring, line, mono, soft_shadow, tk};
use crate::transcript;
use crate::ui::{self, Kind, Tone};

/// What the header says about the shown session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Idle,
    Running,
    Waiting,
    Closed,
}

pub fn status_of(view: &cordata_view::app::App) -> Status {
    match (&view.conn, view.turn_active, view.pending.is_empty()) {
        (ConnState::Connecting, ..) => Status::Connecting,
        (ConnState::Closed { .. }, ..) => Status::Closed,
        (_, _, false) => Status::Waiting,
        (_, true, _) => Status::Running,
        _ => Status::Idle,
    }
}

/// The approval's 2px attention rule. A new question draws it top to bottom (240 ms): the eye goes to the decision.
fn attention_rule(id: &str, cx: &App) -> AnyElement {
    let bar = div().absolute().left(px(0.)).top(px(0.)).w(px(2.)).rounded_full().bg(c(tk(cx).attention));
    if !ui::motion(cx) {
        return bar.bottom(px(0.)).into_any_element();
    }
    bar.with_animation(SharedString::from(format!("rule-{id}")), Animation::new(Duration::from_millis(240)).with_easing(gpui::ease_out_quint()), |el, t| el.h(relative(t)))
        .into_any_element()
}

/// The prompt mark `›`, set in the margin before your words — in the composer and on each of your lines in the
/// transcript. A 12px column, centred on the first line of body text.
pub fn prompt_mark(ink: gpui::Hsla, cx: &App) -> AnyElement {
    div()
        .w(px(12.))
        .flex_shrink_0()
        .h(relative(1.))
        .text_size(px(t::TEXT_BODY + 3.))
        .line_height(px(t::TEXT_BODY * t::LINE_HEIGHT_BODY))
        .font_family(mono(cx))
        .text_color(ink)
        .child("›")
        .into_any_element()
}

/// The header's bottom edge, Cordata's one moving part: a hairline at rest, a short signal segment sweeping across it
/// while the agent works, the attention colour from end to end when it needs you, the danger colour when the engine
/// stopped. Everything else on screen stays still.
pub fn live_line(status: Status, cx: &App) -> AnyElement {
    let tk = tk(cx);
    let base = div().absolute().left(px(0.)).right(px(0.)).bottom(px(0.)).h(px(1.));
    match status {
        Status::Waiting => base.bg(c(tk.attention)).into_any_element(),
        Status::Closed => base.bg(c(tk.danger)).into_any_element(),
        Status::Running if !ui::motion(cx) => base.bg(c(tk.amber)).into_any_element(),
        Status::Running => {
            let mut glow = c(tk.amber);
            glow.a = 0.9;
            base.bg(line(cx, t::LINE_SUBTLE))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top(px(0.))
                        .h(px(1.))
                        .w(relative(0.22))
                        .bg(glow)
                        .with_animation("live-line", Animation::new(Duration::from_millis(1600)).repeat().with_easing(ease_in_out), |el, delta| {
                            // From just off the left edge to just off the right.
                            el.left(relative(-0.22 + 1.22 * delta))
                        }),
                )
                .into_any_element()
        }
        Status::Idle | Status::Connecting => base.bg(line(cx, t::LINE_SUBTLE)).into_any_element(),
    }
}

impl Workspace {
    pub(super) fn header(&self, column_w: f32, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let title = view.title();
        let status = status_of(view);
        // Said only when it is not the ordinary case; the live line below says the rest.
        let word = match status {
            Status::Connecting => Some(("connecting", tk.text_faint)),
            Status::Waiting => Some(("needs you", tk.attention)),
            Status::Closed => Some(("engine stopped", tk.danger)),
            Status::Idle | Status::Running => None,
        };
        let _ = column_w;
        let panel_on = self.panel_open;
        let sidebar_on = self.sidebar_open;
        div()
            .id("conversation-header")
            .relative()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(10.))
            .h(px(t::HEADER_H))
            // Reserve space for macOS traffic lights when the left sidebar is hidden.
            .pl(px(if sidebar_on { 24. } else { 78. }))
            .pr(px(12.))
            .overflow_hidden()
            .child(live_line(status, cx))
            .when(!sidebar_on, |row| {
                row.child(
                    ui::icon_action("show-sidebar", gpui_kit_assets::IconName::PanelLeft, cx)
                        .aria_label("Show sidebar (⌘B)")
                        .tooltip(ui::tip("Show sidebar  ⌘B"))
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(&ToggleSidebar, window, cx))),
                )
                .child(
                    ui::chip("header-project", SharedString::from(format!("{} ⌄", self.project_name())), cx)
                        .role(gpui::Role::Button)
                        .aria_label("Switch project (⌘O)")
                        .tooltip(ui::tip("Switch project  ⌘O"))
                        .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Picker, window, cx))),
                )
            })
            // A fresh thread has no name yet: the header stays empty rather than repeating the rail's "New thread".
            .when(title != "New thread", |row| row.child(div().min_w(px(60.)).flex_shrink_1().truncate().text_size(px(t::TEXT_TITLE)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(shown(&title)))))
            .when_some(word, |row, (word, ink)| row.child(div().flex_shrink_0().whitespace_nowrap().text_size(px(t::TEXT_META)).text_color(c(ink)).child(word)))
            // The empty middle drags the window.
            .child(div().flex_1().h_full().min_w(px(8.)).on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()))
            .children(self.branch_chip(cx))
            .children(self.pr_chip(cx))
            .child(
                ui::icon_action("panel-toggle", gpui_kit_assets::IconName::PanelRight, cx)
                    .aria_label("Toggle side panel (⌘\\)")
                    .aria_toggled(panel_on.into())
                    .tooltip(ui::tip("Files, changes and terminal  ⌘\\"))
                    .when(panel_on, |b| b.text_color(c(tk.text_primary)))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_panel(&TogglePanel, window, cx))),
            )
            .into_any_element()
    }

    /// The column when there is nothing said yet: the project's name and three things to start from, as plain text
    /// on the left of the reading column. No logo, no cards, no keycaps; errors, if any, under them.
    pub(super) fn empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let project = self.project_name();
        let dirty_count = self.review.file_count();
        // The notes the session started with (settings warnings, instructions read): small, under the starters.
        let notes = transcript::opening_notes(&self.session.read(cx).view);
        let notices: Vec<AnyElement> = self
            .session
            .read(cx)
            .view
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Error(e) => Some(
                    div()
                        .w_full()
                        .pl(px(12.))
                        .border_l_2()
                        .border_color(c(tk.danger))
                        .text_size(px(t::TEXT_SMALL))
                        .text_color(c(tk.danger))
                        .child(SharedString::from(shown(e)))
                        .into_any_element(),
                ),
                _ => None,
            })
            .collect();

        let starter = |id: &'static str, text: SharedString| {
            let (hover_ink, arrow) = (c(tk.text_primary), c(tk.amber));
            div()
                .id(id)
                .role(gpui::Role::Button)
                .group(id)
                .flex()
                .items_center()
                .gap(px(10.))
                .py(px(5.))
                .cursor_pointer()
                .text_size(px(t::TEXT_BODY))
                .text_color(c(tk.text_secondary))
                .hover(move |s| s.text_color(hover_ink))
                .ml(px(-t::HANG))
                .child(div().w(px(12.)).flex_shrink_0().text_color(c(tk.text_faint)).group_hover(id, move |s| s.text_color(arrow)).child("→"))
                .child(text)
        };
        let review_text = if dirty_count > 0 {
            SharedString::from(format!("Review the {dirty_count} uncommitted file{}", if dirty_count == 1 { "" } else { "s" }))
        } else {
            SharedString::from("Review the latest changes")
        };
        let explore = starter("starter-explore", "Map how this project fits together".into()).on_click(cx.listener(|this, _, window, cx| {
            this.apply_starter("Map out the architecture, key modules, and main entry points of this repository.", false, window, cx);
        }));
        let review = starter("starter-review", review_text).on_click(cx.listener(move |this, _, window, cx| {
            if dirty_count > 0 {
                this.request_review(false, window, cx);
            } else {
                this.apply_starter("Review the most recent changes in this repository for bugs, edge cases, and test coverage gaps.", false, window, cx);
            }
        }));
        let fix = starter("starter-fix", "Run the checks and fix what fails".into()).on_click(cx.listener(|this, _, window, cx| {
            this.apply_starter("Run the project's test suite and build checks, diagnose any failures, and propose clean fixes.", false, window, cx);
        }));

        div()
            .id("empty-state-scroll")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_center()
            .justify_end()
            .px(px(24.))
            .pb(px(28.))
            .child(
                div()
                    .w_full()
                    .max_w(px(t::CONTENT_MAX_W))
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(ui::cordata_logo(36., cx))
                            .child(
                                div()
                                    .text_size(px(t::TEXT_EMPTY))
                                    .font_weight(FontWeight::LIGHT)
                                    .text_color(c(tk.text_primary))
                                    .child(SharedString::from(project)),
                            ),
                    )
                    .child(div().flex().flex_col().child(explore).child(review).child(fix))
                    .children(notices)
                    .when(!notes.is_empty(), |col| {
                        col.child(
                            div()
                                .pt(px(6.))
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .text_size(px(t::TEXT_META))
                                .text_color(c(tk.text_faint))
                                .children(notes.into_iter().map(|n| div().truncate().child(SharedString::from(shown(&n))))),
                        )
                    }),
            )
            .into_any_element()
    }

    pub(super) fn approval_card(&self, info: &PermissionInfo, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let session = self.session.clone();
        let answer = move |decision: ApprovalDecision| {
            let session = session.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let decision = decision.clone();
                session.update(cx, |session, cx| session.answer(decision, cx));
            }
        };
        let input = info.raw_input.as_ref();
        let command = input.and_then(|i| i.get("cmd")).and_then(|v| v.as_str()).map(str::to_owned);
        let summary = input.and_then(cordata_view::view::summarize_input).unwrap_or_default();
        let place = tilde(&self.session.read(cx).cwd);
        let diffs = self.session.read(cx).view.card(&info.tool_call_id).map(|card| card.diffs.clone()).filter(|d| !d.is_empty());
        let has_diff = diffs.is_some();
        let tool = info.tool_name.as_str();
        let (_, sentence, block, buttons): (&str, String, Option<String>, Vec<AnyElement>) = {
            {
                let sentence = match (&command, info.kind) {
                    (Some(_), _) => format!("Run shell command in {place}"),
                    (None, acp_core::ToolKind::Edit) => format!("Modify files in {place}"),
                    _ => format!("Allow {tool}?"),
                };
                // The engine words the offer ("Always allow run_command this session"): the button says "Always",
                // the tooltip says the rest.
                let always = info.always_label.as_ref().map(|label| {
                    ui::button("always", "Always", Kind::Ghost, cx)
                        .tooltip(ui::tip(SharedString::from(shown(label))))
                        .on_click(answer(ApprovalDecision::AllowAlways))
                        .into_any_element()
                });
                let mut buttons = vec![
                    ui::button("allow", "Allow", Kind::Primary, cx)
                        .child(div().text_color(c(tk.amber_dim_2)).font_family(mono(cx)).text_size(px(t::TEXT_META)).child("↵"))
                        .on_click(answer(ApprovalDecision::Allow))
                        .into_any_element(),
                ];
                buttons.extend(always);
                buttons.push(
                    ui::button("say-why", "Reply", Kind::Ghost, cx)
                        .tooltip(ui::tip("Answer in your own words instead  W"))
                        .on_click(cx.listener(|this, _, window, cx| this.begin_saying(window, cx)))
                        .into_any_element(),
                );
                buttons.push(div().flex_1().into_any_element());
                buttons.push(
                    ui::button("reject", "Deny", Kind::Ghost, cx)
                        .tooltip(ui::tip("Deny  esc"))
                        .on_click(answer(ApprovalDecision::Reject))
                        .into_any_element(),
                );
                ("", sentence, command.clone().map(|cmd| format!("$ {cmd}")).or_else(|| (!summary.is_empty()).then(|| summary.clone())), buttons)
            }
        };
        // Not a box: a 2px attention rule down the left, the question, the command, the answers.
        div()
            .id("approval")
            .role(gpui::Role::AlertDialog)
            .aria_label("Approval needed")
            .relative()
            .w_full()
            .mb(px(12.))
            .pl(px(14.))
            .py(px(4.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(attention_rule(&info.tool_call_id, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_size(px(t::TEXT_BODY)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(shown(&sentence))))
                    .when(self.saying, |row| row.child(ui::badge("type below · ↵ sends", Tone::Amber, cx))),
            )
            // An edit shows the change it asks about; anything else, its command or input.
            .when_some(diffs, |card, diffs| card.child(transcript::diff_preview(&diffs, cx)))
            .when(!has_diff, |card| card.when_some(block, |card, text| card.child(ui::code_block(SharedString::from(shown(&text)), cx).text_color(c(tk.text_body)))))
            .child(div().flex().flex_wrap().items_center().gap(px(6.)).children(buttons))
            .into_any_element()
    }

    /// After a reply: review the changed files, copy the answer, undo the turn's changes.
    pub(super) fn post_reply_actions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        if view.turn_active || !view.pending.is_empty() || !matches!(view.messages.last(), Some(Message::Agent(_))) {
            return None;
        }
        // "Review" only when this turn changed files: the tree's older changes are the rail's ±N, not this reply's.
        let edited = view
            .messages
            .iter()
            .rev()
            .take_while(|m| !matches!(m, Message::User(_)))
            .any(|m| matches!(m, Message::Tool(card) if !card.diffs.is_empty()));
        let link = |id: &'static str, text: &'static str| {
            let hover = c(tk.text_primary);
            div()
                .id(id)
                .role(gpui::Role::Button)
                .cursor_pointer()
                .text_size(px(t::TEXT_SMALL))
                .text_color(c(tk.text_faint))
                .hover(move |s| s.text_color(hover))
                .child(text)
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .pb(px(10.))
                .when(edited, |row| {
                    row.child(link("review-files", "review changes").tooltip(ui::tip("Changes, hunk by hunk  ⌘⌥4")).on_click(cx.listener(|this, _, _, cx| {
                        this.set_panel(true, cx);
                        this.switch_dock(DockTab::Review, cx);
                    })))
                })
                .child(link("copy-reply", "copy").tooltip(ui::tip("Copy the reply as Markdown")).on_click(cx.listener(|this, _, _, cx| this.copy_last_reply(cx))))
                .into_any_element(),
        )
    }

    /// The `/` and `@` menu, above the composer.
    pub(super) fn slash_menu(&self, cx: &App) -> Option<AnyElement> {
        let tk = tk(cx);
        let menu = self.session.read(cx).view.menu.as_ref()?;
        let rows = menu.items.iter().enumerate().take(cordata_view::menu::MAX_ROWS + 3).map(|(index, item)| {
            let selected = index == menu.selected;
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(t::RADIUS_ROW))
                .when(selected, |row| row.bg(amber(cx, t::AMBER_FILL_MID)).shadow(inset_ring(amber(cx, t::AMBER_BORDER))))
                .child(div().w(px(160.)).flex_shrink_0().truncate().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).font_weight(if selected { FontWeight::MEDIUM } else { FontWeight::NORMAL }).text_color(c(if selected { tk.amber } else { tk.amber_soft })).child(SharedString::from(item.label.clone())))
                .child(div().flex_shrink_0().text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(item.hint.clone())))
                .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child(SharedString::from(item.detail.clone())))
        });
        Some(
            ui::popover(cx)
                .id("composer-menu")
                .role(gpui::Role::ListBox)
                .aria_label("Suggestions")
                .mb(px(8.))
                .p(px(5.))
                .children(rows)
                .child(
                    div()
                        .px(px(10.))
                        .pt(px(5.))
                        .pb(px(2.))
                        .border_t_1()
                        .border_color(line(cx, t::LINE_SUBTLE))
                        .child(ui::meta("↑↓ navigate · Tab complete · ↵ run · esc dismiss", cx)),
                )
                .into_any_element(),
        )
    }

    pub(super) fn composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let busy = view.turn_active || !view.pending.is_empty();
        let typed = !self.composer.read(cx).value().trim().is_empty();
        let ready = typed || !self.staged.is_empty();
        let active_border = ready || busy || self.saying;

        let mut ring = Vec::new();
        if active_border {
            ring.push(BoxShadow { color: amber(cx, 0.05), offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(t::COMPOSER_RING), inset: false });
        }
        ring.extend(soft_shadow(cx));

        let chips: Vec<AnyElement> = self
            .staged
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let hover = c(tk.danger);
                let thumb = item.preview.clone();
                let tall = thumb.is_some();
                let icon = match thumb {
                    // A live GPU thumbnail of the staged image.
                    Some(image) => div()
                        .flex_shrink_0()
                        .size(px(40.))
                        .rounded(px(t::RADIUS_CONTROL))
                        .overflow_hidden()
                        .border_1()
                        .border_color(line(cx, t::LINE_CONTROL_STRONG))
                        .child(gpui::img(image).size_full().object_fit(gpui::ObjectFit::Cover))
                        .into_any_element(),
                    None => div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .child(div().text_color(c(if item.image { tk.teal } else { tk.amber_soft })).text_size(px(t::TEXT_META)).child(item.kind.glyph()))
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(item.kind.tag()))
                        .into_any_element(),
                };
                div()
                    .id(("attachment", index))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(8.))
                    .h(px(if tall { 52. } else { 26. }))
                    .pl(px(if tall { 6. } else { 8. }))
                    .pr(px(8.))
                    .rounded(px(t::RADIUS_CONTROL))
                    .border_1()
                    .border_color(line(cx, t::LINE_CONTROL_STRONG))
                    .bg(line(cx, t::FILL_FAINT))
                    .child(icon)
                    .child(
                        div()
                            .flex()
                            .when(tall, |d| d.flex_col().gap(px(2.)))
                            .when(!tall, |d| d.items_center().gap(px(6.)))
                            .child(div().max_w(px(180.)).truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_secondary)).child(SharedString::from(item.name.clone())))
                            .child(ui::meta(SharedString::from(super::attachments::size_label(item.size)), cx)),
                    )
                    .child(
                        div()
                            .id(("attachment-remove", index))
                            .role(gpui::Role::Button)
                            .aria_label(SharedString::from(format!("Remove {}", item.name)))
                            .cursor_pointer()
                            .text_color(c(tk.text_faint))
                            .hover(move |s| s.text_color(hover))
                            .child("✕")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if index < this.staged.len() {
                                    this.staged.remove(index);
                                }
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        let queued: Vec<AnyElement> = view
            .queue
            .iter()
            .enumerate()
            .map(|(index, text)| {
                div()
                    .id(("queued", index))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(5.))
                    .child(ui::badge("Queued", Tone::Neutral, cx))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_secondary)).child(SharedString::from(shown(text))))
                    .child(ui::icon_button(("queued-remove", index), "✕", cx).size(px(18.)).aria_label("Remove from the queue").on_click(cx.listener(move |this, _, _, cx| {
                        this.session.update(cx, |s, cx| {
                            if index < s.view.queue.len() {
                                s.view.queue.remove(index);
                            }
                            s.version += 1;
                            cx.notify();
                        });
                    })))
                    .into_any_element()
            })
            .collect();

        let note = self.attach_note.clone();
        let send_key = if self.prefs.send_with == "cmd-enter" { "⌘↵" } else { "↵" };
        let model = view.session.as_ref().map(|s| short_model(&s.model)).filter(|m| !m.is_empty());
        let esc = self.esc_hint();
        let action: Option<AnyElement> = if busy && !self.saying {
            Some(
                ui::button("stop-turn", "Stop", Kind::Ghost, cx)
                    .h(px(24.))
                    .px(px(8.))
                    .text_size(px(t::TEXT_SMALL))
                    .aria_label("Stop (esc esc)")
                    .tooltip(ui::tip("Stop the run  esc esc"))
                    .on_click(cx.listener(|this, _, _, cx| this.session.update(cx, |s, cx| s.cancel(cx))))
                    .into_any_element(),
            )
        } else if ready {
            let hover = c(tk.amber_hover);
            Some(
                div()
                    .id("send")
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(format!("Send ({send_key})")))
                    .flex_shrink_0()
                    .cursor_pointer()
                    .text_size(px(t::TEXT_SMALL))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(c(tk.amber))
                    .hover(move |s| s.text_color(hover))
                    .on_click(cx.listener(|this, _, window, cx| this.send_composer(window, cx)))
                    .child(SharedString::from(format!("send {send_key}")))
                    .into_any_element(),
            )
        } else {
            None
        };
        // The prompt mark: the signal colour once there is something to send or the agent is busy.
        let mark = if active_border { c(tk.amber) } else { c(tk.text_faint) };

        div()
            .id("composer")
            .role(gpui::Role::Group)
            .aria_label("Composer")
            .capture_key_down(cx.listener(Self::composer_keys))
            .w_full()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .when(!queued.is_empty(), |col| col.child(div().flex().flex_col().pt(px(6.)).children(queued)))
            .when(!chips.is_empty(), |col| col.child(div().flex().flex_wrap().gap(px(6.)).pt(px(10.)).children(chips)))
            .when_some(note, |col, note| col.child(div().pt(px(8.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.attention)).child(SharedString::from(note))))
            .child(
                // The mark hangs in the margin (12px + 10px gap), so what you type starts on the column's text edge.
                div()
                    .flex()
                    .items_start()
                    .gap(px(10.))
                    .ml(px(-t::HANG))
                    .pt(px(12.))
                    .child(prompt_mark(mark, cx))
                    // The Textarea pads its text 10px: pull it back so typing starts on the text edge, under your lines above.
                    .child(div().flex_1().min_w_0().ml(px(-10.)).text_size(px(t::TEXT_BODY)).child(Textarea::new(&self.composer).appearance(false).aria_label("Message"))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    // Icons sit in 24px boxes: pull the first back so its glyph lines up with the text edge.
                    .ml(px(-5.))
                    .h(px(28.))
                    .text_size(px(t::TEXT_META))
                    .text_color(c(tk.text_faint))
                    .child(
                        ui::icon_action("attach", gpui_kit_assets::IconName::Paperclip, cx)
                            .size(px(22.))
                            .aria_label("Attach files or images (Shift+Command+A, or paste / drop them)")
                            .tooltip(ui::tip("Attach files or images  ⇧⌘A — or paste / drop them"))
                            .on_click(cx.listener(|this, _, window, cx| this.choose_attachments(window, cx))),
                    )
                    .child(
                        ui::icon_action("dictate", gpui_kit_assets::IconName::Mic, cx)
                            .size(px(22.))
                            .aria_label("Dictate (system dictation)")
                            .tooltip(ui::tip("Dictate"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.composer.update(cx, |s, cx| s.focus(window, cx));
                                cx.defer(|_| crate::platform::start_dictation());
                            })),
                    )
                    .child(div().flex_1().min_w_0().truncate().children(view.hint.clone().map(|hint| SharedString::from(shown(&hint)))))
                    .when_some(esc, |row, hint| row.child(div().flex_shrink_0().text_color(c(tk.attention)).child(hint)))
                    .children(self.meter(cx))
                    .when_some(model, |row, model| row.child(self.model_trigger(model, cx)))
                    .children(action),
            )
            .into_any_element()
    }

    /// The composer's placeholder for the session's state.
    pub(super) fn placeholder(&self, cx: &App) -> &'static str {
        let view = &self.session.read(cx).view;
        if self.saying {
            "Reply in your own words — ↵ sends, esc returns to approval"
        } else if !view.pending.is_empty() {
            "Approve above (↵), or press W to redirect in your own words…"
        } else if view.turn_active {
            "Queue a follow-up (↵)…"
        } else if view.messages.iter().any(transcript::is_conversation) {
            "Ask a follow-up, @mention files, or type / for commands…"
        } else {
            "Ask anything, @mention files, or type / for commands…"
        }
    }
}

/// `opus-4` from `claude-opus-4-20250514`; names that are already short stay.
fn short_model(name: &str) -> String {
    let name = name.trim();
    // `ollama_cloud::kimi-k3` is `kimi-k3`: the provider is not the person's concern here.
    let name = name.rsplit_once("::").map_or(name, |(_, model)| model);
    let name = name.strip_prefix("claude-").unwrap_or(name);
    // Drop a trailing date stamp (`-20250514`).
    let name = match name.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => name,
    };
    name.chars().take(28).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_read_short() {
        assert_eq!(short_model("claude-opus-4-20250514"), "opus-4");
        assert_eq!(short_model("kimi-k2.7-code"), "kimi-k2.7-code");
        assert_eq!(short_model("ollama_cloud::kimi-k3"), "kimi-k3");
    }
}
