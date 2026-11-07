//! Multi-Agent Supervisory Control Room Canvas (`⌘0`), Graph Topology, and Governor.
//!
//! Visualizes agent orchestration as an interactive control room rather than a chat transcript:
//! - Governor Blueprint (`#governor-blueprint`): wave status, confidence signaling, batch actions.
//! - Graph Canvas (`#orchestration-graph-canvas`): node cards (`#node-agent-{slot}`) with role badges & live action feeds.
//! - Scope Collision Radar (`#scope-collision-{ix}`): cross-agent file overlap warnings.
//! - Adaptive Node Inspector (`#node-inspector-{slot}`): drill-down controls and in-situ gate decisions.

use acp_core::ApprovalDecision;
use cordata_theme::tokens as t;
use cordata_view::app::{ConnState, Message, TabState};
use cordata_view::desk;
use cordata_view::shown;
use cordata_view::work::{self, AgentRole, ConfidenceSignal, TriageIn};
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
                let title = desk::brief(&view.messages).map(|b| shown(&b)).unwrap_or_else(|| view.title());
                let r = desk::receipt(&view.messages, check.as_deref());
                let state = match (&view.conn, view.tab_state()) {
                    (ConnState::Closed { .. }, _) => TabState::Closed,
                    (_, st) => st,
                };
                let (gate_title, gate_signature, gate_high_risk) = match view.pending.front() {
                    Some(p) => {
                        let summary = p.info.raw_input.as_ref().and_then(cordata_view::view::summarize_input).unwrap_or_else(|| p.info.tool_name.clone());
                        let analysis = acp_core::auto::gate_analysis(&p.info.tool_name, p.info.raw_input.as_ref(), &ses.cwd, p.info.reason.as_deref());
                        let sig = canonical_gate_signature(&ses.cwd, &p.info.tool_name, p.info.raw_input.as_ref(), p.info.reason.as_deref());
                        (Some(format!("{} · {}", p.info.tool_name, shown(&summary))), Some(sig), analysis.high_risk)
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
                let role = work::infer_role(ix + 1, &title);
                let current_action = if let Some(p) = view.pending.front() {
                    Some(format!("gate: {}", p.info.tool_name))
                } else if state == TabState::Working {
                    view.messages.iter().rev().find_map(|m| match m {
                        Message::Tool(t) if !t.diffs.is_empty() => Some(format!("editing {}", t.diffs[0].path)),
                        Message::Tool(t) => Some(format!("tool: {}", t.name)),
                        _ => None,
                    }).or_else(|| Some("running in sandbox".to_owned()))
                } else {
                    None
                };
                let confidence = work::infer_confidence(state == TabState::Working, falsification_warning, gate_high_risk);
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
                    role,
                    current_action,
                    confidence,
                    wave: 1,
                    parent_slot: if ix > 0 { Some(1) } else { None },
                }
            })
            .collect();

        let rack = work::triage_rack(&inputs);
        let graph = work::build_work_graph(&inputs);
        let sel_slot = self.rack_selected_slot;

        // Governor Blueprint Bar
        let blueprint_bar = div()
            .id("governor-blueprint")
            .p(px(10.))
            .rounded(px(t::RADIUS_CARD))
            .border_1()
            .border_color(ca(tk.amber, 0.35))
            .bg(ca(tk.amber, 0.05))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.))
            .child(
                div().flex().items_center().gap(px(8.))
                    .child(ui::mark("gov-mark", if graph.high_risk_count > 0 { MarkState::NeedsYou } else { MarkState::Working }, 11., cx))
                    .child(div().flex().flex_col().gap(px(1.))
                        .child(div().text_size(px(t::TEXT_SMALL)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary))
                            .child(SharedString::from(format!("Wave {} Governor Blueprint · {} Agents ({} High-Risk)", graph.active_wave, inputs.len(), graph.high_risk_count))))
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_secondary))
                            .child("Propose–Probe–Compose concurrency · APFS clone isolated sandboxes"))))
            .child(
                div().flex().items_center().gap(px(6.))
                    .child(ui::button("gov-spawn-worker", "+ Dispatch Worker ⌘N", Kind::Ghost, cx)
                        .on_click(cx.listener(|this, _, window, cx| { this.open_new(window, cx); })))
                    .when(!rack.batch_gates.is_empty(), |b| {
                        let total: usize = rack.batch_gates.iter().map(|g| g.thread_indices.len()).sum();
                        let sig = rack.batch_gates[0].signature.clone();
                        b.child(ui::button("gov-batch-allow", SharedString::from(format!("Approve {total} Gates")), Kind::Primary, cx)
                            .on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&sig, ApprovalDecision::Allow, cx))))
                    })
                    .child(ui::button("gov-pause-all", "Pause All", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| {
                        for ses in &this.sessions { ses.update(cx, |s, cx| s.cancel(cx)); }
                        cx.notify();
                    }))));

        // Scope Collision Radar
        let collision_radar = (!graph.scope_collisions.is_empty()).then(|| {
            div().flex().flex_col().gap(px(4.))
                .children(graph.scope_collisions.iter().enumerate().map(|(c_ix, (s1, s2, desc))| {
                    div().id(("scope-collision", c_ix)).p(px(6.)).rounded(px(t::RADIUS_CARD)).border_1().border_color(ca(tk.amber, 0.5)).bg(ca(tk.amber, 0.08))
                        .flex().items_center().justify_between()
                        .child(div().flex().items_center().gap(px(6.))
                            .child(ui::mark(("coll-mark", c_ix), MarkState::NeedsYou, 10., cx))
                            .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.amber)).child(SharedString::from(desc.clone()))))
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.text_muted)).child(SharedString::from(format!("#{s1} ⟷ #{s2}"))))
                }))
        });

        // Interactive Graph Topology Canvas
        let graph_canvas = div()
            .id("orchestration-graph-canvas")
            .p(px(10.))
            .rounded(px(t::RADIUS_CARD))
            .border_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .bg(ca(tk.bg_panel, 0.4))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().flex().items_center().justify_between()
                .child(ui::quiet_label("Agent Graph Topology & Live Feeds", cx))
                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.text_faint)).child("Click node to inspect · ⌘N to focus")))
            .child(div().flex().flex_wrap().gap(px(8.))
                .children(inputs.iter().map(|item| {
                    let slot = item.slot;
                    let is_sel = sel_slot == Some(slot);
                    let role_color = match item.role {
                        AgentRole::Supervisor => tk.amber,
                        AgentRole::InvariantProbe => tk.teal,
                        AgentRole::DomainWorker => tk.amber_soft,
                        AgentRole::Synthesizer => tk.text_primary,
                        AgentRole::Reviewer => tk.attention,
                        AgentRole::Worker => tk.text_secondary,
                    };
                    let conf_color = match item.confidence {
                        ConfidenceSignal::HighSolid => ca(role_color, 0.7),
                        ConfidenceSignal::Medium => ca(role_color, 0.4),
                        ConfidenceSignal::LowHatched => ca(tk.attention, 0.8),
                    };
                    div()
                        .id(("node-agent", slot))
                        .role(gpui::Role::Button)
                        .cursor_pointer()
                        .w(px(255.))
                        .p(px(8.))
                        .rounded(px(t::RADIUS_CARD))
                        .border_1()
                        .border_color(if is_sel { c(tk.amber) } else { conf_color })
                        .bg(if is_sel { ca(tk.amber, 0.12) } else { ca(tk.bg_panel, 0.8) })
                        .hover(|s| s.bg(ca(tk.amber, 0.08)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.rack_selected_slot = if this.rack_selected_slot == Some(slot) { None } else { Some(slot) };
                            record_ux_event("node_select", serde_json::json!({ "slot": slot }));
                            cx.notify();
                        }))
                        .flex().flex_col().gap(px(3.))
                        .child(div().flex().items_center().justify_between()
                            .child(div().flex().items_center().gap(px(5.))
                                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(role_color)).child(SharedString::from(format!("#{slot}"))))
                                .child(div().px(px(4.)).py(px(1.)).rounded(px(3.)).bg(ca(role_color, 0.15))
                                    .font_family(mono(cx)).text_size(px(9.)).font_weight(FontWeight::BOLD).text_color(c(role_color)).child(item.role.badge())))
                            .child(ui::mark(("node-mark", slot), if item.state == TabState::Working { MarkState::Working } else if item.gate_title.is_some() { MarkState::NeedsYou } else { MarkState::Idle }, 9., cx)))
                        .child(div().truncate().text_size(px(t::TEXT_SMALL)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(item.title.clone())))
                        .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_TAG))
                            .text_color(c(if item.confidence == ConfidenceSignal::LowHatched { tk.attention } else { tk.teal }))
                            .child(SharedString::from(item.current_action.clone().unwrap_or_else(|| "idle".to_owned()))))
                        .child(div().flex().items_center().justify_between().pt(px(2.))
                            .child(div().font_family(mono(cx)).text_size(px(9.)).text_color(c(tk.text_faint)).child(SharedString::from(format!("±{}f · +{} −{}", item.files, item.added, item.removed))))
                            .child(div().font_family(mono(cx)).text_size(px(9.)).text_color(c(tk.text_faint)).child(SharedString::from(item.confidence.label()))))
                })));

        // Adaptive Node Inspector for selected node
        let node_inspector = sel_slot.and_then(|slot| inputs.iter().find(|i| i.slot == slot)).map(|item| {
            let (ix, slot) = (item.index, item.slot);
            div()
                .id(("node-inspector", slot))
                .p(px(12.))
                .rounded(px(t::RADIUS_CARD))
                .border_1()
                .border_color(ca(tk.amber, 0.4))
                .bg(ca(tk.amber, 0.06))
                .flex().flex_col().gap(px(8.))
                .child(div().flex().items_center().justify_between()
                    .child(div().flex().items_center().gap(px(8.))
                        .child(div().font_family(voice(cx)).text_size(px(15.)).font_weight(FontWeight::BOLD).text_color(c(tk.text_primary)).child(SharedString::from(format!("Agent #{slot} · {}", item.role.badge()))))
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child(SharedString::from(item.title.clone()))))
                    .child(div().flex().items_center().gap(px(6.))
                        .child(ui::button(("insp-focus", slot), "Focus Thread ⌘N", Kind::Primary, cx).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(target) = this.sessions.get(ix).cloned() { this.show(target, cx); }
                            this.desk_mode = DeskMode::Transcript;
                            cx.notify();
                        })))
                        .child(ui::button(("insp-diff", slot), "Change Sheet ⌘D", Kind::Ghost, cx).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(target) = this.sessions.get(ix).cloned() { this.show(target, cx); }
                            this.load_review(cx);
                            this.desk_mode = DeskMode::ChangeSheet;
                            cx.notify();
                        })))
                        .child(ui::button(("insp-pause", slot), "Intervene", Kind::Ghost, cx).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(target) = this.sessions.get(ix).cloned() { target.update(cx, |s, cx| s.cancel(cx)); }
                            cx.notify();
                        })))))
                .when_some(item.gate_title.as_ref(), |insp, gate| {
                    let sig = item.gate_signature.clone().unwrap_or_default();
                    let sig_deny = sig.clone();
                    insp.child(div().p(px(8.)).rounded(px(t::RADIUS_CARD)).border_1().border_color(ca(tk.attention, 0.4)).bg(ca(tk.attention, 0.08))
                        .flex().items_center().justify_between()
                        .child(div().flex().items_center().gap(px(6.))
                            .child(ui::mark(("insp-gate-mark", slot), MarkState::NeedsYou, 11., cx))
                            .child(div().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_primary)).child(SharedString::from(format!("Pending Gate: {gate}")))))
                        .child(div().flex().items_center().gap(px(6.))
                            .child(ui::button(("insp-gate-allow", slot), "Allow", Kind::Primary, cx).on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&sig, ApprovalDecision::Allow, cx))))
                            .child(ui::button(("insp-gate-deny", slot), "Deny", Kind::Ghost, cx).on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&sig_deny, ApprovalDecision::Reject, cx))))))
                })
        });

        // Supervisory Bands View (when rack_graph_mode == false)
        let render_band = |title: &'static str, mark: MarkState, items: &[TriageIn], open_sheet: bool, cx: &mut Context<Self>| -> Option<AnyElement> {
            if items.is_empty() { return None; }
            let rows: Vec<AnyElement> = items.iter().map(|item| {
                let (ix, slot) = (item.index, item.slot);
                let row_mark = if item.state == TabState::Closed { MarkState::Error } else { mark };
                let detail = if let Some(gate) = &item.gate_title {
                    format!("gate: {gate}")
                } else if item.files > 0 {
                    format!("{} files (+{} −{})", item.files, item.added, item.removed)
                } else {
                    item.current_action.clone().unwrap_or_else(|| "idle".to_owned())
                };
                div().id(("rack-thread", ix)).role(gpui::Role::Button).cursor_pointer()
                    .p(px(8.)).rounded(px(t::RADIUS_CARD)).border_1().border_color(line(cx, t::LINE_DEFAULT))
                    .hover(|s| s.bg(ca(tk.amber, 0.08)))
                    .flex().items_center().gap(px(10.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(target) = this.sessions.get(ix).cloned() { this.show(target, cx); }
                        this.desk_mode = if open_sheet { this.load_review(cx); DeskMode::ChangeSheet } else { DeskMode::Transcript };
                        cx.notify();
                    }))
                    .child(div().w(px(24.)).font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(format!("#{slot}"))))
                    .child(ui::mark(("rack-mark", ix), row_mark, 10., cx))
                    .child(div().flex_1().min_w_0().flex().flex_col().gap(px(1.))
                        .child(div().truncate().text_size(px(t::TEXT_UI)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(item.title.clone())))
                        .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child(SharedString::from(detail))))
                    .child(ui::meta(if open_sheet { "sheet →" } else { "open →" }, cx))
                    .into_any_element()
            }).collect();
            Some(div().flex().flex_col().gap(px(4.)).child(ui::quiet_label(title, cx)).children(rows).into_any_element())
        };

        let bands = (!self.rack_graph_mode).then(|| {
            div().flex().flex_col().gap(px(10.))
                .children(render_band("At the Gate", MarkState::NeedsYou, &rack.at_gate, false, cx))
                .children(render_band("Ready to Review", MarkState::Done, &rack.ready, true, cx))
                .children(render_band("In Sandbox", MarkState::Working, &rack.in_sandbox, false, cx))
                .children(render_band("Idle", MarkState::Idle, &rack.idle, false, cx))
        });

        // Batch Gate Clusters
        let batch_cards: Vec<AnyElement> = rack.batch_gates.iter().enumerate().map(|(b_ix, batch)| {
            let (count, sig, deny_sig) = (batch.thread_indices.len(), batch.signature.clone(), batch.signature.clone());
            let slots: Vec<String> = batch.thread_slots.iter().map(|s| format!("#{s}")).collect();
            div().id(("batch-gate", b_ix)).p(px(10.)).rounded(px(t::RADIUS_CARD)).border_1().border_color(ca(tk.attention, 0.45)).bg(ca(tk.attention, 0.06))
                .flex().items_center().gap(px(10.))
                .child(ui::mark(("batch-mark", b_ix), MarkState::NeedsYou, 12., cx))
                .child(div().flex_1().min_w_0().flex().flex_col().gap(px(1.))
                    .child(div().text_size(px(t::TEXT_SMALL)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary))
                        .child(SharedString::from(format!("{count} threads ({}) waiting on identical gate", slots.join(", ")))))
                    .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_secondary)).child(SharedString::from(batch.title.clone()))))
                .when(!batch.high_risk, |row| {
                    row.child(ui::button(("batch-allow", b_ix), SharedString::from(format!("Allow all {count}")), Kind::Primary, cx)
                        .on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&sig, ApprovalDecision::Allow, cx))))
                })
                .child(ui::button(("batch-deny", b_ix), SharedString::from(format!("Deny all {count}")), Kind::Ghost, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&deny_sig, ApprovalDecision::Reject, cx))))
                .into_any_element()
        }).collect();

        div()
            .id("triage-rack")
            .size_full()
            .overflow_y_scroll()
            .child(div().w_full().flex().justify_center().px(px(24.)).py(px(18.))
                .child(div().w_full().max_w(px(t::CONTENT_MAX_W)).flex().flex_col().gap(px(12.))
                    .child(div().flex().items_baseline().justify_between().pb(px(8.)).border_b_1().border_color(line(cx, t::LINE_SUBTLE))
                        .child(div().flex().items_baseline().gap(px(10.))
                            .child(div().font_family(voice(cx)).text_size(px(22.)).text_color(c(tk.text_primary)).child("Supervisory Control Room"))
                            .child(ui::meta(SharedString::from(format!("{} active agents · wave {}", inputs.len(), graph.active_wave)), cx)))
                        .child(div().flex().items_center().gap(px(10.))
                            .child(ui::button("rack-mode-toggle", SharedString::from(if self.rack_graph_mode { "Switch to Bands" } else { "Switch to Graph" }), Kind::Ghost, cx)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rack_graph_mode = !this.rack_graph_mode;
                                    cx.notify();
                                })))
                            .child(div().id("rack-close").role(gpui::Role::Button).cursor_pointer().font_family(mono(cx)).text_size(px(t::TEXT_META))
                                .text_color(c(tk.text_muted)).hover(|s| s.text_color(c(tk.text_primary)))
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_rack(cx))).child("back to thread  ⌘0"))))
                    .child(blueprint_bar)
                    .children(collision_radar)
                    .when(self.rack_graph_mode, |c| c.child(graph_canvas).children(node_inspector))
                    .children(bands)
                    .children(batch_cards)))
            .into_any_element()
    }
}
