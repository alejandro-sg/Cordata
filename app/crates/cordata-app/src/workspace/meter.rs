//! The instrument line under the composer: how the agent is doing, read at a glance. While a turn runs it counts the
//! wait and says when the first word came; between turns it keeps the last turn's numbers and how full the context is.
//! Clicking it opens the readout (the last turns as a strip, medians, tokens, cache); the model name beside it opens
//! the model switcher. Both are on by default and off in the readout's footer or Settings → Appearance.

use std::time::Instant;

use cordata_theme::tokens as t;
use cordata_view::models;
use cordata_view::telemetry::{self, Telemetry};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, deferred, div, px,
};
use gpui_component::input::{Input, InputEvent};

use super::Workspace;
use crate::theme::{amber, c, line, mono, tk};
use crate::ui;

/// Turns the readout's strip shows.
const STRIP: usize = 24;
const STRIP_H: f32 = 34.;
/// The gauge turns attention-coloured past this share: compaction or a fresh thread soon.
const CONTEXT_WARN: f64 = 0.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Popup {
    Readout,
    Models,
}

/// Which provider keys are set, read once off the UI thread (`None` until then).
#[derive(Debug, Clone, Default)]
pub struct Keys(pub Option<Vec<(&'static str, bool)>>);

impl Keys {
    pub fn ready(&self, provider: Option<&str>) -> Option<bool> {
        let Some(provider) = provider else { return Some(true) };
        self.0.as_ref().and_then(|keys| keys.iter().find(|(p, _)| *p == provider).map(|(_, ok)| *ok))
    }
}

impl Workspace {
    /// Reads the keys in the background, and keeps the meter ticking while a turn runs.
    pub(super) fn start_meter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // No runtime in window tests: the keys stay unknown, and nothing reads the Keychain.
        if let Some(runtime) = cx.try_global::<crate::session::Runtime>().map(|r| r.0.clone()) {
            cx.spawn_in(window, async move |this, cx| {
                let keys = runtime
                    .spawn_blocking(|| ["anthropic", "openai", "gemini", "ollama_cloud"].map(|p| (p, acp_core::native::key_present(p))).to_vec())
                    .await
                    .unwrap_or_default();
                let _ = this.update(cx, |this, cx| {
                    this.keys = Keys(Some(keys));
                    cx.notify();
                });
            })
            .detach();
        }
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                let alive = this.update(cx, |this, cx| {
                    if this.prefs.show_meter && this.session.read(cx).view.turn_active {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn on_model_filter(&mut self, event: &InputEvent, cx: &mut Context<Self>) {
        if let InputEvent::PressEnter { .. } = event {
            let typed = self.model_filter.read(cx).value().trim().to_owned();
            if !typed.is_empty() {
                self.choose_model(&typed, cx);
            }
        }
        cx.notify();
    }

    fn choose_model(&mut self, model: &str, cx: &mut Context<Self>) {
        self.popup = None;
        let model = model.to_owned();
        self.session.update(cx, |s, cx| s.set_model(&model, cx));
        cx.notify();
    }

    fn toggle_popup(&mut self, which: Popup, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = if self.popup == Some(which) { None } else { Some(which) };
        if self.popup == Some(Popup::Models) {
            self.model_filter.update(cx, |s, cx| {
                s.set_value("", window, cx);
                s.focus(window, cx);
            });
        }
        cx.notify();
    }

    fn close_popup(&mut self, cx: &mut Context<Self>) {
        self.popup = None;
        cx.notify();
    }

    /// The meter itself, in the composer's footer row.
    pub(super) fn meter(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.prefs.show_meter {
            return None;
        }
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let tm = view.telemetry.clone();
        let tm = &tm;
        let model = view.session.as_ref().map(|s| s.model.clone()).unwrap_or_default();
        let mut parts: Vec<AnyElement> = Vec::new();
        let sep = || div().text_color(c(tk.text_faint)).child("·").into_any_element();
        if let Some((elapsed, answered)) = tm.running(Instant::now()) {
            parts.push(pulse(cx));
            parts.push(div().text_color(c(tk.text_secondary)).child(SharedString::from(telemetry::secs(elapsed))).into_any_element());
            parts.push(sep());
            parts.push(match (answered, tm.live_first()) {
                (true, Some(first)) => div().child(SharedString::from(format!("first {}", telemetry::secs(first)))).into_any_element(),
                _ => div().text_color(c(if elapsed > 8.0 { tk.attention } else { tk.text_muted })).child("waiting for the model").into_any_element(),
            });
        } else if let Some(last) = tm.last() {
            if let Some(first) = last.first {
                parts.push(div().child(SharedString::from(format!("first {}", telemetry::secs(first)))).into_any_element());
                parts.push(sep());
            }
            parts.push(div().text_color(c(tk.text_secondary)).child(SharedString::from(telemetry::secs(last.total))).into_any_element());
            if let Some(rate) = last.rate() {
                parts.push(sep());
                parts.push(div().child(SharedString::from(format!("{rate:.0} tok/s"))).into_any_element());
            }
        }
        if self.prefs.show_context && tm.context_used > 0 {
            if !parts.is_empty() {
                parts.push(sep());
            }
            parts.push(context_gauge(tm, &model, 36., cx));
        }
        if parts.is_empty() {
            return None;
        }
        let open = self.popup == Some(Popup::Readout);
        let hover = line(cx, t::HOVER);
        Some(
            div()
                .relative()
                .flex()
                .flex_col()
                .items_end()
                .flex_shrink_0()
                .when(open, |el| el.child(self.readout(tm, &model, cx)))
                .child(
                    div()
                        .id("meter")
                        .role(gpui::Role::Button)
                        .aria_label("Turn timing and context — open the readout")
                        .aria_expanded(open)
                        .tooltip(ui::tip("Timing, tokens and context"))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(6.))
                        .h(px(22.))
                        .rounded(px(t::RADIUS_ROW))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .font_family(mono(cx))
                        .text_size(px(t::TEXT_META))
                        .text_color(c(tk.text_muted))
                        .children(parts)
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_popup(Popup::Readout, window, cx))),
                )
                .into_any_element(),
        )
    }

    /// The model's name, which opens the switcher.
    pub(super) fn model_trigger(&self, short: String, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let open = self.popup == Some(Popup::Models);
        let hover = line(cx, t::HOVER);
        let text_hover = c(tk.text_primary);
        div()
            .relative()
            .flex()
            .flex_col()
            .items_end()
            .flex_shrink_0()
            .when(open, |el| el.child(self.model_menu(cx)))
            .child(
                div()
                    .id("model-trigger")
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(format!("Model: {short} — switch")))
                    .aria_expanded(open)
                    .tooltip(ui::tip("Switch model  /model"))
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .px(px(6.))
                    .h(px(22.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .font_family(mono(cx))
                    .text_color(c(if open { tk.text_primary } else { tk.text_muted }))
                    .hover(move |s| s.bg(hover).text_color(text_hover))
                    .child(SharedString::from(short))
                    .child(gpui_component::Icon::new(gpui_kit_assets::IconName::ChevronDown).size(px(11.)))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_popup(Popup::Models, window, cx))),
            )
            .into_any_element()
    }

    fn model_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let current = view.session.as_ref().map(|s| s.model.clone()).unwrap_or_default();
        let busy = view.turn_active;
        let filter = self.model_filter.read(cx).value().trim().to_lowercase();
        let shown: Vec<&models::Model> = models::CATALOG
            .iter()
            .filter(|m| filter.is_empty() || m.id.to_lowercase().contains(&filter) || m.label.to_lowercase().contains(&filter))
            .collect();
        let exact = filter.is_empty() || models::CATALOG.iter().any(|m| m.id == filter);
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut provider = "";
        for (ix, m) in shown.into_iter().enumerate() {
            if m.provider != provider {
                provider = m.provider;
                rows.push(group_label(provider_name(provider), cx));
            }
            let is_current = m.id == current;
            let ready = self.keys.ready(Some(m.provider));
            let id = m.id;
            rows.push(
                ui::row(("model-row", ix), is_current, cx)
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_SMALL)).text_color(c(if is_current { tk.amber } else { tk.text_primary })).child(SharedString::from(m.label)))
                    .child(div().flex_shrink_0().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(format!("{} ctx", telemetry::tokens(m.window)))))
                    .child(
                        div()
                            .w(px(84.))
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .truncate()
                            .text_size(px(t::TEXT_META))
                            .text_color(c(if ready == Some(false) { tk.attention } else { tk.text_faint }))
                            .child(SharedString::from(match ready {
                                Some(false) => "no key",
                                _ => m.note,
                            })),
                    )
                    .when(!busy, |row| row.on_click(cx.listener(move |this, _, _, cx| this.choose_model(id, cx))))
                    .into_any_element(),
            );
        }
        if !exact {
            let typed = self.model_filter.read(cx).value().trim().to_owned();
            rows.push(
                ui::row("model-custom", false, cx)
                    .h(px(30.))
                    .font_family(mono(cx))
                    .text_size(px(t::TEXT_SMALL))
                    .text_color(c(tk.text_secondary))
                    .child(SharedString::from(format!("use “{typed}”  ↵")))
                    .when(!busy, |row| row.on_click(cx.listener(move |this, _, _, cx| this.choose_model(&typed, cx))))
                    .into_any_element(),
            );
        }
        let note = if busy { "a turn is running — switch when it ends" } else { "the conversation carries on with the new model" };
        deferred(
            div().absolute().bottom(px(26.)).right_0().child(
                ui::popover(cx)
                    .id("model-menu")
                    .role(gpui::Role::ListBox)
                    .aria_label("Models")
                    .w(px(360.))
                    .flex()
                    .flex_col()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_popup(cx)))
                    .child(
                        div()
                            .px(px(10.))
                            .py(px(8.))
                            .border_b_1()
                            .border_color(line(cx, t::LINE_SUBTLE))
                            .text_size(px(t::TEXT_SMALL))
                            .child(Input::new(&self.model_filter).appearance(false)),
                    )
                    .child(div().id("model-rows").max_h(px(400.)).overflow_y_scroll().p(px(4.)).children(rows))
                    .child(div().px(px(12.)).py(px(7.)).border_t_1().border_color(line(cx, t::LINE_SUBTLE)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(note)),
            ),
        )
        .with_priority(2)
        .into_any_element()
    }

    fn readout(&self, tm: &Telemetry, model: &str, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let stat = |label: &'static str, value: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .min_w(px(64.))
                .child(div().text_size(px(t::TEXT_LABEL)).text_color(c(tk.text_faint)).child(label))
                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_primary)).child(SharedString::from(value)))
        };
        let dash = || "—".to_owned();
        let window = models::window(model);
        let context_line = match window {
            Some(w) => format!("{} of {} · {:.0}%", telemetry::tokens(tm.context_used), telemetry::tokens(w), tm.context_used as f64 / w as f64 * 100.0),
            None => format!("{} (window unknown for this model)", telemetry::tokens(tm.context_used)),
        };
        let breakdown = telemetry::context_breakdown(&self.session.read(cx).view.messages);
        let xray = (breakdown.total_bytes() > 0).then(|| context_xray(&breakdown, 332., cx));
        let toggles = [
            ("meter-toggle", "Meter", self.prefs.show_meter, 0usize),
            ("context-toggle", "Context gauge", self.prefs.show_context, 1usize),
        ];
        deferred(
            div().absolute().bottom(px(26.)).right_0().child(
                ui::popover(cx)
                    .id("readout")
                    .role(gpui::Role::Dialog)
                    .aria_label("Turn readout")
                    .w(px(360.))
                    .p(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_popup(cx)))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .justify_between()
                            .child(div().text_size(px(t::TEXT_SMALL)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child("This thread"))
                            .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(format!("{} turns", tm.turns.len())))),
                    )
                    .child(strip(tm, cx))
                    .child(
                        div()
                            .flex()
                            .gap(px(12.))
                            .child(stat("first word", tm.median_first().map_or_else(dash, telemetry::secs)))
                            .child(stat("turn", tm.median_total().map_or_else(dash, telemetry::secs)))
                            .child(stat("in / out", format!("{} / {}", telemetry::tokens(tm.session_in), telemetry::tokens(tm.session_out))))
                            .child(stat("cached", tm.cache_share().map_or_else(dash, |s| format!("{:.0}%", s * 100.0)))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(div().flex().justify_between().text_size(px(t::TEXT_LABEL)).text_color(c(tk.text_faint)).child("context").child(div().font_family(mono(cx)).child(SharedString::from(context_line))))
                            .child(context_bar(tm, model, 332., cx))
                            .when_some(xray, |col, xray| col.pt(px(4.)).child(xray)),
                    )
                    .child(div().text_size(px(t::TEXT_LABEL)).text_color(c(tk.text_faint)).child("medians over the kept turns · first word = prompt sent to first streamed output"))
                    .child(
                        div().flex().gap(px(16.)).pt(px(10.)).border_t_1().border_color(line(cx, t::LINE_SUBTLE)).children(toggles.into_iter().map(|(id, label, on, which)| {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .text_size(px(t::TEXT_SMALL))
                                .text_color(c(tk.text_secondary))
                                .child(ui::switch(id, on, cx).aria_label(label).on_click(cx.listener(move |this, _, _, cx| {
                                    if which == 0 {
                                        this.prefs.show_meter = !this.prefs.show_meter;
                                        this.popup = None;
                                    } else {
                                        this.prefs.show_context = !this.prefs.show_context;
                                    }
                                    this.prefs.save();
                                    cx.notify();
                                })))
                                .child(label)
                        })),
                    ),
            ),
        )
        .with_priority(2)
        .into_any_element()
    }
}

fn provider_name(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "gemini" => "Google",
        "ollama_cloud" => "Ollama cloud",
        _ => "Other",
    }
}

fn group_label(text: &'static str, cx: &gpui::App) -> AnyElement {
    div().px(px(8.)).pt(px(8.)).pb(px(2.)).text_size(px(t::TEXT_LABEL)).text_color(c(tk(cx).text_faint)).child(text).into_any_element()
}

/// The live dot: the signal colour, still — the seconds beside it are what moves.
fn pulse(cx: &gpui::App) -> AnyElement {
    div().size(px(6.)).rounded_full().bg(c(tk(cx).amber)).into_any_element()
}

fn fill_colour(share: f64, cx: &gpui::App) -> Hsla {
    if share >= CONTEXT_WARN { c(tk(cx).attention) } else { c(tk(cx).amber) }
}

/// The small gauge in the meter: a bar and the share, or the token count when the window is unknown.
fn context_gauge(tm: &Telemetry, model: &str, width: f32, cx: &gpui::App) -> AnyElement {
    match tm.context_share(model) {
        Some(share) => div()
            .flex()
            .items_center()
            .gap(px(5.))
            .child(context_bar(tm, model, width, cx))
            .child(div().text_color(fill_colour(share, cx)).child(SharedString::from(format!("{:.0}%", share * 100.0))))
            .into_any_element(),
        None => div().child(SharedString::from(format!("{} ctx", telemetry::tokens(tm.context_used)))).into_any_element(),
    }
}

fn context_bar(tm: &Telemetry, model: &str, width: f32, cx: &gpui::App) -> AnyElement {
    let share = tm.context_share(model).unwrap_or(0.0);
    let fill = fill_colour(share, cx);
    div()
        .w(px(width))
        .h(px(3.))
        .rounded_full()
        .bg(line(cx, 0.12))
        .child(div().h_full().rounded_full().w(px((width as f64 * share).max(if share > 0.0 { 2.0 } else { 0.0 }) as f32)).bg(fill))
        .into_any_element()
}

/// Four-part Context Composition X-Ray: instructions, tool output, diffs, and dialogue, plus offloaded count.
fn context_xray(breakdown: &telemetry::ContextBreakdown, width: f32, cx: &gpui::App) -> AnyElement {
    let tk = tk(cx);
    let shares = breakdown.shares();
    let colours = [c(tk.teal), c(tk.amber_dim), c(tk.amber), line(cx, 0.32)];
    let short_names = ["instr", "tools", "diffs", "chat"];
    let segs: Vec<AnyElement> = shares
        .iter()
        .enumerate()
        .filter(|(_, (_, s))| *s > 0.0)
        .map(|(i, (_, s))| {
            let w = ((width as f64 * *s).round() as f32).max(3.0);
            div().h_full().w(px(w)).bg(colours[i]).into_any_element()
        })
        .collect();
    let legend: Vec<String> = shares
        .iter()
        .enumerate()
        .filter(|(_, (_, s))| *s >= 0.01)
        .map(|(i, (_, s))| format!("{} {:.0}%", short_names[i], s * 100.0))
        .collect();
    let mut summary = legend.join(" · ");
    if breakdown.offloaded_count > 0 {
        summary.push_str(&format!(" · {} offloaded", breakdown.offloaded_count));
    }
    div()
        .id("context-xray")
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(div().w(px(width)).h(px(4.)).rounded_full().overflow_hidden().flex().gap(px(1.)).bg(line(cx, 0.08)).children(segs))
        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.text_faint)).child(SharedString::from(summary)))
        .into_any_element()
}

/// The last turns as bars: height is the turn's length, the darker foot the wait for the first word; a failed or
/// stopped turn is attention-coloured.
fn strip(tm: &Telemetry, cx: &gpui::App) -> AnyElement {
    let tk = tk(cx);
    let turns: Vec<_> = tm.turns.iter().rev().take(STRIP).rev().collect();
    let longest = turns.iter().map(|t| t.total).fold(0.0_f64, f64::max).max(0.001);
    let wait = amber(cx, 0.35);
    let bars = (0..STRIP).map(|slot| {
        let offset = STRIP - turns.len();
        let Some(turn) = slot.checked_sub(offset).and_then(|i| turns.get(i)) else {
            return div().w(px(8.)).h(px(2.)).bg(line(cx, 0.08)).into_any_element();
        };
        let h = ((turn.total / longest) * STRIP_H as f64).max(2.0) as f32;
        let first_h = turn.first.map_or(0.0, |f| ((f / longest) * STRIP_H as f64) as f32).min(h);
        let body = if turn.ok { c(tk.amber) } else { c(tk.attention) };
        div()
            .id(("strip-bar", slot))
            .tooltip(ui::tip(format!(
                "{} · first {} · {} in / {} out · {} tools",
                telemetry::secs(turn.total),
                turn.first.map_or_else(|| "—".into(), telemetry::secs),
                telemetry::tokens(turn.tokens_in),
                telemetry::tokens(turn.tokens_out),
                turn.tools
            )))
            .w(px(8.))
            .h(px(h))
            .flex()
            .flex_col()
            .child(div().flex_1().bg(body).rounded_t(px(1.)))
            .child(div().h(px(first_h)).bg(wait))
            .into_any_element()
    });
    div().h(px(STRIP_H)).flex().items_end().gap(px(4.)).children(bars).into_any_element()
}
