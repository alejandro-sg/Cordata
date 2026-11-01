//! The left sidebar (256px): traffic-light bar with collapse toggle (`⌘B`), brand header, project & trust card,
//! quick `+ New Thread` (`⌘N`) and `Search or run…` (`⌘K`), active threads, recent project history, and daemon footer.

use cordata_theme::tokens as t;
use cordata_view::app::ConnState;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, div, px};

use super::{DockTab, Overlay, Workspace, age, matches_target, tilde};
use crate::hud;
use crate::theme::{amber, c, inset_ring, line, mono, tk};
use crate::ui::{self, Dot, Kind, Tone};

impl Workspace {
    /// The ⋯ on a sidebar row (hub conversations only): Rename, Archive. Shown on hover, or while its menu is open.
    fn row_actions(&self, group: SharedString, handle: Option<String>, title: String, cx: &mut Context<Self>) -> Option<AnyElement> {
        let _ = (&group, &handle, &title, &cx);
        return None;
        #[allow(unreachable_code)]
        let handle = handle?;
        let open = self.row_menu.as_deref() == Some(handle.as_str());
        let toggle = handle.clone();
        let button = ui::icon_button(SharedString::from(format!("row-more-{handle}")), "⋯", cx)
            .size(px(20.))
            .aria_label("Conversation actions")
            .tooltip(ui::tip("Rename or archive"))
            .when(!open, |b| b.opacity(0.).group_hover(group, |s| s.opacity(1.)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.row_menu = if this.row_menu.as_deref() == Some(toggle.as_str()) { None } else { Some(toggle.clone()) };
                cx.notify();
            }));
        let menu = open.then(|| {
            let (rename, archive) = (handle.clone(), handle.clone());
            gpui::deferred(
                gpui::anchored().snap_to_window_with_margin(px(8.)).child(
                    ui::popover(cx)
                        .id("row-menu")
                        .role(gpui::Role::Menu)
                        .mt(px(22.))
                        .w(px(160.))
                        .p(px(4.))
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.row_menu = None;
                            cx.notify();
                        }))
                        .child(ui::row("row-rename", false, cx).text_size(px(t::TEXT_UI)).child("Rename…").on_click(cx.listener(move |this, _, window, cx| this.begin_rename(rename.clone(), title.clone(), window, cx))))
                        .child(ui::row("row-archive", false, cx).text_size(px(t::TEXT_UI)).child("Archive").on_click(cx.listener(move |this, _, _, cx| this.archive(archive.clone(), cx)))),
                ),
            )
            .with_priority(2)
        });
        Some(div().flex().flex_col().child(button).children(menu).into_any_element())
    }

    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        div()
            .id("sidebar")
            .role(gpui::Role::Navigation)
            .aria_label("Projects and conversations")
            .w(px(t::SIDEBAR_W))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(c(tk.bg_panel))
            .border_r_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            // Traffic lights occupy the left ~76px of this row; empty space drags the window, right holds the collapse button.
            .child(
                div()
                    .h(px(t::HEADER_H))
                    .flex_shrink_0()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .pl(px(78.))
                    .pr(px(10.))
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(
                        ui::icon_button("hide-sidebar", "◧", cx)
                            .aria_label("Hide sidebar (⌘B)")
                            .tooltip(ui::tip("Hide sidebar  ⌘B"))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(&super::ToggleSidebar, window, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(t::GUTTER))
                    .pb(px(12.))
                    .child(hud::bracket_title("Cordata", cx))
                    .child(ui::badge(&format!("v{}", env!("CARGO_PKG_VERSION")), Tone::Neutral, cx)),
            )
            .child(div().px(px(12.)).child(self.project_card(cx)))
            .child(
                div()
                    .px(px(12.))
                    .pt(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(self.new_thread_button(cx))
                    .child(self.search_field(cx)),
            )
            .child(self.open_list(cx))
            .child(self.recent_list(cx))
            .child(self.sidebar_footer(cx))
            .into_any_element()
    }

    fn new_thread_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let hover_bg = amber(cx, t::AMBER_FILL_STRONG);
        let hover_ring = amber(cx, t::AMBER_BORDER_STRONG);
        div()
            .id("new-session")
            .role(gpui::Role::Button)
            .aria_label("New thread (⌘N)")
            .tooltip(ui::tip("Start a new thread  ⌘N"))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(32.))
            .px(px(10.))
            .rounded(px(t::RADIUS_CARD))
            .bg(amber(cx, t::AMBER_FILL))
            .border_1()
            .border_color(amber(cx, t::AMBER_BORDER))
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg).border_color(hover_ring))
            .on_click(cx.listener(|this, _, window, cx| this.new_session(&super::NewSession, window, cx)))
            .child(div().text_size(px(t::TEXT_TITLE)).font_weight(FontWeight::MEDIUM).text_color(c(tk.amber)).child("+"))
            .child(div().flex_1().truncate().text_size(px(t::TEXT_UI)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child("New thread"))
            .child(ui::keycap("⌘N", cx))
            .into_any_element()
    }

    fn project_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let name_hover = line(cx, t::HOVER);
        let dirty_count = self.review.files.len();
        let dirty_hover = line(cx, t::HOVER_STRONG);
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(10.))
            .rounded(px(t::RADIUS_CARD))
            .bg(line(cx, t::FILL_FAINT))
            .border_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(ui::section_label("workspace", cx))
                    .when(dirty_count > 0, |row| {
                        row.child(
                            div()
                                .id("sidebar-dirty-pill")
                                .role(gpui::Role::Button)
                                .tooltip(ui::tip("Inspect uncommitted changes  ⌘⌥4"))
                                .px(px(6.))
                                .py(px(1.))
                                .rounded(px(t::RADIUS_BADGE))
                                .bg(line(cx, t::HOVER))
                                .cursor_pointer()
                                .hover(move |s| s.bg(dirty_hover))
                                .font_family(mono(cx))
                                .text_size(px(t::TEXT_LABEL))
                                .text_color(c(tk.amber))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_panel(true, cx);
                                    this.switch_dock(DockTab::Review, cx);
                                }))
                                .child(SharedString::from(format!("{dirty_count} changed"))),
                        )
                    }),
            )
            .child(
                div()
                    .id("project-name")
                    .role(gpui::Role::Button)
                    .aria_label("Switch project (⌘O)")
                    .tooltip(ui::tip("Switch project  ⌘O"))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .mx(px(-4.))
                    .px(px(6.))
                    .py(px(4.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .hover(move |s| s.bg(name_hover))
                    .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Picker, window, cx)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_TITLE)).font_weight(FontWeight::SEMIBOLD).text_color(c(tk.text_primary)).child(SharedString::from(self.project_name())))
                            .child(div().flex_shrink_0().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("⌄")),
                    )
                    .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(tilde(&self.cwd))))
                    .when_some(self.git.branch.clone(), |col, branch| {
                        col.child(
                            div()
                                .pt(px(2.))
                                .truncate()
                                .font_family(mono(cx))
                                .text_size(px(t::TEXT_META))
                                .text_color(c(tk.teal))
                                .child(SharedString::from(format!("⎇ {branch}"))),
                        )
                    }),
            )
            .into_any_element()
    }

    fn search_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let hover = line(cx, t::LINE_CONTROL_STRONG);
        div()
            .id("sidebar-search")
            .role(gpui::Role::Button)
            .aria_label("Search or run (⌘K)")
            .tooltip(ui::tip("Command palette & thread search  ⌘K"))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(10.))
            .rounded(px(t::RADIUS_CARD))
            .bg(c(tk.bg_input))
            .border_1()
            .border_color(line(cx, t::LINE_CONTROL))
            .cursor_pointer()
            .hover(move |s| s.border_color(hover))
            .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Palette, window, cx)))
            .child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_faint)).child("⌕"))
            .child(div().flex_1().truncate().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("Search or run…"))
            .child(ui::keycap("⌘K", cx))
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
                let s = entity.read(cx);
                let view = &s.view;
                let active = entity.entity_id() == self.session.entity_id();
                let waiting = !view.pending.is_empty();
                let kind = match (&view.conn, view.turn_active) {
                    (ConnState::Connecting, _) => Dot::Hollow,
                    (ConnState::Closed { .. }, _) => Dot::Danger,
                    _ if waiting => Dot::Amber,
                    (_, true) => Dot::Live,
                    _ => Dot::Ok,
                };
                let title = view.title.clone().unwrap_or_else(|| "New thread".into());
                let group = SharedString::from(format!("open-row-{index}"));
                let actions = self.row_actions(group.clone(), s.hub_handle(), title.clone(), cx);
                let target = entity.clone();
                let hover = line(cx, t::HOVER);
                let close_group = group.clone();
                div()
                    .id(("open-session", index))
                    .group(group)
                    .role(gpui::Role::Tab)
                    .aria_selected(active)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(32.))
                    .px(px(10.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .when(active, |row| row.bg(amber(cx, t::AMBER_FILL_MID)).shadow(inset_ring(amber(cx, t::AMBER_BORDER))))
                    .when(!active, move |row| row.hover(move |s| s.bg(hover)))
                    .on_click(cx.listener(move |this, _, _, cx| this.show(target.clone(), cx)))
                    .child(ui::dot(kind, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(t::TEXT_UI))
                            .font_weight(if active { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                            .text_color(c(if active { tk.text_primary } else { tk.text_secondary }))
                            .child(SharedString::from(shown(&title))),
                    )
                    .when(waiting, |row| row.child(ui::badge("Approval", Tone::Amber, cx)))
                    .when(index < 9 && !waiting, |row| row.child(ui::meta(SharedString::from(format!("⌘{}", index + 1)), cx).text_size(px(t::TEXT_LABEL))))
                    .children(actions)
                    .when(can_close, |row| {
                        row.child(
                            ui::icon_button(("close-session", index), "×", cx)
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
            .gap(px(2.))
            .px(px(12.))
            .pt(px(16.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(4.))
                    .pb(px(6.))
                    .child(ui::section_label("active threads", cx))
                    .child(ui::meta(SharedString::from(format!("{}", self.sessions.len())), cx).text_size(px(t::TEXT_LABEL))),
            )
            .children(rows)
            .into_any_element()
    }

    fn recent_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let now = super::now_secs();
        let shown_rows: Vec<crate::session::Row> = self.listing.iter().filter(|row| !self.sessions.iter().any(|s| matches_target(s.read(cx), &row.target))).take(20).cloned().collect();
        let is_empty = shown_rows.is_empty() && self.listing_error.is_none();
        let rows: Vec<AnyElement> = shown_rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let target = row.target.clone();
                let hover = line(cx, t::HOVER);
                let group = SharedString::from(format!("recent-row-{index}"));
                let handle = match &row.target {
                    crate::session::Target::Conversation(h) => Some(h.clone()),
                    _ => None,
                };
                let actions = self.row_actions(group.clone(), handle, row.title.clone(), cx);
                div()
                    .id(("recent", index))
                    .group(group)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(30.))
                    .px(px(10.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.open(target.clone(), window, cx)))
                    .child(ui::dot(if row.busy { Dot::Live } else if row.live { Dot::Ok } else { Dot::Muted }, cx).size(px(t::DOT_SMALL)))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_UI)).text_color(c(tk.text_secondary)).child(SharedString::from(shown(&row.title))))
                    .child(ui::meta(SharedString::from(format!("{}{}", super::door_glyph(&row.origin), age(now - row.when))), cx))
                    .children(actions)
                    .into_any_element()
            })
            .collect();
        div()
            .id("recent-list")
            .flex()
            .flex_col()
            .gap(px(2.))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(12.))
            .pt(px(14.))
            .pb(px(8.))
            .child(div().px(px(4.)).pb(px(6.)).child(ui::section_label("history", cx)))
            .children(rows)
            .when(is_empty, |list| {
                list.child(
                    div()
                        .px(px(8.))
                        .py(px(6.))
                        .text_size(px(t::TEXT_SMALL))
                        .text_color(c(tk.text_faint))
                        .child("Saved threads for this folder appear here."),
                )
            })
            .when_some(self.listing_error.clone(), |list, error| list.child(div().px(px(4.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.danger)).child(SharedString::from(shown(&error)))))
            .into_any_element()
    }

    fn sidebar_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let (kind, text) = self.connection_status(cx);
        let settings_hover = line(cx, t::HOVER_STRONG);
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(44.))
            .flex_shrink_0()
            .px(px(12.))
            .border_t_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .child(ui::dot(kind, cx))
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .font_family(mono(cx))
                    .text_size(px(t::TEXT_META))
                    .text_color(c(if kind == Dot::Amber { tk.amber } else { tk.text_muted }))
                    .child(SharedString::from(text)),
            )
            .when(kind == Dot::Amber, |row| {
                row.child(ui::button("retry-hub", "Retry", Kind::Ghost, cx).h(px(22.)).px(px(6.)).text_size(px(t::TEXT_SMALL)).on_click(cx.listener(|this, _, window, cx| this.reconnect(window, cx))))
            })
            .child(
                div()
                    .id("open-settings")
                    .role(gpui::Role::Button)
                    .aria_label("Settings (⌘,)")
                    .tooltip(ui::tip("Settings  ⌘,"))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(26.))
                    .px(px(8.))
                    .rounded(px(t::RADIUS_CONTROL))
                    .cursor_pointer()
                    .text_size(px(t::TEXT_SMALL))
                    .text_color(c(tk.text_secondary))
                    .hover(move |s| s.bg(settings_hover).text_color(c(tk.text_primary)))
                    .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Settings, window, cx)))
                    .child("⚙")
                    .child("Settings"),
            )
            .into_any_element()
    }
}
