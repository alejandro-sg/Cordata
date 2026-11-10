//! The rail (232px): the project and its branch, said once; the threads, each with a 2px edge mark for what it is
//! doing (signal: working, attention: needs you, a dot: done while you were elsewhere); and one quiet footer line.
//! No cards, no badges, no keycaps: shortcuts live in tooltips and the palette (⌘K).

use cordata_theme::tokens as t;
use cordata_view::app::{ConnState, TabState};
use cordata_view::shown;

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, div, px};

use super::{DockTab, Overlay, Workspace};
use crate::theme::{c, ca, line, mono, tk};
use crate::ui::{self, Dot, Kind};

impl Workspace {
    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        ui::drawer("sidebar", ui::DrawerSide::Left, self.rail_w(), cx)
            .role(gpui::Role::Navigation)
            .aria_label("Project and threads")
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

    /// The rail's list, three kinds of thing kept apart (`cordata_view::work`): **Threads** (conversations open in this
    /// window, in the order they were opened, each marked with what it is doing), **Earlier** (saved conversations of
    /// this project; a click resumes one as a thread) and **Other checkouts** (git worktrees other agents made; a
    /// click moves the window to that folder, so they are folded away and drawn as places, not conversations).
    fn work_board(&self, cx: &mut Context<Self>) -> AnyElement {
        use cordata_view::work::{self, Mark, PlaceIn, SavedIn, ThreadIn};
        const EARLIER_SHOWN: usize = 5;
        let tk = tk(cx);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let threads: Vec<ThreadIn> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                let s = entity.read(cx);
                let view = &s.view;
                ThreadIn {
                    index,
                    session_id: view.session.as_ref().map(|i| i.session_id.clone()).unwrap_or_default(),
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
        let saved: Vec<SavedIn> = self.saved.iter().map(|s| SavedIn { id: s.id.clone(), title: s.title.clone(), prompts: s.prompts, age_secs: now.saturating_sub(s.updated) }).collect();
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
        let rail = work::rail(&threads, &saved, &places, &self.cwd);
        let needs = rail.needs_you();
        let can_close = self.sessions.len() > 1;
        let (hover, selected) = (line(cx, t::HOVER), line(cx, t::HOVER_STRONG));
        let section = |label: &str| div().pl(px(10.)).pt(px(12.)).pb(px(3.)).truncate().text_size(px(t::TEXT_TAG)).text_color(c(tk.text_faint)).child(SharedString::from(label.to_owned()));

        let mut col = div().id("work-board").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().gap(px(1.)).px(px(8.)).pb(px(8.));

        // Threads: the header carries the one-line brief (only when there is something to say) and the way to start more.
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pl(px(10.))
                .pr(px(2.))
                .pb(px(3.))
                .child(div().flex_shrink_0().text_size(px(t::TEXT_TAG)).text_color(c(tk.text_faint)).child("Threads"))
                .child(
                    div()
                        .id("work-brief")
                        // What needs you is never the part that gets cut: the brief keeps its width.
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .text_size(px(t::TEXT_TAG))
                        .text_color(c(if needs { tk.attention } else { tk.text_faint }))
                        .when(needs, |d| d.cursor_pointer().tooltip(ui::tip("Go to what needs you  ⌘J")).on_click(cx.listener(|this, _, window, cx| this.next_attention(&super::NextAttention, window, cx))))
                        .child(SharedString::from(rail.brief().map(|b| format!("· {b}")).unwrap_or_default())),
                )
                .child(
                    div().flex_1().min_w_0(),
                )
                .child(
                    div()
                        .id("open-rack")
                        .flex_shrink_0()
                        .role(gpui::Role::Button)
                        .cursor_pointer()
                        .px(px(6.))
                        .py(px(1.))
                        .rounded(px(t::RADIUS_BADGE))
                        .bg(if self.desk_mode == super::DeskMode::Rack { ca(tk.amber, 0.18) } else { gpui::transparent_black() })
                        .border_1()
                        .border_color(if self.desk_mode == super::DeskMode::Rack { ca(tk.amber, 0.6) } else { gpui::transparent_black() })
                        .font_family(mono(cx))
                        .text_size(px(t::TEXT_TAG))
                        .text_color(c(if self.desk_mode == super::DeskMode::Rack { tk.amber } else { tk.text_muted }))
                        .hover(|s| s.text_color(c(tk.text_primary)))
                        .tooltip(ui::tip("Every thread at once  ⌘0"))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_rack(cx)))
                        .aria_label("Every thread at once (⌘0)")
                        .child("⌘0"),
                )
                .child(
                    ui::icon_action("new-session", gpui_kit_assets::IconName::SquarePen, cx)
                        .size(px(22.))
                        .aria_label("New thread (⌘N)")
                        .tooltip(ui::tip("New thread  ⌘N"))
                        .on_click(cx.listener(|this, _, window, cx| this.new_session(&super::NewSession, window, cx))),
                ),
        );
        for (ix, row) in rail.threads.iter().enumerate() {
            // The brand's cell says what the thread is doing, by shape as well as colour.
            let mark = match row.mark {
                Mark::NeedsYou => ui::MarkState::NeedsYou,
                Mark::Working => ui::MarkState::Working,
                Mark::Unread => ui::MarkState::Done,
                Mark::Idle => ui::MarkState::Idle,
            };
            let index = row.index;
            let beat = self.beat_of(index, cx);
            let group = SharedString::from(format!("thread-row-{ix}"));
            let close_group = group.clone();
            let tip = if index <= 8 { format!("{}  ⌘{}", shown(&row.title), index + 1) } else { shown(&row.title) };
            let meta: Vec<String> = [row.detail.clone(), row.elsewhere.clone().unwrap_or_default()].into_iter().filter(|s| !s.is_empty()).collect();
            let (mark_kind, active) = (row.mark, row.active && self.desk_mode != super::DeskMode::Rack);
            let item = div()
                .id(("thread", ix))
                .group(group)
                .role(gpui::Role::Tab)
                .aria_selected(active)
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
                .when(active, move |r| r.bg(selected))
                .when(!active, move |r| r.hover(move |s| s.bg(hover)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.desk_mode == super::DeskMode::Rack {
                        this.desk_mode = super::DeskMode::Transcript;
                    }
                    if let Some(entity) = this.sessions.get(index).cloned() {
                        this.show(entity, cx);
                    }
                }))
                .child(ui::mark_live(("thread-mark", ix), mark, 11., beat, cx))
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
                                .font_weight(if active { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                                .text_color(c(match mark_kind {
                                    Mark::NeedsYou => tk.attention,
                                    _ if active => tk.text_primary,
                                    _ => tk.text_body,
                                }))
                                .child(SharedString::from(shown(&row.title))),
                        )
                        .when(!meta.is_empty(), |c2| {
                            c2.child(div().min_w_0().truncate().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.text_faint)).child(SharedString::from(shown(&meta.join(" · ")))))
                        }),
                )
                .when(can_close, |r| {
                    r.child(
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
            col = col.child(item);
        }

        // Earlier: saved conversations of this project. A click resumes one as a thread.
        if !rail.earlier.is_empty() {
            col = col.child(section("Earlier"));
            let shown_n = if self.rail_more { rail.earlier.len() } else { EARLIER_SHOWN.min(rail.earlier.len()) };
            for (ix, row) in rail.earlier.iter().take(shown_n).enumerate() {
                let id = row.id.clone();
                col = col.child(
                    div()
                        .id(("earlier", ix))
                        .role(gpui::Role::Button)
                        .tooltip(ui::tip(SharedString::from(format!("Resume this conversation\n{}", shown(&row.title)))))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .py(px(5.))
                        .pl(px(10.))
                        .pr(px(6.))
                        .rounded(px(t::RADIUS_ROW))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .on_click(cx.listener(move |this, _, window, cx| this.open_saved(id.clone(), window, cx)))
                        // The mark's column, empty: an earlier conversation is not doing anything.
                        .child(div().w(px(11. * 1.4)).flex_shrink_0())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(1.))
                                .child(div().truncate().text_size(px(t::TEXT_UI)).text_color(c(tk.text_secondary)).child(SharedString::from(shown(&row.title))))
                                .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.text_faint)).child(SharedString::from(row.detail.clone()))),
                        ),
                );
            }
            if rail.earlier.len() > EARLIER_SHOWN {
                let label = if self.rail_more { "Show fewer".to_owned() } else { format!("Show {} more", rail.earlier.len() - EARLIER_SHOWN) };
                col = col.child(
                    div()
                        .id("earlier-more")
                        .role(gpui::Role::Button)
                        .pl(px(10.))
                        .py(px(4.))
                        .text_size(px(t::TEXT_TAG))
                        .text_color(c(tk.text_faint))
                        .cursor_pointer()
                        .hover(|s| s.text_color(c(tk.text_secondary)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.rail_more = !this.rail_more;
                            cx.notify();
                        }))
                        .child(SharedString::from(label)),
                );
            }
        }

        // Other checkouts: places, not conversations. Folded unless the person opens the list.
        if !rail.checkouts.is_empty() {
            let open = self.rail_checkouts;
            let dirty = rail.dirty_checkouts();
            let summary = if open || dirty == 0 { String::new() } else { format!("{dirty} with changes") };
            col = col.child(
                div()
                    .id("checkouts-toggle")
                    .role(gpui::Role::Button)
                    .aria_label("Other checkouts of this repository")
                    .tooltip(ui::tip("Other git checkouts of this repository — places other agents work in, not conversations"))
                    .mt(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(10.))
                    .pr(px(6.))
                    .py(px(4.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.rail_checkouts = !this.rail_checkouts;
                        cx.notify();
                    }))
                    .child(div().flex_shrink_0().text_color(c(tk.text_faint)).child(gpui_component::Icon::new(if open { gpui_kit_assets::IconName::ChevronDown } else { gpui_kit_assets::IconName::ChevronRight }).size(px(12.))))
                    .child(div().flex_shrink_0().text_size(px(t::TEXT_TAG)).text_color(c(tk.text_faint)).child(SharedString::from(format!("Other checkouts · {}", rail.checkouts.len()))))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_TAG)).text_color(c(tk.amber_soft)).child(SharedString::from(summary))),
            );
            if open {
                for (ix, row) in rail.checkouts.iter().enumerate() {
                    let path = row.path.clone();
                    let group = SharedString::from(format!("checkout-row-{ix}"));
                    let remove_group = group.clone();
                    let tip = format!("Move this window to {}\n{}{}", row.branch, super::tilde(&row.path), row.subject.as_ref().map(|s| format!("\nlast commit: {}", shown(s))).unwrap_or_default());
                    let remove = row.can_remove.then(|| (row.path.clone(), Some(row.branch.clone()).filter(|b| !b.is_empty())));
                    col = col.child(
                        div()
                            .id(("checkout", ix))
                            .group(group)
                            .role(gpui::Role::Button)
                            .tooltip(ui::tip(SharedString::from(tip)))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .py(px(4.))
                            .pl(px(10.))
                            .pr(px(6.))
                            .rounded(px(t::RADIUS_ROW))
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .on_click(cx.listener(move |this, _, window, cx| this.switch_project(path.clone(), window, cx)))
                            .child(div().flex_shrink_0().text_color(c(tk.text_faint)).font_family(mono(cx)).text_size(px(t::TEXT_META)).child("⎇"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(1.))
                                    .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_secondary)).child(SharedString::from(shown(&row.name))))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(5.))
                                            .min_w_0()
                                            .font_family(mono(cx))
                                            .text_size(px(t::TEXT_TAG))
                                            .text_color(c(if row.dirty { tk.amber_soft } else { tk.text_faint }))
                                            .when_some(row.agent.clone(), |m, agent| m.child(div().flex_shrink_0().text_color(c(tk.teal)).child(SharedString::from(agent))))
                                            .child(div().min_w_0().truncate().child(SharedString::from(shown(&row.detail)))),
                                    ),
                            )
                            .when_some(remove, |r, (path, branch)| {
                                r.child(
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
                            }),
                    );
                }
            }
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
