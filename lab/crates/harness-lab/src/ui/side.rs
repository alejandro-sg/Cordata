//! The side column: what needs you, the selected agent and its controls, and the tabs that hold
//! everything else.
//!
//! Order is priority. **Needs you** appears only when something does, and is the first thing in the
//! column. **The agent** is the one you are looking at: what it is doing, and the controls that act on
//! it (stop, steer, how hard it thinks, how soon Laya asks). The **tabs** hold the rest, one at a time,
//! so nothing you are not looking at competes for your attention: what just happened, what the team
//! produced, and the diagnostics.

use gpui::{Context, Div, FontWeight, IntoElement, SharedString, div, prelude::*, px};
use harness_client::{ConnectionState, Tone, words};
use harness_proto::v1::{Outcome, Pod};

use super::dial::laya_dial;
use super::gauges::{MEMORY_ALARM_GB, ThreatGauge, free_color, memory_bar, memory_summary, model_color};
use super::kit::{self, MONO, banner, button, button_dyn, caption, chip, keycap, mono, plural, segment, tab, text, tone_color, truncate};
use super::overlays::approval_cards;
use super::pods::role_style;
use crate::app::{LabApp, Tab};
use crate::theme::{self, Theme};

/// The keys, for the System tab. One list, so the tab and its test cannot drift from `key()`.
pub const KEYS: &[(&str, &str)] = &[
    ("/", "steer — the deck's input, or the focused agent"),
    ("m", "autonomy in the deck: plan → build → auto"),
    ("y n k", "answer the approval on screen"),
    ("r", "review the next approval waiting"),
    ("esc", "decide later · close · leave the steer bar"),
    ("↑ ↓", "select an agent"),
    ("tab", "next session"),
    ("1-4", "reasoning effort"),
    ("[ ]", "laya threshold — drag the dial, or nudge it"),
    ("d", "open the newest diff"),
    ("u · ⇧u", "unload memory · and Ollama"),
    ("⌘q", "quit"),
];

const EFFORTS: [(&str, &str, &str); 4] = [("low", "low", "effort-low"), ("medium", "medium", "effort-medium"), ("high", "high", "effort-high"), ("max", "max", "effort-max")];

impl LabApp {
    pub(super) fn side(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let mut column = div().id("side").debug_selector(|| "lab-side".to_owned()).flex().flex_col().flex_none().w(px(410.)).h_full().gap_3().p_3().overflow_scroll().bg(theme.bg);
        if let Some(needs) = self.needs_you(cx) {
            column = column.child(needs);
        }
        column.child(self.agent_drawer(cx)).child(self.tabs(cx))
    }

    // ------------------------------------------------------------------ needs you

    fn needs_you(&self, cx: &mut Context<Self>) -> Option<Div> {
        let theme = self.theme;
        let tray = self.tray_items();
        let mut cards: Vec<Div> = approval_cards(&tray, theme, cx);

        if let Some(omlx) = &self.model.omlx
            && omlx.up
            && omlx.free_gb < MEMORY_ALARM_GB
        {
            cards.push(
                div()
                    .debug_selector(|| "memory-alarm".to_owned())
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(Theme::tint(theme.danger, 0.6))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(theme.danger).child("Memory is nearly full"))
                    .child(text(format!("{} free of a {} budget. Unload before anything heavy starts.", theme::human_gb(omlx.free_gb), theme::human_gb(omlx.budget_gb)), theme.dim))
                    .child(div().flex().flex_row().gap_2().child(button("Unload Memory", theme.danger, true, &theme, "unload-memory-alarm").on_click(cx.listener(|this, _e, _w, cx| this.unload_memory(false, cx))))),
            );
        }
        if let ConnectionState::Offline { error, attempt, .. } = self.connection() {
            cards.push(
                div()
                    .debug_selector(|| "offline-card".to_owned())
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded_lg()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(Theme::tint(theme.danger, 0.6))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(theme.danger).child("The daemon is offline"))
                    .child(text(format!("Showing the last picture it sent; nothing here can be acted on. Retrying (attempt {attempt}): {}", truncate(error, 120)), theme.dim)),
            );
        }
        if let Some(error) = &self.config_error {
            cards.push(banner(format!("No daemon config: {error}"), theme.danger));
        }
        if cards.is_empty() {
            return None;
        }
        let mut section = div().debug_selector(|| "needs-you".to_owned()).flex().flex_col().gap_2().child(
            div().flex().flex_row().items_center().gap_2().px_1().child(caption("needs you", &theme)).child(chip(cards.len().to_string(), theme.warn)),
        );
        for card in cards {
            section = section.child(card);
        }
        Some(section)
    }

    // ------------------------------------------------------------------- the agent

    fn agent_drawer(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let Some(pod) = self.selected_pod() else {
            return div().debug_selector(|| "lab-drawer".to_owned()).p_3().rounded_lg().border_1().border_color(theme.border).bg(theme.panel).child(text("Select an agent on the board.", theme.faint));
        };
        let pod: Pod = pod.clone();
        let role = role_style(&pod.role);
        let (word, tone) = words::status_word(&pod);
        let (line, line_tone) = words::now_line(&pod);
        let online = self.model.connection.is_online() && !self.model.stale;
        let has_children = self.model.lab().is_some_and(|lab| lab.pods.iter().any(|p| p.parent == pod.id));
        let stopped = words::status_word(&pod).0 == "Stopped";
        let id = pod.id.clone();

        // ---- stop / release -----------------------------------------------------------------
        let control = if stopped {
            let target = id.clone();
            button_dyn("Release", theme.ok, true, &theme, format!("release-{id}")).on_click(cx.listener(move |this, _e, _w, cx| this.release_pod(&target, cx)))
        } else {
            let target = id.clone();
            button_dyn(if has_children { "Stop + sub-agents" } else { "Stop" }, theme.danger, false, &theme, format!("stop-{id}")).on_click(cx.listener(move |this, _e, _w, cx| this.stop_pod(&target, cx)))
        };

        let mut drawer = div()
            .debug_selector(|| "lab-drawer".to_owned())
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .rounded_lg()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().text_color(theme::hex(role.color)).child(role.glyph))
                    .child(div().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(pod.name.clone()))
                    .child(chip(word, tone_color(tone, &theme)))
                    .child(div().flex_1())
                    .child(if online { control.into_any_element() } else { div().into_any_element() }),
            )
            .child(div().text_sm().text_color(tone_color(line_tone, &theme)).line_clamp(3).child(line));

        // ---- the knobs ----------------------------------------------------------------------
        let adjustable = pod.effort_adjustable && online && !pod.isolated;
        let mut effort = div().flex().flex_row().gap_1();
        for (label, level, sid) in EFFORTS {
            let active = pod.effort == level;
            let seg = segment(label, active, adjustable, theme.accent, &theme, sid);
            effort = effort.child(if adjustable && !active { seg.on_click(cx.listener(move |this, _e, _w, cx| this.set_effort(level, cx))) } else { seg });
        }
        let dial_enabled = online && !pod.isolated;
        let dragging = self.dial_drag.as_deref() == Some(pod.id.as_str());
        let drag_target = pod.id.clone();
        let bounds_slot = self.dial_bounds.clone();
        // The dial is the only control here that takes a pointer drag: the press starts it and sets
        // the value, the root's mouse move carries it on, and the release ends it.
        let dial = div()
            .id(SharedString::from(format!("dial-{}", pod.id)))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .cursor_pointer()
            .child(laya_dial(&pod, dial_enabled, dragging, &bounds_slot, &theme))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().flex().flex_row().items_center().gap_2().child(mono(format!("{:.2}", pod.dial), theme.text)).child(keycap("[ ]", &theme)))
                    .child(div().text_size(px(10.)).text_color(theme.faint).child(if dial_enabled { "drag the needle, or step it with [ and ]" } else { "offline or stopped — read-only" })),
            );
        let dial = if dial_enabled {
            dial.on_mouse_down(gpui::MouseButton::Left, cx.listener(move |this, event: &gpui::MouseDownEvent, _w, cx| this.begin_dial_drag(&drag_target, event.position, cx))).into_any_element()
        } else {
            dial.into_any_element()
        };
        drawer = drawer.child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_3()
                .rounded_md()
                .bg(theme.bg)
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().flex().flex_row().items_center().gap_2().child(caption("thinks", &theme)).child(keycap("1-4", &theme)))
                        .child(effort)
                        .child(text(if !pod.effort_adjustable { "this agent's model has no reasoning_effort knob" } else if !online { "offline — read-only" } else { "applies to its next model call" }, theme.faint)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().flex().flex_row().items_center().gap_2().child(caption("asks you at", &theme)).child(div().flex_1()))
                        .child(dial)
                        .child(text("Laya holds any call of this agent's at or above this score for your answer — in every mode. Lower = asks sooner.", theme.faint)),
                ),
        );

        // ---- what Laya has made of its calls --------------------------------------------------
        if let Some(guard) = words::guard_line(&pod) {
            drawer = drawer.child(div().flex().flex_col().gap_1().child(caption("laya guard", &theme)).child(div().debug_selector(|| "guard-line".to_owned()).text_xs().text_color(theme.dim).child(guard)));
        }

        // ---- recent work ----------------------------------------------------------------------
        if !pod.trace.is_empty() {
            let mut recent = div().flex().flex_col().gap(px(3.));
            for row in pod.trace.iter().rev().take(6) {
                let (dot, phrase) = match (row.tool.as_str(), row.status.as_str()) {
                    ("GUIDANCE", _) => (theme.violet, "You sent a directive".to_owned()),
                    ("ISOLATE", _) => (theme.danger, "Stopped".to_owned()),
                    (_, "failed") => (theme.danger, words::tool_phrase(&format!("{}: {}", row.tool, row.input))),
                    (_, "running") => (theme.accent, words::tool_phrase(&format!("{}: {}", row.tool, row.input))),
                    _ => (theme.ok, words::tool_phrase(&format!("{}: {}", row.tool, row.input))),
                };
                recent = recent.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().flex_none().size(px(6.)).rounded_full().bg(dot))
                        .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.dim).child(phrase))
                        .child(mono(if row.ms > 0 { theme::human_ms(row.ms as u64) } else { String::new() }, theme.faint)),
                );
            }
            drawer = drawer.child(div().flex().flex_col().gap_1().child(caption("recent work", &theme)).child(recent));
        }
        if !pod.task.is_empty() {
            drawer = drawer.child(div().flex().flex_col().gap_1().child(caption("task", &theme)).child(div().text_xs().text_color(theme.dim).line_clamp(3).child(pod.task.clone())));
        }
        if !pod.reasoning_tail.is_empty() {
            // Live while the model streams its reasoning: what it is thinking about right now.
            drawer = drawer.child(div().flex().flex_col().gap_1().child(caption("thinking", &theme)).child(div().text_xs().italic().text_color(theme.dim).line_clamp(5).child(pod.reasoning_tail.clone())));
        }
        if !pod.stream_tail.is_empty() {
            drawer = drawer.child(div().flex().flex_col().gap_1().child(caption("last said", &theme)).child(div().text_xs().text_color(theme.dim).line_clamp(4).child(pod.stream_tail.clone())));
        }
        drawer.child(mono(
            format!("{} · {} calls · busy {}", if pod.model.is_empty() { "no model reported".to_owned() } else { pod.model.clone() }, pod.calls, theme::human_ms(pod.busy_ms)),
            theme.faint,
        ))
    }

    // --------------------------------------------------------------------- tabs

    fn tabs(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let outputs = self.model.diffs.len();
        let strip = div()
            .flex()
            .flex_row()
            .border_b_1()
            .border_color(theme.border)
            .child(tab("Activity", None, self.tab == Tab::Activity, &theme, "tab-activity").on_click(cx.listener(|this, _e, _w, cx| this.set_tab(Tab::Activity, cx))))
            .child(tab("Outputs", Some(outputs), self.tab == Tab::Outputs, &theme, "tab-outputs").on_click(cx.listener(|this, _e, _w, cx| this.set_tab(Tab::Outputs, cx))))
            .child(tab("System", None, self.tab == Tab::System, &theme, "tab-system").on_click(cx.listener(|this, _e, _w, cx| this.set_tab(Tab::System, cx))));
        let body = match self.tab {
            Tab::Activity => self.activity_tab(cx),
            Tab::Outputs => self.outputs_tab(cx),
            Tab::System => self.system_tab(cx),
        };
        div().flex().flex_col().gap_3().child(strip).child(body)
    }

    fn activity_tab(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let selected = self.selected_pod().map(|p| p.id.clone());
        let only = self.feed_only_selected && selected.is_some();
        let mut list = div().debug_selector(|| "activity".to_owned()).flex().flex_col().gap(px(6.));
        let mut shown = 0;
        for item in self.model.feed.iter().rev() {
            if only && item.pod.as_deref() != selected.as_deref() {
                continue;
            }
            shown += 1;
            if shown > 16 {
                break;
            }
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_2()
                    .child(div().w(px(30.)).flex_none().child(mono(kit::ago(item.at.elapsed()), theme.faint)))
                    .child(div().flex_none().mt(px(5.)).size(px(6.)).rounded_full().bg(tone_color(item.tone, &theme)))
                    .child(div().flex_1().min_w_0().text_xs().text_color(if item.tone == Tone::Dim { theme.faint } else { theme.dim }).line_clamp(2).child(item.text.clone()))
                    .children((item.count > 1).then(|| mono(format!("×{}", item.count), theme.faint))),
            );
        }
        if shown == 0 {
            list = list.child(text(if only { "Nothing from this agent yet." } else { "Nothing yet. This fills in as the team works." }, theme.faint));
        }
        let filter = selected.map(|id| {
            let name = self.selected_pod().map(|p| p.name.clone()).unwrap_or(id);
            segment(&format!("only {name}"), self.feed_only_selected, true, theme.accent, &theme, "feed-filter").on_click(cx.listener(|this, _e, _w, cx| {
                this.feed_only_selected = !this.feed_only_selected;
                cx.notify();
            }))
        });
        div().flex().flex_col().gap_2().child(div().flex().flex_row().items_center().child(caption("what just happened", &theme)).child(div().flex_1()).children(filter)).child(list)
    }

    fn outputs_tab(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mut list = div().flex().flex_col().gap_2();
        if self.model.diffs.is_empty() {
            return list.child(text("Nothing produced yet. A coding task's diff lands here when it finishes.", theme.faint));
        }
        for diff in self.model.diffs.iter().take(8) {
            let id = diff.id.clone();
            let row_id = format!("diff-row-{}", diff.id);
            let files = match diff.files.len() {
                0 => String::new(),
                1 => diff.files[0].clone(),
                n => format!("{} +{} more", diff.files[0], n - 1),
            };
            list = list.child(
                div()
                    .id(SharedString::from(row_id.clone()))
                    .debug_selector(move || row_id.clone())
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .hover(|this| this.bg(theme.card).border_color(theme.accent))
                    .on_click(cx.listener(move |this, _e, _w, cx| this.fetch_diff(Some(id.clone()), cx)))
                    .child(div().flex().flex_row().gap_2().child(div().text_sm().text_color(theme.text).child(diff.source.clone())).child(div().flex_1()).child(mono(format!("{} line{}{}", diff.lines, plural(diff.lines as usize), if diff.truncated { " · truncated" } else { "" }), theme.dim)))
                    .child(div().truncate().text_xs().text_color(theme.faint).child(files)),
            );
        }
        list.child(div().flex().flex_row().items_center().gap_2().child(keycap("d", &theme)).child(text("opens the newest", theme.faint)))
    }

    /// Everything diagnostic. Present, complete and out of the way.
    fn system_tab(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let mut tab = div().debug_selector(|| "system".to_owned()).flex().flex_col().gap_4();

        // ---- the daemon ------------------------------------------------------------------------
        let mut daemon = div().flex().flex_row().flex_wrap().gap_5();
        if let Some(d) = &self.model.daemon {
            daemon = daemon
                .child(kit::readout("daemon", format!("v{} · up {}", d.version, theme::human_uptime(d.uptime_s)), theme.dim, &theme))
                .child(kit::readout("sessions", format!("{} / {} alive", d.sessions, d.alive), theme.dim, &theme))
                .child(kit::readout("ingest", format!("{} ok · {} rejected", d.ingest_accepted, d.ingest_rejected), theme.dim, &theme))
                .child(kit::readout("readers", d.telemetry_subscribers.to_string(), theme.faint, &theme));
        }
        daemon = daemon.child(kit::readout("this window", format!("up {}", theme::human_uptime(self.started.elapsed().as_secs_f64())), theme.faint, &theme));
        daemon = daemon.child(kit::readout("stream", format!("seq {} · {} frames", self.model.stats.last_seq, self.model.stats.frames), theme.faint, &theme));
        tab = tab.child(self.system_section("connection", daemon));

        // ---- Laya ---------------------------------------------------------------------------------
        let threat = self.threat();
        let (score, confidence) = threat.as_ref().map(|t| (t.score, t.confidence)).unwrap_or((None, None));
        let gate = self.model.lab().map(|l| l.default_dial).filter(|g| *g > 0.0).unwrap_or(0.8);
        let mut laya = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().flex().flex_row().gap_5().child(kit::readout("last call", theme::score_text(score), theme::threat_color(score, &theme), &theme)).child(kit::readout("confidence", theme::score_text(confidence), theme.dim, &theme)).child(kit::readout("router gate", format!("{gate:.2}"), theme.danger, &theme)))
            .child(ThreatGauge { score, gate, theme, compact: false })
            .child(text("Laya Guard scores every tool call, by every agent, on its own. A call at or above its agent's gate asks you — in every mode, Auto included. A score never approves anything; it only adds an ask.", theme.faint));
        if let Some(lab) = self.model.lab() {
            for pod in &lab.pods {
                if let Some(line) = words::guard_line(pod) {
                    laya = laya.child(div().flex().flex_row().gap_2().child(div().w(px(110.)).flex_none().truncate().text_xs().text_color(theme.text).child(pod.name.clone())).child(div().flex_1().text_xs().text_color(theme.dim).child(line)));
                }
            }
        }
        tab = tab.child(self.system_section("laya", laya));

        // ---- memory -------------------------------------------------------------------------------
        let mut memory = div().flex().flex_col().gap_2();
        match &self.model.omlx {
            None => memory = memory.child(text("The daemon has not reported memory yet.", theme.faint)),
            Some(omlx) => {
                memory = memory.child(memory_summary(omlx, &theme)).child(memory_bar(omlx, &theme));
                let mut index = 0;
                for model in &omlx.models {
                    let swatch = if model.loaded {
                        let c = model_color(index, &theme);
                        index += 1;
                        c
                    } else {
                        theme.line_strong
                    };
                    memory = memory.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().flex_none().size(px(8.)).rounded(px(2.)).bg(swatch))
                            .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_xs().text_color(if model.loaded { theme.text } else { theme.faint }).child(model.id.clone()))
                            .child(mono(theme::human_gb(model.size_gb), theme.faint))
                            .child(mono(if model.pinned { "pinned" } else if model.loaded { "loaded" } else { "inactive" }, theme.faint)),
                    );
                }
                if !omlx.ollama_resident.is_empty() {
                    memory = memory.child(text(format!("ollama resident: {}", omlx.ollama_resident.join(", ")), theme.dim));
                }
                if omlx.free_gb < MEMORY_ALARM_GB {
                    memory = memory.child(text("Below the safe margin.", tone_color(Tone::Bad, &theme)));
                }
            }
        }
        // Offered whether or not the read-out exists: an out-of-memory emergency is the worst moment to hide the control.
        memory = memory.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(button("Unload Memory", theme.danger, true, &theme, "unload-memory-system").on_click(cx.listener(|this, _e, _w, cx| this.unload_memory(false, cx))))
                .child(button("Unload + Ollama", theme.warn, false, &theme, "unload-ollama").on_click(cx.listener(|this, _e, _w, cx| this.unload_memory(true, cx))))
                .child(div().flex_1())
                .child(keycap("u", &theme))
                .child(keycap("⇧u", &theme)),
        );
        let free = self.model.omlx.as_ref().map(|o| free_color(o.free_gb, &theme)).unwrap_or(theme.faint);
        let _ = free;
        tab = tab.child(self.system_section("unified memory · 48 GB", memory));

        // ---- the GUI's own observer ---------------------------------------------------------------
        let (saves, snippets, skipped, sent) = self.observer_counters.snapshot();
        let (status, status_color) = match (&self.observer_error, &self.observer) {
            (Some(_), _) => ("OFF", theme.danger),
            (None, Some(_)) => ("WATCHING", theme.ok),
            (None, None) => ("OFF", theme.faint),
        };
        let mut observer = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().flex().flex_row().items_center().gap_2().child(chip(status, status_color)).child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(10.5)).text_color(theme.dim).child(self.observer_root.display().to_string())));
        if let Some(error) = &self.observer_error {
            observer = observer.child(text(truncate(error, 140), theme.danger));
        } else {
            observer = observer.child(div().flex().flex_row().flex_wrap().gap_5().child(kit::readout("saves", saves.to_string(), theme.text, &theme)).child(kit::readout("symbols", snippets.to_string(), theme.text, &theme)).child(kit::readout("sent", sent.to_string(), theme.ok, &theme)).child(kit::readout("dropped", skipped.to_string(), if skipped > 0 { theme.warn } else { theme.faint }, &theme)));
            let recent = self.observer_counters.recent();
            if recent.is_empty() {
                observer = observer.child(text("Save a source file in the workspace: only the changed functions are parsed and sent.", theme.faint));
            }
            for line in recent.iter().rev().take(5) {
                observer = observer.child(div().truncate().font_family(MONO).text_size(px(10.5)).text_color(theme.dim).child(line.clone()));
            }
        }
        tab = tab.child(self.system_section("workspace observer", observer));

        // ---- decisions ------------------------------------------------------------------------------
        let mut decisions = div().flex().flex_col().gap_1();
        if self.model.resolved.is_empty() {
            decisions = decisions.child(text("Nothing approved or rejected yet.", theme.faint));
        }
        for note in self.model.resolved.iter().rev().take(8) {
            let (label, color) = match note.outcome {
                Outcome::Approved => ("APPROVED", theme.ok),
                Outcome::Rejected => ("REJECTED", theme.warn),
                Outcome::Aborted => ("ABORTED", theme.danger),
                Outcome::Unspecified => ("RESOLVED", theme.faint),
            };
            decisions = decisions.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(chip(label, color))
                    .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(10.5)).text_color(theme.faint).child(format!("{} · {}", note.decided_by, note.intervention_id)))
                    .child(mono(kit::ago(note.at.elapsed()), theme.faint)),
            );
        }
        tab = tab.child(self.system_section("decisions", decisions));

        // ---- keys ---------------------------------------------------------------------------------------
        let mut keys = div().flex().flex_col().gap(px(5.));
        for (combo, does) in KEYS {
            let mut group = div().w(px(96.)).flex_none().flex().flex_row().gap_1();
            for key in combo.split(' ').filter(|k| !k.is_empty() && *k != "·") {
                group = group.child(keycap(key, &theme));
            }
            keys = keys.child(div().flex().flex_row().items_center().gap_2().child(group).child(div().flex_1().text_xs().text_color(theme.dim).child(*does)));
        }
        tab.child(self.system_section("keyboard", keys))
    }

    fn system_section(&self, title: &str, body: Div) -> Div {
        let theme = self.theme;
        div().flex().flex_col().gap_2().child(caption(title, &theme)).child(body)
    }
}
