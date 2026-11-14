//! The menu bar item's life inside the window: keep it current, answer its clicks, and let the window go away into it.
//!
//! Closing the window hides the app (the item stays, so a gate that needs you can still be answered); ⌘Q ends it.
//! A click only ever does what its menu line said: a gate is answered only if it is still the call that was shown.

use std::sync::atomic::Ordering;
use std::time::Duration;

use acp_core::ApprovalDecision;
use cordata_view::flight::RackViewMode;
use cordata_view::shown;
use cordata_view::tray::{self, Action, GateIn, RunIn, ThreadIn};
use gpui::{Context, Window};

use super::sheet::DeskMode;
use super::{NewSession, OpenSettings, Workspace};
use crate::tray::{IN_MENU_BAR, QUITTING};

/// How often clicks are picked up, and how many of those ticks make one refresh of what the menu says.
const TICK: Duration = Duration::from_millis(250);
const SYNC_EVERY: u32 = 4;
/// The run feed is read this often while the field is not open (it polls itself faster when it is).
const RUNS_EVERY: u32 = 40;

impl Workspace {
    /// Once, at launch: the close button hides the app while the item is up, and a small loop keeps the item honest.
    pub(super) fn start_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Tests and the packaged self-test must not put anything in the real menu bar.
        if cfg!(test) || std::env::args().any(|a| a == "--selftest") || std::env::var_os("CORDATA_APP_TRAY").is_some_and(|v| v == "off") {
            return;
        }
        window.on_window_should_close(cx, |_, cx| {
            if QUITTING.load(Ordering::Relaxed) || !IN_MENU_BAR.load(Ordering::Relaxed) {
                true
            } else {
                cx.hide();
                false
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update_in(cx, |this, window, cx| this.tray_pump(window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn tray_pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The pref decides whether the item exists; flipping it takes effect within a tick.
        if self.prefs.menu_bar && self.tray.is_none() {
            self.tray = crate::tray::Native::new();
            self.tray_tick = 0;
        } else if !self.prefs.menu_bar && self.tray.is_some() {
            self.tray = None;
        }
        let Some(native) = &self.tray else { return };
        for id in native.drain() {
            self.tray_action(&id, window, cx);
        }
        self.tray_tick = self.tray_tick.wrapping_add(1);
        if self.tray_tick.is_multiple_of(RUNS_EVERY) && !(self.desk_mode == DeskMode::Rack && self.rack_view_mode == RackViewMode::Field) {
            self.refresh_runs(cx);
        }
        if self.tray_tick % SYNC_EVERY == 1 {
            self.sync_tray(cx);
        }
    }

    fn sync_tray(&mut self, cx: &mut Context<Self>) {
        let inputs = self.lane_inputs(cx);
        let gates: Vec<GateIn> = inputs
            .iter()
            .filter_map(|i| {
                Some(GateIn {
                    slot: i.slot,
                    thread: i.title.clone(),
                    command: cordata_view::redact::scrub(&shown(i.gate_title.as_deref()?)).0,
                    high_risk: i.gate_high_risk,
                    sig: tray::sig_hash(i.gate_signature.as_deref()?),
                })
            })
            .collect();
        let threads: Vec<ThreadIn> = inputs.iter().map(|i| ThreadIn { slot: i.slot, title: i.title.clone(), now: i.now.clone(), state: i.state }).collect();
        let runs: Vec<RunIn> = self
            .field_runs
            .iter()
            .map(|r| RunIn {
                run_id: r.run_id.clone(),
                goal: r.goal.clone(),
                state: r.state.clone(),
                can_hold: r.can_hold(),
                can_resume: r.can_resume(),
                awaits_approval: r.awaits_approval,
                asks: r.asks(),
                last_handoff: r.handoffs.last().map(|h| format!("{} → {} · {}", h.from, h.to.clone().unwrap_or_else(|| "…".into()), cordata_view::runs::reason_words(&h.reason))),
            })
            .collect();
        if let Some(native) = &self.tray {
            native.set(&tray::build(&gates, &threads, &runs));
        }
    }

    fn bring_forward(window: &mut Window, cx: &mut Context<Self>) {
        cx.activate(true);
        window.activate_window();
    }

    fn tray_action(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = tray::parse(id) else { return };
        match action {
            Action::Open => Self::bring_forward(window, cx),
            Action::ShowField => {
                self.desk_mode = DeskMode::Rack;
                self.rack_view_mode = RackViewMode::Field;
                self.ensure_run_poll(cx);
                Self::bring_forward(window, cx);
                cx.notify();
            }
            Action::NewThread => {
                self.new_session(&NewSession, window, cx);
                Self::bring_forward(window, cx);
            }
            Action::Settings => {
                self.open_settings(&OpenSettings, window, cx);
                Self::bring_forward(window, cx);
            }
            Action::Quit => {
                QUITTING.store(true, Ordering::Relaxed);
                cx.quit();
            }
            Action::OpenThread(slot) => {
                self.switch_slot(slot, cx);
                self.desk_mode = DeskMode::Transcript;
                Self::bring_forward(window, cx);
                cx.notify();
            }
            Action::Allow { slot, sig } => self.answer_from_tray(slot, sig, ApprovalDecision::Allow, cx),
            Action::Reject { slot, sig } => self.answer_from_tray(slot, sig, ApprovalDecision::Reject, cx),
            Action::Hold(run) => {
                if self.field_runs.iter().any(|r| r.run_id == run && r.can_hold()) {
                    self.hold_run(&run, cx);
                }
            }
            Action::Resume(run) => {
                if self.field_runs.iter().any(|r| r.run_id == run && r.can_resume()) {
                    self.resume_run(&run, None, None, cx);
                }
            }
        }
        self.tray_tick = 0; // show the result at the next tick, not in a second
    }

    /// Answer the gate the menu showed, and only that one: if the thread has moved on to another call, do nothing.
    fn answer_from_tray(&mut self, slot: usize, shown_sig: u64, decision: ApprovalDecision, cx: &mut Context<Self>) {
        let inputs = self.lane_inputs(cx);
        let Some(item) = inputs.iter().find(|i| i.slot == slot) else { return };
        let Some(sig) = item.gate_signature.as_deref() else { return };
        let allow = matches!(decision, ApprovalDecision::Allow);
        if tray::sig_hash(sig) != shown_sig || (allow && item.gate_high_risk) {
            return;
        }
        self.answer_batch_gate(sig, decision, cx);
    }
}
