//! The flight deck (`⌘0`): every open thread at once, as lanes and as ground, with no transcript.
//!
//! - **Lanes**: one row per thread. The strip is its real history, one bar per action (see `cordata_view::flight`),
//!   and the line under the title says what it is doing right now.
//! - **Ground**: the project's components and who stands on each; two writers on one is a collision.
//! - Picking a lane shows that thread, so the directive bar always steers the thread you are looking at.
//!
//! Everything shown is read from tool calls, approvals and edited files; nothing is guessed from a title.

use acp_core::ApprovalDecision;
use cordata_theme::tokens as t;
use cordata_view::app::{ConnState, Message, TabState};
use cordata_view::desk::{self, Touch};
use cordata_view::flight::{self, Beat, RackViewMode};
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

/// A colour of its own for each thread, spread round the wheel so neighbours never look alike. It follows the thread
/// everywhere on the deck: its lane, its dots on the ground, the directive bar.
pub(super) fn thread_hue(slot: usize) -> gpui::Hsla {
    gpui::hsla((slot as f32 * 0.381_966 + 0.06).fract(), 0.58, 0.64, 1.0)
}

fn alpha(mut h: gpui::Hsla, a: f32) -> gpui::Hsla {
    h.a = a;
    h
}

fn elapsed(secs: u64) -> String {
    if secs < 60 { format!("{secs}s") } else if secs < 3600 { format!("{}m {:02}s", secs / 60, secs % 60) } else { format!("{}h {:02}m", secs / 3600, secs % 3600 / 60) }
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
        if self.desk_mode == DeskMode::Rack && self.rack_view_mode == RackViewMode::Field {
            self.ensure_run_poll(cx);
        }
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

    /// Show lane `slot`'s thread and mark it picked, so the directive bar targets it.
    pub(super) fn pick_lane(&mut self, slot: usize, cx: &mut Context<Self>) {
        let picked = self.rack_selected_slot != Some(slot);
        self.rack_selected_slot = picked.then_some(slot);
        if picked && let Some(target) = self.sessions.get(slot - 1).cloned() {
            self.show(target, cx);
        }
        record_ux_event("lane_pick", serde_json::json!({ "slot": slot }));
        cx.notify();
    }

    /// 1-based slot of the thread shown (and so the one the directive bar steers).
    pub(super) fn shown_slot(&self) -> usize {
        let id = self.session.entity_id();
        self.sessions.iter().position(|s| s.entity_id() == id).map_or(1, |ix| ix + 1)
    }

    pub(super) fn lane_inputs(&self, cx: &Context<Self>) -> Vec<TriageIn> {
        let check = self.check_command();
        let active_id = self.session.entity_id();
        let edited: Vec<(usize, std::path::PathBuf, Vec<String>)> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(ix, e)| {
                let s = e.read(cx);
                (ix + 1, s.cwd.clone(), desk::edited_paths(&s.view.messages, &s.cwd))
            })
            .collect();
        self.sessions
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
                let steps = flight::steps(&view.messages);
                let falsified = r.has_falsification_warning();
                let (gate_title, gate_signature, gate_high_risk) = match view.pending.front() {
                    Some(p) => {
                        let summary = p.info.raw_input.as_ref().and_then(cordata_view::view::summarize_input).unwrap_or_else(|| p.info.tool_name.clone());
                        let analysis = acp_core::auto::gate_analysis(&p.info.tool_name, p.info.raw_input.as_ref(), &ses.cwd, p.info.reason.as_deref());
                        let sig = canonical_gate_signature(&ses.cwd, &p.info.tool_name, p.info.raw_input.as_ref(), p.info.reason.as_deref());
                        (Some(format!("{} · {}", p.info.tool_name, shown(&summary))), Some(sig), analysis.high_risk)
                    }
                    None => (None, None, false),
                };
                let own = &edited[ix].2;
                let overlaps = edited
                    .iter()
                    .filter(|(slot, cwd, paths)| *slot != ix + 1 && cwd == &ses.cwd && paths.iter().any(|p| own.contains(p)))
                    .map(|(slot, _, _)| *slot)
                    .collect();
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
                    falsification_warning: falsified,
                    now: flight::now_line(&view.messages, state, gate_title.as_deref(), &ses.cwd),
                    beats: steps.iter().map(|(b, _)| *b).collect(),
                    tips: steps.into_iter().map(|(_, t)| t).collect(),
                    gate_title,
                    gate_signature,
                    gate_high_risk,
                    working_secs: view.turn_started.map(|t| t.elapsed().as_secs()),
                    overlaps,
                }
            })
            .collect()
    }

    pub(super) fn triage_rack_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let inputs = self.lane_inputs(cx);
        let rack = work::triage_rack(&inputs);
        let mode = self.rack_view_mode;
        let sel = self.rack_selected_slot;

        let per_thread: Vec<_> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(ix, e)| {
                let s = e.read(cx);
                (ix + 1, s.cwd.clone(), desk::places(&s.view.messages, &s.cwd, s.view.pending.front().map(|p| p.info.tool_call_id.as_str())))
            })
            .collect();
        let zones = flight::territory(&per_thread);
        let collisions = zones.iter().filter(|z| z.contested).count();

        let ink = |b: Beat| match b {
            Beat::Edit => c(tk.teal),
            Beat::Run => c(tk.text_primary),
            Beat::Failed => c(tk.attention),
            Beat::Fetch => ca(tk.text_secondary, 0.85),
            Beat::Look => ca(tk.text_faint, 0.8),
            Beat::Think => ca(tk.text_muted, 0.5),
        };
        let height = |b: Beat| match b {
            Beat::Edit | Beat::Failed => 30.,
            Beat::Run => 22.,
            Beat::Fetch => 16.,
            Beat::Look => 11.,
            Beat::Think => 6.,
        };

        let working = inputs.iter().filter(|i| i.state == TabState::Working).count();
        let waiting = inputs.iter().filter(|i| i.gate_title.is_some()).count();
        let quiet = inputs.len() - working.min(inputs.len()) - waiting.min(inputs.len() - working.min(inputs.len()));

        let stat = |n: usize, word: &'static str, col: gpui::Hsla| div().flex().items_baseline().gap(px(6.))
            .child(div().font_family(voice(cx)).text_size(px(26.)).text_color(if n > 0 { col } else { c(tk.text_faint) }).child(SharedString::from(n.to_string())))
            .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child(word));

        let lane_rows: Vec<AnyElement> = inputs.iter().map(|item| {
            let (slot, ix) = (item.slot, item.index);
            let hue = thread_hue(slot);
            let is_sel = sel == Some(slot);
            let is_working = item.state == TabState::Working;
            let ready = flight::ready(item);
            let needs_you = item.gate_title.is_some();
            let mark = if is_working { MarkState::Working } else if needs_you { MarkState::NeedsYou } else { MarkState::Idle };
            let beat = self.beat_of(ix, cx);
            let bars = item.beats.iter().enumerate().map(|(bi, b)| {
                let bar = div().id(("beat", slot * 1000 + bi)).w(px(6.)).h(px(height(*b))).rounded(px(2.)).bg(ink(*b))
                    .hover(|s| s.bg(hue))
                    .tooltip(ui::tip(item.tips.get(bi).cloned().unwrap_or_default()))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_lane(slot, cx)));
                bar.into_any_element()
            });
            let strip = div().flex().items_end().justify_end().gap(px(3.)).h(px(32.)).w(px(flight::LANE_LEN as f32 * 9.))
                .border_b_1().border_color(ca(tk.text_faint, 0.25)).children(bars);
            let (sig_a, sig_d) = (item.gate_signature.clone().unwrap_or_default(), item.gate_signature.clone().unwrap_or_default());
            let row = div().id(("lane", slot)).role(gpui::Role::Button).cursor_pointer().relative()
                .py(px(14.)).pl(px(18.)).pr(px(10.)).rounded(px(6.)).mb(px(2.))
                .bg(if needs_you { ca(tk.amber, 0.08) } else if is_sel { alpha(hue, 0.09) } else { ca(tk.bg_panel, 0.0) })
                .hover(|s| s.bg(alpha(hue, 0.07)))
                .on_click(cx.listener(move |this, _, _, cx| this.pick_lane(slot, cx)))
                .flex().items_center().gap(px(14.))
                .child(div().absolute().left(px(0.)).top(px(10.)).bottom(px(10.)).w(px(3.)).rounded(px(2.)).bg(alpha(hue, if is_working || is_sel { 1.0 } else { 0.35 })))
                .child(div().w(px(22.)).flex().flex_col().items_center().gap(px(4.))
                    .child(ui::mark_live(("lane-mark", slot), mark, 12., beat, cx))
                    .child(div().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(hue).child(SharedString::from(format!("#{slot}")))))
                .child(div().flex_1().min_w_0().flex().flex_col().gap(px(5.))
                    .child(div().flex().items_center().gap(px(8.))
                        .child(div().truncate().text_size(px(15.)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(item.title.clone())))
                        .when(ready, |d| d.child(div().flex_shrink_0().px(px(7.)).py(px(1.)).rounded_full().bg(ca(tk.teal, 0.16)).font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.teal)).child("ready to review"))))
                    .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META + 1.))
                        .text_color(if needs_you { c(tk.amber) } else if is_working { c(tk.teal) } else { c(tk.text_muted) })
                        .child(SharedString::from(item.now.clone())))
                    .when(needs_you && !is_sel, |d| d.child(div().flex().items_center().gap(px(6.)).pt(px(4.))
                        .when(!item.gate_high_risk, |r| r.child(ui::button(("lane-allow", slot), "Allow", Kind::Primary, cx)
                            .on_click(cx.listener(move |this, _, _, cx| { cx.stop_propagation(); this.answer_batch_gate(&sig_a, ApprovalDecision::Allow, cx) }))))
                        .child(ui::button(("lane-deny", slot), "Deny", Kind::Ghost, cx)
                            .on_click(cx.listener(move |this, _, _, cx| { cx.stop_propagation(); this.answer_batch_gate(&sig_d, ApprovalDecision::Reject, cx) }))))))
                .child(div().flex().flex_col().items_end().gap(px(4.))
                    .child(strip)
                    .child(div().flex().gap(px(12.))
                        .child(ui::meta(SharedString::from(item.working_secs.map(elapsed).unwrap_or_default()), cx))
                        .child(ui::meta(SharedString::from(if item.files > 0 { format!("{} files  +{} −{}", item.files, item.added, item.removed) } else { "no edits yet".to_owned() }), cx))));
            if !is_sel {
                return row.into_any_element();
            }
            let ses = self.sessions[ix].read(cx);
            let calls: Vec<(Beat, String)> = ses.view.messages.iter().rev().filter_map(|m| match m {
                Message::Tool(card) => Some((flight::beat_of(card), flight::target_of(card).map(|s| shown(&s)).unwrap_or_else(|| card.name.clone()))),
                _ => None,
            }).take(6).collect();
            let tally = flight::tally(&item.beats);
            let places: Vec<(Touch, String, usize, usize)> = desk::places(&ses.view.messages, &ses.cwd, None).into_iter().flat_map(|a| a.places).take(6).map(|p| (p.touch, p.path, p.added, p.removed)).collect();
            let (sig_allow, sig_deny) = (item.gate_signature.clone().unwrap_or_default(), item.gate_signature.clone().unwrap_or_default());
            let inspector = div().id(("lane-detail", slot)).ml(px(36.)).pl(px(14.)).py(px(8.)).border_l_1().border_color(alpha(hue, 0.6))
                .flex().flex_col().gap(px(10.))
                .when_some(item.gate_title.clone(), |d, gate| d.child(div().flex().items_center().gap(px(8.))
                    .child(div().flex_1().min_w_0().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_primary)).child(SharedString::from(gate)))
                    .when(!item.gate_high_risk, |r| r.child(ui::button(("gate-allow", slot), "Allow", Kind::Primary, cx)
                        .on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&sig_allow, ApprovalDecision::Allow, cx)))))
                    .child(ui::button(("gate-deny", slot), "Deny", Kind::Ghost, cx)
                        .on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&sig_deny, ApprovalDecision::Reject, cx))))))
                .child(div().flex().flex_col().gap(px(3.))
                    .child(ui::meta(SharedString::from(format!("last actions · {tally}")), cx))
                    .children(calls.into_iter().map(|(b, target)| div().flex().items_center().gap(px(8.))
                        .child(div().w(px(4.)).h(px(10.)).rounded(px(1.)).bg(ink(b)))
                        .child(div().w(px(44.)).font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.text_muted)).child(b.label()))
                        .child(div().flex_1().min_w_0().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_secondary)).child(SharedString::from(target))))))
                .when(!places.is_empty(), |d| d.child(div().flex().flex_col().gap(px(3.))
                    .child(ui::meta("files", cx))
                    .children(places.into_iter().map(|(touch, path, a, r)| div().flex().items_center().gap(px(8.))
                        .child(div().w(px(44.)).font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.text_muted)).child(match touch { Touch::Waiting => "waiting", Touch::Failed => "failed", Touch::Working => "now", Touch::Edited => "edited", Touch::Read => "read" }))
                        .child(div().flex_1().min_w_0().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_secondary)).child(SharedString::from(path)))
                        .child(ui::meta(SharedString::from(if a + r > 0 { format!("+{a} −{r}") } else { String::new() }), cx))))))
                .child(div().flex().items_center().gap(px(6.))
                    .child(ui::button(("lane-open", slot), "Open thread", Kind::Primary, cx).on_click(cx.listener(|this, _, _, cx| { this.desk_mode = DeskMode::Transcript; cx.notify(); })))
                    .child(ui::button(("lane-diff", slot), "Changes ⌘D", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| { this.load_review(cx); this.desk_mode = DeskMode::ChangeSheet; cx.notify(); })))
                    .when(is_working || needs_you, |r| r.child(ui::button(("lane-stop", slot), "Stop", Kind::Ghost, cx)
                        .on_click(cx.listener(move |this, _, _, cx| { if let Some(s) = this.sessions.get(ix).cloned() { s.update(cx, |s, cx| s.cancel(cx)); } cx.notify(); })))));
            div().flex().flex_col().child(row).child(div().pb(px(12.)).child(inspector)).into_any_element()
        }).collect();

        let tiles: Vec<AnyElement> = zones.iter().enumerate().map(|(zi, z)| {
            let tile = div().id(("zone", zi)).w(px(250.)).min_h(px(104.)).p(px(14.)).rounded(px(8.)).border_1()
                .border_color(if z.contested { ca(tk.amber, 0.8) } else { ca(tk.text_faint, 0.3) })
                .bg(if z.contested { ca(tk.amber, 0.08) } else { ca(tk.bg_panel, 0.5) })
                .flex().flex_col().gap(px(10.))
                .child(div().flex().items_baseline().justify_between()
                    .child(div().truncate().text_size(px(15.)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(z.name.clone())))
                    .child(div().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.text_faint)).child(SharedString::from(z.place.clone()))))
                .child(div().flex().flex_wrap().gap(px(8.)).children(z.occupants.iter().enumerate().map(|(oi, o)| {
                    let slot = o.slot;
                    let (word, col) = match o.touch {
                        Touch::Waiting => ("waiting", c(tk.amber)),
                        Touch::Failed => ("failed", c(tk.attention)),
                        Touch::Working => ("working", c(tk.teal)),
                        Touch::Edited => ("edited", c(tk.teal)),
                        Touch::Read => ("reading", c(tk.text_muted)),
                    };
                    div().id(("zone-chip", zi * 64 + oi)).cursor_pointer().flex().items_center().gap(px(6.)).px(px(8.)).py(px(4.)).rounded_full()
                        .bg(alpha(thread_hue(slot), 0.14)).hover(|s| s.bg(alpha(thread_hue(slot), 0.28)))
                        .on_click(cx.listener(move |this, _, _, cx| { this.rack_view_mode = RackViewMode::Lanes; this.pick_lane(slot, cx); }))
                        .child(div().size(px(8.)).rounded_full().bg(thread_hue(slot)))
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(col).child(SharedString::from(format!("#{slot} {word}"))))
                })))
                .when(z.contested, |d| d.child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.amber)).child("collision: more than one thread is writing here")));
            tile.into_any_element()
        }).collect();

        let batch_cards: Vec<AnyElement> = rack.batch_gates.iter().enumerate().map(|(b_ix, batch)| {
            let (count, sig, deny_sig) = (batch.thread_indices.len(), batch.signature.clone(), batch.signature.clone());
            let slots: Vec<String> = batch.thread_slots.iter().map(|s| format!("#{s}")).collect();
            div().id(("batch-gate", b_ix)).py(px(10.)).px(px(12.)).rounded(px(6.)).border_1().border_color(ca(tk.amber, 0.5)).bg(ca(tk.amber, 0.07))
                .flex().items_center().gap(px(10.))
                .child(ui::mark(("batch-mark", b_ix), MarkState::NeedsYou, 12., cx))
                .child(div().flex_1().min_w_0().flex().flex_col().gap(px(1.))
                    .child(div().text_size(px(t::TEXT_SMALL)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary))
                        .child(SharedString::from(format!("{count} threads ({}) waiting on the same call", slots.join(", ")))))
                    .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_secondary)).child(SharedString::from(batch.title.clone()))))
                .when(!batch.high_risk, |row| {
                    row.child(ui::button(("batch-allow", b_ix), SharedString::from(format!("Allow all {count}")), Kind::Primary, cx)
                        .on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&sig, ApprovalDecision::Allow, cx))))
                })
                .child(ui::button(("batch-deny", b_ix), SharedString::from(format!("Deny all {count}")), Kind::Ghost, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&deny_sig, ApprovalDecision::Reject, cx))))
                .into_any_element()
        }).collect();

        let legend = div().flex().items_center().gap(px(14.)).children([Beat::Look, Beat::Edit, Beat::Run, Beat::Think, Beat::Fetch, Beat::Failed].map(|b|
            div().flex().items_center().gap(px(5.)).child(div().w(px(6.)).h(px(10.)).rounded(px(1.)).bg(ink(b)))
                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.text_muted)).child(b.label()))));

        let tab = |id: &'static str, m: RackViewMode, cx: &mut Context<Self>| {
            ui::button(id, m.label(), if mode == m { Kind::Tint } else { Kind::Ghost }, cx)
                .on_click(cx.listener(move |this, _, _, cx| { this.rack_view_mode = m; if m == RackViewMode::Field { this.ensure_run_poll(cx); } cx.notify(); }))
        };

        let head = div().flex().items_center().justify_between()
            .child(div().flex().items_baseline().gap(px(28.))
                .child(div().font_family(voice(cx)).text_size(px(24.)).text_color(c(tk.text_primary)).child("Deck"))
                .child(stat(working, "working", c(tk.teal)))
                .child(stat(waiting, "need you", c(tk.amber)))
                .child(stat(quiet, "quiet", c(tk.text_secondary)))
                .when(collisions > 0, |d| d.child(stat(collisions, "collisions", c(tk.attention)))))
            .child(div().flex().items_center().gap(px(8.))
                .child(div().flex().items_center().gap(px(2.))
                    .child(tab("view-lanes", RackViewMode::Lanes, cx))
                    .child(tab("view-ground", RackViewMode::Ground, cx))
                    .child(tab("view-field", RackViewMode::Field, cx)))
                .child(div().id("rack-close").role(gpui::Role::Button).cursor_pointer().font_family(mono(cx)).text_size(px(t::TEXT_META))
                    .text_color(c(tk.text_muted)).hover(|s| s.text_color(c(tk.text_primary)))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_rack(cx))).child("thread view  ⌘0")));

        // The field is not a column of rows: it takes the whole pane and pans itself.
        if mode == RackViewMode::Field {
            return div()
                .id("triage-rack")
                .size_full()
                .flex()
                .flex_col()
                .child(div().px(px(24.)).pt(px(20.)).pb(px(10.)).flex().flex_col().gap(px(14.))
                    .child(head)
                    .child(div().flex().items_center().justify_between().pb(px(8.)).border_b_1().border_color(line(cx, t::LINE_SUBTLE))
                        .child(ui::meta("threads in their checkouts · scroll pans, ⌘-scroll zooms, click a node to pick it", cx))
                        .child(self.field_zoom_control(cx)))
                    .children(batch_cards))
                .child(div().flex_1().min_h_0().child(self.field_view(&inputs, &zones, cx)))
                .into_any_element();
        }

        div()
            .id("triage-rack")
            .size_full()
            .overflow_y_scroll()
            .child(div().w_full().flex().justify_center().px(px(24.)).py(px(20.))
                .child(div().w_full().max_w(px(t::CONTENT_MAX_W + 120.)).flex().flex_col().gap(px(14.))
                    .child(head)
                    .child(div().flex().items_center().justify_between().pb(px(8.)).border_b_1().border_color(line(cx, t::LINE_SUBTLE))
                        .child(div().min_w_0().child(ui::meta(match mode { RackViewMode::Lanes | RackViewMode::Field => "one bar per action, oldest left · hover to read", RackViewMode::Ground => "components and the threads on them" }, cx)))
                        .child(legend))
                    .children(batch_cards)
                    .child(match mode {
                        RackViewMode::Lanes | RackViewMode::Field => div().id("lanes").flex().flex_col().children(lane_rows).into_any_element(),
                        RackViewMode::Ground if tiles.is_empty() => div().id("ground").py(px(24.)).text_color(c(tk.text_muted)).text_size(px(t::TEXT_SMALL)).child("No thread has read or changed a file yet.").into_any_element(),
                        RackViewMode::Ground => div().id("ground").flex().flex_wrap().gap(px(12.)).children(tiles).into_any_element(),
                    })))
            .into_any_element()
    }
}
