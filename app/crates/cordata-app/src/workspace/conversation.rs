//! The conversation column: the 52px header (title, status, branch and model chips, the panel toggle), the empty
//! state, the approval card, the post-reply actions, the `/` and `@` menu and the composer.

use acp_core::ApprovalDecision;
use cordata_theme::tokens as t;
use cordata_view::app::{ConnState, Message, PendingPermission};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, BoxShadow, ClickEvent, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, point, px, relative};
use gpui_component::input::Textarea;

use super::{Overlay, TogglePanel, Workspace, tilde};
use crate::theme::{amber, amber_glow, c, line, mono, soft_shadow, tk};
use crate::transcript;
use crate::ui::{self, Dot, Kind};

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

/// The permission mode as the composer names it.
pub fn mode_label(mode: &str) -> &str {
    match mode {
        "plan" => "plan only",
        "build" => "ask first",
        "edit" => "auto-edit",
        "auto" => "auto",
        "" => "ask first",
        other => other,
    }
}

impl Workspace {
    pub(super) fn header(&self, column_w: f32, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let title = view.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "New conversation".into());
        let (dot, word, ink) = match status_of(view) {
            Status::Connecting => (Dot::Hollow, "connecting", tk.text_muted),
            Status::Idle => (Dot::Ok, "idle", tk.teal),
            Status::Running => (Dot::Live, "working", tk.amber),
            Status::Waiting => (Dot::Live, "waiting for you", tk.amber),
            Status::Closed => (Dot::Amber, "reconnecting…", tk.amber),
        };
        let model = view.config("model").map(|option| option.current_name().to_owned());
        let window_k = view.context.as_ref().and_then(|g| g.window).map(|w| format!("{}k", w / 1000));
        let model_text = model.map(|m| {
            let m = short_model(&m);
            match (&window_k, column_w >= t::HEADER_SHORT_MODEL_W) {
                (Some(k), true) => format!("{m} · {k} ⌄"),
                _ => format!("{m} ⌄"),
            }
        });
        let branch = view.branch.clone().filter(|_| column_w >= t::HEADER_HIDE_BRANCH_W);
        let panel_on = self.panel_open;
        div()
            .id("conversation-header")
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(10.))
            .h(px(t::HEADER_H))
            .px(px(20.))
            .overflow_hidden()
            .border_b_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .child(div().min_w(px(60.)).flex_shrink_1().truncate().text_size(px(t::TEXT_TITLE)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(shown(&title))))
            .child(div().flex().flex_shrink_0().items_center().gap(px(6.)).child(ui::dot(dot, cx).size(px(t::DOT_SMALL))).child(div().whitespace_nowrap().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(ink)).child(word)))
            // The empty middle drags the window.
            .child(div().flex_1().h_full().min_w(px(8.)).on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()))
            .when_some(branch, |row, branch| row.child(ui::chip("branch-chip", SharedString::from(format!("⎇ {branch}")), cx).cursor_default().aria_label(SharedString::from(format!("Branch {branch}")))))
            .children(self.pr_chip(cx))
            .when_some(model_text, |row, text| {
                row.child(
                    ui::chip("model-picker", SharedString::from(text), cx)
                        .role(gpui::Role::Button)
                        .aria_label("Model")
                        .tooltip(ui::tip("Choose the model"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model_menu = !this.model_menu;
                            cx.notify();
                        })),
                )
            })
            .child(
                ui::chip("panel-toggle", "⊟ panel", cx)
                    .role(gpui::Role::Button)
                    .aria_label("Toggle side panel (⌘\\)")
                    .aria_toggled(panel_on.into())
                    .tooltip(ui::tip("Toggle side panel  ⌘\\"))
                    .when(panel_on, |chip| chip.text_color(c(tk.text_primary)))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_panel(&TogglePanel, window, cx))),
            )
            .into_any_element()
    }

    pub(super) fn model_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.model_menu {
            return None;
        }
        let tk = tk(cx);
        let option = self.session.read(cx).view.config("model")?.clone();
        let rows = option.choices.iter().enumerate().map(|(index, choice)| {
            let current = choice.value == option.current_value;
            let value = choice.value.clone();
            ui::row(("model", index), current, cx)
                .child(div().w(px(10.)).text_color(c(tk.amber)).child(if current { "●" } else { "" }))
                .child(div().flex_1().truncate().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(if current { tk.amber } else { tk.text_primary })).child(SharedString::from(shown(&choice.name))))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let value = value.clone();
                    this.model_menu = false;
                    this.session.update(cx, |session, cx| session.set_config("model", &value, cx));
                }))
        });
        Some(
            ui::popover(cx)
                .id("model-menu")
                .role(gpui::Role::Menu)
                .aria_label("Models")
                .absolute()
                .top(px(t::HEADER_H - 4.))
                .right(px(90.))
                .w(px(300.))
                .max_h(px(420.))
                .overflow_y_scroll()
                .p(px(4.))
                .children(rows)
                .into_any_element(),
        )
    }

    /// The column when there is nothing said yet: the mark, the question, the path, the hints, resume.
    pub(super) fn empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let project = self.project_name();
        let resume = self.listing.iter().find(|row| !self.sessions.iter().any(|s| super::matches_target(s.read(cx), &row.target))).cloned();
        let notices: Vec<AnyElement> = self
            .session
            .read(cx)
            .view
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Error(e) => Some(div().max_w(px(520.)).text_center().text_size(px(t::TEXT_SMALL)).text_color(c(tk.danger)).child(SharedString::from(shown(e))).into_any_element()),
                _ => None,
            })
            .collect();
        let hint = |key: &'static str, word: &'static str| {
            div().flex().flex_shrink_0().whitespace_nowrap().gap(px(6.)).child(div().text_color(c(tk.text_secondary)).child(key)).child(div().text_color(c(tk.text_muted)).child(word))
        };
        let link_hover = c(tk.text_primary);
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .px(px(24.))
            .child(crate::hud::bracket_title("Cordata", cx))
            .child(
                div()
                    .max_w(px(520.))
                    .text_center()
                    .text_size(px(t::TEXT_EMPTY))
                    .line_height(relative(1.35))
                    .text_color(c(tk.text_body))
                    .child("What are we working on in ")
                    .child(div().whitespace_nowrap().font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(format!("{project}?")))),
            )
            .child(
                div()
                    .id("empty-change-project")
                    .role(gpui::Role::Link)
                    .aria_label("Change project (⌘O)")
                    .flex()
                    .items_center()
                    .max_w(px(520.))
                    .gap(px(6.))
                    .cursor_pointer()
                    .font_family(mono(cx))
                    .text_size(px(t::TEXT_SMALL))
                    .text_color(c(tk.text_muted))
                    .hover(move |s| s.text_color(link_hover))
                    .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Picker, window, cx)))
                    .child(div().min_w_0().truncate().child(SharedString::from(tilde(&self.cwd))))
                    .child(div().flex_shrink_0().child("·"))
                    .child(div().flex_shrink_0().text_color(c(tk.text_secondary)).child("change")),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_center()
                    .gap_x(px(18.))
                    .gap_y(px(6.))
                    .font_family(mono(cx))
                    .text_size(px(t::TEXT_SMALL))
                    .child(hint("/", "commands"))
                    .child(hint("@", "files"))
                    .child(hint("!", "shell"))
                    .child(hint("⌘K", "palette")),
            )
            .when_some(resume, |col, row| {
                let target = row.target.clone();
                let hover = line(cx, t::HOVER_STRONG);
                col.child(
                    div()
                        .id("resume-last")
                        .role(gpui::Role::Button)
                        .aria_label("Resume the last conversation")
                        .mt(px(6.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .max_w(px(420.))
                        .h(px(34.))
                        .px(px(14.))
                        .rounded(px(t::RADIUS_CARD))
                        .border_1()
                        .border_color(line(cx, t::LINE_CONTROL_STRONG))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .on_click(cx.listener(move |this, _, window, cx| this.open(target.clone(), window, cx)))
                        .child(ui::section_label("resume", cx))
                        .child(div().min_w_0().truncate().text_size(px(t::TEXT_UI)).text_color(c(tk.text_primary)).child(SharedString::from(shown(&row.title))))
                        .child(ui::meta(SharedString::from(format!("{} ago", super::age(super::now_secs() - row.when))), cx)),
                )
            })
            .children(notices)
            .into_any_element()
    }

    pub(super) fn approval_card(&self, pending: &PendingPermission, cx: &App) -> AnyElement {
        let tk = tk(cx);
        let info = &pending.info;
        let session = self.session.clone();
        let answer = move |decision: ApprovalDecision| {
            let session = session.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let decision = decision.clone();
                session.update(cx, |session, cx| session.answer(decision, cx));
            }
        };
        let input = info.raw_input.as_ref();
        let command = input.and_then(|i| i.get("command")).and_then(|v| v.as_str()).map(str::to_owned);
        let summary = input.and_then(|i| cordata_view::view::summarize_input(i, false)).unwrap_or_default();
        let place = tilde(&self.session.read(cx).cwd);
        let (label, sentence, block, buttons): (&str, String, Option<String>, Vec<AnyElement>) = match info.tool_name.as_str() {
            "present_plan" => (
                "plan ready",
                "The plan is ready. Build it, edit it, or keep planning.".into(),
                None,
                [("approve_build", "Build", Kind::Primary), ("approve_edit", "Edit", Kind::AmberOutline), ("approve_auto", "Auto", Kind::Outline)]
                    .into_iter()
                    .filter(|(id, ..)| info.option(id).is_some())
                    .map(|(id, label, kind)| ui::button(SharedString::from(id), label, kind, cx).on_click(answer(ApprovalDecision::Choose(id.into()))).into_any_element())
                    .chain([div().flex_1().into_any_element(), ui::button("keep-planning", "Keep planning  esc", Kind::Ghost, cx).on_click(answer(ApprovalDecision::Reject)).into_any_element()])
                    .collect(),
            ),
            "ask_user" => (
                "question",
                input.and_then(|i| i.get("question")).and_then(|q| q.as_str()).unwrap_or("The agent has a question").to_owned(),
                None,
                cordata_view::view::ask_choices(info)
                    .into_iter()
                    .enumerate()
                    .map(|(index, choice)| ui::button(SharedString::from(format!("choice-{index}")), SharedString::from(format!("{}  {}", index + 1, shown(&choice.name))), Kind::Outline, cx).on_click(answer(ApprovalDecision::Choose(choice.id.clone()))).into_any_element())
                    .chain([div().flex_1().into_any_element(), ui::button("skip", "Skip  esc", Kind::Ghost, cx).on_click(answer(ApprovalDecision::Reject)).into_any_element()])
                    .collect(),
            ),
            tool => {
                let sentence = match (&command, info.kind) {
                    (Some(_), _) => format!("Run a shell command in {place}"),
                    (None, acp_core::ToolKind::Edit) => format!("Edit files in {place}"),
                    _ => format!("Allow {}?", attribution_label(tool)),
                };
                let always = info.always_label.as_ref().map(|label| {
                    let short = always_prefix(label);
                    ui::button("always", SharedString::from(format!("Always allow `{}`", shown(&short))), Kind::AmberOutline, cx)
                        .max_w(px(320.))
                        .overflow_hidden()
                        .on_click(answer(ApprovalDecision::AllowAlways))
                        .into_any_element()
                });
                let mut buttons = vec![
                    ui::button("allow", "Allow once", Kind::Primary, cx)
                        .child(div().text_color(c(tk.amber_dim_2)).font_family(mono(cx)).text_size(px(t::TEXT_META)).child("↵"))
                        .on_click(answer(ApprovalDecision::Allow))
                        .into_any_element(),
                ];
                buttons.extend(always);
                buttons.push(div().flex_1().into_any_element());
                buttons.push(ui::button("reject", "Deny", Kind::Ghost, cx).child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child("esc")).on_click(answer(ApprovalDecision::Reject)).into_any_element());
                ("approval needed", sentence, command.clone().map(|c| format!("$ {c}")).or_else(|| (!summary.is_empty()).then(|| summary.clone())), buttons)
            }
        };
        div()
            .id("approval")
            .role(gpui::Role::AlertDialog)
            .aria_label("Approval needed")
            .w_full()
            .mb(px(10.))
            .p(px(14.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .rounded(px(t::RADIUS_POPOVER))
            .border_1()
            .border_color(amber(cx, t::AMBER_BORDER_STRONG))
            .bg(amber(cx, t::AMBER_FILL))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(ui::diamond(cx))
                    .child(div().font_family(mono(cx)).text_size(px(t::TEXT_LABEL)).font_weight(FontWeight::MEDIUM).text_color(c(tk.amber)).child(SharedString::from(t::tracked_text(label, t::Tracking::Label, true))))
                    .when(info.untrusted_source, |row| row.child(ui::badge("from untrusted content", ui::Tone::Teal, cx))),
            )
            .child(div().text_size(px(t::TEXT_BODY)).text_color(c(tk.text_primary)).child(SharedString::from(shown(&sentence))))
            .when_some(block, |card, text| card.child(ui::code_block(SharedString::from(shown(&text)), cx).text_color(c(if info.untrusted_source { tk.teal } else { tk.text_body }))))
            .child(div().flex().flex_wrap().items_center().gap(px(8.)).children(buttons))
            .into_any_element()
    }

    /// After a reply: review the changed files, copy the answer, undo the turn's changes.
    pub(super) fn post_reply_actions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = &self.session.read(cx).view;
        if view.turn_active || !view.pending.is_empty() || !matches!(view.messages.last(), Some(Message::Agent(_))) {
            return None;
        }
        let changed = self.review.file_count();
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .pb(px(10.))
                .when(changed > 0, |row| {
                    row.child(ui::button("review-files", SharedString::from(format!("Review {changed} file{}", if changed == 1 { "" } else { "s" })), Kind::AmberOutline, cx).h(px(26.)).on_click(cx.listener(|this, _, _, cx| this.switch_dock(super::DockTab::Review, cx))))
                })
                .child(ui::button("copy-reply", "Copy", Kind::Outline, cx).h(px(26.)).on_click(cx.listener(|this, _, _, cx| this.copy_last_reply(cx))))
                .child(ui::button("undo-turn", "Undo changes", Kind::Ghost, cx).h(px(26.)).tooltip(ui::tip("Preview undoing this turn's file changes (/undo)")).on_click(cx.listener(|this, _, _, cx| this.session.update(cx, |s, cx| s.submit("/undo", cx)))))
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
                .py(px(5.))
                .rounded(px(t::RADIUS_ROW))
                .when(selected, |row| row.bg(amber(cx, t::AMBER_FILL_MID)))
                .child(div().w(px(150.)).flex_shrink_0().truncate().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(if selected { tk.amber } else { tk.amber_soft })).child(SharedString::from(item.label.clone())))
                .child(div().flex_shrink_0().text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(item.hint.clone())))
                .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child(SharedString::from(item.detail.clone())))
        });
        Some(
            ui::popover(cx)
                .id("composer-menu")
                .role(gpui::Role::ListBox)
                .aria_label("Suggestions")
                .mb(px(6.))
                .p(px(4.))
                .children(rows)
                .child(div().px(px(10.)).pt(px(4.)).pb(px(2.)).child(ui::meta("↑↓ choose · Tab complete · ↵ run · esc close", cx)))
                .into_any_element(),
        )
    }

    pub(super) fn composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let busy = view.turn_active || !view.pending.is_empty();
        let mode = view.pending_mode.clone().unwrap_or_else(|| view.mode.clone());
        let mode_text = format!("{} ⌄", mode_label(&mode));
        let mut ring = vec![BoxShadow { color: amber(cx, 0.04), offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(t::COMPOSER_RING), inset: false }];
        ring.extend(soft_shadow(cx));
        let trigger = |id: &'static str, key: &'static str, word: &'static str, cx: &mut Context<Self>| {
            let (hover_bg, hover_ink) = (line(cx, t::HOVER_STRONG), c(tk.text_primary));
            div()
                .id(id)
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(format!("Insert {key} ({word})")))
                .flex()
                .flex_shrink_0()
                .whitespace_nowrap()
                .gap(px(5.))
                .px(px(6.))
                .py(px(2.))
                .rounded(px(t::RADIUS_ROW))
                .cursor_pointer()
                .font_family(mono(cx))
                .text_size(px(t::TEXT_META))
                .text_color(c(tk.text_muted))
                .hover(move |s| s.bg(hover_bg).text_color(hover_ink))
                .on_click(cx.listener(move |this, _, window, cx| this.insert_trigger(key, window, cx)))
                .child(div().text_color(c(tk.text_secondary)).child(key))
                .child(word)
        };
        let typed = !self.composer.read(cx).value().trim().is_empty();
        let running = view.turn_active;
        let steer = (running && typed).then(|| {
            ui::button("steer", "Steer ⌥↵", Kind::Tint, cx)
                .h(px(t::SEND_BUTTON))
                .aria_label("Steer the running turn (Option+Enter)")
                .tooltip(ui::tip("Send this to the running turn now; ↵ queues it for after"))
                .on_click(cx.listener(|this, _, window, cx| this.steer_composer(window, cx)))
        });
        let chips: Vec<AnyElement> = self
            .staged
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let hover = c(tk.danger);
                div()
                    .id(("attachment", index))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(6.))
                    .h(px(24.))
                    .px(px(8.))
                    .rounded(px(t::RADIUS_CONTROL))
                    .border_1()
                    .border_color(line(cx, t::LINE_CONTROL_STRONG))
                    .bg(line(cx, t::FILL_FAINT))
                    .child(div().text_color(c(if item.image { tk.teal } else { tk.amber_soft })).text_size(px(t::TEXT_META)).child(if item.image { "▣" } else { "▤" }))
                    .child(div().max_w(px(180.)).truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_secondary)).child(SharedString::from(item.name.clone())))
                    .child(ui::meta(SharedString::from(super::attachments::size_label(item.size)), cx))
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
                    .px(px(10.))
                    .py(px(4.))
                    .child(ui::meta("queued", cx))
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
        let send = if busy {
            ui::button("stop-turn", "■ Stop", Kind::AmberOutline, cx)
                .h(px(t::SEND_BUTTON))
                .aria_label("Stop (esc esc)")
                .tooltip(ui::tip("Stop the run  esc esc"))
                .on_click(cx.listener(|this, _, _, cx| this.session.update(cx, |s, cx| s.cancel(cx))))
                .into_any_element()
        } else {
            let hover = c(tk.amber_hover);
            div()
                .id("send")
                .role(gpui::Role::Button)
                .aria_label(if self.prefs.send_with == "cmd-enter" { "Send (⌘↵)" } else { "Send (↵)" })
                .tooltip(ui::tip("Send"))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .size(px(t::SEND_BUTTON))
                .rounded(px(t::RADIUS_CARD))
                .bg(c(tk.amber))
                .text_color(c(tk.amber_on_fill))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .on_click(cx.listener(|this, _, window, cx| this.send_composer(window, cx)))
                .child("↵")
                .into_any_element()
        };
        div()
            .id("composer")
            .role(gpui::Role::Group)
            .aria_label("Composer")
            .capture_key_down(cx.listener(Self::composer_keys))
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(t::RADIUS_POPOVER))
            .bg(c(tk.bg_input))
            .border_1()
            .border_color(amber(cx, t::AMBER_BORDER_STRONG))
            .shadow(ring)
            .when(!queued.is_empty(), |col| col.child(div().flex().flex_col().pt(px(4.)).border_b_1().border_color(line(cx, t::LINE_SUBTLE)).children(queued)))
            .when(!chips.is_empty(), |col| col.child(div().flex().flex_wrap().gap(px(6.)).px(px(12.)).pt(px(10.)).children(chips)))
            .when_some(note, |col, note| col.child(div().px(px(14.)).pt(px(8.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.amber)).child(SharedString::from(note))))
            .child(div().px(px(14.)).pt(px(10.)).text_size(px(t::TEXT_BODY)).child(Textarea::new(&self.composer).appearance(false).aria_label("Message")))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(8.))
                    .pb(px(8.))
                    .pt(px(4.))
                    .child(ui::icon_button("attach", "+", cx).aria_label("Attach files (or paste / drop them)").tooltip(ui::tip("Attach files — or paste an image, or drop files here")).on_click(cx.listener(|this, _, window, cx| this.choose_attachments(window, cx))))
                    .child(ui::icon_button("dictate", "◉", cx).aria_label("Dictate (system dictation)").tooltip(ui::tip("Dictate — macOS dictation into the composer")).on_click(cx.listener(|this, _, window, cx| {
                        this.composer.update(cx, |s, cx| s.focus(window, cx));
                        cx.defer(|_| crate::platform::start_dictation());
                    })))
                    .child(trigger("insert-slash", "/", "commands", cx))
                    .child(trigger("insert-at", "@", "files", cx))
                    .child(trigger("insert-bang", "!", "shell", cx))
                    .child(div().flex_1())
                    .child(
                        ui::chip("mode-chip", SharedString::from(mode_text), cx)
                            .role(gpui::Role::Button)
                            .aria_label(SharedString::from(format!("Permission mode: {}. Shift+Tab to change", mode_label(&mode))))
                            .tooltip(ui::tip("Permission mode  ⇧Tab"))
                            .on_click(cx.listener(|this, _, window, cx| this.cycle_mode(&super::CycleMode, window, cx))),
                    )
                    .children(steer)
                    .child(send),
            )
            .into_any_element()
    }

    /// The composer's placeholder for the session's state.
    pub(super) fn placeholder(&self, cx: &App) -> &'static str {
        let view = &self.session.read(cx).view;
        if self.saying {
            "Say why (or answer in your own words) — ↵ sends, esc goes back"
        } else if !view.pending.is_empty() {
            "Reply to steer, or approve the command above…"
        } else if view.turn_active {
            "Queue a follow-up (↵), or steer the run now (⌥↵)…"
        } else if view.messages.iter().any(transcript::is_conversation) {
            "Follow up…"
        } else {
            "Ask Cordata…"
        }
    }
}

/// `opus-4` from `claude-opus-4-20250514`; names that are already short stay.
fn short_model(name: &str) -> String {
    let name = name.trim();
    let name = name.strip_prefix("claude-").unwrap_or(name);
    // Drop a trailing date stamp (`-20250514`).
    let name = match name.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => name,
    };
    name.chars().take(28).collect()
}

/// `shell(pnpm test)` → `pnpm test`: what "Always allow" adds, as the person reads it.
pub fn always_prefix(label: &str) -> String {
    let label = label.trim();
    match label.split_once('(') {
        Some((_, rest)) if rest.ends_with(')') => rest[..rest.len() - 1].trim().trim_end_matches(":*").trim_end_matches('*').trim().to_owned(),
        _ => label.to_owned(),
    }
}

fn attribution_label(tool: &str) -> String {
    match tool.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
        Some((server, name)) => format!("{name} from {server}"),
        None => tool.to_owned(),
    }
}

#[allow(dead_code)]
fn _glow(cx: &App) -> Vec<BoxShadow> {
    amber_glow(cx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_and_rules_read_short() {
        assert_eq!(short_model("claude-opus-4-20250514"), "opus-4");
        assert_eq!(short_model("kimi-k2.7-code"), "kimi-k2.7-code");
        assert_eq!(always_prefix("shell(pnpm test)"), "pnpm test");
        assert_eq!(always_prefix("shell(git status:*)"), "git status");
        assert_eq!(always_prefix("read"), "read");
        assert_eq!(mode_label("build"), "ask first");
        assert_eq!(mode_label("plan"), "plan only");
    }
}
