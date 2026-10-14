//! The sidebar (248px): the traffic-light row, the logo, the project card (name, path, trust), "Search or run…",
//! the open conversations, the project's other conversations, and the connection footer with ⚙.

use cordata_theme::tokens as t;
use cordata_view::app::ConnState;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, div, px};

use super::{Overlay, Workspace, age, matches_target, tilde};
use crate::hud;
use crate::theme::{amber, c, inset_ring, line, mono, tk};
use crate::ui::{self, Dot, Kind, Tone};

impl Workspace {
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
            // The traffic lights sit in this row; the empty part drags the window.
            .child(div().h(px(t::HEADER_H)).flex_shrink_0().w_full().on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(t::GUTTER))
                    .pb(px(14.))
                    .child(hud::bracket_title("Cordata", cx))
                    .child(ui::meta(SharedString::from(format!("v{}", env!("CARGO_PKG_VERSION"))), cx).text_size(px(t::TEXT_LABEL))),
            )
            .child(div().px(px(12.)).child(self.project_card(cx)))
            .child(div().px(px(12.)).pt(px(10.)).child(self.search_field(cx)))
            .child(self.open_list(cx))
            .child(self.recent_list(cx))
            .child(self.sidebar_footer(cx))
            .into_any_element()
    }

    fn project_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let trusted = self.settings_view.trusted;
        let name_hover = line(cx, t::HOVER);
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(12.))
            .rounded(px(t::RADIUS_CARD))
            .bg(line(cx, t::FILL_FAINT))
            .border_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .child(ui::section_label("project", cx))
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
                    .px(px(4.))
                    .py(px(2.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .hover(move |s| s.bg(name_hover))
                    .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Picker, window, cx)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_TITLE)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(self.project_name())))
                            .child(div().flex_shrink_0().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("⌄")),
                    )
                    .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(tilde(&self.cwd)))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .pt(px(2.))
                    .when(trusted, |row| {
                        row.child(ui::badge("● trusted", Tone::Teal, cx))
                            .child(ui::button("revoke-trust", "Revoke", Kind::Ghost, cx).h(px(22.)).px(px(6.)).text_size(px(t::TEXT_SMALL)).on_click(cx.listener(|this, _, _, cx| this.revoke_trust(None, cx))))
                    })
                    .when(!trusted, |row| {
                        row.child(ui::badge("◆ untrusted", Tone::Amber, cx))
                            .child(ui::button("trust-project", "Trust project", Kind::Ghost, cx).h(px(22.)).px(px(6.)).text_size(px(t::TEXT_SMALL)).on_click(cx.listener(|this, _, window, cx| this.ask_trust(window, cx))))
                    }),
            )
            .when(!trusted, |card| card.child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("Asks before every write and shell command.")))
            .into_any_element()
    }

    fn search_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let hover = line(cx, t::LINE_CONTROL_STRONG);
        div()
            .id("sidebar-search")
            .role(gpui::Role::Button)
            .aria_label("Search or run (⌘K)")
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(32.))
            .px(px(10.))
            .rounded(px(t::RADIUS_CARD))
            .bg(c(tk.bg_input))
            .border_1()
            .border_color(line(cx, t::LINE_CONTROL))
            .cursor_pointer()
            .hover(move |s| s.border_color(hover))
            .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Palette, window, cx)))
            .child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_faint)).child("⌕"))
            .child(div().flex_1().truncate().text_size(px(t::TEXT_UI)).text_color(c(tk.text_muted)).child("Search or run…"))
            .child(ui::keycap("⌘K", cx))
            .into_any_element()
    }

    fn open_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
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
                let title = view.title.clone().unwrap_or_else(|| "New conversation".into());
                let target = entity.clone();
                let hover = line(cx, t::HOVER);
                div()
                    .id(("open-session", index))
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
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_UI)).text_color(c(if active { tk.text_primary } else { tk.text_secondary })).child(SharedString::from(shown(&title))))
                    .when(waiting, |row| row.child(ui::badge("ask", Tone::Amber, cx)))
                    .when(index < 9, |row| row.child(ui::meta(SharedString::from(format!("⌘{}", index + 1)), cx).text_size(px(t::TEXT_LABEL))))
                    .into_any_element()
            })
            .collect();
        let new_hover = c(tk.text_primary);
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(12.))
            .pt(px(18.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(4.))
                    .pb(px(6.))
                    .child(ui::section_label("open", cx))
                    .child(
                        div()
                            .id("new-session")
                            .role(gpui::Role::Button)
                            .aria_label("New conversation (⌘N)")
                            .tooltip(ui::tip("New conversation  ⌘N"))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .cursor_pointer()
                            .text_size(px(t::TEXT_UI))
                            .text_color(c(tk.text_secondary))
                            .hover(move |s| s.text_color(new_hover))
                            .on_click(cx.listener(|this, _, window, cx| this.new_session(&super::NewSession, window, cx)))
                            .child("+ New")
                            .child(ui::meta("⌘N", cx).text_size(px(t::TEXT_LABEL))),
                    ),
            )
            .children(rows)
            .into_any_element()
    }

    fn recent_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let now = super::now_secs();
        let rows: Vec<AnyElement> = self
            .listing
            .iter()
            .filter(|row| !self.sessions.iter().any(|s| matches_target(s.read(cx), &row.target)))
            .take(20)
            .enumerate()
            .map(|(index, row)| {
                let target = row.target.clone();
                let hover = line(cx, t::HOVER);
                div()
                    .id(("recent", index))
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
            .pt(px(18.))
            .child(div().px(px(4.)).pb(px(6.)).child(ui::section_label("recent", cx)))
            .children(rows)
            .when_some(self.listing_error.clone(), |list, error| list.child(div().px(px(4.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.danger)).child(SharedString::from(shown(&error)))))
            .into_any_element()
    }

    fn sidebar_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let (kind, text) = self.connection_status(cx);
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(44.))
            .flex_shrink_0()
            .px(px(t::GUTTER))
            .border_t_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .child(ui::dot(kind, cx))
            .child(div().flex_1().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(if kind == Dot::Amber { tk.amber } else { tk.text_muted })).child(SharedString::from(text)))
            .when(kind == Dot::Amber, |row| {
                row.child(ui::button("retry-hub", "Retry", Kind::Ghost, cx).h(px(22.)).px(px(6.)).text_size(px(t::TEXT_SMALL)).on_click(cx.listener(|this, _, window, cx| this.reconnect(window, cx))))
            })
            .child(
                ui::icon_button("open-settings", "⚙", cx)
                    .aria_label("Settings (⌘,)")
                    .tooltip(ui::tip("Settings  ⌘,"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_overlay(Overlay::Settings, window, cx))),
            )
            .into_any_element()
    }
}
