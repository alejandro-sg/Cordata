//! The 28px status bar across the whole window: the context window used, the session's tokens, the turn in progress
//! or the session's hint, and on the right the cost and the hub's health.

use cordata_theme::tokens::{self as t, Tracking, tracked_text};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, div, px};

use super::Workspace;
use crate::hud;
use crate::theme::{c, line, mono, tk};
use crate::ui::{self, Dot};

impl Workspace {
    pub(super) fn status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let (fraction, percent) = match view.context.as_ref() {
            Some(gauge) => (gauge.pct, gauge.pct.map_or_else(|| format!("{}k", gauge.used / 1000), |p| format!("{:.0}%", p * 100.0))),
            None => (None, "—".into()),
        };
        let (tokens_in, tokens_out) = view.session_tokens;
        let (kind, health) = self.hub_health(cx);
        let label = |text: &str| div().text_color(c(tk.text_label)).child(SharedString::from(tracked_text(text, Tracking::Label, true)));
        div()
            .id("status-bar")
            .role(gpui::Role::Status)
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(20.))
            .h(px(t::STATUS_H))
            .px(px(t::GUTTER))
            .bg(c(tk.bg_panel))
            .border_t_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .font_family(mono(cx))
            .text_size(px(t::TEXT_STATUS))
            .whitespace_nowrap()
            .overflow_hidden()
            .child(div().id("context-gauge").tooltip(ui::tip("Context window used. Cordata compacts automatically at 90%.")).child(hud::gauge("context", fraction, &percent, cx)))
            .child(div().flex().items_center().gap(px(6.)).child(label("tokens")).child(div().text_color(c(tk.text_secondary)).child(SharedString::from(format!("{} in · {} out", kilo(tokens_in), kilo(tokens_out))))))
            .when(view.turn_active, |bar| bar.child(div().text_color(c(tk.amber)).child(SharedString::from(format!("● working · {} tool{} · esc esc stops", view.turn_tools, if view.turn_tools == 1 { "" } else { "s" })))))
            .when_some(self.esc_hint(), |bar, hint| bar.child(div().text_color(c(tk.amber)).child(hint)))
            .when_some(view.hint.clone(), |bar, hint| bar.child(div().min_w_0().truncate().text_color(c(tk.text_muted)).child(SharedString::from(shown(&hint)))))
            .child(div().flex_1())
            .child(div().flex().items_center().gap(px(6.)).child(ui::dot(kind, cx).size(px(t::DOT_SMALL))).child(div().text_color(c(if kind == Dot::Amber { tk.amber } else { tk.text_muted })).child(SharedString::from(health))))
            .into_any_element()
    }
}

/// `1.2k`, `0k` → as the reference writes them.
fn kilo(n: u64) -> String {
    match n {
        0..1_000 => format!("{}k", n / 1000),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{}k", n / 1000),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_counts_read_short() {
        assert_eq!(super::kilo(0), "0k");
        assert_eq!(super::kilo(1234), "1.2k");
        assert_eq!(super::kilo(45_000), "45k");
    }
}
