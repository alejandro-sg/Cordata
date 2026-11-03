//! The rail (232px): the project and its branch, said once; the threads, each with a 2px edge mark for what it is
//! doing (signal: working, attention: needs you, a dot: done while you were elsewhere); and one quiet footer line.
//! No cards, no badges, no keycaps: shortcuts live in tooltips and the palette (⌘K).

use cordata_theme::tokens as t;
use cordata_view::app::{ConnState, TabState};
use cordata_view::shown;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{Animation, AnimationExt as _, ease_in_out};
use gpui::{AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, div, px};

use super::{DockTab, Overlay, Workspace};
use crate::theme::{c, line, mono, tk};
use crate::ui::{self, Dot, Kind};

impl Workspace {
    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        div()
            .id("sidebar")
            .role(gpui::Role::Navigation)
            .aria_label("Project and threads")
            .w(px(t::SIDEBAR_W))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(c(tk.bg_panel))
            .border_r_1()
            .border_color(line(cx, t::LINE_SUBTLE))
            // Traffic lights occupy the left ~76px of this row; empty space drags the window.
            .child(
                div()
                    .h(px(t::HEADER_H))
                    .flex_shrink_0()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(2.))
                    .pl(px(78.))
                    .pr(px(10.))
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(
                        ui::icon_action("rail-search", gpui_kit_assets::IconName::Search, cx)
                            .aria_label("Search threads and commands (⌘K)")
                            .tooltip(ui::tip("Search threads and commands  ⌘K"))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Palette, window, cx))),
                    )
                    .child(
                        ui::icon_action("hide-sidebar", gpui_kit_assets::IconName::PanelLeft, cx)
                            .aria_label("Hide sidebar (⌘B)")
                            .tooltip(ui::tip("Hide sidebar  ⌘B"))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(&super::ToggleSidebar, window, cx))),
                    ),
            )
            .child(self.project_block(cx))
            .child(self.open_list(cx))
            .child(div().flex_1())
            .child(self.sidebar_footer(cx))
            .into_any_element()
    }

    /// The project's name (switches project), and under it the branch and what changed — the only place either shows.
    fn project_block(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let hover = line(cx, t::HOVER);
        let changed = self.review.file_count();
        let changed_hover = c(tk.amber);
        div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .px(px(12.))
            .pt(px(4.))
            .pb(px(14.))
            .child(
                div()
                    .id("project-name")
                    .role(gpui::Role::Button)
                    .aria_label("Switch project (⌘O)")
                    .tooltip(ui::tip("Switch project  ⌘O"))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(6.))
                    .py(px(3.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Picker, window, cx)))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_TITLE)).font_weight(FontWeight::SEMIBOLD).text_color(c(tk.text_primary)).child(SharedString::from(self.project_name())))
                    .child(div().flex_shrink_0().text_color(c(tk.text_faint)).child(gpui_component::Icon::new(gpui_kit_assets::IconName::ChevronDown).size(px(13.))))
            )
            .when_some(self.git.branch.clone(), |col, branch| {
                col.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(6.))
                        .font_family(mono(cx))
                        .text_size(px(t::TEXT_META))
                        .child(div().min_w_0().truncate().text_color(c(tk.text_muted)).child(SharedString::from(branch)))
                        .when(changed > 0, |row| {
                            row.child(
                                div()
                                    .id("rail-changed")
                                    .role(gpui::Role::Button)
                                    .tooltip(ui::tip("Review the changes  ⌘⌥4"))
                                    .flex_shrink_0()
                                    .cursor_pointer()
                                    .text_color(c(tk.text_faint))
                                    .hover(move |s| s.text_color(changed_hover))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_panel(true, cx);
                                        this.switch_dock(DockTab::Review, cx);
                                    }))
                                    .child(SharedString::from(format!("±{changed}"))),
                            )
                        }),
                )
            })
            .into_any_element()
    }

    fn open_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let can_close = self.sessions.len() > 1;
        let rows: Vec<AnyElement> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                let view = &entity.read(cx).view;
                let active = entity.entity_id() == self.session.entity_id();
                let state = view.tab_state();
                // The edge mark: what the thread is doing, without a word.
                let mark = match (&view.conn, state) {
                    (ConnState::Closed { .. }, _) => Some(c(tk.danger)),
                    (_, TabState::NeedsApproval) => Some(c(tk.attention)),
                    (_, TabState::Working) => Some(c(tk.amber)),
                    _ => None,
                };
                let done = !active && state == TabState::Done;
                let waiting = state == TabState::NeedsApproval;
                let title = view.title();
                let group = SharedString::from(format!("open-row-{index}"));
                let target = entity.clone();
                let (hover, selected) = (line(cx, t::HOVER), line(cx, t::HOVER_STRONG));
                let close_group = group.clone();
                let tip = match index {
                    0..=8 => format!("{}  ⌘{}", shown(&title), index + 1),
                    _ => shown(&title),
                };
                div()
                    .id(("open-session", index))
                    .group(group)
                    .role(gpui::Role::Tab)
                    .aria_selected(active)
                    .tooltip(ui::tip(SharedString::from(tip)))
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(30.))
                    .pl(px(10.))
                    .pr(px(6.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .when(active, move |row| row.bg(selected))
                    .when(!active, move |row| row.hover(move |s| s.bg(hover)))
                    .on_click(cx.listener(move |this, _, _, cx| this.show(target.clone(), cx)))
                    .when_some(mark, |row, colour| {
                        let bar = div().absolute().left(px(0.)).top(px(7.)).bottom(px(7.)).w(px(2.)).rounded_full().bg(colour);
                        // A working thread's mark breathes: alive, not stuck. Waiting and failed marks hold still.
                        if state == TabState::Working && ui::motion(cx) {
                            row.child(bar.with_animation(("thread-breath", index), Animation::new(Duration::from_millis(1800)).repeat().with_easing(ease_in_out), |el, t| {
                                el.opacity(0.35 + 0.65 * (1.0 - (2.0 * t - 1.0).powi(2)))
                            }))
                        } else {
                            row.child(bar)
                        }
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(t::TEXT_UI))
                            .font_weight(if active { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                            .text_color(c(match () {
                                _ if waiting => tk.attention,
                                _ if active => tk.text_primary,
                                _ => tk.text_secondary,
                            }))
                            .child(SharedString::from(shown(&title))),
                    )
                    // Finished while you were elsewhere; ⌘J goes there.
                    .when(done, |row| row.child(div().flex_shrink_0().size(px(t::DOT_SMALL)).rounded_full().bg(c(tk.amber))))
                    .when(can_close, |row| {
                        row.child(
                            ui::icon_action(("close-session", index), gpui_kit_assets::IconName::X, cx)
                                .size(px(20.))
                                .aria_label("Close thread")
                                .tooltip(ui::tip("Close thread  ⌘W"))
                                .opacity(0.)
                                .group_hover(close_group, |s| s.opacity(1.))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.close_session_at(index, cx);
                                })),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(1.))
            .px(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl(px(10.))
                    .pr(px(2.))
                    .pb(px(4.))
                    .child(div().text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child("Threads"))
                    .child(
                        ui::icon_action("new-session", gpui_kit_assets::IconName::SquarePen, cx)
                            .size(px(22.))
                            .aria_label("New thread (⌘N)")
                            .tooltip(ui::tip("New thread  ⌘N"))
                            .on_click(cx.listener(|this, _, window, cx| this.new_session(&super::NewSession, window, cx))),
                    ),
            )
            .children(rows)
            .into_any_element()
    }

    /// One line: the engine's health (only worded when it is not fine) and Settings.
    fn sidebar_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let (kind, text) = self.connection_status(cx);
        let trouble = kind == Dot::Amber || kind == Dot::Danger;
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(40.))
            .flex_shrink_0()
            .px(px(14.))
            // A healthy engine says nothing; trouble says what and offers Retry.
            .when(trouble, |row| row.child(ui::dot(kind, cx).size(px(t::DOT_SMALL))).child(div().flex_1().truncate().text_size(px(t::TEXT_META)).text_color(c(tk.attention)).child(SharedString::from(text))))
            .when(!trouble, |row| row.child(div().flex_1()))
            .when(trouble, |row| {
                row.child(ui::button("retry-hub", "Retry", Kind::Ghost, cx).h(px(22.)).px(px(6.)).text_size(px(t::TEXT_SMALL)).on_click(cx.listener(|this, _, window, cx| this.reconnect(window, cx))))
            })
            .child(
                ui::icon_action("open-settings", gpui_kit_assets::IconName::Settings, cx)
                    .aria_label("Settings (⌘,)")
                    .tooltip(ui::tip("Settings  ⌘,"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Settings, window, cx))),
            )
            .into_any_element()
    }
}
