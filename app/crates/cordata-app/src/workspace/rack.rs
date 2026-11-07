//! Multi-Agent Supervisory Triage Rack (`⌘0`) and Batch Gate.
//!
//! Groups every open thread into supervisory bands (*At the Gate*, *Ready to Review*, *In Sandbox*, *Idle*)
//! and clusters identical pending gates across threads so routine checks can be answered in one decision.

use acp_core::ApprovalDecision;
use cordata_theme::tokens as t;
use cordata_view::app::{ConnState, TabState};
use cordata_view::desk::{self, CheckState};
use cordata_view::shown;
use cordata_view::work::{self, TriageIn};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, div, px,
};

use super::Workspace;
use super::sheet::{DeskMode, record_ux_event};
use crate::theme::{c, ca, line, mono, tk, voice};
use crate::ui::{self, Kind, MarkState};

fn canonical_gate_signature(cwd: &std::path::Path, tool: &str, raw: Option<&serde_json::Value>, reason: Option<&str>) -> String {
    let payload = raw.and_then(|v| serde_json::to_string(v).ok()).unwrap_or_default();
    format!("{}|{tool}|{payload}|{}", cwd.display(), reason.unwrap_or_default())
}

impl Workspace {
    pub(super) fn toggle_rack(&mut self, cx: &mut Context<Self>) {
        self.desk_mode = match self.desk_mode {
            DeskMode::Rack => DeskMode::Transcript,
            DeskMode::Transcript | DeskMode::ChangeSheet => {
                record_ux_event("mode_switch", serde_json::json!({ "mode": "rack" }));
                DeskMode::Rack
            }
        };
        cx.notify();
    }

    pub(super) fn answer_batch_gate(&mut self, expected_sig: &str, decision: ApprovalDecision, cx: &mut Context<Self>) {
        let mut count = 0usize;
        for entity in self.sessions.clone() {
            entity.update(cx, |s, cx| {
                let matches = s.view.pending.front().is_some_and(|p| {
                    let sig = canonical_gate_signature(&s.cwd, &p.info.tool_name, p.info.raw_input.as_ref(), p.info.reason.as_deref());
                    if sig != expected_sig {
                        return false;
                    }
                    if matches!(decision, ApprovalDecision::Reject) {
                        return true;
                    }
                    !acp_core::auto::gate_analysis(&p.info.tool_name, p.info.raw_input.as_ref(), &s.cwd, p.info.reason.as_deref()).high_risk
                });
                if matches {
                    s.answer(decision.clone(), cx);
                    count += 1;
                }
            });
        }
        record_ux_event("batch_gate_decision", serde_json::json!({ "count": count }));
        self.gate_unlocked = None;
        self.gate_hint = None;
        cx.notify();
    }

    pub(super) fn triage_rack_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let check = self.check_command();
        let active_id = self.session.entity_id();
        let edited_by_slot: Vec<(usize, std::path::PathBuf, Vec<String>)> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(ix, entity)| {
                let s = entity.read(cx);
                (ix + 1, s.cwd.clone(), desk::edited_paths(&s.view.messages, &s.cwd))
            })
            .collect();

        let inputs: Vec<TriageIn> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(ix, entity)| {
                let ses = entity.read(cx);
                let view = &ses.view;
                let title = desk::brief(&view.messages)
                    .map(|b| shown(&b))
                    .unwrap_or_else(|| view.title());
                let r = desk::receipt(&view.messages, check.as_deref());
                let state = match (&view.conn, view.tab_state()) {
                    (ConnState::Closed { .. }, _) => TabState::Closed,
                    (_, st) => st,
                };
                let (gate_title, gate_signature, gate_high_risk) = match view.pending.front() {
                    Some(p) => {
                        let summary = p
                            .info
                            .raw_input
                            .as_ref()
                            .and_then(cordata_view::view::summarize_input)
                            .unwrap_or_else(|| p.info.tool_name.clone());
                        let analysis = acp_core::auto::gate_analysis(
                            &p.info.tool_name,
                            p.info.raw_input.as_ref(),
                            &ses.cwd,
                            p.info.reason.as_deref(),
                        );
                        let sig = canonical_gate_signature(&ses.cwd, &p.info.tool_name, p.info.raw_input.as_ref(), p.info.reason.as_deref());
                        let g_title = format!("{} · {}", p.info.tool_name, shown(&summary));
                        (Some(g_title), Some(sig), analysis.high_risk)
                    }
                    None => (None, None, false),
                };
                let own_paths = &edited_by_slot[ix].2;
                let overlaps: Vec<usize> = edited_by_slot
                    .iter()
                    .filter(|(slot, peer_cwd, paths)| *slot != ix + 1 && peer_cwd == &ses.cwd && paths.iter().any(|p| own_paths.contains(p)))
                    .map(|(slot, _, _)| *slot)
                    .collect();
                let falsification_warning = r.has_falsification_warning();
                TriageIn {
                    index: ix,
                    slot: ix + 1,
                    title,
                    folder: ses.cwd.clone(),
                    state,
                    active: entity.entity_id() == active_id,
                    files: r.files,
                    added: r.added,
                    removed: r.removed,
                    check: r.check,
                    falsification_warning,
                    gate_title,
                    gate_signature,
                    gate_high_risk,
                    working_secs: view.turn_started.map(|t| t.elapsed().as_secs()),
                    overlaps,
                }
            })
            .collect();

        let rack = work::triage_rack(&inputs);

        let batch_cards: Vec<AnyElement> = rack
            .batch_gates
            .iter()
            .enumerate()
            .map(|(b_ix, batch)| {
                let count = batch.thread_indices.len();
                let allow_sig = batch.signature.clone();
                let deny_sig = batch.signature.clone();
                let slots: Vec<String> = batch.thread_slots.iter().map(|s| format!("#{s}")).collect();
                div()
                    .id(("batch-gate", b_ix))
                    .p(px(12.))
                    .rounded(px(t::RADIUS_CARD))
                    .border_1()
                    .border_color(ca(tk.attention, 0.45))
                    .bg(ca(tk.attention, 0.06))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(ui::mark(("batch-mark", b_ix), MarkState::NeedsYou, 13., cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(t::TEXT_SMALL))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(c(tk.text_primary))
                                    .child(SharedString::from(format!(
                                        "{count} threads ({}) waiting on the same gate",
                                        slots.join(", ")
                                    ))),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .font_family(mono(cx))
                                    .text_size(px(t::TEXT_META))
                                    .text_color(c(tk.text_secondary))
                                    .child(SharedString::from(batch.title.clone())),
                            ),
                    )
                    .when(!batch.high_risk, |row| {
                        row.child(
                            ui::button(("batch-allow", b_ix), SharedString::from(format!("Allow all {count}")), Kind::Primary, cx)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.answer_batch_gate(&allow_sig, ApprovalDecision::Allow, cx);
                                })),
                        )
                    })
                    .when(batch.high_risk, |row| {
                        row.child(
                            div()
                                .font_family(mono(cx))
                                .text_size(px(t::TEXT_META))
                                .text_color(c(tk.attention))
                                .child("high-risk · inspect individually"),
                        )
                    })
                    .child(
                        ui::button(("batch-deny", b_ix), SharedString::from(format!("Deny all {count}")), Kind::Ghost, cx)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.answer_batch_gate(&deny_sig, ApprovalDecision::Reject, cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        let render_band = |title: &'static str, mark: MarkState, items: &[TriageIn], open_sheet: bool, cx: &mut Context<Self>| -> Option<AnyElement> {
            if items.is_empty() {
                return None;
            }
            let rows: Vec<AnyElement> = items
                .iter()
                .map(|item| {
                    let ix = item.index;
                    let row_mark = if item.state == TabState::Closed { MarkState::Error } else { mark };
                    let detail = if item.state == TabState::Closed {
                        "engine stopped".to_owned()
                    } else if let Some(gate) = &item.gate_title {
                        format!("gate: {gate}")
                    } else if item.files > 0 {
                        let warn = if item.falsification_warning { " · audit warning" } else { "" };
                        let chk = match &item.check {
                            Some((_, CheckState::Passed)) => " · check passed",
                            Some((_, CheckState::Failed)) => " · check failed",
                            Some((_, CheckState::Running)) => " · check running",
                            None => "",
                        };
                        format!("{} files (+{} −{}){chk}{warn}", item.files, item.added, item.removed)
                    } else if item.state == TabState::Working {
                        match item.working_secs {
                            Some(s) if s >= 60 => format!("running in sandbox ({})", work::age(s)),
                            _ => "running in sandbox…".to_owned(),
                        }
                    } else {
                        "idle".to_owned()
                    };
                    let overlap_tag = (!item.overlaps.is_empty()).then(|| {
                        let list: Vec<String> = item.overlaps.iter().map(|s| format!("#{s}")).collect();
                        format!("⎇ {}", list.join(","))
                    });
                    let hover = line(cx, t::HOVER);
                    div()
                        .id(("rack-thread", ix))
                        .role(gpui::Role::Button)
                        .cursor_pointer()
                        .p(px(10.))
                        .rounded(px(t::RADIUS_CARD))
                        .border_1()
                        .border_color(if item.active { ca(tk.amber, 0.45) } else { line(cx, t::LINE_DEFAULT) })
                        .hover(move |s| s.bg(hover))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(target) = this.sessions.get(ix).cloned() {
                                this.show(target, cx);
                            }
                            this.desk_mode = if open_sheet {
                                this.load_review(cx);
                                DeskMode::ChangeSheet
                            } else {
                                DeskMode::Transcript
                            };
                            cx.notify();
                        }))
                        .child(div().w(px(24.)).font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(format!("#{}", item.slot))))
                        .child(ui::mark(("rack-mark", ix), row_mark, 12., cx))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(div().truncate().text_size(px(t::TEXT_UI)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(item.title.clone())))
                                .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(if item.falsification_warning { tk.attention } else { tk.text_muted })).child(SharedString::from(detail))),
                        )
                        .when_some(overlap_tag, |row, tag| {
                            row.child(div().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.teal)).child(SharedString::from(tag)))
                        })
                        .child(ui::meta(if open_sheet { "sheet →" } else { "open →" }, cx))
                        .into_any_element()
                })
                .collect();

            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(ui::quiet_label(title, cx))
                    .children(rows)
                    .into_any_element(),
            )
        };

        let band_gate = render_band("At the Gate", MarkState::NeedsYou, &rack.at_gate, false, cx);
        let band_review = render_band("Ready to Review", MarkState::Done, &rack.ready, true, cx);
        let band_sandbox = render_band("In Sandbox", MarkState::Working, &rack.in_sandbox, false, cx);
        let band_idle = render_band("Idle", MarkState::Idle, &rack.idle, false, cx);

        div()
            .id("triage-rack")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .px(px(24.))
                    .py(px(18.))
                    .child(
                        div()
                            .w_full()
                            .max_w(px(t::CONTENT_MAX_W))
                            .flex()
                            .flex_col()
                            .gap(px(16.))
                            .child(
                                div()
                                    .flex()
                                    .items_baseline()
                                    .justify_between()
                                    .pb(px(8.))
                                    .border_b_1()
                                    .border_color(line(cx, t::LINE_SUBTLE))
                                    .child(
                                        div()
                                            .flex()
                                            .items_baseline()
                                            .gap(px(10.))
                                            .child(div().font_family(voice(cx)).text_size(px(22.)).text_color(c(tk.text_primary)).child("Supervisory Rack"))
                                            .child(ui::meta(SharedString::from(format!("{} open thread{}", inputs.len(), if inputs.len() == 1 { "" } else { "s" })), cx)),
                                    )
                                    .child(
                                        div()
                                            .id("rack-close")
                                            .role(gpui::Role::Button)
                                            .cursor_pointer()
                                            .font_family(mono(cx))
                                            .text_size(px(t::TEXT_META))
                                            .text_color(c(tk.text_muted))
                                            .hover(|s| s.text_color(c(tk.text_primary)))
                                            .on_click(cx.listener(|this, _, _, cx| this.toggle_rack(cx)))
                                            .child("back to thread  ⌘0"),
                                    ),
                            )
                            .children(batch_cards)
                            .children(band_gate)
                            .children(band_review)
                            .children(band_sandbox)
                            .children(band_idle),
                    ),
            )
            .into_any_element()
    }
}
