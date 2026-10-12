//! The orchestrator's run, as the HUD's RUN panel shows it.
//!
//! The backend reports a run as a stream of small events (`_cordata/orchestrator`: `state`, `step_dispatch`,
//! `step_result`, `handoff`, `rollback`). This folds them into one picture — the run's state, how many steps the plan
//! has, and each step's worker and outcome — so the HUD can draw a board instead of a scrollback of lines. It keeps
//! only what the events carry; nothing is inferred.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunStep {
    pub id: String,
    pub worker: String,
    /// `running`, then whatever the executor reported (`success`, `failed`, `refused`, …); `rolled back` after a rollback.
    pub status: String,
    pub cost_usd: Option<f64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub duration_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunBoard {
    pub run_id: String,
    /// The machine's last state: `planning`, `executing`, `done`, `failed`, `paused`, …
    pub state: String,
    /// How many steps the plan has, once it is known.
    pub planned: Option<u64>,
    pub steps: Vec<RunStep>,
    /// The last handoff between workers, as one line.
    pub handoff: Option<String>,
}

impl RunBoard {
    pub fn apply(&mut self, event: &Value) {
        let text = |key: &str| event.get(key).and_then(Value::as_str).map(str::to_owned);
        if let Some(run_id) = text("run_id")
            && run_id != self.run_id
        {
            *self = RunBoard { run_id, ..RunBoard::default() };
        }
        match text("kind").as_deref() {
            Some("state") => {
                if let Some(state) = text("state") {
                    self.state = state;
                }
                if let Some(steps) = event.get("steps").and_then(Value::as_u64) {
                    self.planned = Some(steps);
                }
            }
            Some("step_dispatch") => {
                let (id, worker) = (text("step_id").unwrap_or_default(), text("worker").unwrap_or_default());
                match self.steps.iter_mut().find(|s| s.id == id) {
                    // A retry on another worker: same step, new worker.
                    Some(step) => {
                        step.worker = worker;
                        step.status = "running".into();
                    }
                    None => self.steps.push(RunStep {
                        id,
                        worker,
                        status: "running".into(),
                        cost_usd: None,
                        tokens_in: None,
                        tokens_out: None,
                        duration_s: None,
                    }),
                }
            }
            Some("step_result") => {
                let id = text("step_id").unwrap_or_default();
                let status = text("status").unwrap_or_else(|| "done".into());
                if let Some(step) = self.steps.iter_mut().find(|s| s.id == id) {
                    step.status = status;
                    if let Some(worker) = text("worker") {
                        step.worker = worker;
                    }
                    if let Some(cost) = event.get("cost_usd").and_then(Value::as_f64) {
                        step.cost_usd = Some(cost);
                    }
                    if let Some(tin) = event.get("tokens_in").and_then(Value::as_u64) {
                        step.tokens_in = Some(tin);
                    }
                    if let Some(tout) = event.get("tokens_out").and_then(Value::as_u64) {
                        step.tokens_out = Some(tout);
                    }
                    if let Some(dur) = event.get("duration_s").and_then(Value::as_f64) {
                        step.duration_s = Some(dur);
                    }
                }
            }
            Some("rollback") => {
                let id = text("step_id").unwrap_or_default();
                if let Some(step) = self.steps.iter_mut().find(|s| s.id == id) {
                    step.status = "rolled back".into();
                }
            }
            Some("handoff") => {
                self.handoff = Some(format!(
                    "{} → {} ({})",
                    text("from_worker").unwrap_or_default(),
                    text("to_worker").unwrap_or_else(|| "none".into()),
                    text("reason").unwrap_or_default()
                ));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn events_fold_into_one_board() {
        let mut board = RunBoard::default();
        board.apply(&json!({"kind": "state", "run_id": "r1", "state": "planning"}));
        board.apply(&json!({"kind": "state", "run_id": "r1", "state": "executing", "steps": 3}));
        board.apply(&json!({"kind": "step_dispatch", "run_id": "r1", "step_id": "s1", "worker": "ollama:kimi-k2.7-code"}));
        board.apply(&json!({"kind": "step_result", "run_id": "r1", "step_id": "s1", "status": "success", "cost_usd": 0.0125, "tokens_in": 1200, "tokens_out": 350, "duration_s": 2.1}));
        board.apply(&json!({"kind": "step_dispatch", "run_id": "r1", "step_id": "s2", "worker": "claude_code:sonnet"}));
        board.apply(&json!({"kind": "handoff", "run_id": "r1", "from_worker": "claude_code:sonnet", "to_worker": "antigravity:gemini", "reason": "quota"}));
        board.apply(&json!({"kind": "step_dispatch", "run_id": "r1", "step_id": "s2", "worker": "antigravity:gemini"}));
        assert_eq!((board.state.as_str(), board.planned, board.steps.len()), ("executing", Some(3), 2));
        assert_eq!(
            board.steps[0],
            RunStep {
                id: "s1".into(),
                worker: "ollama:kimi-k2.7-code".into(),
                status: "success".into(),
                cost_usd: Some(0.0125),
                tokens_in: Some(1200),
                tokens_out: Some(350),
                duration_s: Some(2.1),
            }
        );
        assert_eq!((board.steps[1].worker.as_str(), board.steps[1].status.as_str()), ("antigravity:gemini", "running"), "a retry moves the step to its new worker");
        assert_eq!(board.handoff.as_deref(), Some("claude_code:sonnet → antigravity:gemini (quota)"));
        board.apply(&json!({"kind": "rollback", "step_id": "s2"}));
        assert_eq!(board.steps[1].status, "rolled back");
        board.apply(&json!({"kind": "state", "state": "done"}));
        assert_eq!(board.state, "done");
    }

    #[test]
    fn a_new_run_replaces_the_old_one() {
        let mut board = RunBoard::default();
        board.apply(&json!({"kind": "state", "run_id": "old", "state": "done", "steps": 2}));
        board.apply(&json!({"kind": "step_dispatch", "run_id": "old", "step_id": "s1", "worker": "w"}));
        board.apply(&json!({"kind": "state", "run_id": "new", "state": "planning"}));
        assert_eq!((board.run_id.as_str(), board.state.as_str(), board.steps.len(), board.planned), ("new", "planning", 0, None));
    }
}
