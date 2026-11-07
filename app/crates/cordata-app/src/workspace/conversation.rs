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
use gpui::{AnyElement, App, BoxShadow, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, StyledImage as _, div, point, px, relative};
use gpui_component::input::Textarea;

use super::{Overlay, TogglePanel, ToggleSidebar, Workspace, tilde};
use crate::theme::{amber, c, ca, inset_ring, line, mono, soft_shadow, tk};
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
        let has_brief = self.desk_cache.snap.brief_shown.is_some();
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
            // The thread's name is its brief, set under this row as the page's title; the row itself carries only state.
            .when(!has_brief && title != "New thread", |row| row.child(div().min_w(px(60.)).flex_shrink_1().truncate().text_size(px(t::TEXT_TITLE)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(shown(&title)))))
            .when_some(word, |row, (word, ink)| row.child(div().flex_shrink_0().whitespace_nowrap().text_size(px(t::TEXT_META)).text_color(c(ink)).child(word)))
            // The empty middle drags the window.
            .child(div().flex_1().h_full().min_w(px(8.)).on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()))
            .child(
                div()
                    .id("workspace-mode-selector")
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .p(px(2.))
                    .rounded(px(t::RADIUS_ROW))
                    .bg(ca(tk.bg_panel, 0.7))
                    .border_1()
                    .border_color(line(cx, t::LINE_DEFAULT))
                    .child(
                        div()
                            .id("mode-canvas-pill")
                            .role(gpui::Role::Button)
                            .cursor_pointer()
                            .px(px(7.))
                            .py(px(2.))
                            .rounded(px(t::RADIUS_BADGE))
                            .font_family(mono(cx))
                            .text_size(px(t::TEXT_META))
                            .border_1()
                            .border_color(if self.desk_mode == super::DeskMode::Rack { ca(tk.amber, 0.6) } else { gpui::transparent_black() })
                            .bg(if self.desk_mode == super::DeskMode::Rack { ca(tk.amber, 0.16) } else { gpui::transparent_black() })
                            .text_color(c(if self.desk_mode == super::DeskMode::Rack { tk.amber } else { tk.text_muted }))
                            .hover(|s| s.text_color(c(tk.text_primary)))
                            .tooltip(ui::tip("Supervisory Control Room & Agent Graph Canvas  ⌘0"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.desk_mode = super::DeskMode::Rack;
                                super::sheet::record_ux_event("mode_switch", serde_json::json!({ "mode": "rack" }));
                                cx.notify();
                            }))
                            .child("Canvas ⌘0")
                    )
                    .child(
                        div()
                            .id("mode-transcript-pill")
                            .role(gpui::Role::Button)
                            .cursor_pointer()
                            .px(px(7.))
                            .py(px(2.))
                            .rounded(px(t::RADIUS_BADGE))
                            .font_family(mono(cx))
                            .text_size(px(t::TEXT_META))
                            .border_1()
                            .border_color(if self.desk_mode == super::DeskMode::Transcript { ca(tk.amber, 0.6) } else { gpui::transparent_black() })
                            .bg(if self.desk_mode == super::DeskMode::Transcript { ca(tk.amber, 0.16) } else { gpui::transparent_black() })
                            .text_color(c(if self.desk_mode == super::DeskMode::Transcript { tk.amber } else { tk.text_muted }))
                            .hover(|s| s.text_color(c(tk.text_primary)))
                            .tooltip(ui::tip("Conversational steering transcript  ⌘1"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.desk_mode = super::DeskMode::Transcript;
                                super::sheet::record_ux_event("mode_switch", serde_json::json!({ "mode": "transcript" }));
                                cx.notify();
                            }))
                            .child("Transcript ⌘1")
                    )
                    .child(
                        div()
                            .id("mode-sheet-pill")
                            .role(gpui::Role::Button)
                            .cursor_pointer()
                            .px(px(7.))
                            .py(px(2.))
                            .rounded(px(t::RADIUS_BADGE))
                            .font_family(mono(cx))
                            .text_size(px(t::TEXT_META))
                            .border_1()
                            .border_color(if self.desk_mode == super::DeskMode::ChangeSheet { ca(tk.amber, 0.6) } else { gpui::transparent_black() })
                            .bg(if self.desk_mode == super::DeskMode::ChangeSheet { ca(tk.amber, 0.16) } else { gpui::transparent_black() })
                            .text_color(c(if self.desk_mode == super::DeskMode::ChangeSheet { tk.amber } else { tk.text_muted }))
                            .hover(|s| s.text_color(c(tk.text_primary)))
                            .tooltip(ui::tip("Proof-first change sheet and diff verification  ⌘D"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load_review(cx);
                                this.desk_mode = super::DeskMode::ChangeSheet;
                                super::sheet::record_ux_event("mode_switch", serde_json::json!({ "mode": "sheet" }));
                                cx.notify();
                            }))
                            .child("Sheet ⌘D")
                    )
            )
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
        let open_canvas = starter("starter-canvas", "Open Supervisory Control Room & Agent Graph  ⌘0".into()).on_click(cx.listener(|this, _, _, cx| {
            this.toggle_rack(cx);
        }));
        let open_sheet = starter(
            "starter-sheet",
            SharedString::from(format!("Open Proof-First Change Sheet ({} uncommitted file{})  ⌘D", dirty_count, if dirty_count == 1 { "" } else { "s" })),
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_change_sheet(cx)));

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
                    // A blank brief: the project, the question, and what "done" will mean here.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .font_family(mono(cx))
                            .text_size(px(t::TEXT_META))
                            .text_color(c(tk.text_faint))
                            .child(ui::cordata_logo(18., cx))
                            .child(SharedString::from(project)),
                    )
                    .child(div().font_family(crate::theme::voice(cx)).text_size(px(30.)).line_height(relative(1.2)).text_color(c(tk.text_primary)).child("What should change?"))
                    .child(
                        div()
                            .text_size(px(t::TEXT_SMALL))
                            .text_color(c(tk.text_muted))
                            .child(SharedString::from(match self.check_command() {
                                Some(check) => format!("Say it in your own words. Here, done means {check} passes; you decide at every gate."),
                                None => "Say it in your own words; you decide at every gate.".to_owned(),
                            })),
                    )
                    .child(div().flex().flex_col().child(open_canvas).child(open_sheet).child(explore).child(review).child(fix))
                    .children(notices)
                    // The session's opening notes, as one line: hover lists them. A server that failed to start is
                    // the one note worth its own words.
                    .when(!notes.is_empty(), |col| {
                        let failed = notes.iter().filter(|n| n.contains("did not start") || n.contains("error")).count();
                        let said = match failed {
                            0 => format!("{} startup note{}", notes.len(), if notes.len() == 1 { "" } else { "s" }),
                            f => format!("{} startup notes · {f} need a look", notes.len()),
                        };
                        let list: Vec<String> = notes.iter().map(|n| shown(n)).collect();
                        col.child(
                            div()
                                .id("startup-notes")
                                .pt(px(6.))
                                .font_family(mono(cx))
                                .text_size(px(t::TEXT_META))
                                .text_color(c(if failed > 0 { tk.attention } else { tk.text_faint }))
                                .tooltip(ui::tip(SharedString::from(list.join("\n"))))
                                .child(SharedString::from(said)),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Whether the frontmost pending approval is a high-risk gate that has not yet been unlocked via its risk token.
    pub(super) fn active_gate_locked(&self, cx: &App) -> bool {
        let ses = self.session.read(cx);
        let Some(pending) = ses.view.pending.front() else {
            return false;
        };
        let info = &pending.info;
        if info.tool_name == "ask_question" {
            return false;
        }
        let analysis = acp_core::auto::gate_analysis(
            &info.tool_name,
            info.raw_input.as_ref(),
            &ses.cwd,
            info.reason.as_deref(),
        );
        analysis.high_risk && self.gate_unlocked.as_deref() != Some(info.tool_call_id.as_str())
    }

    pub(super) fn answer_gate(&mut self, decision: ApprovalDecision, cx: &mut Context<Self>) {
        let (tool, high_risk) = {
            let ses = self.session.read(cx);
            ses.view
                .pending
                .front()
                .map(|pending| {
                    let info = &pending.info;
                    let a = acp_core::auto::gate_analysis(
                        &info.tool_name,
                        info.raw_input.as_ref(),
                        &ses.cwd,
                        info.reason.as_deref(),
                    );
                    (info.tool_name.clone(), a.high_risk)
                })
                .unwrap_or_default()
        };
        let decision_tag = match &decision {
            ApprovalDecision::Allow => "allow",
            ApprovalDecision::AllowAlways => "always",
            ApprovalDecision::Reject => "deny",
            ApprovalDecision::Say(_) => "reply",
        };
        super::sheet::record_ux_event(
            "gate_decision",
            serde_json::json!({ "tool": tool, "high_risk": high_risk, "decision": decision_tag }),
        );
        self.gate_unlocked = None;
        self.gate_hint = None;
        self.saying = false;
        self.session.update(cx, |session, cx| session.answer(decision, cx));
        cx.notify();
    }

    pub(super) fn approval_card(&self, info: &PermissionInfo, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let input = info.raw_input.as_ref();
        let command = input
            .and_then(|i| i.get("cmd").or_else(|| i.get("command")))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let summary = input.and_then(cordata_view::view::summarize_input).unwrap_or_default();
        let cwd = self.session.read(cx).cwd.clone();
        let place = tilde(&cwd);
        // Display only: the target is shortened as the transcript shows it; `raw_input` stays whole.
        let fold = cordata_view::fold::Context::here(cwd.clone());
        let diffs = self.session.read(cx).view.card(&info.tool_call_id).map(|card| card.diffs.clone()).filter(|d| !d.is_empty());
        let has_diff = diffs.is_some();
        let tool = info.tool_name.as_str();
        let is_ask_question = info.tool_name == "ask_question";
        let analysis = acp_core::auto::gate_analysis(tool, input, &cwd, info.reason.as_deref());
        let locked = !is_ask_question && analysis.high_risk && self.gate_unlocked.as_deref() != Some(info.tool_call_id.as_str());
        let risk_token = analysis.risk_token.clone().unwrap_or_else(|| "high-risk call".to_owned());
        // What saying yes means, in one line: an edit can be taken back, a command's effects cannot.
        let consequence: Option<&'static str> = match (is_ask_question, info.kind, command.is_some()) {
            (true, ..) => None,
            (_, acp_core::ToolKind::Edit, _) => Some("An edit: you can revert it afterwards in Changes."),
            (_, _, true) => Some("A command: it runs inside the sandbox, and what it does is not undone automatically."),
            _ => None,
        };
        let (_, sentence, block, buttons): (&str, String, Option<String>, Vec<AnyElement>) = {
            if is_ask_question {
                let questions_arr = input.and_then(|i| i.get("questions")).and_then(|q| q.as_array());
                let mut q_prompts = Vec::new();
                let mut buttons = Vec::new();
                if let Some(questions) = questions_arr {
                    for (q_ix, q) in questions.iter().enumerate() {
                        if let Some(text) = q.get("question").and_then(|v| v.as_str()) {
                            q_prompts.push(text.to_string());
                        }
                        if let Some(opts) = q.get("options").and_then(|v| v.as_array()) {
                            for (ix, opt) in opts.iter().enumerate() {
                                if let Some(opt_str) = opt.as_str() {
                                    let choice = opt_str.to_string();
                                    let is_rec = choice.starts_with("(Recommended)");
                                    buttons.push(
                                        ui::button(
                                            ("opt", q_ix * 100 + ix),
                                            SharedString::from(choice.clone()),
                                            if is_rec { Kind::Primary } else { Kind::Ghost },
                                            cx,
                                        )
                                        .on_click(cx.listener(move |this, _, _, cx| this.answer_gate(ApprovalDecision::Say(choice.clone()), cx)))
                                        .into_any_element(),
                                    );
                                }
                            }
                        }
                    }
                }
                let q_prompt = if q_prompts.is_empty() {
                    "Clarification requested by agent:".to_string()
                } else {
                    q_prompts.join(" · ")
                };
                buttons.push(
                    ui::button("say-why", "Other Answer...", Kind::Ghost, cx)
                        .tooltip(ui::tip("Type your own answer in the composer  W"))
                        .on_click(cx.listener(|this, _, window, cx| this.begin_saying(window, cx)))
                        .into_any_element(),
                );
                buttons.push(div().flex_1().into_any_element());
                buttons.push(
                    ui::button("reject", "Dismiss", Kind::Ghost, cx)
                        .tooltip(ui::tip("Dismiss  esc"))
                        .on_click(cx.listener(|this, _, _, cx| this.answer_gate(ApprovalDecision::Reject, cx)))
                        .into_any_element(),
                );
                ("", q_prompt, None, buttons)
            } else {
                let sentence = match (&info.reason, &command, info.kind) {
                    // Auto mode let everything else through: say why this one still needs a person.
                    (Some(reason), ..) => format!("Auto mode asks: {}", shown(reason)),
                    (None, Some(_), _) => format!("Run shell command in {place}"),
                    (None, None, acp_core::ToolKind::Edit) => format!("Modify files in {place}"),
                    _ => format!("Allow {tool}?"),
                };
                // Program-scoped session grant ("Always allow run_command (cargo) this session") surfaces "(cargo)" right on the button.
                let always_short = info.always_label.as_deref().and_then(|lbl| {
                    let start = lbl.find('(')?;
                    let end = lbl[start..].find(')')? + start;
                    Some(format!("Always {}", &lbl[start..=end]))
                });
                let always = (!locked).then_some(info.always_label.as_ref()).flatten().map(|label| {
                    let text: SharedString = always_short.clone().unwrap_or_else(|| "Always".to_owned()).into();
                    ui::button("always", text, Kind::Ghost, cx)
                        .tooltip(ui::tip(SharedString::from(shown(label))))
                        .on_click(cx.listener(|this, _, _, cx| this.answer_gate(ApprovalDecision::AllowAlways, cx)))
                        .into_any_element()
                });
                let allow_btn = if locked {
                    let hint_tok = risk_token.clone();
                    ui::button("allow", "Allow", Kind::Outline, cx)
                        .tooltip(ui::tip("Confirm the risk token first, or press ⌘Y"))
                        .child(div().text_color(c(tk.text_faint)).font_family(mono(cx)).text_size(px(t::TEXT_META)).child("⌘Y"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.gate_hint = Some(format!("High-risk gate: click Confirm {hint_tok} or press ⌘Y to allow."));
                            cx.notify();
                        }))
                        .into_any_element()
                } else {
                    ui::button("allow", "Allow", Kind::Primary, cx)
                        .child(div().text_color(c(tk.amber_dim_2)).font_family(mono(cx)).text_size(px(t::TEXT_META)).child("↵"))
                        .on_click(cx.listener(|this, _, _, cx| this.answer_gate(ApprovalDecision::Allow, cx)))
                        .into_any_element()
                };
                let mut buttons = vec![allow_btn];
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
                        .on_click(cx.listener(|this, _, _, cx| this.answer_gate(ApprovalDecision::Reject, cx)))
                        .into_any_element(),
                );
                ("", sentence, command.clone().map(|cmd| fold.shorten(&format!("$ {cmd}"))).or_else(|| (!summary.is_empty()).then(|| fold.shorten(&summary))), buttons)
            }
        };
        let counterfactual = (!is_ask_question).then(|| {
            let allowed = shown(&fold.shorten(&analysis.if_allowed));
            let denied = shown(&analysis.if_denied);
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .font_family(mono(cx))
                .text_size(px(t::TEXT_META))
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(div().flex_shrink_0().text_color(c(tk.text_faint)).child("If allowed →"))
                        .child(div().min_w_0().truncate().text_color(c(tk.text_secondary)).child(SharedString::from(allowed))),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(div().flex_shrink_0().text_color(c(tk.text_faint)).child("If denied (esc) →"))
                        .child(div().min_w_0().truncate().text_color(c(tk.text_muted)).child(SharedString::from(denied))),
                )
                .into_any_element()
        });
        let challenge = (!is_ask_question && analysis.high_risk).then(|| {
            let call_id = info.tool_call_id.clone();
            let tok_log = risk_token.clone();
            if locked {
                let hover = ca(tk.attention, 0.18);
                div()
                    .id("gate-risk-token")
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(format!("Confirm high-risk token {risk_token}")))
                    .tooltip(ui::tip("Click to unlock Allow, or press ⌘Y"))
                    .self_start()
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(9.))
                    .py(px(4.))
                    .rounded(px(t::RADIUS_CONTROL))
                    .bg(ca(tk.attention, 0.10))
                    .border_1()
                    .border_color(ca(tk.attention, 0.40))
                    .hover(move |s| s.bg(hover))
                    .font_family(mono(cx))
                    .text_size(px(t::TEXT_SMALL))
                    .text_color(c(tk.attention))
                    .child(SharedString::from(format!("Confirm {}", shown(&risk_token))))
                    .child(div().text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child("unlocks ↵ · or ⌘Y"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.gate_unlocked = Some(call_id.clone());
                        this.gate_hint = None;
                        super::sheet::record_ux_event("gate_token_confirmed", serde_json::json!({ "token": tok_log }));
                        cx.notify();
                    }))
                    .into_any_element()
            } else {
                div()
                    .id("gate-risk-token")
                    .self_start()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(t::RADIUS_CONTROL))
                    .bg(ca(tk.amber, 0.12))
                    .font_family(mono(cx))
                    .text_size(px(t::TEXT_META))
                    .text_color(c(tk.amber))
                    .child(SharedString::from(format!("Confirmed {} · press ↵ to allow", shown(&risk_token))))
                    .into_any_element()
            }
        });
        // Not a box: a 2px attention rule down the left, the question, the command, the counterfactual, the answers.
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
            .gap(px(9.))
            .child(attention_rule(&info.tool_call_id, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(ui::mark("gate-mark", ui::MarkState::NeedsYou, 13., cx))
                    .child(div().text_size(px(t::TEXT_BODY)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(shown(&sentence))))
                    .when(self.saying, |row| row.child(ui::badge("type below · ↵ sends", Tone::Amber, cx))),
            )
            // An edit shows the change it asks about; anything else, its command or input.
            .when_some(diffs, |card, diffs| card.child(transcript::diff_preview(&diffs, cx)))
            .when(!has_diff, |card| card.when_some(block, |card, text| card.child(ui::code_block(SharedString::from(shown(&text)), cx).text_color(c(tk.text_body)))))
            .children(counterfactual)
            .children(challenge)
            .child(div().flex().flex_wrap().items_center().gap(px(6.)).children(buttons))
            .when_some(self.gate_hint.clone(), |card, hint| {
                card.child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.attention)).child(SharedString::from(hint)))
            })
            .when_some(consequence, |card, said| card.child(div().text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(said)))
            .into_any_element()
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
                    .child(self.mode_chip(cx))
                    .when_some(model, |row, model| row.child(self.model_trigger(model, cx)))
                    .children(action),
            )
            .into_any_element()
    }

    /// The approval mode, next to the model: `ask` muted, `auto` in amber. A click (or Shift+Tab) switches it.
    pub(super) fn mode_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let auto = self.session.read(cx).view.mode == acp_core::Mode::Auto;
        let hover = line(cx, t::HOVER);
        div()
            .id("mode-chip")
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(if auto { "Approval mode: auto — switch to ask" } else { "Approval mode: ask — switch to auto" }))
            .tooltip(ui::tip(if auto { "Auto: safe calls run without asking  ⇧⇥" } else { "Ask: every edit and command asks first  ⇧⇥" }))
            .flex_shrink_0()
            .flex()
            .items_center()
            .px(px(6.))
            .h(px(22.))
            .rounded(px(t::RADIUS_ROW))
            .cursor_pointer()
            .font_family(mono(cx))
            .text_color(c(if auto { tk.amber } else { tk.text_muted }))
            .when(auto, |el| el.bg(ca(tk.amber, 0.12)))
            .hover(move |s| s.bg(hover))
            .child(if auto { "auto" } else { "ask" })
            .on_click(cx.listener(|this, _, _, cx| this.cycle_mode(cx)))
            .into_any_element()
    }

    pub(super) fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| session.cycle_mode(cx));
        cx.notify();
    }

    /// The composer's placeholder for the session's state.
    pub(super) fn placeholder(&self, cx: &App) -> &'static str {
        let view = &self.session.read(cx).view;
        if self.desk_mode == super::DeskMode::Rack {
            "Message active thread (↵), click any thread above, or press ⌘0 to return…"
        } else if self.saying {
            "Reply in your own words — ↵ sends, esc returns to approval"
        } else if !view.pending.is_empty() {
            if self.active_gate_locked(cx) {
                "High-risk gate above — confirm token (⌘Y) to allow, esc denies, W redirects…"
            } else {
                "Approve above (↵), or press W to redirect in your own words…"
            }
        } else if view.turn_active {
            "Queue a follow-up (↵)…"
        } else if view.messages.iter().any(transcript::is_conversation) {
            "Steer, ask, or add to the brief — @ files, / commands…"
        } else {
            "Describe the change you want — @ files, / commands…"
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
