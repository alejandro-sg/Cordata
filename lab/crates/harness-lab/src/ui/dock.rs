//! The floating command deck: one panel at the bottom of the canvas holding everything that drives
//! the team — the mode dial, the steer input, and the global E-Stop.
//!
//! It used to be three places: autonomy in the header, steering in a bar at the foot of the window.
//! Two bars at opposite ends of the screen is two places to look for one decision, so they are one
//! panel now, floating over the canvas rather than taking a row from it.
//!
//! It is translucent, not blurred: gpui 0.2.2 has no backdrop filter (documented in
//! docs/meta-harness.md), so "glass" here is a tinted panel over the canvas — the canvas shows
//! around the deck, not through it.

use gpui::{Context, Div, div, prelude::*, px};

use super::kit::{button, caption, chip, keycap, mono, segment};
use crate::app::LabApp;
use crate::theme::Theme;

impl LabApp {
    /// The deck. Anchored to the bottom of the canvas and centred on it, above the canvas but under
    /// any modal — so a containment request still has the last word.
    pub(super) fn command_deck(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        div()
            .debug_selector(|| "lab-dock".to_owned())
            .absolute()
            .bottom(px(14.))
            .left_0()
            // The side column is 410 wide, plus its hairline: centring on the canvas rather than on
            // the window keeps the deck out from under the side column.
            .right(px(411.))
            .flex()
            .flex_row()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w_full()
                    .max_w(px(820.))
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(Theme::tint(theme.panel, 0.94))
                    .border_1()
                    .border_color(theme.line_strong)
                    .child(self.deck_head(cx))
                    .child(self.steer_row(cx)),
            )
    }

    /// The deck's top row: the mode dial on the left, the global E-Stop on the right.
    fn deck_head(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mode = self.current_mode();
        let online = self.model.connection.is_online() && !self.model.stale;
        let mut modes = div().debug_selector(|| "mode-selector".to_owned()).flex().flex_row().gap_1();
        for (label, key, hue, id) in [("Plan", "plan", theme.violet, "mode-plan"), ("Build", "build", theme.accent, "mode-build"), ("Auto", "auto", theme.warn, "mode-auto")] {
            let active = mode == Some(key);
            let enabled = online && mode.is_some();
            let seg = segment(label, active, enabled, hue, &theme, id);
            modes = modes.child(if enabled && !active { seg.on_click(cx.listener(move |this, _e, _w, cx| this.set_mode(key, cx))) } else { seg });
        }

        // Wrapping, not clipping: at the minimum window width the mode blurb and the E-Stop cannot
        // share a line, so the head takes a second row rather than pushing the E-Stop off the deck.
        let mut head = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_3()
            .child(caption("autonomy", &theme))
            .child(modes)
            .child(div().text_size(px(10.)).text_color(theme.faint).child(harness_client::words::mode_blurb(mode.unwrap_or(""))));

        // The global E-Stop. It refuses, it does not kill: the daemon has no sandbox to signal, so
        // this isolates every agent of the session (see THREAT_MODEL.md). The count is what it would
        // actually reach, so the button never overstates itself.
        let reach = self.estop_reach();
        if reach == 0 {
            head = head.child(chip("every agent is stopped", theme.danger)).child(div().text_size(px(10.)).text_color(theme.faint).child("press k on one to release it"));
        } else {
            let label = if reach > 1 { format!("Stop all {reach}") } else { "Stop".to_owned() };
            head = head
                .child(if online { button(label, theme.danger, true, &theme, "estop-all").on_click(cx.listener(|this, _e, _w, cx| this.estop_all(cx))).into_any_element() } else { div().into_any_element() })
                .child(div().text_size(px(10.)).text_color(theme.faint).child("isolates every agent of this session"));
        }

        head.child(div().flex().flex_row().items_center().gap_3().child(keys(shortcuts(), &theme)))
    }
}

/// The keys the deck itself explains, so the panel says what it answers to where it is. Built from
/// one list so the `keyboard list says the keys the app actually handles` test can hold it honest.
pub fn shortcuts() -> [(&'static str, &'static str); 4] {
    [("/", "steer"), ("m", "mode"), ("k", "stop"), ("esc", "later")]
}

fn keys(list: [(&'static str, &'static str); 4], theme: &Theme) -> Div {
    let mut row = div().flex().flex_row().items_center().gap_3();
    for (key, does) in list {
        row = row.child(div().flex().flex_row().items_center().gap_1().child(keycap(key, theme)).child(mono(does, theme.faint)));
    }
    row
}
