//! The frame around the board: the title bar, the mission banner, and the steer bar.
//!
//! Read top to bottom they answer, in order: *is the connection good and how much freedom have I
//! given the team* (header), *what is the team doing and is anything risky* (banner), and *what do I
//! want it to do next* (the steer bar — the one place the window takes text from you).

use gpui::{Context, Div, FontWeight, WindowControlArea, div, prelude::*, px};
use harness_client::{ConnectionState, Tone, words};

use super::gauges::{MEMORY_ALARM_GB, ThreatGauge, memory_ring, memory_summary};
use super::kit::{MONO, button, caption, chip, keycap, led, mono, plural, text, tone_color};
use crate::app::{LabApp, Tab};
use crate::theme::{self, Theme};

/// The connection as the header's chip says it, and its colour.
pub fn connection_short(state: &ConnectionState, theme: &Theme) -> (String, gpui::Rgba, bool) {
    match state {
        ConnectionState::Connecting => ("CONNECTING".to_owned(), theme.warn, true),
        ConnectionState::Online { .. } => ("ONLINE".to_owned(), theme.ok, false),
        ConnectionState::Offline { attempt, .. } => (format!("OFFLINE · RETRY {attempt}"), theme.danger, true),
    }
}

impl LabApp {
    // --------------------------------------------------------------------- header

    pub(super) fn header(&self, _cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let (status, color, pulse) = connection_short(self.connection(), &theme);
        // The autonomy selector moved to the command deck, with the steer input it belongs beside.
        let mut row = div()
            .debug_selector(|| "lab-header".to_owned())
            .window_control_area(WindowControlArea::Drag)
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .gap_4()
            // Room for the traffic lights: the titlebar is transparent, so this row *is* the titlebar.
            .pl(px(84.))
            .pr_4()
            .h(px(46.))
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .child(div().flex().flex_row().items_center().gap_2().child(div().text_color(theme.accent).child("◆")).child(div().font_weight(FontWeight::BOLD).text_size(px(13.)).child("CORDATA")))
            .child(div().w(px(1.)).h(px(18.)).bg(theme.border))
            .child(div().flex_1());
        if self.model.labs.len() > 1 {
            row = row.child(chip(format!("session {} of {} · tab", self.session_position(), self.model.labs.len()), theme.dim));
        }
        row.child(div().flex().flex_row().items_center().gap_2().child(led("conn-led", color, pulse)).child(chip(status, color)))
    }

    fn session_position(&self) -> usize {
        let focus = self.model.focus.clone().unwrap_or_default();
        self.model.labs.keys().position(|k| *k == focus).map(|i| i + 1).unwrap_or(1)
    }

    // --------------------------------------------------------------------- banner

    /// The mission banner: what the team is doing, whether it is fine, and the two numbers that decide
    /// whether to worry (Laya's last reading and the memory left).
    pub(super) fn mission_banner(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let lab = self.model.lab();
        let online = self.model.connection.is_online();
        let summary = words::team_summary(lab, self.model.interventions.len(), online);
        let state_color = tone_color(summary.tone(), &theme);
        let pulse = matches!(summary.tone(), Tone::Info | Tone::Warn);

        let mut counts = Vec::new();
        if summary.working > 0 {
            counts.push(format!("{} working", summary.working));
        }
        let waiting = self.model.interventions.len().max(summary.waiting);
        if waiting > 0 {
            counts.push(format!("{waiting} waiting on you"));
        }
        if summary.idle > 0 {
            counts.push(format!("{} idle", summary.idle));
        }
        if summary.stopped > 0 {
            counts.push(format!("{} stopped", summary.stopped));
        }

        let mission = lab.map(|l| l.mission.trim().to_owned()).filter(|m| !m.is_empty());
        let mission_text = mission.unwrap_or_else(|| match summary.state {
            words::TeamState::Offline => "The daemon is not reachable — this is the last picture it sent.".to_owned(),
            words::TeamState::Idle | words::TeamState::Stopped => "Nothing is running. Tell the team what to do below, or start a task from the terminal.".to_owned(),
            _ => lab.and_then(|l| l.pods.iter().find(|p| p.parent.is_empty()).or(l.pods.first())).map(|p| words::now_line(p).0).unwrap_or_else(|| "Working…".to_owned()),
        });

        let left = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(led("team-led", state_color, pulse))
                    .child(div().debug_selector(|| "team-state".to_owned()).text_size(px(22.)).font_weight(FontWeight::BOLD).text_color(state_color).child(summary.headline()))
                    .child(div().text_sm().text_color(theme.dim).child(counts.join(" · "))),
            )
            .child(div().debug_selector(|| "mission".to_owned()).text_sm().text_color(theme.text).line_clamp(2).child(mission_text))
            .child(div().text_xs().text_color(theme.faint).child(words::mode_blurb(self.current_mode().unwrap_or(""))));

        // ---- the two numbers: Laya's last reading, and the memory left -------------------------
        let threat = self.threat();
        let (score, band, tool_pod, pending) = match &threat {
            Some(t) => (t.score, t.band.clone(), if t.pod.is_empty() { String::new() } else { format!("{} · {}", t.pod, t.tool) }, t.pending),
            None => (None, String::new(), String::new(), false),
        };
        let gate = lab.map(|l| l.default_dial).filter(|g| *g > 0.0).unwrap_or(0.8);
        let risk_color = theme::threat_color(score, &theme);
        let band_word = if pending {
            "AT THE GATE".to_owned()
        } else if !band.is_empty() {
            band.to_uppercase()
        } else {
            score.map(|s| theme::band(s).to_uppercase()).unwrap_or_else(|| "NO READING".to_owned())
        };
        let laya = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(caption("laya · last call", &theme))
                    .child(div().debug_selector(|| "threat-score".to_owned()).font_family(MONO).text_size(px(15.)).text_color(risk_color).child(theme::score_text(score)))
                    .child(chip(band_word, if pending { theme.warn } else { risk_color }))
                    .child(div().flex_1())
                    .child(div().max_w(px(130.)).truncate().text_size(px(10.5)).text_color(theme.faint).child(tool_pod)),
            )
            .child(ThreatGauge { score, gate, theme, compact: true });

        let memory = match &self.model.omlx {
            Some(omlx) => div()
                .flex()
                .flex_col()
                .gap_1()
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(caption("memory", &theme))
                        .child(theme::human_gb(omlx.free_gb) + " free of " + &theme::human_gb(omlx.budget_gb))
                        .child(div().flex_1())
                        .child(button("Unload", theme.danger, false, &theme, "unload-memory").on_click(cx.listener(|this, _e, _w, cx| this.unload_memory(false, cx)))),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_3()
                        .child(
                            div().id("memory-ring-hover").flex_none().on_hover(cx.listener(|this, hovered: &bool, _w, cx| {
                                if this.memory_hovered != *hovered {
                                    this.memory_hovered = *hovered;
                                    cx.notify();
                                }
                            })).child(memory_ring(omlx, &theme, self.memory_hovered)),
                        )
                        .child(div().flex_1().min_w_0().child(memory_summary(omlx, &theme))),
                ),
            None => div().flex().flex_row().items_center().gap_2().child(caption("memory", &theme)).child(text("no data yet", theme.faint)).child(div().flex_1()).child(button("Unload", theme.danger, false, &theme, "unload-memory").on_click(cx.listener(|this, _e, _w, cx| this.unload_memory(false, cx)))),
        };
        let _ = MEMORY_ALARM_GB;

        div()
            .debug_selector(|| "lab-banner".to_owned())
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .gap_6()
            .px_4()
            .py_3()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .child(left)
            .child(
                div()
                    .id("banner-safety")
                    .debug_selector(|| "banner-safety".to_owned())
                    .flex()
                    .flex_none()
                    .flex_col()
                    .gap_2()
                    .w(px(340.))
                    .overflow_hidden()
                    .child(laya)
                    .child(memory)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _e, _w, cx| this.set_tab(Tab::System, cx))),
            )
    }

    // ---------------------------------------------------------------------- steer bar

    /// The steer row, inside the command deck. The text goes to the selected agent as an operator
    /// directive (it reads it before its next model call, and it outranks earlier plans); selecting
    /// the orchestrator steers the whole team.
    pub(super) fn steer_row(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let online = self.model.connection.is_online() && !self.model.stale;
        let pod = self.selected_pod();
        let target = pod.map(|p| p.name.clone());
        let can_send = online && target.is_some() && !self.composer.trim().is_empty();

        let placeholder = match (&target, online) {
            (_, false) => "Offline — you can't steer right now".to_owned(),
            (None, true) => "No agent to steer yet".to_owned(),
            (Some(name), true) => format!("Tell {name} what to do next — e.g. “focus on the failing tests first”"),
        };
        let shown = if self.composer.is_empty() {
            div().text_sm().text_color(theme.faint).child(placeholder)
        } else {
            div().text_sm().text_color(theme.text).child(format!("{}{}", self.composer, if self.composing { "▏" } else { "" }))
        };
        let field = div()
            .id("composer")
            .debug_selector(|| "lab-composer-field".to_owned())
            .flex_1()
            .min_w_0()
            .px_3()
            .py(px(7.))
            .rounded_md()
            .bg(theme.bg)
            .border_1()
            .border_color(if self.composing { theme.accent } else { theme.line_strong })
            .cursor_text()
            .on_click(cx.listener(|this, _e, _w, cx| this.begin_composing(cx)))
            .child(if self.composing && self.composer.is_empty() { div().text_sm().text_color(theme.text).child("▏") } else { shown });

        let mut bar = div()
            .debug_selector(|| "lab-composer".to_owned())
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .gap_2()
            .child(chip(format!("→ {}", target.clone().unwrap_or_else(|| "—".to_owned())), if online { theme.accent } else { theme.faint }))
            .child(field);
        if let Some(pod) = pod
            && pod.guidance_pending > 0
        {
            bar = bar.child(chip(format!("{} directive{} waiting", pod.guidance_pending, plural(pod.guidance_pending as usize)), theme.violet));
        }
        bar = if can_send {
            bar.child(button("Send", theme.accent, true, &theme, "steer-send").on_click(cx.listener(|this, _e, _w, cx| this.steer(cx))))
        } else {
            bar.child(div().flex().flex_row().items_center().gap_1().child(keycap("/", &theme)).child(mono("steer", theme.faint)))
        };
        bar
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn the_header_chip_says_the_state_and_the_attempt() {
        let t = Theme::dark();
        let offline = ConnectionState::Offline { since: Instant::now(), attempt: 3, retry_at: Instant::now(), error: "cannot reach the daemon at 127.0.0.1:8766".into() };
        let (text, color, pulse) = connection_short(&offline, &t);
        assert!(text.contains("OFFLINE") && text.contains('3') && color == t.danger && pulse, "{text}");
        let (text, color, pulse) = connection_short(&ConnectionState::Online { since: Instant::now() }, &t);
        assert_eq!((text.as_str(), color, pulse), ("ONLINE", t.ok, false), "a settled state does not pulse");
    }

    #[test]
    fn the_memory_alarm_and_the_free_colour_agree() {
        let t = Theme::dark();
        assert_eq!(crate::ui::gauges::free_color(MEMORY_ALARM_GB - 0.1, &t), t.danger);
        assert_ne!(crate::ui::gauges::free_color(MEMORY_ALARM_GB + 10.0, &t), t.danger);
    }
}
