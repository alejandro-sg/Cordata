//! The side panel: a 52px tab strip (mission · files · editor · review · terminal, the rest under ⋯) and the tab's
//! content. Mission Control is drawn here; the other tabs are the panes in this folder.

use cordata_theme::tokens as t;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, div, px};
use gpui_component::input::Textarea;
use serde_json::Value;

use super::{DockTab, Workspace, editor};
use crate::mission;
use crate::theme::{amber, c, inset_bottom, line, mono, tk};
use crate::ui::{self, Dot, Kind};

/// The tabs in the strip, given the window width; the rest are in the ⋯ menu.
pub fn strip_tabs(window_w: f32) -> (Vec<DockTab>, Vec<DockTab>) {
    let mut strip = vec![DockTab::Mission, DockTab::Files, DockTab::Editor, DockTab::Review];
    let mut overflow = vec![DockTab::Browser, DockTab::Models];
    if window_w >= t::TERMINAL_OVERFLOW_W {
        strip.push(DockTab::Terminal);
    } else {
        overflow.insert(0, DockTab::Terminal);
    }
    (strip, overflow)
}

/// The panel's width for a window width: `clamp(300, 28vw, 380)`, wider for code.
pub fn panel_width(tab: DockTab, window_w: f32) -> f32 {
    match tab {
        DockTab::Editor | DockTab::Review => (window_w * t::PANEL_WIDE_VW).clamp(t::PANEL_WIDE_MIN_W, t::PANEL_WIDE_MAX_W),
        _ => (window_w * t::PANEL_VW).clamp(t::PANEL_MIN_W, t::PANEL_MAX_W),
    }
}

impl Workspace {
    fn tab_name(&self, tab: DockTab) -> (&'static str, Option<String>) {
        match tab {
            DockTab::Mission => ("mission", Some(self.mission.runs.iter().filter(|r| r.get("live").and_then(Value::as_str) == Some("running")).count()).filter(|n| *n > 0).map(|n| n.to_string())),
            DockTab::Files => ("files", None),
            DockTab::Editor => ("editor", match (self.files.buffers.len(), self.files.dirty()) {
                (0, _) => None,
                (n, 0) => Some(n.to_string()),
                (n, _) => Some(format!("{n}●")),
            }),
            DockTab::Review => ("review", (self.review.file_count() > 0).then(|| self.review.file_count().to_string())),
            DockTab::Terminal => ("terminal", None),
            DockTab::Browser => ("browser", None),
            DockTab::Models => ("models", None),
        }
    }

    pub(super) fn side_panel(&self, window_w: f32, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let (strip, overflow) = strip_tabs(window_w);
        let in_overflow = overflow.contains(&self.dock_tab);
        let tab = |tab: DockTab, cx: &mut Context<Self>| {
            let selected = self.dock_tab == tab;
            let (name, badge) = self.tab_name(tab);
            let hover = c(tk.text_primary);
            div()
                .id(("dock-tab", tab as usize))
                .role(gpui::Role::Tab)
                .aria_selected(selected)
                .aria_label(SharedString::from(name))
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(5.))
                .h_full()
                .px(px(8.))
                .whitespace_nowrap()
                .cursor_pointer()
                .font_family(mono(cx))
                .text_size(px(t::TEXT_META))
                .text_color(c(if selected { tk.amber } else { tk.text_muted }))
                .when(selected, |el| el.shadow(inset_bottom(c(tk.amber))))
                .when(!selected, move |el| el.hover(move |s| s.text_color(hover)))
                .on_click(cx.listener(move |this, _, _, cx| this.switch_dock(tab, cx)))
                .child(name)
                .when_some(badge, |el, badge| el.child(div().px(px(4.)).rounded(px(t::RADIUS_BADGE)).bg(amber(cx, t::AMBER_FILL_STRONG)).text_color(c(tk.amber)).text_size(px(t::TEXT_TAG)).child(SharedString::from(badge))))
        };
        let strip_el: Vec<AnyElement> = strip.iter().map(|&t| tab(t, cx).into_any_element()).collect();
        let more_label = if in_overflow { format!("{} ⌄", self.tab_name(self.dock_tab).0) } else { "⋯".to_owned() };
        let overflow_menu = self.overflow_open.then(|| {
            let rows: Vec<AnyElement> = overflow
                .iter()
                .enumerate()
                .map(|(index, &item)| {
                    ui::row(("overflow", index), self.dock_tab == item, cx)
                        .font_family(mono(cx))
                        .text_size(px(t::TEXT_SMALL))
                        .text_color(c(tk.text_primary))
                        .child(self.tab_name(item).0)
                        .when(item == DockTab::Terminal, |r| r.child(div().flex_1()).child(ui::meta("⌃`", cx)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.overflow_open = false;
                            this.switch_dock(item, cx);
                        }))
                        .into_any_element()
                })
                .collect();
            ui::popover(cx).id("overflow-menu").role(gpui::Role::Menu).aria_label("More panels").absolute().top(px(t::HEADER_H - 6.)).right(px(8.)).w(px(180.)).p(px(4.)).children(rows).into_any_element()
        });
        let body: AnyElement = match self.dock_tab {
            DockTab::Mission => self.mission_tab(cx),
            DockTab::Files => editor::pane("files", self.files_panel(cx), cx),
            DockTab::Editor => self.editor_panel(cx),
            DockTab::Review => self.review_panel(cx),
            DockTab::Terminal => editor::pane("terminal", self.terminal_panel(cx), cx),
            DockTab::Browser => editor::pane("browser agent", self.browser_panel(cx), cx),
            DockTab::Models => editor::pane("models & voice", self.models_panel(cx), cx),
        };
        div()
            .id("side-panel")
            .role(gpui::Role::Complementary)
            .aria_label("Side panel")
            .relative()
            .w(px(panel_width(self.dock_tab, window_w)))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(c(tk.bg_panel))
            .border_l_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .child(
                div()
                    .id("dock-tabs")
                    .role(gpui::Role::TabList)
                    .flex()
                    .flex_shrink_0()
                    .items_stretch()
                    .h(px(t::HEADER_H))
                    .px(px(8.))
                    .border_b_1()
                    .border_color(line(cx, t::LINE_DEFAULT))
                    .children(strip_el)
                    .child(div().flex_1().min_w(px(4.)).on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()))
                    .child(
                        div()
                            .id("dock-more")
                            .role(gpui::Role::Button)
                            .aria_label("More panels")
                            .tooltip(ui::tip("More panels"))
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .px(px(8.))
                            .cursor_pointer()
                            .whitespace_nowrap()
                            .font_family(mono(cx))
                            .text_size(px(t::TEXT_META))
                            .text_color(c(if in_overflow { tk.amber } else { tk.text_muted }))
                            .when(in_overflow, |el| el.shadow(inset_bottom(c(tk.amber))))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.overflow_open = !this.overflow_open;
                                cx.notify();
                            }))
                            .child(SharedString::from(more_label)),
                    ),
            )
            .child(div().flex_1().min_h_0().p(px(14.)).child(body))
            .children(overflow_menu)
            .into_any_element()
    }

    fn mission_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let checklist = (!view.checklist.is_empty()).then(|| {
            div().flex().flex_col().gap(px(4.)).children(view.checklist.iter().map(|item| {
                let (glyph, ink) = match item.status {
                    cordata_view::extras::TodoStatus::Completed => ("✓", tk.teal),
                    cordata_view::extras::TodoStatus::InProgress => ("◆", tk.amber),
                    cordata_view::extras::TodoStatus::Pending => ("·", tk.text_faint),
                };
                div()
                    .flex()
                    .gap(px(8.))
                    .text_size(px(t::TEXT_SMALL))
                    .child(div().w(px(10.)).flex_shrink_0().text_color(c(ink)).child(glyph))
                    .child(div().min_w_0().text_color(c(if ink == tk.amber { tk.text_primary } else { tk.text_secondary })).child(SharedString::from(shown(&item.content))))
            }))
        });
        let goal = view.goal.as_ref().map(|g| shown(&g.condition));
        div()
            .id("mission-scroll")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(12.))
                    .rounded(px(t::RADIUS_CARD))
                    .border_1()
                    .border_color(line(cx, t::LINE_DEFAULT))
                    .bg(line(cx, t::FILL_FAINT))
                    .child(div().flex().items_center().justify_between().child(ui::heading("mission control", cx)).child(ui::meta("orchestrator", cx)))
                    .child(self.mission_body(cx)),
            )
            .when_some(checklist, |col, list| col.child(crate::hud::frame("tasks", list, cx)))
            .when_some(goal, |col, goal| col.child(crate::hud::frame("goal", div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_secondary)).child(SharedString::from(goal)), cx)))
            .into_any_element()
    }

    fn mission_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        if let Some((name, text)) = &self.mission.artifact {
            return div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(div().flex_1().truncate().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.amber)).child(SharedString::from(name.clone())))
                        .child(ui::button("close-artifact", "Close", Kind::Ghost, cx).h(px(24.)).on_click(cx.listener(|this, _, _, cx| {
                            this.mission.artifact = None;
                            cx.notify();
                        }))),
                )
                .child(div().id("artifact-text").max_h(px(560.)).overflow_y_scroll().child(ui::code_block(SharedString::from(shown(text)), cx)))
                .into_any_element();
        }
        if self.mission.selected.is_some() {
            let mut buttons: Vec<AnyElement> = vec![ui::button("runs-back", "← Runs", Kind::Ghost, cx).h(px(26.)).on_click(cx.listener(|this, _, _, cx| this.select_run(None, cx))).into_any_element()];
            if self.mission.can_pause() {
                buttons.push(ui::button("run-pause", "Pause", Kind::Outline, cx).h(px(26.)).on_click(cx.listener(|this, _, _, cx| this.control_run("pause", serde_json::json!({}), cx))).into_any_element());
            }
            if self.mission.can_resume() {
                buttons.push(ui::button("run-resume", "Resume", Kind::Tint, cx).h(px(26.)).on_click(cx.listener(|this, _, _, cx| this.control_run("resume", serde_json::json!({}), cx))).into_any_element());
            }
            if self.mission.can_cancel() {
                buttons.push(ui::button("run-cancel", "Cancel", Kind::Danger, cx).h(px(26.)).on_click(cx.listener(|this, _, _, cx| this.control_run("cancel", serde_json::json!({ "now": true }), cx))).into_any_element());
            }
            let artifacts: Vec<AnyElement> = self
                .mission
                .artifacts()
                .into_iter()
                .enumerate()
                .map(|(index, name)| {
                    let open = name.clone();
                    let hover = c(tk.amber);
                    div()
                        .id(("artifact", index))
                        .cursor_pointer()
                        .font_family(mono(cx))
                        .text_size(px(t::TEXT_SMALL))
                        .text_color(c(tk.teal))
                        .hover(move |s| s.text_color(hover))
                        .child(SharedString::from(format!("▤ {name}")))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_artifact(open.clone(), cx)))
                        .into_any_element()
                })
                .collect();
            return div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(div().flex().flex_wrap().gap(px(6.)).children(buttons))
                .child(self.run_card(cx))
                .child(mission::run_detail(&self.mission, cx))
                .when(!artifacts.is_empty(), |col| col.child(ui::section_label("artifacts", cx)).child(div().flex().flex_col().gap(px(4.)).children(artifacts)))
                .into_any_element();
        }
        let rows: Vec<AnyElement> = self
            .mission
            .runs
            .iter()
            .enumerate()
            .map(|(index, run)| {
                let run_id = run.get("runId").and_then(Value::as_str).unwrap_or_default().to_owned();
                let live = run.get("live").and_then(Value::as_str).unwrap_or_default().to_owned();
                let goal = run.get("goal").and_then(Value::as_str).unwrap_or_default().to_owned();
                let hover = line(cx, t::HOVER);
                div()
                    .id(("run-row", index))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(t::RADIUS_CARD))
                    .border_1()
                    .border_color(line(cx, t::LINE_DEFAULT))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_run(Some(run_id.clone()), cx)))
                    .child(ui::dot(run_dot(&live), cx))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_UI)).text_color(c(tk.text_primary)).child(SharedString::from(shown(&goal))))
                    .child(ui::meta(SharedString::from(live), cx))
                    .into_any_element()
            })
            .collect();
        let count = rows.len();
        let live_here = self.session.read(cx).view.run.as_ref().filter(|r| r.is_live()).map(|r| r.run_id.clone());
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .when_some(live_here, |col, run_id| col.child(ui::button("follow-live", SharedString::from(format!("Follow {run_id}")), Kind::Tint, cx).on_click(cx.listener(move |this, _, _, cx| this.select_run(Some(run_id.clone()), cx)))))
            .child(
                div()
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(t::RADIUS_CARD))
                    .bg(c(tk.bg_input))
                    .border_1()
                    .border_color(line(cx, t::LINE_CONTROL_STRONG))
                    .text_size(px(t::TEXT_UI))
                    .child(Textarea::new(&self.goal_input).appearance(false).aria_label("Mission goal")),
            )
            .child(
                ui::button("start-run", "Start run", Kind::Tint, cx)
                    .w_full()
                    .h(px(32.))
                    .aria_label("Start run (⌘↵)")
                    .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.amber_dim)).child("⌘↵"))
                    .on_click(cx.listener(|this, _, window, cx| this.start_run(window, cx))),
            )
            .when_some(self.mission.notice.clone(), |col, notice| col.child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.amber)).child(SharedString::from(shown(&notice)))))
            .child(div().flex().items_center().justify_between().pt(px(4.)).child(ui::section_label("runs in this project", cx)).child(ui::meta(SharedString::from(count.to_string()), cx)))
            .when(rows.is_empty(), |col| {
                col.child(
                    div()
                        .p(px(14.))
                        .rounded(px(t::RADIUS_CARD))
                        .border_1()
                        .border_dashed()
                        .border_color(line(cx, t::LINE_CONTROL_STRONG))
                        .text_size(px(t::TEXT_SMALL))
                        .line_height(gpui::relative(1.5))
                        .text_color(c(tk.text_muted))
                        .child("No runs yet. Start one above, or ask for a multi-step change in the conversation."),
                )
            })
            .children(rows)
            .into_any_element()
    }

    /// The selected run as a card: glowing dot while it runs, the goal, progress, the steps.
    fn run_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let board = &self.mission.board;
        let detail = self.mission.detail.as_ref();
        let goal = detail.and_then(|d| d.get("goal")).and_then(Value::as_str).unwrap_or_default().to_owned();
        let live = detail.and_then(|d| d.get("live")).and_then(Value::as_str).unwrap_or(board.state.as_str()).to_owned();
        let done = board.steps.iter().filter(|s| matches!(s.status.as_str(), "done" | "completed" | "succeeded")).count();
        let total = board.steps.len();
        let steps = board.steps.iter().map(|step| {
            let name = self.mission.step_name(&step.id).unwrap_or_else(|| step.id.clone());
            let (glyph, ink, text) = match step.status.as_str() {
                "done" | "completed" | "succeeded" => ("✓", tk.teal, tk.text_secondary),
                "running" | "in_progress" | "active" => ("◆", tk.amber, tk.text_primary),
                "failed" | "error" => ("✕", tk.danger, tk.text_secondary),
                _ => ("·", tk.text_faint, tk.text_muted),
            };
            div().flex().gap(px(8.)).text_size(px(t::TEXT_SMALL)).child(div().w(px(10.)).flex_shrink_0().text_color(c(ink)).child(glyph)).child(div().min_w_0().truncate().text_color(c(text)).child(SharedString::from(shown(&name))))
        });
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(12.))
            .rounded(px(t::RADIUS_CARD))
            .border_1()
            .border_color(if live == "running" { amber(cx, t::AMBER_BORDER) } else { line(cx, t::LINE_DEFAULT) })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(ui::dot(run_dot(&live), cx))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_UI)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(shown(&goal))))
                    .when(total > 0, |row| row.child(ui::meta(SharedString::from(format!("{done}/{total}")), cx))),
            )
            .children(steps)
            .into_any_element()
    }
}

fn run_dot(live: &str) -> Dot {
    match live {
        "running" => Dot::Live,
        "paused" | "interrupted" => Dot::Amber,
        "failed" | "cancelled" | "budget_exhausted" => Dot::Danger,
        "done" => Dot::Ok,
        _ => Dot::Muted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_moves_to_overflow_on_narrow_windows() {
        assert!(strip_tabs(1300.).0.contains(&DockTab::Terminal));
        assert!(strip_tabs(1200.).1.contains(&DockTab::Terminal));
        assert!(!strip_tabs(1200.).0.contains(&DockTab::Terminal));
    }

    #[test]
    fn the_panel_width_is_clamped() {
        assert_eq!(panel_width(DockTab::Mission, 900.), 300.);
        assert_eq!(panel_width(DockTab::Mission, 1200.), 336.);
        assert_eq!(panel_width(DockTab::Mission, 1600.), 380.);
        assert_eq!(panel_width(DockTab::Editor, 1600.), 560.);
    }
}
