//! The side panel: a 52px tab strip (mission · files · editor · review · terminal, the rest under ⋯) and the tab's
//! content. Mission Control is drawn here; the other tabs are the panes in this folder.

use cordata_theme::tokens as t;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, div, px};

use super::{DockTab, Workspace, editor};
use crate::theme::{amber, c, inset_bottom, line, mono, tk};
use crate::ui::{self};

/// The tabs in the strip, given the window width; the rest are in the ⋯ menu.
pub fn strip_tabs(window_w: f32) -> (Vec<DockTab>, Vec<DockTab>) {
    let mut strip = vec![DockTab::Files, DockTab::Editor, DockTab::Review];
    let mut overflow = Vec::new();
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
    fn tab_name(&self, tab: DockTab) -> (&'static str, &'static str, Option<String>) {
        match tab {
            DockTab::Files => ("Files", "Project files  ⌘⌥2 · ⌘P", None),
            DockTab::Editor => ("Editor", "Code editor  ⌘⌥3", match (self.files.buffers.len(), self.files.dirty()) {
                (0, _) => None,
                (n, 0) => Some(n.to_string()),
                (n, _) => Some(format!("{n}●")),
            }),
            DockTab::Review => ("Changes", "Diff review & Git  ⌘⌥4", (self.review.file_count() > 0).then(|| self.review.file_count().to_string())),
            DockTab::Terminal => ("Terminal", "Project terminal  ⌘⌥5 · ⌃`", None),
        }
    }

    pub(super) fn side_panel(&self, window_w: f32, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let (strip, overflow) = strip_tabs(window_w);
        let in_overflow = overflow.contains(&self.dock_tab);
        let tab = |tab: DockTab, cx: &mut Context<Self>| {
            let selected = self.dock_tab == tab;
            let (name, tip, badge) = self.tab_name(tab);
            let hover = c(tk.text_primary);
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
                .on_click(cx.listener(move |this, _, _, cx| this.switch_dock(tab, cx)))
                .child(name)
                .when_some(badge, |el, badge| el.child(div().px(px(5.)).py(px(1.)).rounded(px(t::RADIUS_BADGE)).bg(amber(cx, t::AMBER_FILL_STRONG)).font_family(mono(cx)).text_color(c(tk.amber)).text_size(px(t::TEXT_TAG)).child(SharedString::from(badge))))
        };
        let strip_el: Vec<AnyElement> = strip.iter().map(|&t| tab(t, cx).into_any_element()).collect();
        let more_label = if in_overflow { format!("{} ⌄", self.tab_name(self.dock_tab).0) } else { "⋯".to_owned() };
        let overflow_menu = self.overflow_open.then(|| {
            let rows: Vec<AnyElement> = overflow
                .iter()
                .enumerate()
                .map(|(index, &item)| {
                    let (label, _, _) = self.tab_name(item);
                    let shortcut = match item {
                        DockTab::Terminal => "⌃`",
                        _ => "",
                    };
                    ui::row(("overflow", index), self.dock_tab == item, cx)
                        .text_size(px(t::TEXT_UI))
                        .text_color(c(tk.text_primary))
                        .child(label)
                        .when(!shortcut.is_empty(), |r| r.child(div().flex_1()).child(ui::meta(shortcut, cx)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.overflow_open = false;
                            this.switch_dock(item, cx);
                        }))
                        .into_any_element()
                })
                .collect();
            ui::popover(cx)
                .id("overflow-menu")
                .role(gpui::Role::Menu)
                .aria_label("More panels")
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.overflow_open = false;
                    cx.notify();
                }))
                .absolute()
                .top(px(t::HEADER_H - 6.))
                .right(px(8.))
                .w(px(196.))
                .p(px(4.))
                .children(rows)
                .into_any_element()
        });
        let body: AnyElement = match self.dock_tab {
            DockTab::Files => editor::pane("files", self.files_panel(cx), cx),
            DockTab::Editor => self.editor_panel(cx),
            DockTab::Review => {
                let git = self.git_section(cx);
                div().size_full().flex().flex_col().children(git).child(div().flex_1().min_h_0().child(self.review_panel(cx))).into_any_element()
            }
            DockTab::Terminal => editor::pane("terminal", self.terminal_panel(cx), cx),
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
                            .text_size(px(t::TEXT_SMALL))
                            .text_color(c(if in_overflow { tk.text_primary } else { tk.text_muted }))
                            .when(in_overflow, |el| el.shadow(inset_bottom(c(tk.amber))))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.overflow_open = !this.overflow_open;
                                cx.notify();
                            }))
                            .child(SharedString::from(more_label)),
                    )
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
            .children(overflow_menu)
            .into_any_element()
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
        assert_eq!(panel_width(DockTab::Files, 900.), 300.);
        assert_eq!(panel_width(DockTab::Files, 1200.), 336.);
        assert_eq!(panel_width(DockTab::Files, 1600.), 380.);
        assert_eq!(panel_width(DockTab::Editor, 1600.), 560.);
    }
}
