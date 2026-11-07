//! The side panel: a 52px 3-tab strip (`Code` · `Changes` · `Terminal`) and the tab's content;
//! `Code` unifies the file tree (`⌘P` / `⌘⌥2`) and open buffers (`⌘⌥3`) with zero `⋯` overflow menu.

use cordata_theme::tokens as t;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px};

use super::{DockTab, Workspace, editor};
use crate::theme::{c, inset_bottom, line, mono, tk};
use crate::ui::{self};

/// The 3 permanent tabs in the strip across every window width; overflow is always empty.
pub fn strip_tabs(_window_w: f32) -> (Vec<DockTab>, Vec<DockTab>) {
    (vec![DockTab::Files, DockTab::Review, DockTab::Terminal], Vec::new())
}

/// The panel's width for a window width: `clamp(300, 28vw, 380)`, wider for code and review.
pub fn panel_width(tab: DockTab, window_w: f32) -> f32 {
    match tab {
        DockTab::Editor | DockTab::Review | DockTab::Artifacts => (window_w * t::PANEL_WIDE_VW).clamp(t::PANEL_WIDE_MIN_W, t::PANEL_WIDE_MAX_W),
        _ => (window_w * t::PANEL_VW).clamp(t::PANEL_MIN_W, t::PANEL_MAX_W),
    }
}

impl Workspace {
    fn code_badge(&self) -> Option<String> {
        match (self.files.buffers.len(), self.files.dirty()) {
            (0, _) => None,
            (n, 0) => Some(n.to_string()),
            (n, _) => Some(format!("{n}●")),
        }
    }

    fn tab_name(&self, tab: DockTab) -> (&'static str, &'static str, Option<String>) {
        match tab {
            DockTab::Files => ("Code", "Files, buffers & canvas  ⌘P · ⌘⌥2 · ⌘⌥3", self.code_badge()),
            DockTab::Editor => ("Editor", "Code editor  ⌘⌥3", self.code_badge()),
            DockTab::Review => ("Changes", "Diff review & Git  ⌘⌥4", (self.review.file_count() > 0).then(|| self.review.file_count().to_string())),
            DockTab::Artifacts => ("Canvas", "Living Artifacts, Plans & Specs  ⌘⌥6", {
                let total = self.artifacts.total_tasks();
                (total > 0).then(|| format!("{}/{}", self.artifacts.completed_tasks(), total))
            }),
            DockTab::Terminal => ("Terminal", "Project terminal  ⌘⌥5 · ⌃`", None),
        }
    }

    fn code_subbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let active_tab = self.dock_tab;
        let buf_label = match self.files.buffers.len() {
            0 => "Buffers".to_owned(),
            n => format!("Buffers ({n})"),
        };
        let chip = |id: &'static str, label: SharedString, active: bool, target: DockTab, cx: &mut Context<Self>| {
            let hover = c(tk.text_primary);
            div()
                .id(id)
                .role(gpui::Role::Tab)
                .aria_selected(active)
                .cursor_pointer()
                .px(px(7.))
                .py(px(2.))
                .rounded(px(t::RADIUS_CONTROL))
                .font_family(mono(cx))
                .text_size(px(t::TEXT_META))
                .text_color(c(if active { tk.text_primary } else { tk.text_muted }))
                .when(active, |d| d.bg(line(cx, t::HOVER_STRONG)))
                .when(!active, move |d| d.hover(move |s| s.text_color(hover)))
                .on_click(cx.listener(move |this, _, _, cx| this.switch_dock(target, cx)))
                .child(label)
        };
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(4.))
            .pb(px(8.))
            .mb(px(8.))
            .border_b_1()
            .border_color(line(cx, t::LINE_SUBTLE))
            .child(chip("code-sub-files", "Files".into(), active_tab == DockTab::Files, DockTab::Files, cx))
            .child(chip("code-sub-editor", buf_label.into(), active_tab == DockTab::Editor, DockTab::Editor, cx))
            .child(chip("code-sub-canvas", "Canvas".into(), active_tab == DockTab::Artifacts, DockTab::Artifacts, cx))
            .child(div().flex_1())
            .child(ui::meta(if active_tab == DockTab::Files { "⌘P" } else { "⌘⌥2 tree" }, cx))
            .into_any_element()
    }

    pub(super) fn side_panel(&self, window_w: f32, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let (strip, _overflow) = strip_tabs(window_w);
        let tab = |tab: DockTab, cx: &mut Context<Self>| {
            let selected = match tab {
                DockTab::Files => matches!(self.dock_tab, DockTab::Files | DockTab::Editor | DockTab::Artifacts),
                other => self.dock_tab == other,
            };
            let (name, tip, badge) = self.tab_name(tab);
            let hover = c(tk.text_primary);
            let target = if tab == DockTab::Files && self.dock_tab == DockTab::Files && !self.files.buffers.is_empty() {
                DockTab::Editor
            } else {
                tab
            };
            div()
                .id(("dock-tab", tab as usize))
                .role(gpui::Role::Tab)
                .aria_selected(selected)
                .aria_label(SharedString::from(name))
                .tooltip(ui::tip(tip))
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(5.))
                .h_full()
                .px(px(9.))
                .whitespace_nowrap()
                .cursor_pointer()
                .text_size(px(t::TEXT_SMALL))
                .font_weight(if selected { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                .text_color(c(if selected { tk.text_primary } else { tk.text_muted }))
                .when(selected, |el| el.shadow(inset_bottom(c(tk.amber))))
                .when(!selected, move |el| el.hover(move |s| s.text_color(hover)))
                .on_click(cx.listener(move |this, _, _, cx| this.switch_dock(target, cx)))
                .child(name)
                .when_some(badge, |el, badge| el.child(div().font_family(mono(cx)).text_color(c(tk.text_faint)).text_size(px(t::TEXT_META)).child(SharedString::from(badge))))
        };
        let strip_el: Vec<AnyElement> = strip.iter().map(|&t| tab(t, cx).into_any_element()).collect();
        let body: AnyElement = match self.dock_tab {
            DockTab::Files => {
                let sub = self.code_subbar(cx);
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(sub)
                    .child(div().flex_1().min_h_0().child(editor::pane("files", self.files_panel(cx), cx)))
                    .into_any_element()
            }
            DockTab::Editor => {
                let sub = self.code_subbar(cx);
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(sub)
                    .child(div().flex_1().min_h_0().child(self.editor_panel(cx)))
                    .into_any_element()
            }
            DockTab::Review => {
                let git = self.git_section(cx);
                div().size_full().flex().flex_col().children(git).child(div().flex_1().min_h_0().child(self.review_panel(cx))).into_any_element()
            }
            DockTab::Artifacts => {
                let sub = self.code_subbar(cx);
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(sub)
                    .child(div().flex_1().min_h_0().child(editor::pane("artifacts", self.artifacts_panel(cx), cx)))
                    .into_any_element()
            }
            DockTab::Terminal => editor::pane("terminal", self.terminal_panel(window, cx), cx),
        };
        div()
            .id("side-panel")
            .role(gpui::Role::Complementary)
            .aria_label("Side panel")
            .relative()
            .w(px(self.dock_w(window_w)))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(crate::theme::glass_panel_bg(cx))
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
                            .flex()
                            .items_center()
                            .pl(px(2.))
                            .child(
                                ui::icon_button("close-side-panel", "✕", cx)
                                    .size(px(24.))
                                    .aria_label("Hide inspector (⌘\\)")
                                    .tooltip(ui::tip("Hide inspector  ⌘\\"))
                                    .on_click(cx.listener(|this, _, window, cx| this.toggle_panel(&super::TogglePanel, window, cx))),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().p(px(14.)).child(body))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_holds_three_permanent_tabs_with_zero_overflow_at_every_width() {
        for width in [900., 1100., 1200., 1300., 1600.] {
            let (strip, overflow) = strip_tabs(width);
            assert_eq!(strip, vec![DockTab::Files, DockTab::Review, DockTab::Terminal]);
            assert!(overflow.is_empty(), "side panel has no overflow menu at width {width}");
        }
    }

    #[test]
    fn the_panel_width_is_clamped() {
        assert_eq!(panel_width(DockTab::Files, 900.), 300.);
        assert_eq!(panel_width(DockTab::Files, 1200.), 336.);
        assert_eq!(panel_width(DockTab::Files, 1600.), 380.);
        assert_eq!(panel_width(DockTab::Editor, 1600.), 560.);
    }
}
