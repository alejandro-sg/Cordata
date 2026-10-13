//! The orchestrator's run, as the HUD's RUN panel shows it.
//!
//! The backend reports a run as a stream of small events (`_cordata/orchestrator`: `state`, `step_dispatch`,
//! `step_result`, `handoff`, `rollback`). This folds them into one picture — the run's state, how many steps the plan
//! has, and each step's worker and outcome — so the HUD can draw a board instead of a scrollback of lines. It keeps
//! only what the events carry; nothing is inferred.
//!
//! Mission Control (the native app's dock) draws more of the same stream: a **pod** per worker (what it spent, its pool,
//! what it is doing right now, from the `worker` events), each **pool**'s gauge with its soft and hard lines, and the
//! run's budget **envelope** (all from `usage`).

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

/// One worker in the run — a "pod" in Mission Control.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pod {
    /// `claude_code:sonnet`, `ollama:kimi-k2.7-code`.
    pub worker: String,
    /// Roles it has played: `plan`, `execute`, `harden`, `audit`, …
    pub roles: Vec<String>,
    pub pool_id: Option<String>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub requests: u64,
    /// The step it is on now, while it works.
    pub step: Option<String>,
    /// The last thing it reported doing (a tool it called, an error), one line.
    pub activity: Option<String>,
    pub busy: bool,
}

/// A quota pool's gauge: how full, and where the soft and hard lines are.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PoolGauge {
    pub pool_id: String,
    pub family: String,
    pub pct: f64,
    /// `ok`, `soft`, `hard`.
    pub level: String,
    pub soft: f64,
    pub hard: f64,
}

/// The run's budget: what it may spend and what it has.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Envelope {
    pub max_usd: Option<f64>,
    pub spent_usd: f64,
    pub exhausted: bool,
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
    pub pods: Vec<Pod>,
    pub pools: Vec<PoolGauge>,
    pub envelope: Option<Envelope>,
    /// Why it stopped, when it is paused (`user_pause`, a quota line…).
    pub reason: Option<String>,
}

impl RunBoard {
    fn pod(&mut self, worker: &str) -> &mut Pod {
        if let Some(index) = self.pods.iter().position(|p| p.worker == worker) {
            return &mut self.pods[index];
        }
        self.pods.push(Pod { worker: worker.to_owned(), ..Pod::default() });
        self.pods.last_mut().expect("just pushed")
    }

    /// Is anything left to steer (pause, cancel)?
    pub fn is_live(&self) -> bool {
        !matches!(self.state.as_str(), "done" | "failed" | "budget_exhausted" | "cancelled" | "")
    }

    /// One line for a worker event: what a pod is doing.
    fn activity(event: &Value) -> Option<String> {
        let payload = event.get("payload")?;
        let kind = event.get("event_type").and_then(Value::as_str).unwrap_or_default();
        let pick = |keys: &[&str]| keys.iter().find_map(|k| payload.get(*k).and_then(Value::as_str)).map(str::to_owned);
        let line = match kind {
            "tool_use" => pick(&["name", "tool"]).map(|tool| match pick(&["command", "path", "file_path", "pattern"]) {
                Some(detail) => format!("{tool} {detail}"),
                None => tool,
            }),
            "error" => pick(&["message", "error"]).map(|m| format!("error: {m}")),
            "quota" => Some("quota reading".into()),
            "init" => Some("starting".into()),
            "text" | "progress" => pick(&["text", "message"]),
            _ => None,
        }?;
        let line: String = line.lines().next().unwrap_or_default().chars().take(160).collect();
        (!line.trim().is_empty()).then_some(line)
    }
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
                    if state != "paused" {
                        self.reason = None;
                    }
                    if matches!(state.as_str(), "done" | "failed" | "budget_exhausted" | "cancelled" | "paused") {
                        for pod in &mut self.pods {
                            pod.busy = false;
                        }
                    }
                    self.state = state;
                }
                if let Some(steps) = event.get("steps").and_then(Value::as_u64) {
                    self.planned = Some(steps);
                }
            }
            Some("step_dispatch") => {
                let (id, worker) = (text("step_id").unwrap_or_default(), text("worker").unwrap_or_default());
                if !worker.is_empty() {
                    let pool = text("pool_id");
                    let pod = self.pod(&worker);
                    if !pod.roles.iter().any(|r| r == "execute") {
                        pod.roles.push("execute".into());
                    }
                    pod.pool_id = pool.or(pod.pool_id.take());
                    pod.step = Some(id.clone());
                    pod.busy = true;
                }
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
                if let Some(worker) = text("worker") {
                    let pod = self.pod(&worker);
                    pod.busy = false;
                    pod.step = None;
                }
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
            Some("dispatch") => {
                // A one-shot call (plan, research, harden, audit, conformance).
                if let (Some(worker), Some(role)) = (text("worker"), text("role")) {
                    let pool = text("pool_id");
                    let pod = self.pod(&worker);
                    if !pod.roles.contains(&role) {
                        pod.roles.push(role.clone());
                    }
                    pod.pool_id = pool.or(pod.pool_id.take());
                    pod.step = Some(role);
                    pod.busy = true;
                }
            }
            Some("worker") => {
                if let Some(worker) = text("worker") {
                    let (step, line) = (text("step_id"), Self::activity(event));
                    let pod = self.pod(&worker);
                    if let Some(line) = line {
                        pod.activity = Some(line);
                    }
                    pod.step = step.or(pod.step.take());
                    pod.busy = true;
                }
            }
            Some("usage") => {
                if let Some(rows) = event.get("pods").and_then(Value::as_array) {
                    for row in rows {
                        // Rows are per (worker, role): first make sure every pod and role exists, then sum per pod below.
                        let Some(worker) = row.get("worker").and_then(Value::as_str).filter(|w| !w.is_empty()) else { continue };
                        let (role, pool) = (row.get("role").and_then(Value::as_str).map(str::to_owned), row.get("pool_id").and_then(Value::as_str).map(str::to_owned));
                        let pod = self.pod(worker);
                        if let Some(role) = role
                            && !pod.roles.contains(&role)
                        {
                            pod.roles.push(role);
                        }
                        if pool.is_some() {
                            pod.pool_id = pool;
                        }
                    }
                    for pod in &mut self.pods {
                        let mine = rows.iter().filter(|r| r.get("worker").and_then(Value::as_str) == Some(pod.worker.as_str()));
                        let (mut tin, mut tout, mut cost, mut requests) = (0, 0, 0.0, 0);
                        for row in mine {
                            tin += row.get("tokens_in").and_then(Value::as_u64).unwrap_or(0);
                            tout += row.get("tokens_out").and_then(Value::as_u64).unwrap_or(0);
                            cost += row.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
                            requests += row.get("calls").or_else(|| row.get("requests")).and_then(Value::as_u64).unwrap_or(0);
                        }
                        (pod.tokens_in, pod.tokens_out, pod.cost_usd, pod.requests) = (tin, tout, cost, requests);
                        // A one-shot call (plan, research, harden, audit, conformance) reports its usage when it ends:
                        // its pod is free again. An execute step is freed by its `step_result`.
                        if pod.step.as_deref().is_some_and(|step| matches!(step, "plan" | "research" | "harden" | "audit" | "conformance")) {
                            pod.busy = false;
                            pod.step = None;
                        }
                    }
                }
                if let Some(pools) = event.get("pools").and_then(Value::as_array) {
                    self.pools = pools
                        .iter()
                        .filter_map(|pool| {
                            let number = |k: &str| pool.get(k).and_then(Value::as_f64);
                            Some(PoolGauge {
                                pool_id: pool.get("pool_id")?.as_str()?.to_owned(),
                                family: pool.get("family").and_then(Value::as_str).unwrap_or_default().to_owned(),
                                pct: number("pct").unwrap_or(0.0),
                                level: pool.get("level").and_then(Value::as_str).unwrap_or("ok").to_owned(),
                                soft: number("soft_threshold_pct").or_else(|| number("soft")).unwrap_or(0.8),
                                hard: number("hard_threshold_pct").or_else(|| number("hard")).unwrap_or(0.95),
                            })
                        })
                        .collect();
                }
                if let Some(envelope) = event.get("envelope") {
                    self.envelope = Some(Envelope {
                        max_usd: envelope.get("max_usd").and_then(Value::as_f64),
                        spent_usd: envelope.get("spent_usd").and_then(Value::as_f64).unwrap_or(0.0),
                        exhausted: envelope.get("exhausted").and_then(Value::as_bool).unwrap_or(false),
                    });
                }
            }
            Some("paused") => {
                self.reason = text("reason");
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
    fn pods_pools_and_the_envelope_come_from_usage_and_worker_events() {
        let mut board = RunBoard::default();
        board.apply(&json!({"kind": "state", "run_id": "r1", "state": "plan"}));
        board.apply(&json!({"kind": "dispatch", "run_id": "r1", "role": "plan", "worker": "ollama:kimi", "pool_id": "ollama-cloud"}));
        board.apply(&json!({"kind": "step_dispatch", "run_id": "r1", "step_id": "s1", "worker": "claude_code:sonnet", "pool_id": "claude-max"}));
        board.apply(&json!({"kind": "worker", "run_id": "r1", "step_id": "s1", "worker": "claude_code:sonnet", "event_type": "tool_use", "payload": {"name": "Edit", "path": "calc/mul.py"}}));
        let sonnet = board.pods.iter().find(|p| p.worker == "claude_code:sonnet").unwrap();
        assert_eq!((sonnet.busy, sonnet.step.as_deref(), sonnet.activity.as_deref(), sonnet.pool_id.as_deref()), (true, Some("s1"), Some("Edit calc/mul.py"), Some("claude-max")));
        board.apply(&json!({
            "kind": "usage", "run_id": "r1",
            "pods": [
                {"worker": "ollama:kimi", "role": "plan", "tokens_in": 2000, "tokens_out": 500, "cost_usd": 0.0, "calls": 1},
                {"worker": "claude_code:sonnet", "role": "execute", "tokens_in": 1000, "tokens_out": 300, "cost_usd": 0.02, "requests": 1},
                {"worker": "claude_code:sonnet", "role": "harden", "tokens_in": 500, "tokens_out": 100, "cost_usd": 0.01, "requests": 1}
            ],
            "pools": [{"pool_id": "claude-max", "family": "claude_code", "pct": 0.83, "level": "soft", "soft_threshold_pct": 0.8, "hard_threshold_pct": 0.95}],
            "envelope": {"max_usd": 2.0, "spent_usd": 0.03, "exhausted": false}
        }));
        let sonnet = board.pods.iter().find(|p| p.worker == "claude_code:sonnet").unwrap();
        assert_eq!((sonnet.tokens_in, sonnet.tokens_out), (1500, 400), "a pod sums its roles");
        let kimi = board.pods.iter().find(|p| p.worker == "ollama:kimi").unwrap();
        assert_eq!((kimi.requests, kimi.busy), (1, false), "a planner whose usage has landed is free (and its calls counted)");
        assert!((sonnet.cost_usd - 0.03).abs() < 1e-9 && sonnet.roles.contains(&"harden".to_owned()));
        assert_eq!((board.pools[0].level.as_str(), board.pools[0].soft, board.pools[0].hard), ("soft", 0.8, 0.95));
        assert_eq!(board.envelope.as_ref().map(|e| (e.max_usd, e.spent_usd)), Some((Some(2.0), 0.03)));
        board.apply(&json!({"kind": "step_result", "run_id": "r1", "step_id": "s1", "worker": "claude_code:sonnet", "status": "completed"}));
        assert!(!board.pods.iter().find(|p| p.worker == "claude_code:sonnet").unwrap().busy, "a finished step frees its pod");
        board.apply(&json!({"kind": "paused", "run_id": "r1", "reason": "user_pause"}));
        board.apply(&json!({"kind": "state", "run_id": "r1", "state": "paused"}));
        assert_eq!((board.state.as_str(), board.reason.as_deref(), board.is_live()), ("paused", Some("user_pause"), true));
        board.apply(&json!({"kind": "state", "run_id": "r1", "state": "cancelled"}));
        assert!(!board.is_live() && board.reason.is_none());
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
