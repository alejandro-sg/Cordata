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
use crate::theme::{c, ca, line, mono, tk};
use crate::ui::{self, Dot, Kind};

impl Workspace {
    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("sidebar")
            .role(gpui::Role::Navigation)
            .aria_label("Project and threads")
            .w(px(self.rail_w()))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(crate::theme::glass_panel_bg(cx))
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
            .child(self.work_board(cx))
            .child(self.sidebar_footer(cx))
            .into_any_element()
    }

    /// The project's name (switches project), and under it the branch, worktree status and what changed.
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
                    .gap(px(8.))
                    .px(px(6.))
                    .py(px(4.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Picker, window, cx)))
                    .child(ui::cordata_logo(20., cx))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_TITLE)).font_weight(FontWeight::SEMIBOLD).text_color(c(tk.text_primary)).child(SharedString::from(self.project_name())))
                    .child(div().flex_shrink_0().text_color(c(tk.text_faint)).child(gpui_component::Icon::new(gpui_kit_assets::IconName::ChevronDown).size(px(13.))))
            )
            .when_some(self.git.branch.clone(), |col, branch| {
                let in_wt = self.git.is_worktree;
                let wt_name = self.git.worktree_name.clone();
                col.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(6.))
                        .font_family(mono(cx))
                        .text_size(px(t::TEXT_META))
                        .child(div().text_color(c(tk.amber)).child("⎇"))
                        .child(div().min_w_0().truncate().text_color(c(tk.text_primary)).child(SharedString::from(branch.clone())))
                        .when(in_wt, |row| {
                            let badge_text = if let Some(ref name) = wt_name {
                                if branch.ends_with(name) || name == &self.project_name() {
                                    "worktree".to_owned()
                                } else {
                                    name.clone()
                                }
                            } else {
                                "worktree".to_owned()
                            };
                            let tip = wt_name.as_ref().map(|n| format!("Linked Git worktree: {n}")).unwrap_or_else(|| "Linked Git worktree".into());
                            row.child(
                                div()
                                    .id("rail-worktree-badge")
                                    .flex_shrink_0()
                                    .px(px(4.))
                                    .py(px(1.))
                                    .rounded(px(t::RADIUS_BADGE))
                                    .bg(ca(tk.amber, 0.16))
                                    .text_color(c(tk.amber_soft))
                                    .tooltip(ui::tip(SharedString::from(tip)))
                                    .child(SharedString::from(badge_text)),
                            )
                        })
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

    /// The work board (`cordata_view::work`): threads and the repository's other checkouts in one list, laned by what
    /// the person has to do — Needs you, Working, To review, Quiet — under a one-line brief. Scrolls; the footer stays.
    fn work_board(&self, cx: &mut Context<Self>) -> AnyElement {
        use cordata_view::work::{self, Lane, PlaceIn, Target, ThreadIn};
        let tk = tk(cx);
        let threads: Vec<ThreadIn> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                let s = entity.read(cx);
                let view = &s.view;
                ThreadIn {
                    index,
                    title: view.title(),
                    folder: s.cwd.clone(),
                    state: match (&view.conn, view.tab_state()) {
                        (ConnState::Closed { .. }, _) => TabState::Closed,
                        (_, state) => state,
                    },
                    waiting_on: view.pending.front().map(|p| p.info.title.clone()).or_else(|| view.attention_reason.clone()),
                    working_secs: view.turn_started.map(|t| t.elapsed().as_secs()),
                    active: entity.entity_id() == self.session.entity_id(),
                }
            })
            .collect();
        let places: Vec<PlaceIn> = self
            .git
            .worktrees
            .iter()
            .map(|w| PlaceIn {
                path: w.path.clone(),
                branch: w.branch.clone(),
                agent: w.agent_label.clone(),
                is_main: w.is_main,
                changes: w.changes,
                last_subject: w.last_subject.clone(),
                last_age_secs: w.last_age_secs,
                merged: w.merged,
            })
            .collect();
        let items = work::board(&threads, &places, &self.cwd);
        let brief = work::brief(&items);
        let needs = items.iter().any(|i| i.lane == Lane::NeedsYou);
        let can_close = self.sessions.len() > 1;
        let (hover, selected) = (line(cx, t::HOVER), line(cx, t::HOVER_STRONG));

        let mut col = div().id("work-board").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().gap(px(1.)).px(px(8.)).pb(px(8.));
        // The brief: the whole board in one line, and the way to start more work.
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pl(px(10.))
                .pr(px(2.))
                .pb(px(6.))
                .child(
                    div()
                        .id("work-brief")
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(t::TEXT_META))
                        .text_color(c(if needs { tk.attention } else { tk.text_faint }))
                        .when(needs, |d| d.cursor_pointer().tooltip(ui::tip("Go to what needs you  ⌘J")).on_click(cx.listener(|this, _, window, cx| this.next_attention(&super::NextAttention, window, cx))))
                        .child(SharedString::from(brief)),
                )
                .child(
                    // Two ways to read the same board: by what needs the person, or by checkout.
                    div()
                        .id("board-grouping")
                        .flex_shrink_0()
                        .cursor_pointer()
                        .text_size(px(t::TEXT_TAG))
                        .text_color(c(tk.text_faint))
                        .hover(|s| s.text_color(c(tk.text_secondary)))
                        .tooltip(ui::tip(if self.prefs.board_by_place { "Group by what needs you" } else { "Group by checkout" }))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.prefs.board_by_place = !this.prefs.board_by_place;
                            this.prefs.save();
                            cx.notify();
                        }))
                        .child(if self.prefs.board_by_place { "by place" } else { "by need" }),
                )
                .child(
                    ui::icon_action("new-session", gpui_kit_assets::IconName::SquarePen, cx)
                        .size(px(22.))
                        .aria_label("New thread (⌘N)")
                        .tooltip(ui::tip("New thread  ⌘N"))
                        .on_click(cx.listener(|this, _, window, cx| this.new_session(&super::NewSession, window, cx))),
                ),
        );
        // Sections: lanes, or checkouts. A single section needs no header — the brief already names it.
        let sections: Vec<(String, bool, Vec<work::Item>)> = if self.prefs.board_by_place {
            work::by_place(items, &self.cwd, &self.project_name()).into_iter().map(|(label, list)| (label, false, list)).collect()
        } else {
            let mut out: Vec<(String, bool, Vec<work::Item>)> = Vec::new();
            for item in items {
                match out.last_mut() {
                    Some((label, _, list)) if *label == item.lane.label() => list.push(item),
                    _ => out.push((item.lane.label().to_owned(), item.lane == Lane::NeedsYou, vec![item])),
                }
            }
            out
        };
        let headed = sections.len() > 1;
        let rows = sections.into_iter().enumerate().flat_map(|(n, (label, urgent, list))| {
            list.into_iter().enumerate().map(move |(k, item)| ((k == 0 && headed).then(|| (label.clone(), urgent, n == 0)), item))
        });
        for (ix, (header, item)) in rows.enumerate() {
            if let Some((label, urgent, first)) = header {
                let ink = if urgent { tk.attention } else { tk.text_faint };
                col = col.child(div().pl(px(10.)).pt(px(if first { 0. } else { 8. })).pb(px(2.)).truncate().text_size(px(t::TEXT_TAG)).text_color(c(ink)).child(SharedString::from(shown(&label))));
            }
            let lane = item.lane;
            let mark = match lane {
                Lane::NeedsYou => Some(c(tk.attention)),
                Lane::Working => Some(c(tk.amber)),
                Lane::Review => Some(ca(tk.amber, 0.45)),
                Lane::Quiet => None,
            };
            let thread_ix = match &item.target {
                Target::Thread(i) => Some(*i),
                Target::Place(_) => None,
            };
            let group = SharedString::from(format!("work-row-{ix}"));
            let close_group = group.clone();
            let remove_group = group.clone();
            let tip = match (&item.target, thread_ix) {
                (_, Some(i @ 0..=8)) => format!("{}  ⌘{}", shown(&item.title), i + 1),
                (Target::Place(path), _) => {
                    let branch = self.git.worktrees.iter().find(|w| &w.path == path).and_then(|w| w.branch.clone()).unwrap_or_default();
                    format!("Open this checkout · {branch}\n{}", super::tilde(path))
                }
                _ => shown(&item.title),
            };
            // The detail first: in a narrow rail the place is what may be cut.
            // Grouped by place, the section header already says where.
            let place = if self.prefs.board_by_place { String::new() } else { item.place.clone() };
            let meta: Vec<String> = [item.detail.clone(), place].into_iter().filter(|s| !s.is_empty()).collect();
            let target = item.target.clone();
            let row = div()
                .id(("work-item", ix))
                .group(group)
                .role(gpui::Role::Tab)
                .aria_selected(item.active)
                .tooltip(ui::tip(SharedString::from(tip)))
                .relative()
                .flex()
                .items_center()
                .gap(px(8.))
                .py(px(5.))
                .pl(px(10.))
                .pr(px(6.))
                .rounded(px(t::RADIUS_ROW))
                .cursor_pointer()
                .when(item.active, move |row| row.bg(selected))
                .when(!item.active, move |row| row.hover(move |s| s.bg(hover)))
                .on_click(cx.listener(move |this, _, window, cx| match &target {
                    Target::Thread(i) => {
                        if let Some(entity) = this.sessions.get(*i).cloned() {
                            this.show(entity, cx);
                        }
                    }
                    Target::Place(path) => this.switch_project(path.clone(), window, cx),
                }))
                .when_some(mark, |row, colour| {
                    let bar = div().absolute().left(px(0.)).top(px(8.)).bottom(px(8.)).w(px(2.)).rounded_full().bg(colour);
                    // A working thread's mark breathes: alive, not stuck. Waiting and finished marks hold still.
                    if lane == Lane::Working && ui::motion(cx) {
                        row.child(bar.with_animation(("work-breath", ix), Animation::new(Duration::from_millis(1800)).repeat().with_easing(ease_in_out), |el, t| {
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
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(t::TEXT_UI))
                                .font_weight(if item.active { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                                .text_color(c(match lane {
                                    Lane::NeedsYou => tk.attention,
                                    _ if item.active => tk.text_primary,
                                    Lane::Quiet => tk.text_secondary,
                                    _ => tk.text_body,
                                }))
                                .child(SharedString::from(shown(&item.title))),
                        )
                        .when(!meta.is_empty() || item.agent.is_some(), |c2| {
                            c2.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.))
                                    .min_w_0()
                                    .font_family(mono(cx))
                                    .text_size(px(t::TEXT_TAG))
                                    .text_color(c(tk.text_faint))
                                    .when_some(item.agent.clone(), |m, agent| m.child(div().flex_shrink_0().text_color(c(tk.teal)).child(SharedString::from(agent))))
                                    .child(div().min_w_0().truncate().child(SharedString::from(shown(&meta.join(" · "))))),
                            )
                        }),
                )
                .when(item.can_remove, |row| {
                    let (path, branch) = match &item.target {
                        Target::Place(p) => (p.clone(), self.git.worktrees.iter().find(|w| &w.path == p).and_then(|w| w.branch.clone())),
                        Target::Thread(_) => (std::path::PathBuf::new(), None),
                    };
                    row.child(
                        div()
                            .id(("remove-worktree", ix))
                            .flex_shrink_0()
                            .px(px(4.))
                            .rounded(px(t::RADIUS_BADGE))
                            .text_size(px(t::TEXT_TAG))
                            .text_color(c(tk.text_faint))
                            .opacity(0.)
                            .group_hover(remove_group.clone(), |s| s.opacity(1.))
                            .hover(|s| s.text_color(c(tk.danger)))
                            .tooltip(ui::tip("Remove this merged checkout…"))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, window, cx| this.ask_confirm(super::Confirm::RemoveWorktree { path: path.clone(), branch: branch.clone() }, window, cx)))
                            .child("Remove"),
                    )
                })
                .when(can_close && thread_ix.is_some(), |row| {
                    let index = thread_ix.unwrap_or_default();
                    row.child(
                        ui::icon_action(("close-session", index), gpui_kit_assets::IconName::X, cx)
                            .size(px(20.))
                            .aria_label("Close thread")
                            .tooltip(ui::tip("Close thread  ⌘W"))
                            .opacity(0.)
                            .group_hover(close_group, |s| s.opacity(1.))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| this.close_session_at(index, cx))),
                    )
                });
            col = col.child(row);
        }
        col.into_any_element()
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
