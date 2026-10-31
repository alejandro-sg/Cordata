//! The conversation column: the 52px header (sidebar toggle when collapsed, title, status, branch and model chips,
//! inspector toggle), the welcome hero with starter cards & folder trust banner, the approval card, post-reply
//! actions, the `/` and `@` menu, mode & model popovers, and the unified composer.

use acp_core::{ApprovalDecision, PermissionInfo};
use cordata_theme::tokens as t;
use cordata_view::app::{ConnState, Message};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, BoxShadow, ClickEvent, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, StyledImage as _, Window, div, point, px, relative};
use gpui_component::input::Textarea;

use super::{DockTab, Overlay, TogglePanel, ToggleSidebar, Workspace, tilde};
use crate::theme::{amber, amber_glow, c, inset_ring, line, mono, soft_shadow, tk};
use crate::transcript;
use crate::ui::{self, Dot, Kind, Tone};

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

/// The permission mode in canonical lowercase form.
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

/// Human-friendly icon and Title Case label for a permission mode.
pub fn mode_badge(mode: &str) -> (&'static str, &'static str) {
    match mode {
        "plan" => ("◎", "Plan only"),
        "build" | "" => ("◈", "Ask first"),
        "edit" => ("✎", "Auto-edit"),
        "auto" => ("⚡", "Auto"),
        _ => ("◈", "Custom"),
    }
}

impl Workspace {
    fn auto_routing_enabled(&self) -> bool {
        self.settings_view.values.get("orchestrator.dispatch").and_then(|v| v.as_str()) == Some("auto")
    }

    pub(super) fn header(&self, column_w: f32, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let title = view.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "New thread".into());
        let (dot, word, ink) = match status_of(view) {
            Status::Connecting => (Dot::Hollow, "Connecting…", tk.text_muted),
            Status::Idle => (Dot::Ok, "Ready", tk.teal),
            Status::Running => (Dot::Live, "Working…", tk.amber),
            Status::Waiting => (Dot::Live, "Needs approval", tk.amber),
            Status::Closed => (Dot::Amber, "Reconnecting…", tk.amber),
        };
        let auto_on = self.auto_routing_enabled();
        let model = view.config("model").map(|option| option.current_name().to_owned());
        let window_k = view.context.as_ref().and_then(|g| g.window).map(|w| format!("{}k", w / 1000));
        let model_text = model.map(|m| {
            let m = short_model(&m);
            let prefix = if auto_on { "⚡ " } else { "" };
            match (&window_k, column_w >= t::HEADER_SHORT_MODEL_W) {
                (Some(k), true) => format!("{prefix}{m} · {k} ⌄"),
                _ => format!("{prefix}{m} ⌄"),
            }
        });
        let branch = view.branch.clone().or_else(|| self.git.branch.clone()).filter(|_| column_w >= t::HEADER_HIDE_BRANCH_W);
        let panel_on = self.panel_open;
        let sidebar_on = self.sidebar_open;
        div()
            .id("conversation-header")
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(10.))
            .h(px(t::HEADER_H))
            // Reserve space for macOS traffic lights when the left sidebar is hidden.
            .pl(px(if sidebar_on { 20. } else { 78. }))
            .pr(px(16.))
            .overflow_hidden()
            .border_b_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .when(!sidebar_on, |row| {
                row.child(
                    ui::icon_button("show-sidebar", "◧", cx)
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
            .child(div().min_w(px(60.)).flex_shrink_1().truncate().text_size(px(t::TEXT_TITLE)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(shown(&title))))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(t::RADIUS_BADGE))
                    .bg(line(cx, t::FILL_FAINT))
                    .child(ui::dot(dot, cx).size(px(t::DOT_SMALL)))
                    .child(div().whitespace_nowrap().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(ink)).child(word)),
            )
            // The empty middle drags the window.
            .child(div().flex_1().h_full().min_w(px(8.)).on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()))
            .when_some(branch, |row, branch| {
                row.child(
                    ui::chip("branch-chip", SharedString::from(format!("⎇ {branch}")), cx)
                        .role(gpui::Role::Button)
                        .aria_label(SharedString::from(format!("Branch {branch} — view changes")))
                        .tooltip(ui::tip("Open Changes & Git inspector  ⌘⌥4"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_panel(true, cx);
                            this.switch_dock(DockTab::Review, cx);
                        })),
                )
            })
            .children(self.pr_chip(cx))
            .when_some(model_text, |row, text| {
                row.child(
                    ui::chip("model-picker", SharedString::from(text), cx)
                        .role(gpui::Role::Button)
                        .aria_label("Model and routing")
                        .tooltip(ui::tip("Choose model or toggle Auto Mode routing"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.mode_menu = false;
                            if this.model_menu && !this.model_menu_bottom {
                                this.model_menu = false;
                            } else {
                                this.model_menu = true;
                                this.model_menu_bottom = false;
                            }
                            cx.notify();
                        })),
                )
            })
            .child(
                ui::chip("panel-toggle", "⊟ Inspector", cx)
                    .role(gpui::Role::Button)
                    .aria_label("Toggle side panel (⌘\\)")
                    .aria_toggled(panel_on.into())
                    .tooltip(ui::tip("Toggle side panel  ⌘\\"))
                    .when(panel_on, |chip| chip.bg(amber(cx, t::AMBER_FILL)).border_color(amber(cx, t::AMBER_BORDER)).text_color(c(tk.text_primary)))
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
        let auto_on = self.auto_routing_enabled();
        let auto_hover = line(cx, t::HOVER_STRONG);
        let rows = option.choices.iter().enumerate().map(|(index, choice)| {
            let current = choice.value == option.current_value;
            let value = choice.value.clone();
            let is_cloud = choice.value.contains(":cloud") || choice.name.contains(":cloud");
            ui::row(("model", index), current, cx)
                .py(px(6.))
                .child(div().w(px(12.)).text_color(c(tk.amber)).child(if current { "●" } else { "" }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(mono(cx))
                        .text_size(px(t::TEXT_SMALL))
                        .text_color(c(if current { tk.amber } else { tk.text_primary }))
                        .child(SharedString::from(shown(&choice.name))),
                )
                .when(is_cloud, |row| row.child(ui::badge("cloud", Tone::Teal, cx)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let value = value.clone();
                    this.model_menu = false;
                    this.session.update(cx, |session, cx| session.set_config("model", &value, cx));
                }))
        });
        let popover = ui::popover(cx)
            .id("model-menu")
            .role(gpui::Role::Menu)
            .aria_label("Models and Auto Mode")
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.model_menu = false;
                cx.notify();
            }))
            .absolute()
            .when(!self.model_menu_bottom, |p| p.top(px(t::HEADER_H - 4.)).right(px(90.)))
            .when(self.model_menu_bottom, |p| p.bottom(px(88.)).right(px(32.)))
            .w(px(340.))
            .max_h(px(460.))
            .overflow_y_scroll()
            .p(px(6.))
            .child(
                div()
                    .id("toggle-auto-routing")
                    .role(gpui::Role::MenuItemCheckBox)
                    .aria_label("Toggle Auto Mode routing")
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(10.))
                    .p(px(8.))
                    .mb(px(4.))
                    .rounded(px(t::RADIUS_CARD))
                    .bg(if auto_on { amber(cx, t::AMBER_FILL) } else { line(cx, t::FILL_FAINT) })
                    .border_1()
                    .border_color(if auto_on { amber(cx, t::AMBER_BORDER) } else { line(cx, t::LINE_SUBTLE) })
                    .cursor_pointer()
                    .hover(move |s| s.bg(auto_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let next = if auto_on { "live" } else { "auto" };
                        this.set_user_setting("orchestrator.dispatch", serde_json::json!(next), cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .text_size(px(t::TEXT_UI))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(c(tk.text_primary))
                                    .child("⚡ Auto Mode Routing"),
                            )
                            .child(
                                div()
                                    .text_size(px(t::TEXT_META))
                                    .line_height(relative(1.35))
                                    .text_color(c(tk.text_muted))
                                    .child("Dynamically selects Claude, Antigravity, or Ollama :cloud per task"),
                            ),
                    )
                    .child(ui::badge(if auto_on { "ON" } else { "OFF" }, if auto_on { Tone::Amber } else { Tone::Neutral }, cx)),
            )
            .child(div().px(px(6.)).py(px(4.)).child(ui::section_label("primary model", cx)))
            .children(rows);
        Some(popover.into_any_element())
    }

    pub(super) fn mode_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.mode_menu {
            return None;
        }
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let current = view.pending_mode.clone().unwrap_or_else(|| view.mode.clone());
        let current = if current.is_empty() { "build".to_owned() } else { current };
        let trusted = self.settings_view.trusted;
        let specs: [(&str, &str, &str, &str); 4] = [
            ("plan", "◎", "Plan only", "Read, search, and design an implementation plan without changing files."),
            ("build", "◈", "Ask first", "Ask for your approval before editing files or running shell commands."),
            ("edit", "✎", "Auto-edit", "Apply file edits automatically; still ask before shell commands."),
            ("auto", "⚡", "Auto", "Run edits and safe commands autonomously in trusted workspaces."),
        ];
        let available: Vec<String> = view.modes.iter().map(|m| m.id.clone()).collect();
        let rows = specs.into_iter().enumerate().filter(|(_, (id, ..))| available.is_empty() || available.iter().any(|m| m == id)).map(|(index, (id, icon, title, desc))| {
            let selected = current == id;
            let needs_trust = id == "auto" && !trusted;
            let mode_id = id.to_owned();
            ui::row(("mode-choice", index), selected, cx)
                .items_start()
                .py(px(7.))
                .child(div().w(px(16.)).pt(px(1.)).text_color(c(if selected { tk.amber } else { tk.text_secondary })).child(icon))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(
                                    div()
                                        .text_size(px(t::TEXT_UI))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(c(if selected { tk.amber } else { tk.text_primary }))
                                        .child(title),
                                )
                                .when(needs_trust, |r| r.child(ui::badge("needs trust", Tone::Amber, cx))),
                        )
                        .child(div().text_size(px(t::TEXT_META)).line_height(relative(1.35)).text_color(c(tk.text_muted)).child(desc)),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.mode_menu = false;
                    if needs_trust {
                        this.ask_trust(window, cx);
                    } else {
                        let mode_id = mode_id.clone();
                        this.session.update(cx, |session, cx| session.set_mode(&mode_id, cx));
                    }
                    cx.notify();
                }))
        });
        Some(
            ui::popover(cx)
                .id("mode-menu")
                .role(gpui::Role::Menu)
                .aria_label("Permission mode")
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.mode_menu = false;
                    cx.notify();
                }))
                .absolute()
                .bottom(px(88.))
                .right(px(136.))
                .w(px(332.))
                .p(px(6.))
                .child(div().px(px(8.)).py(px(4.)).child(ui::section_label("execution mode", cx)))
                .children(rows)
                .child(
                    div()
                        .px(px(8.))
                        .pt(px(6.))
                        .pb(px(2.))
                        .border_t_1()
                        .border_color(line(cx, t::LINE_SUBTLE))
                        .child(ui::meta("⇧Tab cycles Plan → Ask first → Auto-edit", cx)),
                )
                .into_any_element(),
        )
    }

    /// The column when there is nothing said yet: brand mark, greeting, context pills, folder trust banner,
    /// 2×2 clickable starter cards, resume thread card, and shortcut hints.
    pub(super) fn empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let project = self.project_name();
        let trusted = self.settings_view.trusted;
        let dirty_count = self.review.file_count();
        let branch = self.session.read(cx).view.branch.clone().or_else(|| self.git.branch.clone());
        let resume = self.listing.iter().find(|row| !self.sessions.iter().any(|s| super::matches_target(s.read(cx), &row.target))).cloned();
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
                        .max_w(px(620.))
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(t::RADIUS_CARD))
                        .border_1()
                        .border_color(c(tk.danger))
                        .bg(line(cx, t::FILL_FAINT))
                        .text_size(px(t::TEXT_SMALL))
                        .text_color(c(tk.danger))
                        .child(SharedString::from(shown(e)))
                        .into_any_element(),
                ),
                _ => None,
            })
            .collect();

        let starter_card = |id: &'static str, icon: &'static str, title: &'static str, subtitle: String, cx: &mut Context<Self>| {
            let hover_bg = line(cx, t::HOVER_STRONG);
            let hover_border = amber(cx, t::AMBER_BORDER);
            div()
                .id(id)
                .role(gpui::Role::Button)
                .flex_1()
                .min_w(px(240.))
                .flex()
                .items_start()
                .gap(px(10.))
                .p(px(12.))
                .rounded(px(t::RADIUS_CARD))
                .bg(line(cx, t::FILL_FAINT))
                .border_1()
                .border_color(line(cx, t::LINE_CONTROL))
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg).border_color(hover_border))
                .child(
                    div()
                        .size(px(26.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(t::RADIUS_CONTROL))
                        .bg(amber(cx, t::AMBER_FILL))
                        .font_family(mono(cx))
                        .text_size(px(t::TEXT_SMALL))
                        .text_color(c(tk.amber))
                        .child(icon),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_size(px(t::TEXT_UI)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(title))
                        .child(div().text_size(px(t::TEXT_SMALL)).line_height(relative(1.35)).text_color(c(tk.text_muted)).child(SharedString::from(subtitle))),
                )
        };

        let review_sub = if dirty_count > 0 {
            format!("Audit {dirty_count} uncommitted file{} for bugs & edge cases", if dirty_count == 1 { "" } else { "s" })
        } else {
            "Audit recent git changes for bugs, regressions & missing tests".into()
        };

        let card_explore = starter_card("starter-explore", "⌕", "Explore codebase", "Map architecture, key modules, and main entry points".into(), cx)
            .on_click(cx.listener(|this, _, window, cx| {
                this.apply_starter("Map out the architecture, key modules, and main entry points of this repository.", Some("plan"), false, window, cx);
            }))
            .into_any_element();

        let card_review = starter_card("starter-review", "⎇", "Review changes", review_sub, cx)
            .on_click(cx.listener(move |this, _, window, cx| {
                if dirty_count > 0 {
                    this.request_review(false, window, cx);
                } else {
                    this.apply_starter("Review the most recent changes in this repository for bugs, edge cases, and test coverage gaps.", Some("plan"), false, window, cx);
                }
            }))
            .into_any_element();

        let card_plan = starter_card("starter-plan", "◎", "Plan a feature", "Draft a grounded step-by-step implementation plan".into(), cx)
            .on_click(cx.listener(|this, _, window, cx| {
                this.apply_starter("Design a step-by-step implementation plan for: ", Some("plan"), false, window, cx);
            }))
            .into_any_element();

        let card_fix = starter_card("starter-fix", "⚡", "Run checks & fix issues", "Run tests or linters, diagnose failures, and patch".into(), cx)
            .on_click(cx.listener(|this, _, window, cx| {
                this.apply_starter("Run the project's test suite and build checks, diagnose any failures, and propose clean fixes.", None, false, window, cx);
            }))
            .into_any_element();

        let hint = |key: &'static str, word: &'static str, cx: &App| {
            div()
                .flex()
                .items_center()
                .flex_shrink_0()
                .whitespace_nowrap()
                .gap(px(5.))
                .child(ui::keycap(key, cx))
                .child(div().text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child(word))
        };

        div()
            .id("empty-state-scroll")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px(px(24.))
            .py(px(20.))
            .child(
                div()
                    .w_full()
                    .max_w(px(620.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(16.))
                    .child(crate::hud::bracket_title("Cordata", cx))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .justify_center()
                            .gap_x(px(6.))
                            .text_center()
                            .text_size(px(t::TEXT_EMPTY))
                            .line_height(relative(1.3))
                            .text_color(c(tk.text_body))
                            .child("What should we build in")
                            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(c(tk.text_primary)).child(SharedString::from(format!("{project}?")))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_center()
                            .gap(px(8.))
                            .child(
                                ui::chip("empty-change-project", SharedString::from(format!("{}  ⌄", tilde(&self.cwd))), cx)
                                    .role(gpui::Role::Button)
                                    .aria_label("Change project (⌘O)")
                                    .tooltip(ui::tip("Switch workspace folder  ⌘O"))
                                    .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Picker, window, cx))),
                            )
                            .when_some(branch, |row, b| {
                                row.child(
                                    ui::chip("empty-branch", SharedString::from(format!("⎇ {b}")), cx)
                                        .role(gpui::Role::Button)
                                        .tooltip(ui::tip("Inspect git status & changes  ⌘⌥4"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.set_panel(true, cx);
                                            this.switch_dock(DockTab::Review, cx);
                                        })),
                                )
                            }),
                    )
                    .when(!trusted, |col| {
                        col.child(
                            div()
                                .id("empty-trust-banner")
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(12.))
                                .px(px(14.))
                                .py(px(10.))
                                .rounded(px(t::RADIUS_CARD))
                                .bg(amber(cx, t::AMBER_FILL))
                                .border_1()
                                .border_color(amber(cx, t::AMBER_BORDER))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .gap(px(2.))
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(6.))
                                                .text_size(px(t::TEXT_SMALL))
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(c(tk.text_primary))
                                                .child(ui::diamond(cx))
                                                .child("Restricted folder mode"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(t::TEXT_META))
                                                .text_color(c(tk.text_secondary))
                                                .child("Cordata asks before every file write or shell command, and holds back project MCP servers until you trust this folder."),
                                        ),
                                )
                                .child(
                                    ui::button("empty-trust-btn", "Trust folder", Kind::AmberOutline, cx)
                                        .h(px(28.))
                                        .on_click(cx.listener(|this, _, window, cx| this.ask_trust(window, cx))),
                                ),
                        )
                    })
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .child(div().w_full().flex().flex_wrap().gap(px(10.)).child(card_explore).child(card_review))
                            .child(div().w_full().flex().flex_wrap().gap(px(10.)).child(card_plan).child(card_fix)),
                    )
                    .when_some(resume, |col, row| {
                        let target = row.target.clone();
                        let hover = line(cx, t::HOVER_STRONG);
                        col.child(
                            div()
                                .id("resume-last")
                                .role(gpui::Role::Button)
                                .aria_label("Resume the last conversation")
                                .w_full()
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .h(px(36.))
                                .px(px(14.))
                                .rounded(px(t::RADIUS_CARD))
                                .bg(line(cx, t::FILL_FAINT))
                                .border_1()
                                .border_color(line(cx, t::LINE_CONTROL))
                                .cursor_pointer()
                                .hover(move |s| s.bg(hover))
                                .on_click(cx.listener(move |this, _, window, cx| this.open(target.clone(), window, cx)))
                                .child(ui::badge("Resume", Tone::Neutral, cx))
                                .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_UI)).text_color(c(tk.text_primary)).child(SharedString::from(shown(&row.title))))
                                .child(ui::meta(SharedString::from(format!("{} ago →", super::age(super::now_secs() - row.when))), cx)),
                        )
                    })
                    .child(
                        div()
                            .pt(px(2.))
                            .flex()
                            .flex_wrap()
                            .justify_center()
                            .gap_x(px(14.))
                            .gap_y(px(6.))
                            .child(hint("/", "commands", cx))
                            .child(hint("@", "mention file", cx))
                            .child(hint("⇧Tab", "mode", cx))
                            .child(hint("⌘K", "palette", cx))
                            .child(hint("⌘B", "sidebar", cx))
                            .child(hint("⌘\\", "inspector", cx)),
                    )
                    .children(notices),
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
        let command = input.and_then(|i| i.get("command")).and_then(|v| v.as_str()).map(str::to_owned);
        let summary = input.and_then(|i| cordata_view::view::summarize_input(i, false)).unwrap_or_default();
        let place = tilde(&self.session.read(cx).cwd);
        let (label, sentence, block, buttons): (&str, String, Option<String>, Vec<AnyElement>) = match info.tool_name.as_str() {
            "present_plan" => (
                "plan ready",
                "The implementation plan is ready. Choose how you want Cordata to proceed:".into(),
                None,
                [("approve_build", "Build (ask first)", Kind::Primary), ("approve_edit", "Auto-edit", Kind::AmberOutline), ("approve_auto", "Run autonomously", Kind::Outline)]
                    .into_iter()
                    .filter(|(id, ..)| info.option(id).is_some())
                    .map(|(id, label, kind)| ui::button(SharedString::from(id), label, kind, cx).on_click(answer(ApprovalDecision::Choose(id.into()))).into_any_element())
                    .chain([
                        ui::button("reply-plan", "Refine plan  W", Kind::Ghost, cx)
                            .tooltip(ui::tip("Type feedback to revise the plan"))
                            .on_click(cx.listener(|this, _, window, cx| this.begin_saying(window, cx)))
                            .into_any_element(),
                        div().flex_1().into_any_element(),
                        ui::button("keep-planning", "Keep planning  esc", Kind::Ghost, cx).on_click(answer(ApprovalDecision::Reject)).into_any_element(),
                    ])
                    .collect(),
            ),
            "ask_user" => (
                "question",
                input.and_then(|i| i.get("question")).and_then(|q| q.as_str()).unwrap_or("The agent has a question").to_owned(),
                None,
                cordata_view::view::ask_choices(info)
                    .into_iter()
                    .enumerate()
                    .map(|(index, choice)| {
                        ui::button(SharedString::from(format!("choice-{index}")), SharedString::from(format!("{}  {}", index + 1, shown(&choice.name))), Kind::Outline, cx)
                            .on_click(answer(ApprovalDecision::Choose(choice.id.clone())))
                            .into_any_element()
                    })
                    .chain([
                        ui::button("reply-own-words", "Own words  W", Kind::Ghost, cx)
                            .on_click(cx.listener(|this, _, window, cx| this.begin_saying(window, cx)))
                            .into_any_element(),
                        div().flex_1().into_any_element(),
                        ui::button("skip", "Skip  esc", Kind::Ghost, cx).on_click(answer(ApprovalDecision::Reject)).into_any_element(),
                    ])
                    .collect(),
            ),
            tool => {
                let sentence = match (&command, info.kind) {
                    (Some(_), _) => format!("Run shell command in {place}"),
                    (None, acp_core::ToolKind::Edit) => format!("Modify files in {place}"),
                    _ => format!("Allow {}?", attribution_label(tool)),
                };
                let always = info.always_label.as_ref().map(|label| {
                    let short = always_prefix(label);
                    ui::button("always", SharedString::from(format!("Always allow `{}`", shown(&short))), Kind::AmberOutline, cx)
                        .max_w(px(300.))
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
                buttons.push(
                    ui::button("say-why", "Reply / Steer", Kind::Outline, cx)
                        .tooltip(ui::tip("Redirect the agent in your own words (W)"))
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child("W"))
                        .on_click(cx.listener(|this, _, window, cx| this.begin_saying(window, cx)))
                        .into_any_element(),
                );
                buttons.push(div().flex_1().into_any_element());
                buttons.push(
                    ui::button("reject", "Deny", Kind::Ghost, cx)
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child("esc"))
                        .on_click(answer(ApprovalDecision::Reject))
                        .into_any_element(),
                );
                ("approval needed", sentence, command.clone().map(|cmd| format!("$ {cmd}")).or_else(|| (!summary.is_empty()).then(|| summary.clone())), buttons)
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
            .shadow(soft_shadow(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(6.))
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
                                    .child(SharedString::from(t::tracked_text(label, t::Tracking::Label, true))),
                            )
                            .when(info.untrusted_source, |row| row.child(ui::badge("from untrusted content", Tone::Teal, cx))),
                    )
                    .when(self.saying, |row| row.child(ui::badge("Type response below · ↵ sends", Tone::Amber, cx))),
            )
            .child(div().text_size(px(t::TEXT_BODY)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(shown(&sentence))))
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
                    row.child(
                        ui::button("review-files", SharedString::from(format!("⎇ Review {changed} changed file{}", if changed == 1 { "" } else { "s" })), Kind::AmberOutline, cx)
                            .h(px(26.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_panel(true, cx);
                                this.switch_dock(DockTab::Review, cx);
                            })),
                    )
                })
                .child(ui::button("copy-reply", "Copy reply", Kind::Outline, cx).h(px(26.)).on_click(cx.listener(|this, _, _, cx| this.copy_last_reply(cx))))
                .child(
                    ui::button("undo-turn", "Undo turn", Kind::Ghost, cx)
                        .h(px(26.))
                        .tooltip(ui::tip("Preview undoing this turn's file changes (/undo)"))
                        .on_click(cx.listener(|this, _, _, cx| this.session.update(cx, |s, cx| s.submit("/undo", cx)))),
                )
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
        let mode = view.pending_mode.clone().unwrap_or_else(|| view.mode.clone());
        let (mode_icon, mode_title) = mode_badge(&mode);
        let mode_text = format!("{mode_icon} {mode_title} ⌄");
        let auto_on = self.auto_routing_enabled();
        let composer_model = view.config("model").map(|option| {
            let short = short_model(option.current_name());
            if auto_on { format!("⚡ {short} ⌄") } else { format!("{short} ⌄") }
        });

        let typed = !self.composer.read(cx).value().trim().is_empty();
        let ready = typed || !self.staged.is_empty();
        let active_border = ready || busy || self.saying;

        let mut ring = Vec::new();
        if active_border {
            ring.push(BoxShadow { color: amber(cx, 0.05), offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(t::COMPOSER_RING), inset: false });
        }
        ring.extend(soft_shadow(cx));

        let trigger = |id: &'static str, key: &'static str, word: &'static str, cx: &mut Context<Self>| {
            let (hover_bg, hover_ink) = (line(cx, t::HOVER_STRONG), c(tk.text_primary));
            div()
                .id(id)
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(format!("Insert {key} ({word})")))
                .tooltip(ui::tip(SharedString::from(format!("Insert {key} for {word}"))))
                .flex()
                .flex_shrink_0()
                .items_center()
                .whitespace_nowrap()
                .gap(px(4.))
                .h(px(26.))
                .px(px(7.))
                .rounded(px(t::RADIUS_CONTROL))
                .cursor_pointer()
                .font_family(mono(cx))
                .text_size(px(t::TEXT_META))
                .text_color(c(tk.text_muted))
                .hover(move |s| s.bg(hover_bg).text_color(hover_ink))
                .on_click(cx.listener(move |this, _, window, cx| this.insert_trigger(key, window, cx)))
                .child(div().font_weight(FontWeight::MEDIUM).text_color(c(tk.text_secondary)).child(key))
                .child(word)
        };

        let running = view.turn_active;
        let steer = (running && typed).then(|| {
            ui::button("steer", "Steer ⌥↵", Kind::Tint, cx)
                .h(px(t::SEND_BUTTON))
                .aria_label("Steer the running turn (Option+Enter)")
                .tooltip(ui::tip("Send this to the running turn immediately; ↵ queues for after"))
                .on_click(cx.listener(|this, _, window, cx| this.steer_composer(window, cx)))
        });

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
        let send_tip = if self.prefs.send_with == "cmd-enter" { "Send message  ⌘↵" } else { "Send message  ↵" };
        let send = if busy && !self.saying {
            ui::button("stop-turn", "■ Stop", Kind::AmberOutline, cx)
                .h(px(t::SEND_BUTTON))
                .aria_label("Stop (esc esc)")
                .tooltip(ui::tip("Stop the current run  esc esc"))
                .on_click(cx.listener(|this, _, _, cx| this.session.update(cx, |s, cx| s.cancel(cx))))
                .into_any_element()
        } else {
            let hover = c(tk.amber_hover);
            div()
                .id("send")
                .role(gpui::Role::Button)
                .aria_label(if self.prefs.send_with == "cmd-enter" { "Send (⌘↵)" } else { "Send (↵)" })
                .tooltip(ui::tip(send_tip))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .size(px(t::SEND_BUTTON))
                .rounded(px(t::RADIUS_CARD))
                .bg(c(tk.amber))
                .text_color(c(tk.amber_on_fill))
                .font_weight(FontWeight::BOLD)
                .when(ready, move |btn| btn.cursor_pointer().hover(move |s| s.bg(hover)))
                .when(!ready, |btn| btn.opacity(0.42))
                .on_click(cx.listener(|this, _, window, cx| this.send_composer(window, cx)))
                .child("↑")
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
            .border_color(if active_border { amber(cx, t::AMBER_BORDER_STRONG) } else { line(cx, t::LINE_CONTROL_STRONG) })
            .shadow(ring)
            .when(!queued.is_empty(), |col| col.child(div().flex().flex_col().pt(px(4.)).border_b_1().border_color(line(cx, t::LINE_SUBTLE)).children(queued)))
            .when(!chips.is_empty(), |col| col.child(div().flex().flex_wrap().gap(px(6.)).px(px(12.)).pt(px(10.)).children(chips)))
            .when_some(note, |col, note| col.child(div().px(px(14.)).pt(px(8.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.amber)).child(SharedString::from(note))))
            .child(div().px(px(14.)).pt(px(10.)).pb(px(2.)).text_size(px(t::TEXT_BODY)).child(Textarea::new(&self.composer).appearance(false).aria_label("Message")))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(10.))
                    .pb(px(8.))
                    .pt(px(4.))
                    .child(
                        ui::button("attach", "+ Attach", Kind::Ghost, cx)
                            .h(px(26.))
                            .rounded_full()
                            .aria_label("Attach files or images (Shift+Command+A, or paste / drop them)")
                            .tooltip(ui::tip("Attach files or images  ⇧⌘A — or paste / drag & drop"))
                            .on_click(cx.listener(|this, _, window, cx| this.choose_attachments(window, cx))),
                    )
                    .child(
                        ui::icon_button("dictate", "◉", cx)
                            .aria_label("Dictate (system dictation)")
                            .tooltip(ui::tip("Voice dictation"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.composer.update(cx, |s, cx| s.focus(window, cx));
                                cx.defer(|_| crate::platform::start_dictation());
                            })),
                    )
                    .child(trigger("insert-slash", "/", "commands", cx))
                    .child(trigger("insert-at", "@", "files", cx))
                    .child(trigger("insert-bang", "!", "shell", cx))
                    .child(div().flex_1())
                    .child(
                        ui::chip("mode-chip", SharedString::from(mode_text), cx)
                            .role(gpui::Role::Button)
                            .aria_label(SharedString::from(format!("Execution mode: {}. Click to choose or press Shift+Tab", mode_label(&mode))))
                            .tooltip(ui::tip("Choose execution mode (Plan / Ask first / Auto-edit / Auto)  ⇧Tab"))
                            .when(mode == "auto", |chip| chip.bg(amber(cx, t::AMBER_FILL)).border_color(amber(cx, t::AMBER_BORDER)).text_color(c(tk.amber)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model_menu = false;
                                this.mode_menu = !this.mode_menu;
                                cx.notify();
                            })),
                    )
                    .when_some(composer_model, |row, label| {
                        row.child(
                            ui::chip("composer-model-chip", SharedString::from(label), cx)
                                .role(gpui::Role::Button)
                                .aria_label("Choose model or Auto Mode")
                                .tooltip(ui::tip("Choose model or toggle Auto Mode routing"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.mode_menu = false;
                                    if this.model_menu && this.model_menu_bottom {
                                        this.model_menu = false;
                                    } else {
                                        this.model_menu = true;
                                        this.model_menu_bottom = true;
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .children(steer)
                    .child(send),
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
            "Queue a follow-up (↵), or steer the active run immediately (⌥↵)…"
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
        assert_eq!(mode_badge("build"), ("◈", "Ask first"));
        assert_eq!(mode_badge("auto"), ("⚡", "Auto"));
    }
}
