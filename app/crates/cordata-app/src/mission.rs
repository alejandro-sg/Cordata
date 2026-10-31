//! Mission Control (docs/app.md §4): the orchestrator's runs in this project, and one run in full — its steps, a pod
//! per worker with its pool gauge (soft and hard lines) and what it is doing now, the budget envelope, the handoffs,
//! the artifacts (plan.md, walkthrough.md) — with pause, resume and cancel.
//!
//! Everything comes through the session (`_cordata/orchestrator/*`, `_cordata/artifacts/*`, docs/app.md §6.2), so it
//! works on the hub and over stdio alike. A selected run is followed by reading its events file from an offset and
//! folding the events into the shared `RunBoard` (the same reducer the terminal's HUD uses); a run started elsewhere —
//! a terminal's `cordata run`, another session — is followed the same way.

use cordata_theme::{Role, segbar};
use cordata_view::run_board::{PoolGauge, RunBoard};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, Div, IntoElement, ParentElement, SharedString, Styled, div, px, relative};
use serde_json::Value;

use crate::hud;
use crate::theme::{color, tint};

pub const RUNS_TAG: &str = "gui-runs";
pub const SHOW_TAG: &str = "gui-run";
pub const EVENTS_TAG: &str = "gui-run-events";
pub const CONTROL_TAG: &str = "gui-run-control";
pub const START_TAG: &str = "gui-run-start";
pub const ARTIFACT_TAG: &str = "gui-artifact";

#[derive(Default)]
pub struct Mission {
    pub runs: Vec<Value>,
    pub selected: Option<String>,
    pub board: RunBoard,
    /// Byte offset into the selected run's events file.
    pub offset: u64,
    /// `orchestrator/show` for the selected run (goal, step names, artifacts, what can be done now).
    pub detail: Option<Value>,
    /// An artifact being read: (name, text).
    pub artifact: Option<(String, String)>,
    /// The last control answer or problem, one line.
    pub notice: Option<String>,
    /// An events request is out (don't stack them).
    pub polling: bool,
}

impl Mission {
    pub fn select(&mut self, run_id: Option<String>) {
        self.selected = run_id;
        self.board = RunBoard::default();
        self.offset = 0;
        self.detail = None;
        self.artifact = None;
        self.notice = None;
        self.polling = false;
    }

    /// Fold an `orchestrator/events` answer in; returns whether there may be more right away.
    pub fn take_events(&mut self, value: &Value) -> bool {
        self.polling = false;
        let events = value.get("events").and_then(Value::as_array).cloned().unwrap_or_default();
        for event in &events {
            self.board.apply(event);
        }
        if let Some(next) = value.get("next").and_then(Value::as_u64) {
            self.offset = next;
        }
        events.len() >= 200
    }

    pub fn live(&self) -> bool {
        let state = self.detail.as_ref().and_then(|d| d.get("live")).and_then(Value::as_str).unwrap_or(self.board.state.as_str());
        matches!(state, "running" | "plan" | "research" | "execute" | "synthesize" | "harden" | "audit" | "conformance")
    }

    fn can(&self, what: &str) -> bool {
        self.detail.as_ref().and_then(|d| d.get("control")).and_then(|c| c.get(what)).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn can_pause(&self) -> bool {
        self.can("canPause")
    }

    pub fn can_resume(&self) -> bool {
        self.can("canResume")
    }

    pub fn can_cancel(&self) -> bool {
        self.can("canCancel")
    }

    pub fn step_name(&self, id: &str) -> Option<String> {
        self.detail.as_ref()?.get("steps")?.as_array()?.iter().find(|s| s.get("step_id").and_then(Value::as_str) == Some(id))?.get("name")?.as_str().map(str::to_owned)
    }

    pub fn artifacts(&self) -> Vec<String> {
        self.detail
            .as_ref()
            .and_then(|d| d.get("artifacts"))
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(|a| a.get("name").and_then(Value::as_str).map(str::to_owned)).filter(|n| n.ends_with(".md") || n.ends_with(".patch")).collect())
            .unwrap_or_default()
    }
}

/// The colour of a run or step state.
pub fn state_role(state: &str) -> Role {
    match state {
        "done" | "completed" | "success" => Role::Ok,
        "failed" | "refused" | "rolled back" | "budget_exhausted" => Role::Danger,
        "cancelled" | "interrupted" => Role::TextSecondary,
        "paused" => Role::Attention,
        "pending" | "" => Role::TextDim,
        _ => Role::Accent,
    }
}

/// A pool's gauge: the fill, and ticks at its soft and hard lines.
pub fn pool_gauge(pool: &PoolGauge, cx: &App) -> Div {
    let role = match pool.level.as_str() {
        "hard" => Role::Danger,
        "soft" => Role::Attention,
        _ => Role::Ok,
    };
    let tick = |at: f64, r: Role| div().absolute().top_0().bottom_0().left(relative(at.clamp(0.0, 1.0) as f32)).w(px(1.)).bg(color(cx, r));
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .flex()
                .text_xs()
                .child(div().flex_1().text_color(color(cx, Role::TextSecondary)).child(SharedString::from(pool.pool_id.clone())))
                .child(div().text_color(color(cx, role)).child(SharedString::from(format!("{:.0}% · {}", pool.pct * 100.0, pool.level)))),
        )
        .child(
            div()
                .relative()
                .h(px(5.))
                .rounded(px(1.))
                .bg(color(cx, Role::Raised))
                .child(div().absolute().top_0().bottom_0().left_0().w(relative(pool.pct.clamp(0.0, 1.0) as f32)).bg(tint(cx, role, 0.85)))
                .child(tick(pool.soft, Role::Attention))
                .child(tick(pool.hard, Role::Danger)),
        )
}

/// The selected run, in full. Buttons are added by the workspace (they need its listeners).
pub fn run_detail(mission: &Mission, cx: &App) -> AnyElement {
    let board = &mission.board;
    let detail = mission.detail.as_ref();
    let goal = detail.and_then(|d| d.get("goal")).and_then(Value::as_str).unwrap_or_default();
    let live = detail.and_then(|d| d.get("live")).and_then(Value::as_str).unwrap_or(board.state.as_str()).to_owned();
    let reason = detail.and_then(|d| d.get("reason")).and_then(Value::as_str).filter(|r| !r.is_empty()).map(str::to_owned).or_else(|| board.reason.clone());

    let steps = board.steps.iter().map(|step| {
        let name = mission.step_name(&step.id).unwrap_or_else(|| step.id.clone());
        let spend = match (step.cost_usd, step.tokens_in, step.tokens_out) {
            (Some(cost), Some(tin), Some(tout)) if cost > 0.0 => format!("${cost:.3} · {}k", (tin + tout) / 1000),
            (_, Some(tin), Some(tout)) => format!("{}k tok", (tin + tout) / 1000),
            _ => String::new(),
        };
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .text_xs()
            .child(hud::dot(Some(state_role(&step.status)), cx))
            .child(div().flex_1().text_color(color(cx, Role::TextSecondary)).truncate().child(SharedString::from(shown(&name))))
            .child(div().text_color(color(cx, Role::Agent)).child(SharedString::from(short_worker(&step.worker))))
            .child(div().w(px(64.)).text_color(color(cx, Role::TextDim)).child(SharedString::from(spend)))
    });

    let pods = board.pods.iter().map(|pod| {
        let pool = pod.pool_id.as_ref().and_then(|id| board.pools.iter().find(|p| &p.pool_id == id));
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(8.))
            .rounded(px(3.))
            .border_1()
            .border_color(if pod.busy { tint(cx, Role::Agent, 0.7) } else { color(cx, Role::Hairline) })
            .bg(color(cx, Role::Raised))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(hud::dot(Some(if pod.busy { Role::Agent } else { Role::TextDim }), cx))
                    .child(div().flex_1().text_sm().text_color(color(cx, Role::Text)).child(SharedString::from(pod.worker.clone())))
                    .child(div().text_xs().text_color(color(cx, Role::TextDim)).child(SharedString::from(pod.roles.join(" · ")))),
            )
            .child(div().text_xs().text_color(color(cx, Role::TextSecondary)).child(SharedString::from(format!(
                "{}k in · {}k out · {} call{}{}",
                pod.tokens_in / 1000,
                pod.tokens_out / 1000,
                pod.requests,
                if pod.requests == 1 { "" } else { "s" },
                if pod.cost_usd > 0.0 { format!(" · ${:.3}", pod.cost_usd) } else { String::new() }
            ))))
            .when_some(pod.activity.clone().filter(|_| pod.busy), |this, activity| {
                this.child(div().text_xs().font_family(crate::theme::mono(cx)).text_color(color(cx, Role::Agent)).truncate().child(SharedString::from(format!("{} {}", pod.step.clone().unwrap_or_default(), shown(&activity)))))
            })
            .when_some(pool, |this, pool| this.child(pool_gauge(pool, cx)))
    });

    let envelope = board.envelope.as_ref().map(|e| {
        let fraction = e.max_usd.filter(|m| *m > 0.0).map(|m| e.spent_usd / m);
        hud::gauge("budget", fraction, &format!("${:.3}{}", e.spent_usd, e.max_usd.map(|m| format!(" / ${m:.2}")).unwrap_or_default()), cx)
    });

    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().flex().items_center().gap(px(8.)).child(hud::pill(&live, state_role(&live), cx)).child(div().text_xs().text_color(color(cx, Role::TextDim)).child(SharedString::from(mission.selected.clone().unwrap_or_default()))))
                .when(!board.steps.is_empty() || board.planned.is_some(), |this| this.child(div().text_xs().font_family(crate::theme::mono(cx)).text_color(color(cx, Role::Accent)).child(SharedString::from(progress(board)))))
                .child(div().text_sm().text_color(color(cx, Role::Text)).child(SharedString::from(shown(goal))))
                .when_some(reason, |this, reason| this.child(div().text_xs().text_color(color(cx, Role::Attention)).child(SharedString::from(shown(&reason))))),
        )
        .children(envelope)
        .when(!board.steps.is_empty(), |this| this.child(hud::label("steps", cx)).child(div().flex().flex_col().gap(px(3.)).children(steps)))
        .when(!board.pods.is_empty(), |this| this.child(hud::label("pods", cx)).child(div().flex().flex_col().gap(px(6.)).children(pods)))
        .when_some(board.synthesis.clone(), |this, synthesis| this.child(div().text_xs().text_color(color(cx, Role::Accent)).child(SharedString::from(format!("⊕ {}", shown(&synthesis))))))
        .when_some(board.handoff.clone(), |this, handoff| this.child(div().text_xs().text_color(color(cx, Role::Agent)).child(SharedString::from(format!("⇄ {}", shown(&handoff))))))
        .when_some(mission.notice.clone(), |this, notice| this.child(div().text_xs().text_color(color(cx, Role::Attention)).child(SharedString::from(shown(&notice)))))
        .into_any_element()
}

/// `claude_code:sonnet` → `sonnet`; the family is the pod's colour, the model is what a person reads.
fn short_worker(worker: &str) -> String {
    worker.split_once(':').map_or(worker, |(_, model)| model).to_owned()
}

/// The steps' completion as a segmented bar, for the run list's selected row.
pub fn progress(board: &RunBoard) -> String {
    let done = board.steps.iter().filter(|s| matches!(s.status.as_str(), "completed" | "success")).count();
    let total = board.planned.map(|n| n as usize).unwrap_or(board.steps.len()).max(1);
    format!("{} {done}/{total}", segbar(done as f64 / total as f64, 10))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_fold_into_the_selected_run_and_the_offset_advances() {
        let mut mission = Mission::default();
        mission.select(Some("run-1".into()));
        let more = mission.take_events(&json!({"events": [{"kind": "state", "run_id": "run-1", "state": "execute", "steps": 3}, {"kind": "step_dispatch", "run_id": "run-1", "step_id": "s1", "worker": "claude_code:sonnet"}], "next": 812}));
        assert!(!more);
        assert_eq!((mission.offset, mission.board.state.as_str(), mission.board.steps.len()), (812, "execute", 1));
        assert_eq!(progress(&mission.board), "▱▱▱▱▱▱▱▱▱▱ 0/3");
        mission.detail = Some(json!({"live": "running", "control": {"canPause": true, "canCancel": true, "canResume": false}, "steps": [{"step_id": "s1", "name": "Add mul"}]}));
        assert!(mission.live() && mission.can_pause() && !mission.can_resume());
        assert_eq!(mission.step_name("s1").as_deref(), Some("Add mul"));
        assert_eq!(short_worker("claude_code:sonnet"), "sonnet");
        mission.select(None);
        assert!(mission.board.steps.is_empty() && mission.offset == 0, "choosing another run starts clean");
    }
}
