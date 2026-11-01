//! The durable RESEARCH → PLAN → EXECUTE → (SYNTHESIZE) → HARDEN → AUDIT state machine, ported from
//! `backend/orchestrator/machine.py`.
//!
//! ```text
//!    (RESEARCH) ─► PLAN ─► EXECUTE ─► (SYNTHESIZE) ─► HARDEN ─► AUDIT ─┬─► DONE
//!       │           ▲        │                        ├─► drift: a fix pass (≤ 2), then AUDIT again
//!       │ refused   │        ▼ (quota)                └─► replan (≤ 1): back to the plan-start checkpoint, PLAN with the reasons
//!       ▼ twice     │     safe-point handoff (executor)
//!     FAILED        └──────────── replan (≤ 1) ───────────────
//! ```
//!
//! Every transition is written to `state.db` (`orch_runs` + an `orchestrator` event), so a run outlives the process that
//! started it: [`resume`] picks a *paused* run up where it stopped. Bounded by construction: at most `max_drift_retries`
//! fix passes and `max_replans` replans, and the plan's budget envelope over everything. Past any bound the run ends
//! `failed` and the project goes back to the checkpoint taken before the plan's first step — a failed run leaves the tree
//! as it found it (the discarded work is first saved as a patch).
//!
//! Who does what is the [`Roster`]'s call (it ranks candidates per role). The machine only enforces the rules no roster
//! may bend: the harden reviewer comes from a different model family than every executor that touched the change, and
//! nothing learned may skip Tier 1.
//!
//! Collaborators that stay in Python for now sit behind traits and are optional: [`Research`] (the evidence-checked
//! brief), [`Synthesis`] (the cross-file consistency check after parallel steps). Without them a run simply has no
//! research phase / no synthesis problems.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cordata_ledger::Ledger;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::audit::{self, AuditEnv, AuditReport, Gap};
use crate::checkpoints::Checkpoints;
use crate::dispatch::{self, Candidate, Outcome};
use crate::executor::{self, Config as ExecConfig, ExecResult, Runnable, StepExecutor};
use crate::handoff::{self, HandoffPacket};
use crate::plan::{self, Options, Plan, PlanError, Step};
use crate::secrets;
use crate::worker::{EventSink, TaskPacket, WorkerEvent, WorkerResult};

pub const STATES: [&str; 12] = ["research", "plan", "execute", "synthesize", "harden", "audit", "conformance", "done", "failed", "paused", "budget_exhausted", "cancelled"];
pub const TERMINAL: [&str; 4] = ["done", "failed", "budget_exhausted", "cancelled"];

pub const PLAN_BRIEF: &str = "Plan the work below for this project. Steps must be small and ordered; each lists exactly the files it may create or change (file_scope, project-relative). Acceptance criteria must be checkable without judgement: a command or test with its expected exit code, a file that must exist or must not change, or 'path::regex'. Prefer the project's own test command as a criterion.";

/// The draft schema a planner answers against.
pub fn draft_schema() -> Value {
    json!({"type": "object", "required": ["goal", "acceptance_criteria", "steps"], "additionalProperties": true, "properties": {
        "goal": {"type": "string", "minLength": 1},
        "acceptance_criteria": {"type": "array", "minItems": 1, "maxItems": plan::MAX_CRITERIA, "items": {"type": "object", "required": ["id", "type", "description"], "properties": {
            "id": {"type": "string"}, "type": {"enum": plan::CRITERION_TYPES}, "description": {"type": "string"}, "target": {"type": "string"},
            "expected_exit_code": {"type": "integer"}, "timeout_s": {"type": "number"}}}},
        "steps": {"type": "array", "minItems": 1, "maxItems": plan::MAX_STEPS, "items": {"type": "object", "required": ["step_id", "name", "description", "file_scope"], "properties": {
            "step_id": {"type": "string"}, "name": {"type": "string"}, "description": {"type": "string"}, "file_scope": {"type": "array", "items": {"type": "string"}},
            "risk": {"enum": plan::RISKS}, "rollback_policy": {"enum": plan::ROLLBACK}, "grounded_in": {"type": "array", "items": {"type": "string"}},
            "complexity": {"enum": plan::COMPLEXITIES}, "depends_on": {"type": "array", "items": {"type": "string"}}}}},
        "verifier_commands": {"type": "array", "items": {"type": "string"}}, "assumptions": {"type": "array", "items": {"type": "string"}}}})
}

// ---------------------------------------------------------------------------------------------- the record

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfig {
    pub max_usd: f64,
    pub max_tokens: Option<i64>,
    pub max_drift_retries: i64,
    pub max_replans: i64,
    /// How many times a step that failed every attempt is tried again — strongest worker first, told to work in small
    /// pieces — before the run gives up on the plan. Completed steps are kept.
    pub max_step_recoveries: i64,
    pub harden: bool,
    pub tier2: bool,
    pub step_timeout_s: f64,
    pub plan_timeout_s: f64,
    pub research: bool,
    pub conformance: bool,
    pub max_rounds: i64,
    pub max_parallel: usize,
    pub isolate_waves: bool,
    pub max_synth_fixes: i64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            max_usd: 2.0,
            max_tokens: None,
            max_drift_retries: 2,
            max_replans: 1,
            max_step_recoveries: 2,
            harden: true,
            tier2: true,
            step_timeout_s: 1800.0,
            plan_timeout_s: 600.0,
            research: false,
            conformance: true,
            max_rounds: 3,
            max_parallel: 3,
            isolate_waves: true,
            max_synth_fixes: 1,
        }
    }
}

fn d_state() -> String {
    "plan".into()
}
fn d_class() -> String {
    "general".into()
}
fn d_workers() -> HashMap<String, Vec<String>> {
    ["research", "plan", "execute", "harden", "audit", "conformance"].iter().map(|r| ((*r).to_owned(), Vec::new())).collect()
}
fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Persisted in `orch_runs.record_json` — the same document the Python machine writes, so either can resume the other's
/// run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub goal: String,
    pub cwd: String,
    #[serde(default = "d_state")]
    pub state: String,
    #[serde(default = "d_class")]
    pub task_class: String,
    #[serde(default)]
    pub plan: Option<Value>,
    #[serde(default)]
    pub start_checkpoint: Option<String>,
    #[serde(default)]
    pub drift_retries: i64,
    #[serde(default)]
    pub replans: i64,
    #[serde(default)]
    pub recoveries: i64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub audit: Option<Value>,
    #[serde(default = "d_workers")]
    pub workers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub research: Option<Value>,
    #[serde(default)]
    pub planner: Option<String>,
    #[serde(default)]
    pub rounds: i64,
    #[serde(default)]
    pub usage: Vec<Value>,
    #[serde(default)]
    pub handoffs: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub history: Vec<Value>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// The phase a person's pause stopped it in: a resume goes back there, not to EXECUTE.
    #[serde(default)]
    pub resume_state: Option<String>,
    #[serde(default)]
    pub synthesis: Option<Value>,
    #[serde(default)]
    pub synth_fixes: i64,
    /// step id → every attempt at it (worker, status, cost, files, scope escapes).
    #[serde(default)]
    pub step_log: HashMap<String, Vec<Value>>,
    #[serde(default = "now")]
    pub created: f64,
    #[serde(default = "now")]
    pub updated: f64,
}

impl RunRecord {
    pub fn new(run_id: &str, goal: &str, cwd: &str, task_class: &str, session_id: Option<&str>) -> Self {
        Self {
            run_id: run_id.into(),
            goal: goal.into(),
            cwd: cwd.into(),
            state: d_state(),
            task_class: task_class.into(),
            plan: None,
            start_checkpoint: None,
            drift_retries: 0,
            replans: 0,
            recoveries: 0,
            reason: String::new(),
            audit: None,
            workers: d_workers(),
            research: None,
            planner: None,
            rounds: 0,
            usage: Vec::new(),
            handoffs: Vec::new(),
            warnings: Vec::new(),
            history: Vec::new(),
            session_id: session_id.map(Into::into),
            resume_state: None,
            synthesis: None,
            synth_fixes: 0,
            step_log: HashMap::new(),
            created: now(),
            updated: now(),
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("a record serializes")
    }

    pub fn from_json(data: &Value) -> Option<Self> {
        let mut record: Self = serde_json::from_value(data.clone()).ok()?;
        for (role, list) in d_workers() {
            record.workers.entry(role).or_insert(list); // a record from before a role existed
        }
        Some(record)
    }

    fn workers_of(&self, role: &str) -> &[String] {
        self.workers.get(role).map_or(&[], Vec::as_slice)
    }
}

pub fn load(ledger: &Ledger, run_id: &str) -> Option<RunRecord> {
    RunRecord::from_json(&ledger.load_run(run_id)?)
}

pub fn recent(ledger: &Ledger, limit: usize) -> Vec<RunRecord> {
    ledger.recent_runs(limit).iter().filter_map(RunRecord::from_json).collect()
}

/// `<home>/orchestrator/<run_id>/` — where a run's readable artifacts live.
pub fn artifacts_dir(home: &Path, run_id: &str) -> PathBuf {
    home.join("orchestrator").join(run_id)
}

/// What the run did, for a person: the outcome, each step with its worker, the audit, handoffs and warnings, from the
/// record (no model writes it).
pub fn walkthrough(record: &RunRecord) -> String {
    let first = |role: &str, default: &str| record.workers_of(role).first().cloned().unwrap_or_else(|| default.to_owned());
    let (plan_worker, harden_worker, audit_worker) = (first("plan", "planner"), first("harden", "reviewer"), first("audit", "auditor"));
    let mut lines = vec![
        format!("# Walkthrough — {}", record.run_id),
        String::new(),
        format!("**Goal:** {}", record.goal),
        String::new(),
        format!("**Outcome:** {}{}", record.state, if record.reason.is_empty() { String::new() } else { format!(" — {}", record.reason) }),
        String::new(),
        "```mermaid".into(),
        "flowchart TD".into(),
    ];
    if record.research.is_some() {
        let research_worker = record.planner.clone().unwrap_or_else(|| first("research", "researcher"));
        lines.push(format!("    Start([\"Start\"]) --> Research[\"Research ({research_worker})\"]"));
        lines.push(format!("    Research --> Plan[\"Plan ({plan_worker})\"]"));
    } else {
        lines.push(format!("    Start([\"Start\"]) --> Plan[\"Plan ({plan_worker})\"]"));
    }
    let steps: Vec<&Value> = record.plan.as_ref().and_then(|p| p.get("steps")).and_then(Value::as_array).into_iter().flatten().collect();
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let mut prev = "Plan".to_owned();
    for (i, step) in steps.iter().enumerate() {
        let name = text(step, "name").filter(|n| !n.is_empty()).unwrap_or_else(|| format!("Step {}", i + 1)).replace('"', "'");
        let worker = text(step, "worker").filter(|w| !w.is_empty()).unwrap_or_else(|| "executor".into());
        lines.push(format!("    {prev} --> Step_{}[\"{name} ({worker})\"]", i + 1));
        prev = format!("Step_{}", i + 1);
    }
    lines.push(format!("    {prev} --> Harden[\"Harden ({harden_worker})\"]"));
    lines.push(format!("    Harden --> Audit[\"Audit ({audit_worker})\"]"));
    lines.push(format!("    Audit --> End([\"{}\"])", record.state.to_uppercase()));
    lines.push("```".into());

    if record.plan.is_some() {
        lines.extend(["".into(), "## Steps".into()]);
        for step in &steps {
            let attempts = step.get("attempts").and_then(Value::as_i64).unwrap_or(0);
            lines.push(format!(
                "- [{}] **{}** (`{}`) — {}{}",
                text(step, "status").unwrap_or_default(),
                text(step, "name").unwrap_or_default(),
                text(step, "step_id").unwrap_or_default(),
                text(step, "worker").filter(|w| !w.is_empty()).unwrap_or_else(|| "not run".into()),
                if attempts > 1 { format!(", {attempts} attempt(s)") } else { String::new() }
            ));
        }
    }
    if let Some(audit) = record.audit.as_ref().filter(|a| a.as_object().is_some_and(|o| !o.is_empty())) {
        let verdict = text(audit, "verdict").filter(|v| !v.is_empty());
        lines.extend(["".into(), "## Audit".into(), format!("- deterministic checks: {}{}", if audit.get("tier1_ok").and_then(Value::as_bool).unwrap_or(false) { "passed" } else { "FAILED" }, verdict.map_or(String::new(), |v| format!("; reviewer verdict: {v}")))]);
        for c in audit.get("checks").and_then(Value::as_array).into_iter().flatten() {
            let detail = text(c, "detail").map(|d| d.lines().next().unwrap_or("").to_owned()).unwrap_or_default();
            lines.push(format!("- {} ({}): {} — {}", text(c, "id").unwrap_or_default(), text(c, "kind").unwrap_or_default(), if c.get("ok").and_then(Value::as_bool).unwrap_or(false) { "ok" } else { "FAIL" }, detail));
        }
        let changed: Vec<&str> = audit.get("changed").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).take(200).collect();
        if !changed.is_empty() {
            lines.extend(["".into(), "## Changed files".into()]);
            lines.extend(changed.iter().map(|p| format!("- `{p}`")));
        }
    }
    if !record.handoffs.is_empty() || record.drift_retries > 0 || record.replans > 0 {
        lines.extend([
            "".into(),
            "## Recovery".into(),
            format!("- handoffs: {} · fix passes: {} · replans: {}", record.handoffs.len(), record.drift_retries, record.replans),
            "> [!NOTE]".into(),
            format!("> Autonomous self-correction invoked: {} handoff(s), {} drift fix pass(es), {} replan(s).", record.handoffs.len(), record.drift_retries, record.replans),
        ]);
    }
    if !record.usage.is_empty() {
        lines.extend(["".into(), "## Usage".into(), "| role | worker | calls | tokens in | tokens out | cost |".into(), "| --- | --- | ---: | ---: | ---: | ---: |".into()]);
        for row in &record.usage {
            let n = |k: &str| row.get(k).and_then(Value::as_i64).unwrap_or(0);
            lines.push(format!(
                "| {} | `{}` | {} | {} | {} | ${:.4} |",
                text(row, "role").unwrap_or_default(),
                text(row, "worker").unwrap_or_default(),
                n("calls"),
                n("tokens_in"),
                n("tokens_out"),
                row.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0)
            ));
        }
    }
    if !record.warnings.is_empty() {
        lines.extend(["".into(), "## Warnings".into(), "> [!WARNING]".into()]);
        lines.extend(record.warnings.iter().map(|w| format!("> - {w}")));
    }
    let mut artifacts: Vec<&str> = Vec::new();
    if record.research.is_some() {
        artifacts.push("research.md");
    }
    if record.plan.is_some() {
        artifacts.push("plan.md");
    }
    artifacts.push("walkthrough.md");
    lines.extend(["".into(), "## Artifacts".into()]);
    lines.extend(artifacts.iter().map(|a| format!("- `{a}`")));
    lines.join("\n") + "\n"
}

// ---------------------------------------------------------------------------------------------- collaborators

/// Ranks candidates per role (the learned dispatcher, or a fixed order) and learns from outcomes.
pub trait Roster {
    fn rank(&mut self, role: &str, task_class: &str, exclude_model_families: &HashSet<String>) -> Vec<Runnable>;
    fn observe(&mut self, role: &str, task_class: &str, candidate: &Candidate, outcome: &Outcome);
    /// Candidate key → (tier rank, unit price): routes a step by its complexity. Empty keeps list order.
    fn profile(&self) -> HashMap<String, (i64, f64)> {
        HashMap::new()
    }
}

/// A fixed order per role (tests, and the baseline).
#[derive(Default)]
pub struct StaticRoster {
    pub roles: HashMap<String, Vec<Runnable>>,
    pub observed: Vec<(String, String, String, Outcome)>,
    pub profile: HashMap<String, (i64, f64)>,
}

impl Roster for StaticRoster {
    fn rank(&mut self, role: &str, _task_class: &str, exclude: &HashSet<String>) -> Vec<Runnable> {
        self.roles.get(role).into_iter().flatten().filter(|c| !exclude.contains(&c.candidate.model_family)).cloned().collect()
    }
    fn observe(&mut self, role: &str, task_class: &str, candidate: &Candidate, outcome: &Outcome) {
        self.observed.push((role.into(), task_class.into(), candidate.key(), outcome.clone()));
    }
    fn profile(&self) -> HashMap<String, (i64, f64)> {
        self.profile.clone()
    }
}

/// A [`dispatch::Dispatcher`] (the learned policy) plus the workers its candidates run on.
pub struct DispatchRoster<'d> {
    pub dispatcher: &'d mut dyn dispatch::Dispatcher,
    pub workers: HashMap<String, Arc<dyn crate::worker::Worker>>,
    pub profile: HashMap<String, (i64, f64)>,
}

impl Roster for DispatchRoster<'_> {
    fn rank(&mut self, role: &str, task_class: &str, exclude: &HashSet<String>) -> Vec<Runnable> {
        self.dispatcher
            .rank(role, task_class, exclude)
            .into_iter()
            .filter_map(|c| self.workers.get(&c.worker_id).cloned().map(|worker| Runnable { candidate: c, worker }))
            .collect()
    }
    fn observe(&mut self, role: &str, task_class: &str, candidate: &Candidate, outcome: &Outcome) {
        self.dispatcher.observe(role, task_class, candidate, outcome);
    }
    fn profile(&self) -> HashMap<String, (i64, f64)> {
        self.profile.clone()
    }
}

/// The verified research brief (`research.py`): kept in Python until ported.
pub struct Verified {
    pub brief: Value,
    pub refused: bool,
    pub reasons: Vec<String>,
    pub dropped: Vec<String>,
}

pub trait Research {
    fn schema(&self) -> Value;
    fn prompt(&self, goal: &str, feedback: &[String]) -> String;
    fn verify(&self, structured: Option<&Value>, project: &str) -> Verified;
    fn render(&self, brief: &Value) -> String;
}

/// The consistency check on what parallel steps harvested (`synthesizer.check`): each problem rendered as one line.
pub trait Synthesis {
    fn check(&self, project: &str, files: &[String]) -> Vec<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlRequest {
    /// `pause` | `cancel`
    pub kind: String,
    pub by: String,
    /// A cancel that also puts the project back to the plan-start checkpoint.
    pub restore: bool,
}

/// The person's pause/cancel (`control.Controls`), asked at every safe point.
pub trait RunControls: Send + Sync {
    fn pending(&self) -> Option<ControlRequest>;
    fn clear(&self);
}

struct ExecControls(Arc<dyn RunControls>);

impl executor::Controls for ExecControls {
    fn pending(&self) -> Option<String> {
        self.0.pending().map(|r| r.kind)
    }
}

pub type EventListener = Arc<dyn Fn(Value) + Send + Sync>;

// ---------------------------------------------------------------------------------------------- the machine

pub struct Machine<'a> {
    pub record: RunRecord,
    pub plan: Option<Plan>,
    project: String,
    config: RunConfig,
    roster: &'a mut dyn Roster,
    ledger: &'a Ledger,
    checkpoints: &'a dyn Checkpoints,
    audit_env: AuditEnv<'a>,
    research: Option<&'a dyn Research>,
    synthesis: Option<&'a dyn Synthesis>,
    on_event: Option<EventListener>,
    controls: Option<Arc<dyn RunControls>>,
    /// Where artifacts go: `$CORDATA_HOME` or `~/.cordata`.
    home: PathBuf,
    exec_families: HashSet<String>,
    resume_packet: Option<HandoffPacket>,
    last_report: Option<AuditReport>,
    recovery_note: String,
}

/// Everything a machine needs from the world besides its goal.
pub struct World<'a> {
    pub roster: &'a mut dyn Roster,
    pub ledger: &'a Ledger,
    pub checkpoints: &'a dyn Checkpoints,
    pub audit_env: AuditEnv<'a>,
    pub research: Option<&'a dyn Research>,
    pub synthesis: Option<&'a dyn Synthesis>,
    pub on_event: Option<EventListener>,
    pub controls: Option<Arc<dyn RunControls>>,
    pub home: PathBuf,
}

fn json_min<T: PartialOrd + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if y < x { y } else { x }),
        (x, None) => x,
        (None, y) => y,
    }
}

impl<'a> Machine<'a> {
    pub fn new(goal: &str, project: &str, config: RunConfig, world: World<'a>, run_id: Option<&str>, task_class: Option<&str>, session_id: Option<&str>) -> Self {
        let project = std::fs::canonicalize(project).map_or_else(|_| project.to_owned(), |p| p.to_string_lossy().into_owned());
        let id = run_id.map_or_else(|| format!("run-{}", short_id()), str::to_owned);
        let class = task_class.map_or_else(|| dispatch::classify(goal, None), str::to_owned);
        let record = RunRecord::new(&id, goal, &project, &class, session_id);
        Self::with_record(record, config, world)
    }

    pub fn with_record(record: RunRecord, config: RunConfig, world: World<'a>) -> Self {
        let plan = record.plan.as_ref().and_then(|p| Plan::from_json(p).ok());
        let project = std::fs::canonicalize(&record.cwd).map_or_else(|_| record.cwd.clone(), |p| p.to_string_lossy().into_owned());
        Self {
            project,
            plan,
            record,
            config,
            roster: world.roster,
            ledger: world.ledger,
            checkpoints: world.checkpoints,
            audit_env: world.audit_env,
            research: world.research,
            synthesis: world.synthesis,
            on_event: world.on_event,
            controls: world.controls,
            home: world.home,
            exec_families: HashSet::new(),
            resume_packet: None,
            last_report: None,
            recovery_note: String::new(),
        }
    }

    // ------------------------------------------------------------------ bookkeeping

    fn persist(&mut self) {
        self.record.updated = now();
        let _ = self.ledger.save_run(&self.record.to_json()); // never fails a run
    }

    fn transition(&mut self, state: &str, detail: Value) {
        self.record.state = state.to_owned();
        let mut entry = serde_json::Map::new();
        entry.insert("ts".into(), json!(now()));
        entry.insert("state".into(), json!(state));
        for (k, v) in detail.as_object().cloned().unwrap_or_default() {
            if !v.is_null() {
                entry.insert(k, v);
            }
        }
        let entry = Value::Object(entry);
        self.record.history.push(entry.clone());
        if let Some(plan) = &self.plan {
            self.record.plan = Some(plan.to_json());
        }
        if TERMINAL.contains(&state) || state == "paused" {
            self.record.usage = self.ledger.run_usage(&self.record.run_id);
        }
        self.persist();
        let mut payload = json!({"run_id": self.record.run_id});
        payload.as_object_mut().expect("object").extend(entry.as_object().cloned().unwrap_or_default());
        let _ = self.ledger.append_event("orchestrator", &payload, Some(&self.record.run_id), Some("orchestrator"));
        self.emit("state", entry);
        self.write_artifacts();
        if matches!(state, "done" | "failed") {
            self.credit(state == "done");
        }
    }

    fn emit(&self, kind: &str, payload: Value) {
        let mut event = json!({"kind": kind, "run_id": self.record.run_id});
        event.as_object_mut().expect("object").extend(payload.as_object().cloned().unwrap_or_default());
        if let Some(listener) = &self.on_event {
            listener(event.clone());
        }
        if !matches!(kind, "worker" | "state" | "usage") {
            // Durable: dispatches, step results, handoffs, rollbacks, waves.
            let scrubbed = secrets::scrub(&event.to_string()).0;
            if let Ok(value) = serde_json::from_str::<Value>(&scrubbed) {
                let _ = self.ledger.append_event("orchestrator_event", &value, Some(&self.record.run_id), Some("orchestrator"));
            }
        }
    }

    fn emit_usage(&self) {
        let rows = self.ledger.run_usage(&self.record.run_id);
        let sum_i = |k: &str| rows.iter().map(|r| r.get(k).and_then(Value::as_i64).unwrap_or(0)).sum::<i64>();
        let cost: f64 = rows.iter().map(|r| r.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0)).sum();
        let envelope = self.ledger.envelope(&self.record.run_id, Some(self.config.max_usd), self.config.max_tokens);
        self.emit(
            "usage",
            json!({"pods": rows, "totals": {"tokens_in": sum_i("tokens_in"), "tokens_out": sum_i("tokens_out"), "cost_usd": (cost * 1e4).round() / 1e4, "calls": sum_i("calls")},
                   "pools": self.ledger.states(), "envelope": envelope}),
        );
    }

    fn pool_id(&self, c: &Runnable) -> String {
        self.ledger.pool_for(&c.candidate.family, c.candidate.model.as_deref()).pool_id
    }

    fn level(&self, c: &Runnable) -> &'static str {
        self.ledger.level(&c.candidate.family, c.candidate.model.as_deref())
    }

    fn usable(&self, ranked: Vec<Runnable>) -> Vec<Runnable> {
        ranked.into_iter().filter(|c| matches!(self.level(c), "ok" | "soft")).collect()
    }

    fn rank(&mut self, role: &str) -> Vec<Runnable> {
        self.roster.rank(role, &self.record.task_class.clone(), &HashSet::new())
    }

    fn observe(&mut self, role: &str, candidate: &Candidate, outcome: Outcome) {
        let class = self.record.task_class.clone();
        self.roster.observe(role, &class, candidate, &outcome);
    }

    fn envelope_left(&self) -> (Option<f64>, Option<i64>, bool) {
        let env = self.ledger.envelope(&self.record.run_id, Some(self.config.max_usd), self.config.max_tokens);
        (env.remaining_usd, env.remaining_tokens, env.exhausted)
    }

    /// Rewards at the end of a run. Every role that touched the change earns the run's verdict as a continuous value
    /// (`rlaif.role_reward`): +1 for a pass, -0.25 for a failure whose deterministic checks held, -1 for a broken build.
    /// Paused and budget stops credit no one. (The Python machine additionally scores each executor per step from the
    /// attempts' scope, first-pass and cost — `rlaif.attribute` — which is not ported.)
    fn credit(&mut self, ok: bool) {
        let tier1_ok = self.record.audit.as_ref().and_then(|a| a.get("tier1_ok")).and_then(Value::as_bool).unwrap_or(ok);
        let reward = if ok { 1.0 } else if tier1_ok { -0.25 } else { -1.0 };
        let mut by_role: Vec<(String, Vec<String>)> = Vec::new();
        for role in ["research", "plan", "execute", "harden", "audit", "conformance"] {
            let observed = if matches!(role, "research" | "conformance") { "plan" } else { role };
            let keys = self.record.workers_of(role).to_vec();
            match by_role.iter_mut().find(|(r, _)| r == observed) {
                Some((_, existing)) => existing.extend(keys),
                None => by_role.push((observed.to_owned(), keys)),
            }
        }
        for (role, keys) in by_role {
            let mut used: Vec<String> = Vec::new();
            for k in keys {
                if !used.contains(&k) {
                    used.push(k);
                }
            }
            if used.is_empty() {
                continue;
            }
            let ranked = self.rank(&role);
            for key in used {
                if let Some(r) = ranked.iter().find(|r| r.key() == key) {
                    self.observe(&role, &r.candidate.clone(), Outcome { ok, reward: Some(reward), final_: true, ..Default::default() });
                }
            }
        }
    }

    fn write_artifacts(&self) {
        use std::os::unix::fs::PermissionsExt;
        let target = artifacts_dir(&self.home, &self.record.run_id);
        let private = |p: &Path, mode: u32| {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode));
        };
        if std::fs::create_dir_all(&target).is_err() {
            return;
        }
        private(&target, 0o700);
        let mut files: Vec<(&str, String)> = Vec::new();
        if let (Some(brief), Some(research)) = (&self.record.research, self.research) {
            files.push(("research.md", research.render(brief)));
        }
        if let Some(plan) = self.record.plan.as_ref().and_then(|p| Plan::from_json(p).ok()) {
            files.push(("plan.md", plan.render()));
        }
        files.push(("walkthrough.md", walkthrough(&self.record)));
        for (name, text) in files {
            if std::fs::write(target.join(name), text).is_ok() {
                private(&target.join(name), 0o600);
            }
        }
    }

    /// One worker call. `role` is the worker's role (what its sandbox allows); `label` is the machine's phase — the task
    /// id's middle part, the ledger's role and the record's `workers` key.
    async fn one_shot(&mut self, role: &str, candidate: &Runnable, prompt: &str, schema: Option<Value>, scope: Vec<String>, timeout: f64, label: &str) -> WorkerResult {
        let (usd, tokens, _) = self.envelope_left();
        let allowance = self.ledger.step_allowance(&candidate.candidate.family, candidate.candidate.model.as_deref());
        let mut task = TaskPacket::new(&format!("{}:{label}:{}", self.record.run_id, self.record.history.len()), role, prompt, &self.project);
        task.file_scope = scope;
        task.budget_usd = json_min(usd, if let cordata_ledger::Allowance::Usd(v) = allowance { Some(v) } else { None });
        task.budget_tokens = json_min(tokens, if let cordata_ledger::Allowance::Tokens(v) = allowance { Some(v) } else { None });
        task.timeout_s = timeout;
        task.model = candidate.candidate.model.clone();
        task.effort = candidate.candidate.effort.clone();
        task.json_schema = schema;
        let pool = self.pool_id(candidate);
        self.emit("dispatch", json!({"role": label, "worker": candidate.key(), "pool_id": pool}));
        let sink: EventSink = {
            let listener = self.on_event.clone();
            let (run_id, label, key, pool) = (self.record.run_id.clone(), label.to_owned(), candidate.key(), pool.clone());
            Arc::new(move |event: WorkerEvent| {
                if event.event_type == "done" {
                    return;
                }
                let payload = serde_json::from_str::<Value>(&secrets::scrub(&event.payload.to_string()).0).unwrap_or(json!({}));
                if let Some(l) = &listener {
                    l(json!({"kind": "worker", "run_id": run_id, "step_id": label, "role": label, "worker": key, "pool_id": pool, "event_type": event.event_type, "payload": payload}));
                }
            })
        };
        let result = candidate.worker.run(task, sink).await;
        let model = candidate.candidate.model.clone().or_else(|| result.model.clone());
        self.ledger.record_result(&self.record.run_id, Some(label), &candidate.candidate.family, model.as_deref(), &result.ledger_outcome(), Some(label), Some(&candidate.key()));
        self.record.workers.entry(label.to_owned()).or_default().push(candidate.key());
        self.emit_usage();
        result
    }

    // ------------------------------------------------------------------ phases

    async fn research_phase(&mut self) {
        let Some(research) = self.research else {
            self.record.warnings.push("research skipped: no research backend is configured; planning without a brief".into());
            self.transition("plan", json!({"research": "skipped"}));
            return;
        };
        let mut problems: Vec<String> = Vec::new();
        let ranked = self.rank("plan");
        for candidate in self.usable(ranked) {
            let mut feedback: Vec<String> = Vec::new();
            for attempt in 1..=2 {
                let prompt = research.prompt(&self.record.goal, &feedback);
                let timeout = self.config.plan_timeout_s;
                let result = self.one_shot("plan", &candidate, &prompt, Some(research.schema()), Vec::new(), timeout, "research").await;
                if result.status != "completed" {
                    problems.push(format!("{}: {}", candidate.key(), result.error_message.clone().unwrap_or_else(|| result.status.clone())));
                    break;
                }
                let verified = research.verify(result.structured.as_ref(), &self.project);
                if !verified.refused {
                    let findings = verified.brief.get("findings").and_then(Value::as_array).map_or(0, Vec::len);
                    if !verified.dropped.is_empty() {
                        let names: Vec<String> = verified.dropped.iter().take(10).map(|d| if d.is_empty() { "?".into() } else { d.clone() }).collect();
                        self.record.warnings.push(format!("research: {} finding(s) without verifiable evidence dropped ({})", verified.dropped.len(), names.join(", ")));
                    }
                    self.record.research = Some(verified.brief);
                    self.record.planner = Some(candidate.key());
                    self.transition("plan", json!({"researcher": candidate.key(), "findings": findings, "dropped": (!verified.dropped.is_empty()).then_some(verified.dropped.len())}));
                    return;
                }
                if attempt == 2 {
                    self.observe("plan", &candidate.candidate, Outcome::default());
                    self.fail(&format!("the research brief by {} was refused twice: {}", candidate.key(), verified.reasons.iter().take(5).cloned().collect::<Vec<_>>().join("; ")));
                    return;
                }
                feedback = verified.reasons.into_iter().take(20).collect();
            }
            self.observe("plan", &candidate.candidate, Outcome::default()); // only a failed call gets here: try the next planner
        }
        let tail = if problems.is_empty() { String::new() } else { format!(" ({})", problems.iter().take(3).cloned().collect::<Vec<_>>().join("; ")) };
        self.record.warnings.push(format!("research skipped: no usable planner produced a brief{tail}; planning without one"));
        self.transition("plan", json!({"research": "skipped"}));
    }

    async fn plan_phase(&mut self, feedback: &[String]) -> bool {
        let mut prompt = format!("{PLAN_BRIEF}\n\nThe work:\n{}", self.record.goal);
        let mut findings: Option<HashSet<String>> = None;
        if let Some(brief) = self.record.research.clone() {
            findings = Some(brief.get("findings").and_then(Value::as_array).into_iter().flatten().filter_map(|f| f.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)).collect());
            let rendered = self.research.map(|r| r.render(&brief)).unwrap_or_default();
            prompt.push_str(&format!("\n\nA verified research brief of this project follows. Ground every step in it: list the ids of the findings each step relies on in its grounded_in field, and cite only these ids.\n\n{rendered}"));
        }
        if !feedback.is_empty() {
            prompt.push_str(&format!("\n\nA previous plan did not work out. Plan again, taking this into account:\n- {}", feedback.iter().take(30).cloned().collect::<Vec<_>>().join("\n- ")));
        }
        let mut problems: Vec<String> = Vec::new();
        let ranked = self.rank("plan");
        let mut planners = self.usable(ranked);
        if let Some(preferred) = self.record.planner.clone() {
            planners.sort_by_key(|c| c.key() != preferred); // the worker that researched the project plans it, when it still can
        }
        for candidate in planners.into_iter().take(2) {
            let timeout = self.config.plan_timeout_s;
            let result = self.one_shot("plan", &candidate, &prompt, Some(draft_schema()), Vec::new(), timeout, "plan").await;
            if result.status != "completed" {
                problems.push(format!("{}: {}", candidate.key(), result.error_message.clone().unwrap_or_else(|| result.status.clone())));
                self.observe("plan", &candidate.candidate, Outcome::default());
                continue;
            }
            let draft = result.structured.clone().unwrap_or(Value::Null);
            let options = Options { max_usd: self.config.max_usd, max_tokens: self.config.max_tokens, plan_id: None, version: self.record.replans + 1 };
            match plan::from_draft(&draft, &self.project, options, findings.as_ref()) {
                Ok(plan) => {
                    let (id, steps) = (plan.plan_id.clone(), plan.steps.len());
                    self.plan = Some(plan);
                    self.transition("execute", json!({"plan_id": id, "steps": steps, "planner": candidate.key()}));
                    return true;
                }
                Err(PlanError { problems: refusals }) => {
                    problems.push(format!("{}: the plan was refused: {}", candidate.key(), refusals.join("; ")));
                    self.observe("plan", &candidate.candidate, Outcome::default());
                    prompt.push_str(&format!("\n\nYour previous answer was refused:\n- {}", refusals.iter().take(20).cloned().collect::<Vec<_>>().join("\n- ")));
                }
            }
        }
        self.record.reason = format!("no usable plan: {}", if problems.is_empty() { "no planner has quota left".to_owned() } else { problems.join("; ") });
        false
    }

    async fn execute_phase(&mut self, extra_prompt: &str, steps: Option<Vec<Step>>) -> ExecResult {
        let base = self.plan.clone().expect("a plan exists");
        let fix_pass = steps.is_some();
        let plan = match steps {
            Some(steps) => Plan { steps, ..base.clone() }, // a fix pass runs its own synthetic step against the same goal and criteria
            None => base.clone(),
        };
        let start = self.resume_packet.take();
        let recovering = !self.recovery_note.is_empty() && !fix_pass;
        let ranked = self.rank("execute");
        let usable = self.usable(ranked.clone());
        let candidates = if usable.is_empty() { ranked } else { usable };
        let listener = self.on_event.clone();
        let cfg = ExecConfig {
            timeout_s: self.config.step_timeout_s,
            extra_prompt: if extra_prompt.is_empty() && recovering { self.recovery_note.clone() } else { extra_prompt.to_owned() },
            start_packet: start,
            profile: self.roster.profile(),
            max_parallel: self.config.max_parallel,
            // A drift fix is the hard case — the plan was done and was not good enough — so it goes to the strongest
            // worker still allowed, not to whichever pool has the most room.
            strongest_first: recovering || fix_pass,
            isolate: self.config.isolate_waves,
            on_event: listener.map(|l| Box::new(move |e| l(e)) as Box<dyn Fn(Value) + Send + Sync>),
            controls: self.controls.clone().map(|c| Arc::new(ExecControls(c)) as Arc<dyn executor::Controls>),
            ..ExecConfig::default()
        };
        let executor = StepExecutor::new(&self.record.run_id, Path::new(&self.project), plan, candidates, self.ledger, self.checkpoints, cfg);
        let result = executor.run().await;
        let ran_plan = executor.plan.lock().unwrap().clone();
        let used_candidates = executor.candidates.lock().unwrap().clone();
        drop(executor);
        if recovering && result.status == "completed" {
            self.recovery_note.clear();
        }
        if !fix_pass {
            self.plan = Some(ran_plan);
        }
        self.record.handoffs.extend(result.handoffs.iter().map(|p| p.handoff_id.clone()));
        for (step_id, rows) in &result.results {
            self.record.step_log.entry(step_id.clone()).or_default().extend(rows.iter().cloned());
        }
        for key in &result.workers_used {
            self.record.workers.entry("execute".into()).or_default().push(key.clone());
        }
        for c in &used_candidates {
            if result.workers_used.contains(&c.key()) {
                self.exec_families.insert(c.candidate.model_family.clone());
            }
        }
        self.emit_usage();
        result
    }

    /// The model families that wrote this change — this process's, plus a resumed run's from its record.
    fn executor_families(&mut self) -> HashSet<String> {
        let mut families = self.exec_families.clone();
        let used: HashSet<String> = self.record.workers_of("execute").iter().cloned().collect();
        for c in self.rank("execute") {
            if used.contains(&c.key()) {
                families.insert(c.candidate.model_family);
            }
        }
        families
    }

    async fn harden_phase(&mut self) {
        let plan = self.plan.clone().expect("a plan exists");
        let families = self.executor_families();
        let class = self.record.task_class.clone();
        let ranked = self.roster.rank("harden", &class, &families);
        let ranked: Vec<Runnable> = self.usable(ranked).into_iter().filter(|c| !families.contains(&c.candidate.model_family)).collect();
        let Some(candidate) = ranked.first().cloned() else {
            let mut names: Vec<&String> = families.iter().collect();
            names.sort();
            self.record.warnings.push(format!("harden skipped: no reviewer from a model family other than the executors' ({}) is enabled and has quota", names.into_iter().cloned().collect::<Vec<_>>().join(", ")));
            return;
        };
        let snap = self.checkpoints.take(Path::new(&self.project), &self.record.run_id, "harden", "harden");
        let (diff, stat) = self.record.start_checkpoint.as_deref().map_or((String::new(), String::new()), |id| self.checkpoints.diff_since(Path::new(&self.project), id, None));
        let scope: Vec<String> = plan.scope_union().into_iter().map(str::to_owned).collect();
        let commands: Vec<String> = plan.acceptance_criteria.iter().filter(|c| matches!(c.kind.as_str(), "command" | "test") && !c.target.is_empty()).map(|c| c.target.clone()).chain(plan.verifier_commands.iter().cloned()).collect();
        let findings: Vec<Value> = self.record.research.as_ref().and_then(|r| r.get("findings")).and_then(Value::as_array).into_iter().flatten().filter(|f| f.is_object()).cloned().collect();
        let diff_masked = secrets::scrub(&diff.chars().take(audit::MAX_DIFF_FOR_MODEL).collect::<String>()).0;
        let criteria = plan.acceptance_criteria.iter().map(|c| format!("- {}: {}", c.id, c.description)).collect::<Vec<_>>().join("\n");
        let command_lines = if commands.is_empty() { "   (the plan lists none)".to_owned() } else { commands.iter().map(|c| format!("   - `{c}`")).collect::<Vec<_>>().join("\n") };
        let gap_lines = if findings.is_empty() {
            " (there is none: close gaps against the goal and criteria)".to_owned()
        } else {
            format!(":\n{}", findings.iter().map(|f| format!("   - [{}] {}", f.get("id").and_then(Value::as_str).unwrap_or("None"), f.get("claim").and_then(Value::as_str).unwrap_or("None"))).collect::<Vec<_>>().join("\n"))
        };
        let prompt = format!(
            "Goal: {}\n\nThe change so far ({stat}):\n<<<UNTRUSTED_DIFF>>>\n{diff_masked}\n<<<END_UNTRUSTED_DIFF>>>\n\nReview it for bugs, unhandled edge cases and security problems against the goal and these criteria:\n{criteria}\n\nYour mandate:\n1. Run the plan's acceptance commands:\n{command_lines}\n2. Fix real failures only, and only inside the step scope: {}\n3. Close gaps against the research brief{gap_lines}\n4. Do not widen the scope: no new files or edits outside it, no unrelated refactors.",
            plan.goal,
            if scope.is_empty() { "(none)".to_owned() } else { scope.join(", ") }
        );
        let timeout = self.config.step_timeout_s;
        let result = self.one_shot("harden", &candidate, &prompt, None, scope, timeout, "harden").await;
        let bad = result.status != "completed" || !result.out_of_scope.is_empty();
        if bad && snap.ok {
            if let Some(id) = &snap.id {
                self.checkpoints.restore(Path::new(&self.project), id, None, &self.record.run_id);
            }
        }
        if bad {
            let why = result.error_message.clone().unwrap_or_else(|| format!("wrote outside the scope: {}", result.out_of_scope.join(", ")));
            self.record.warnings.push(format!("harden by {} discarded: {why}", candidate.key()));
        }
    }

    fn conformance_loop(&self) -> bool {
        self.config.conformance && self.record.research.is_some()
    }

    async fn audit_phase(&mut self) -> AuditReport {
        let plan = self.plan.clone().expect("a plan exists");
        let mut reviewer = None;
        if self.config.tier2 && !self.conformance_loop() {
            // in the loop, conformance replaces the Tier-2 reviewer
            let ranked = self.rank("audit");
            reviewer = self.usable(ranked).into_iter().next();
        }
        let start = self.record.start_checkpoint.clone();
        let report = audit::audit(&plan, &self.project, start.as_deref(), reviewer.as_ref(), &self.record.run_id, &self.audit_env, self.config.plan_timeout_s).await;
        if let (Some(result), Some(reviewer)) = (&report.tier2_result, &reviewer) {
            let model = reviewer.candidate.model.clone().or_else(|| result.model.clone());
            self.ledger.record_result(&self.record.run_id, Some("audit"), &reviewer.candidate.family, model.as_deref(), &result.ledger_outcome(), Some("audit"), Some(&reviewer.key()));
            self.record.workers.entry("audit".into()).or_default().push(reviewer.key());
            self.emit_usage();
        }
        self.record.audit = Some(report.to_json());
        self.last_report = Some(report.clone());
        report
    }

    /// The planner that researched and planned the run checks it; when it is gone or out of quota, the cheapest usable
    /// standard-tier planner does (with a warning naming both).
    fn conformance_candidate(&mut self) -> Option<Runnable> {
        let ranked = self.rank("plan");
        let planner = self.record.planner.clone();
        let found = planner.as_ref().and_then(|p| ranked.iter().find(|c| &c.key() == p)).cloned();
        if let Some(p) = found.as_ref().filter(|p| matches!(self.level(p), "ok" | "soft")) {
            return Some(p.clone());
        }
        let profile = self.roster.profile();
        let standard = 1;
        let mut usable: Vec<Runnable> = self.usable(ranked).into_iter().filter(|c| Some(c.key()) != planner && profile.get(&c.key()).map_or(standard, |p| p.0) == standard).collect();
        usable.sort_by(|a, b| profile.get(&a.key()).map_or(f64::INFINITY, |p| p.1).total_cmp(&profile.get(&b.key()).map_or(f64::INFINITY, |p| p.1))); // stable: list order breaks ties
        let why = match &found {
            None => "is not ranked for planning".to_owned(),
            Some(p) => format!("is at pool level {}", self.level(p)),
        };
        let name = planner.unwrap_or_else(|| "(none)".into());
        match usable.into_iter().next() {
            None => {
                self.record.warnings.push(format!("conformance: planner {name} {why} and no usable standard-tier planner can stand in"));
                None
            }
            Some(stand_in) => {
                self.record.warnings.push(format!("conformance: planner {name} {why}; {} checks conformance instead", stand_in.key()));
                Some(stand_in)
            }
        }
    }

    /// Does the change conform to the plan and the brief? `accept` ends the run; `gaps` buys a fix round (then HARDEN →
    /// AUDIT → CONFORMANCE again) up to `max_rounds`; `replan` or too many rounds goes back to PLAN. An unavailable or
    /// unusable check is a warning, and the run is accepted on Tier 1. Returns the replan feedback, if any.
    async fn conformance_phase(&mut self) -> Option<Vec<String>> {
        let plan = self.plan.clone().expect("a plan exists");
        let start = self.record.start_checkpoint.clone();
        let report = match self.last_report.clone() {
            Some(r) => r,
            None => audit::tier1(&plan, &self.project, start.as_deref(), &self.audit_env).await, // a resumed run has no report in memory
        };
        let Some(candidate) = self.conformance_candidate() else {
            self.transition("done", json!({"verdict": "accept(tier1)"}));
            return None;
        };
        let brief_text = self.record.research.as_ref().zip(self.research).map(|(b, r)| r.render(b));
        // The machine's own one-shot call keeps the ledger.
        let prompt = audit::conformance_request(&plan, brief_text.as_deref(), &report, &self.project, start.as_deref(), self.checkpoints);
        let timeout = self.config.plan_timeout_s;
        let called = self.one_shot("plan", &candidate, &prompt, Some(audit::conformance_schema()), Vec::new(), timeout, "conformance").await;
        let (verdict, gaps, result) = audit::conformance_verdict(called);

        let items: Vec<String> = gaps.iter().map(|g: &Gap| format!("{}: {} ({})", g.reference, g.missing, g.severity)).collect();
        if let Some(audit_json) = self.record.audit.as_mut().and_then(Value::as_object_mut) {
            audit_json.insert("conformance".into(), if verdict.is_empty() { Value::Null } else { json!(verdict) });
            audit_json.insert("conformance_worker".into(), json!(candidate.key()));
            audit_json.insert("gaps".into(), json!(gaps));
            if let Some(scores) = result.structured.as_ref().and_then(|s| s.get("step_scores")).and_then(Value::as_array) {
                audit_json.insert("step_scores".into(), json!(scores.iter().filter(|s| s.is_object()).take(100).collect::<Vec<_>>()));
            }
        }
        if verdict.is_empty() {
            let problem = if result.status != "completed" { result.error_message.clone().unwrap_or_else(|| result.status.clone()) } else { "no usable verdict in the answer".to_owned() };
            self.record.warnings.push(format!("conformance check by {} unavailable ({problem}); accepted on the deterministic checks", candidate.key()));
            self.transition("done", json!({"verdict": "accept(tier1)"}));
            return None;
        }
        match verdict.as_str() {
            "accept" => {
                self.transition("done", json!({"verdict": "accept", "conformance": candidate.key()}));
                None
            }
            "replan" => {
                let reasons = if items.is_empty() { vec!["the conformance check asked for a new plan".to_owned()] } else { items };
                self.replan_or_fail(&reasons, "the conformance check asked for a new plan");
                Some(reasons)
            }
            _ => {
                self.record.rounds += 1;
                if self.record.rounds <= self.config.max_rounds {
                    self.transition("execute", json!({"round": self.record.rounds, "gaps": items.iter().take(20).collect::<Vec<_>>()}));
                    self.fix(&items, true).await;
                    return None;
                }
                let why = format!("conformance gaps remained after {} round(s)", self.config.max_rounds);
                self.replan_or_fail(&items, &why);
                Some(items)
            }
        }
    }

    async fn synthesize_phase(&mut self) {
        let plan = self.plan.clone().expect("a plan exists");
        let mut synthesis = self.record.synthesis.clone().unwrap_or_else(|| json!({"harvested": [], "conflicts": [], "problems": []}));
        // The combined change: what the sandboxes handed back and what every other step wrote.
        let mut files: Vec<String> = synthesis.get("harvested").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        for row in self.record.step_log.values().flatten() {
            files.extend(row.get("files_changed").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
        }
        files.sort();
        files.dedup();
        let check = |m: &Self| m.synthesis.map_or_else(Vec::new, |s| s.check(&m.project, &files));
        let mut problems = check(self);
        while !problems.is_empty() && self.record.synth_fixes < self.config.max_synth_fixes {
            self.record.synth_fixes += 1;
            self.emit("synthesis", json!({"problems": problems.iter().take(30).collect::<Vec<_>>(), "fix": self.record.synth_fixes}));
            let step = Step {
                step_id: format!("synth-{}", self.record.synth_fixes),
                name: "Make the parallel steps fit together".into(),
                description: format!("Several steps of this plan were written in parallel, each in isolation. Their combined change has these problems; fix them (signatures, imports, names) without undoing what the steps did:\n- {}", problems.iter().take(30).cloned().collect::<Vec<_>>().join("\n- ")),
                file_scope: plan.scope_union().into_iter().map(str::to_owned).collect(),
                ..synthetic_step()
            };
            let result = self.execute_phase("", Some(vec![step])).await;
            if matches!(result.status.as_str(), "paused" | "budget_exhausted" | "cancelled") {
                self.record.reason = result.reason.clone();
                self.transition(&result.status, json!({"reason": result.reason}));
                return;
            }
            problems = check(self);
        }
        synthesis["problems"] = json!(problems);
        self.record.synthesis = Some(synthesis);
        if !problems.is_empty() {
            self.record.warnings.push(format!("synthesis: {} problem(s) left for harden/audit: {}", problems.len(), problems.iter().take(3).cloned().collect::<Vec<_>>().join("; ")));
        }
        let next = if self.config.harden { "harden" } else { "audit" };
        self.transition(next, json!({"synthesized": files.len(), "problems": problems.len()}));
    }

    /// One drift retry (or conformance round): a single extra step over the whole plan scope, told exactly what the audit
    /// found. A conformance round goes through HARDEN again (when on) before the audit.
    async fn fix(&mut self, items: &[String], from_conformance: bool) {
        let plan = self.plan.clone().expect("a plan exists");
        let (step_id, name, found, after) = if from_conformance {
            (format!("gap-{}", self.record.rounds), "Close the conformance gaps", "The conformance check of the completed plan found these gaps. Close them without undoing the rest:\n- ", if self.config.harden { "harden" } else { "audit" })
        } else {
            (format!("fix-{}", self.record.drift_retries), "Fix what the audit found", "The audit of the completed plan found these problems. Fix them without undoing the rest:\n- ", "audit")
        };
        let step = Step {
            step_id: step_id.clone(),
            name: name.into(),
            description: format!("{found}{}", items.iter().take(30).cloned().collect::<Vec<_>>().join("\n- ")),
            file_scope: plan.scope_union().into_iter().map(str::to_owned).collect(),
            ..synthetic_step()
        };
        let result = self.execute_phase("", Some(vec![step])).await;
        match result.status.as_str() {
            "completed" => self.transition(after, json!({"fix": step_id})),
            "paused" | "budget_exhausted" | "cancelled" => {
                self.record.reason = result.reason.clone();
                self.transition(&result.status, json!({"reason": result.reason}));
            }
            _ => self.transition(after, json!({"fix": step_id, "fix_failed": result.reason})),
        }
    }

    /// Before the project goes back to the start of the run, write what is about to be thrown away to the run's artifacts
    /// folder as a patch. A restore is irreversible by design, and a bad step must not silently cost nine good ones.
    fn keep_discarded(&mut self, why: &str) {
        let Some(start) = self.record.start_checkpoint.clone() else { return };
        let (patch, stat) = self.checkpoints.diff_since(Path::new(&self.project), &start, None);
        if patch.trim().is_empty() {
            return;
        }
        let target = artifacts_dir(&self.home, &self.record.run_id);
        if std::fs::create_dir_all(&target).is_err() {
            return;
        }
        let file = target.join(format!("discarded-{:03}.patch", self.record.history.len()));
        let last = stat.trim().lines().last().unwrap_or("");
        if std::fs::write(&file, format!("# discarded by the orchestrator: {why}\n# {last}\n{patch}")).is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
            self.record.warnings.push(format!("work discarded by a restore ({why}) was saved to {}", file.display()));
        }
    }

    fn restore_start(&self) -> Option<bool> {
        let start = self.record.start_checkpoint.as_deref()?;
        Some(self.checkpoints.restore(Path::new(&self.project), start, None, &self.record.run_id).ok)
    }

    fn replan_or_fail(&mut self, reasons: &[String], why: &str) {
        self.record.rounds = 0;
        if self.record.replans < self.config.max_replans {
            self.record.replans += 1;
            self.record.drift_retries = 0;
            self.keep_discarded(why);
            self.restore_start();
            self.plan = None;
            self.record.plan = None;
            self.exec_families.clear();
            self.record.workers.insert("execute".into(), Vec::new());
            self.transition("plan", json!({"replan": self.record.replans, "why": why, "reasons": reasons.iter().take(20).collect::<Vec<_>>()}));
        } else {
            self.fail(&format!("{why}: {}", reasons.iter().take(5).cloned().collect::<Vec<_>>().join("; ")));
        }
    }

    fn fail(&mut self, reason: &str) {
        self.record.reason = reason.to_owned();
        self.keep_discarded(&reason.chars().take(120).collect::<String>());
        let restored = self.restore_start();
        self.transition("failed", json!({"reason": reason, "restored": restored}));
    }

    // ------------------------------------------------------------------ the person's controls

    /// At the top of the loop, between phases: honour a pause or a cancel. True when the loop must stop.
    fn safe_point(&mut self) -> bool {
        let Some(req) = self.controls.as_ref().and_then(|c| c.pending()) else { return false };
        if req.kind == "pause" {
            self.record.resume_state = Some(self.record.state.clone());
            self.record.reason = "paused by the person".into();
            self.emit("paused", json!({"reason": "user_pause", "state": self.record.state}));
            let (resume, by) = (self.record.resume_state.clone(), req.by.clone());
            self.transition("paused", json!({"reason": "user_pause", "resume": resume, "by": (!by.is_empty()).then_some(by)}));
            if let Some(c) = &self.controls {
                c.clear();
            }
            return true;
        }
        self.cancel(&req);
        true
    }

    /// A person's cancel: terminal. The work stays unless they asked for it to go (`restore`), and then it is saved as a
    /// patch first, exactly as a failed run's is.
    fn cancel(&mut self, req: &ControlRequest) {
        self.record.reason = "cancelled by the person".into();
        let mut restored = None;
        if req.restore && self.record.start_checkpoint.is_some() {
            self.keep_discarded("cancelled with restore");
            restored = self.restore_start();
        }
        let reason = self.record.reason.clone();
        self.transition("cancelled", json!({"reason": reason, "restored": restored, "by": (!req.by.is_empty()).then(|| req.by.clone())}));
        if let Some(c) = &self.controls {
            c.clear();
        }
    }

    // ------------------------------------------------------------------ the loop

    pub async fn run(&mut self) -> &RunRecord {
        if self.record.start_checkpoint.is_none() {
            let snap = self.checkpoints.take(Path::new(&self.project), &self.record.run_id, "plan-start", &self.record.goal.chars().take(80).collect::<String>());
            self.record.start_checkpoint = if snap.ok { snap.id.clone() } else { None };
            if !snap.ok {
                self.record.warnings.push(format!("no plan-start checkpoint ({}); a failed run cannot be rolled back", snap.reason));
            }
            let first = if self.plan.is_some() {
                self.record.state.clone()
            } else if (self.record.history.is_empty() && self.config.research) || self.record.state == "research" {
                "research".to_owned() // a fresh run researches first when asked to; an interrupted one picks it back up
            } else {
                "plan".to_owned()
            };
            self.transition(&first, json!({}));
        }
        if self.record.state == "paused" {
            self.resume_packet = handoff::for_session(self.ledger, &self.record.run_id).pop();
            self.record.reason.clear();
            // A person's pause between phases goes back to that phase; a pause inside EXECUTE (quota, or a pause at a step)
            // goes back to EXECUTE, which skips the steps already done.
            let back_to = self.record.resume_state.take().unwrap_or_else(|| "execute".into());
            let packet = self.resume_packet.as_ref().map(|p| p.handoff_id.clone());
            self.transition(&back_to, json!({"resumed": true, "handoff": packet}));
        }
        self.run_loop().await;
        &self.record
    }

    async fn run_loop(&mut self) {
        let mut feedback: Vec<String> = Vec::new();
        while !TERMINAL.contains(&self.record.state.as_str()) && self.record.state != "paused" {
            if self.safe_point() {
                break;
            }
            let state = self.record.state.clone();
            if self.envelope_left().2 {
                self.record.reason = "the plan's budget envelope is spent".into();
                self.transition("budget_exhausted", json!({}));
                break;
            }
            match state.as_str() {
                "research" => self.research_phase().await,
                "plan" => {
                    if !self.plan_phase(&feedback).await {
                        let reason = self.record.reason.clone();
                        self.fail(&reason);
                    }
                }
                "execute" => self.execute_state(&mut feedback).await,
                "synthesize" => self.synthesize_phase().await,
                "harden" => {
                    self.harden_phase().await;
                    self.transition("audit", json!({}));
                }
                "audit" => self.audit_state(&mut feedback).await,
                "conformance" => {
                    if let Some(reasons) = self.conformance_phase().await {
                        feedback = reasons;
                    }
                }
                _ => break,
            }
        }
    }

    async fn execute_state(&mut self, feedback: &mut Vec<String>) {
        let result = self.execute_phase("", None).await;
        let cancel_now = self.controls.as_ref().and_then(|c| c.pending());
        if result.status == "completed" && !result.harvested.is_empty() {
            let mut harvested = result.harvested.clone();
            harvested.sort();
            harvested.dedup();
            let mut conflicts = result.conflicts.clone();
            conflicts.sort();
            conflicts.dedup();
            let count = harvested.len();
            self.record.synthesis = Some(json!({"harvested": harvested, "conflicts": conflicts, "problems": []}));
            self.transition("synthesize", json!({"harvested": count}));
        } else if result.status == "completed" {
            let next = if self.config.harden { "harden" } else { "audit" };
            self.transition(next, json!({}));
        } else if let (true, Some(req)) = (result.status == "cancelled", cancel_now) {
            self.cancel(&req);
        } else if matches!(result.status.as_str(), "paused" | "budget_exhausted" | "cancelled") {
            self.record.reason = result.reason.clone();
            if result.status == "paused" && result.reason == "paused by the person" {
                self.record.resume_state = Some("execute".into());
                if let Some(c) = &self.controls {
                    c.clear();
                }
            }
            let packet = result.last_packet.as_ref().map(|p| p.handoff_id.clone());
            self.transition(&result.status, json!({"reason": result.reason, "handoff": packet}));
        } else if self.plan.as_ref().is_some_and(|p| p.steps.iter().any(|s| s.status == "completed")) && self.record.recoveries < self.config.max_step_recoveries {
            // A step defeated every attempt, but the steps before it are done and good. Keep them: try the failed step
            // again with the strongest worker allowed, rather than restoring the whole project.
            self.record.recoveries += 1;
            let failed = self.plan.as_ref().and_then(|p| p.steps.iter().find(|s| s.status == "failed")).map(|s| s.step_id.clone());
            self.recovery_note = format!(
                "Earlier attempts at this step failed: {}\nWork in several SMALL edits instead of one large one — a long file written in a single reply is what ran out of room before; create it in pieces. Shell commands may be refused here: do not spend your turn on them.",
                result.reason.chars().take(400).collect::<String>()
            );
            self.transition("execute", json!({"recovery": self.record.recoveries, "step": failed, "reason": result.reason.chars().take(300).collect::<String>()}));
        } else {
            self.replan_or_fail(std::slice::from_ref(&result.reason), "execution failed");
            *feedback = vec![result.reason];
        }
    }

    async fn audit_state(&mut self, feedback: &mut Vec<String>) {
        let report = self.audit_phase().await;
        if report.tier1_ok && self.conformance_loop() {
            self.transition("conformance", json!({"checks": report.checks.len()}));
            return;
        }
        let accepted = report.tier1_ok && (report.verdict == "accept" || ((!report.tier2_used || report.tier2_error.is_some()) && !report.ambiguous));
        if report.tier1_ok && report.tier2_used && report.tier2_error.is_some() {
            self.record.warnings.push(format!("tier 2 audit unavailable ({}); accepted on the deterministic checks", report.tier2_error.clone().unwrap_or_default()));
        }
        if accepted {
            let verdict = if report.verdict.is_empty() { "accept(tier1)".to_owned() } else { report.verdict.clone() };
            self.transition("done", json!({"verdict": verdict}));
        } else if report.tier1_ok && report.verdict == "replan" {
            self.replan_or_fail(&report.items, "the auditor asked for a new plan");
            *feedback = report.items.clone();
        } else {
            let items: Vec<String> = report.failures().into_iter().chain(report.items.iter().cloned()).chain(report.warnings.iter().cloned()).collect();
            if !report.tier1_ok || report.verdict == "drift" || report.ambiguous {
                if self.record.drift_retries < self.config.max_drift_retries {
                    self.record.drift_retries += 1;
                    self.transition("execute", json!({"drift_retry": self.record.drift_retries, "items": items.iter().take(20).collect::<Vec<_>>()}));
                    self.fix(&items, false).await;
                    return;
                }
                self.replan_or_fail(&items, "drift persisted after the retries");
                *feedback = items;
            } else {
                let verdict = if report.verdict.is_empty() { "''".to_owned() } else { format!("'{}'", report.verdict) };
                self.fail(&format!("the audit gave no usable verdict ({verdict})"));
            }
        }
    }
}

fn synthetic_step() -> Step {
    Step {
        step_id: String::new(),
        name: String::new(),
        description: String::new(),
        file_scope: Vec::new(),
        risk: "edits_files".into(),
        checkpoint_hash: None,
        rollback_policy: "rollback_checkpoint".into(),
        status: "pending".into(),
        worker: None,
        attempts: 0,
        grounded_in: Vec::new(),
        complexity: "medium".into(),
        depends_on: Vec::new(),
    }
}

fn short_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    format!("{:010x}", hasher.finish() & 0xff_ffff_ffff)
}

/// Continue a paused (or interrupted, non-terminal) run from `state.db`. A terminal or unknown run comes back as it is.
pub async fn resume(run_id: &str, config: RunConfig, world: World<'_>) -> Option<RunRecord> {
    let record = load(world.ledger, run_id)?;
    if TERMINAL.contains(&record.state.as_str()) {
        return Some(record);
    }
    let mut machine = Machine::with_record(record, config, world);
    machine.run().await;
    Some(machine.record)
}
