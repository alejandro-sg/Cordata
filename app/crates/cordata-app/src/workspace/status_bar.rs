//! The 28px status bar across the whole window: active execution mode, context window gauge, session tokens, live
//! turn status or session hint, uncommitted changes shortcut, estimated cost, and daemon health.

use cordata_theme::tokens as t;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, div, px};

use super::conversation::mode_badge;
use super::{DockTab, Workspace};
use crate::hud;
use crate::theme::{c, line, mono, tk};
use crate::ui::{self, Dot};

impl Workspace {
    pub(super) fn status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let mode = view.pending_mode.clone().unwrap_or_else(|| view.mode.clone());
        let (mode_icon, mode_title) = mode_badge(&mode);
        let (fraction, percent) = match view.context.as_ref() {
            Some(gauge) => (gauge.pct, gauge.pct.map_or_else(|| format!("{}k", gauge.used / 1000), |p| format!("{:.0}%", p * 100.0))),
            None => (None, "—".into()),
        };
        let (tokens_in, tokens_out) = view.session_tokens;
        let (kind, health) = self.hub_health(cx);
        let changed = self.review.file_count();
        let mode_hover = c(tk.text_primary);
        let diff_hover = c(tk.amber);
        let label = |text: &str| div().text_color(c(tk.text_label)).child(SharedString::from(text.to_ascii_uppercase()));
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
            .child(
                div()
                    .id("status-mode")
                    .role(gpui::Role::Button)
                    .tooltip(ui::tip("Cycle safe execution modes  ⇧Tab"))
                    .cursor_pointer()
                    .text_color(c(if mode == "auto" { tk.amber } else { tk.text_secondary }))
                    .hover(move |s| s.text_color(mode_hover))
                    .on_click(cx.listener(|this, _, window, cx| this.cycle_mode(&super::CycleMode, window, cx)))
                    .child(SharedString::from(format!("{mode_icon} {mode_title}"))),
            )
            .child(div().id("context-gauge").tooltip(ui::tip(SharedString::from(format!("Context window used. {}", self.compaction_note())))).child(hud::gauge("context", fraction, &percent, cx)))
            .child(div().flex().items_center().gap(px(6.)).child(label("tokens")).child(div().text_color(c(tk.text_secondary)).child(SharedString::from(format!("{} in · {} out", kilo(tokens_in), kilo(tokens_out))))))
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
            .when_some(self.cost_estimate(cx), |bar, (cost, tip)| bar.child(div().id("session-cost").flex().items_center().gap(px(6.)).tooltip(ui::tip(SharedString::from(tip))).child(label("cost")).child(div().text_color(c(tk.text_secondary)).child(SharedString::from(cost)))))
            .child(div().flex().items_center().gap(px(6.)).child(ui::dot(kind, cx).size(px(t::DOT_SMALL))).child(div().text_color(c(if kind == Dot::Amber { tk.amber } else { tk.text_muted })).child(SharedString::from(health))))
            .into_any_element()
    }
}

impl Workspace {
    /// The session's cost at its model's list price, and what the estimate leaves out. `None` when it is turned off, or
    /// the model has no price (local and subscription models): no number is better than a made-up one.
    fn cost_estimate(&self, cx: &Context<Self>) -> Option<(String, String)> {
        if !self.prefs.show_cost {
            return None;
        }
        let view = &self.session.read(cx).view;
        let model = view.config("model").map(|o| o.current_value.clone()).unwrap_or_default();
        let (tokens_in, tokens_out) = view.session_tokens;
        let cost = crate::pricing::estimate(&model, tokens_in, tokens_out, &self.prefs.prices)?;
        let (input, output) = crate::pricing::price(&model, &self.prefs.prices)?;
        let tip = format!(
            "Estimated: this session's tokens at {model}'s list price (${input}/${output} per million in/out). Cache discounts, \
             earlier models in the conversation and your plan are not counted. Change prices under [prices] in prefs.toml."
        );
        Some((crate::pricing::dollars(cost), tip))
    }

    /// When the harness compacts, from `context.autoCompact` (unset: Strands' 70%).
    fn compaction_note(&self) -> String {
        match self.settings_view.values.get("context.autoCompact").and_then(|v| v.as_str()) {
            Some("off") => "Automatic compaction is off.".into(),
            Some(pct) => format!("Cordata compacts automatically at {pct}%."),
            None => "Cordata compacts automatically at 70%.".into(),
        }
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
