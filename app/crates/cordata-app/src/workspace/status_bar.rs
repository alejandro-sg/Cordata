//! The 28px status bar across the whole window: active execution mode, context window gauge, session tokens, live
//! turn status or session hint, uncommitted changes shortcut, estimated cost, and daemon health.

use cordata_theme::tokens as t;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, div, px};

use super::{DockTab, Workspace};
use crate::theme::{c, line, mono, tk};
use crate::ui::{self, Dot};

impl Workspace {
    pub(super) fn status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let (kind, health) = self.hub_health(cx);
        let changed = self.review.file_count();
        let diff_hover = c(tk.amber);
        div()
            .id("status-bar")
            .role(gpui::Role::Status)
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(18.))
            .h(px(t::STATUS_H))
            .px(px(t::GUTTER))
            .bg(c(tk.bg_panel))
            .border_t_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .font_family(mono(cx))
            .text_size(px(t::TEXT_STATUS))
            .whitespace_nowrap()
            .overflow_hidden()
            .when(view.turn_active, |bar| bar.child(div().text_color(c(tk.amber)).child(SharedString::from(format!("● Working · {} tool{} · esc esc stops", view.turn_tools, if view.turn_tools == 1 { "" } else { "s" })))))
            .when_some(self.esc_hint(), |bar, hint| bar.child(div().text_color(c(tk.amber)).child(hint)))
            .when_some(view.hint.clone(), |bar, hint| bar.child(div().min_w_0().truncate().text_color(c(tk.text_muted)).child(SharedString::from(shown(&hint)))))
            .child(div().flex_1())
            .when(changed > 0, |bar| {
                bar.child(
                    div()
                        .id("status-changed-files")
                        .role(gpui::Role::Button)
                        .tooltip(ui::tip("Open Changes & Diff Review  ⌘⌥4"))
                        .cursor_pointer()
                        .text_color(c(tk.teal))
                        .hover(move |s| s.text_color(diff_hover))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_panel(true, cx);
                            this.switch_dock(DockTab::Review, cx);
                        }))
                        .child(SharedString::from(format!("⎇ {changed} changed"))),
                )
            })
            .child(div().flex().items_center().gap(px(6.)).child(ui::dot(kind, cx).size(px(t::DOT_SMALL))).child(div().text_color(c(if kind == Dot::Amber { tk.amber } else { tk.text_muted })).child(SharedString::from(health))))
            .into_any_element()
    }
}
