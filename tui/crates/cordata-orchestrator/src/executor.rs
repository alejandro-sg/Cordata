//! The EXECUTE phase: run a plan's steps in order, each inside a
//! checkpoint, handing off at safe points.
//!
//! For each pending step: **checkpoint** (a failed snapshot degrades the rollback policy to `keep_partial` — never
//! claim a rollback that cannot be done); **pick a worker** (`ok` pools before `soft`, never `hard`/`exhausted`; the
//! plan's envelope is checked first); **bound the step** (the smaller of what the envelope has left and what the pool
//! has before its hard line); **read the result** — completed → done; `quota_limit` → apply the rollback policy, build
//! and save a [`HandoffPacket`], resume the same step on the next pool; failed or out-of-scope → roll back, try another
//! candidate, fail the phase after `max_attempts`.
//!
//! A plan with `depends_on` runs in **waves**: pending steps whose dependencies are done, greedily taken in plan order
//! while their scopes are pairwise disjoint. With an [`Isolation`] a step in a multi-step wave runs in a sandbox of its
//! own; without one the wave shares the tree and a step rolls back only its own scope.
//!
//! Futures here are not `Send` (they borrow the SQLite-backed ledger): drive them on a current-thread runtime.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cordata_ledger::{Allowance, Ledger};
use serde_json::{Value, json};

use crate::checkpoints::Checkpoints;
use crate::dispatch::Candidate;
use crate::handoff::{self, Build, HandoffPacket};
use crate::plan::{Plan, Step};
use crate::scope;
use crate::worker::{EventSink, TaskPacket, Worker, WorkerResult};

pub const MAX_ATTEMPTS: usize = 3;

/// A candidate the policy ranked plus the worker that runs it.
#[derive(Clone)]
pub struct Runnable {
    pub candidate: Candidate,
    pub worker: Arc<dyn Worker>,
}

impl Runnable {
    pub fn key(&self) -> String {
        self.candidate.key()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    /// `completed` | `failed` | `paused` | `budget_exhausted` | `cancelled`
    pub status: String,
    pub reason: String,
    pub handoffs: Vec<HandoffPacket>,
    pub results: BTreeMap<String, Vec<Value>>,
    pub last_packet: Option<HandoffPacket>,
    pub workers_used: Vec<String>,
    /// Files copied back from isolated steps' sandboxes, and harvest conflicts seen on the way.
    pub harvested: Vec<String>,
    pub conflicts: Vec<String>,
}

/// The person's pause/cancel (`control.Controls`): asked before each worker call and between waves.
pub trait Controls: Send + Sync {
    /// `"pause"` or `"cancel"` when one was requested.
    fn pending(&self) -> Option<String>;
}

#[derive(Debug, Clone)]
pub struct SandboxHandle {
    pub root: PathBuf,
    pub id: String,
}

#[derive(Debug, Clone, Default)]
pub struct Harvest {
    pub written: Vec<String>,
    pub deleted: Vec<String>,
    pub conflicts: Vec<String>,
    pub skipped: Vec<String>,
}

/// Per-step copy-on-write sandboxes (`synthesizer.py`). `create` failing means "run this step in the shared tree".
pub trait Isolation: Send + Sync {
    fn create(&self, project: &Path, label: &str, scope: &[String]) -> Result<SandboxHandle, String>;
    fn harvest(&self, sandbox: &SandboxHandle, changed: &[String], scope: &[String]) -> Harvest;
    fn discard(&self, sandbox: &SandboxHandle);
}

type Rerank = Box<dyn Fn(&Step, &[Runnable]) -> Vec<Runnable> + Send + Sync>;
type Listener = Box<dyn Fn(Value) + Send + Sync>;

pub struct Config {
    pub timeout_s: f64,
    pub extra_prompt: String,
    pub start_packet: Option<HandoffPacket>,
    pub max_attempts: usize,
    /// candidate key → (tier rank, unit price); routes high/low-complexity steps.
    pub profile: HashMap<String, (i64, f64)>,
    pub max_parallel: usize,
    /// A recovery pass: take candidates in the dispatcher's order whatever their pool's level (`ok` or `soft`).
    pub strongest_first: bool,
    pub isolate: bool,
    pub on_event: Option<Listener>,
    pub on_usage: Option<Listener>,
    pub controls: Option<Arc<dyn Controls>>,
    pub isolation: Option<Arc<dyn Isolation>>,
    /// Per-step routing: may only reorder the list it is given.
    pub rerank: Option<Rerank>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout_s: 1800.0,
            extra_prompt: String::new(),
            start_packet: None,
            max_attempts: MAX_ATTEMPTS,
            profile: HashMap::new(),
            max_parallel: 3,
            strongest_first: false,
            isolate: true,
            on_event: None,
            on_usage: None,
            controls: None,
            isolation: None,
            rerank: None,
        }
    }
}

/// The mutable state one step's attempts share: who ran last, the note it carries, and the retry rotation.
struct StepCtx {
    candidates: Vec<Runnable>,
    current: Option<Runnable>,
    packet: Option<HandoffPacket>,
    scoped: bool,
    /// Every other step's scope in the same wave: their files are theirs, not stray writes of this step.
    sibling_scope: Vec<String>,
    reason: String,
    last_packet: Option<HandoffPacket>,
    isolated: bool,
    sandbox: Option<SandboxHandle>,
}

impl StepCtx {
    fn new(candidates: Vec<Runnable>, current: Option<Runnable>, packet: Option<HandoffPacket>) -> Self {
        Self { candidates, current, packet, scoped: false, sibling_scope: Vec::new(), reason: String::new(), last_packet: None, isolated: false, sandbox: None }
    }
}

pub fn step_prompt(plan: &Plan, index: usize, extra: &str) -> String {
    let step = &plan.steps[index];
    let listed = |steps: &[Step], only_done: bool| -> Vec<String> {
        steps.iter().filter(|s| !only_done || matches!(s.status.as_str(), "completed" | "partial")).map(|s| format!("- {}: {}", s.step_id, s.name)).collect()
    };
    let (done, later) = (listed(&plan.steps[..index], true), listed(&plan.steps[index + 1..], false));
    let criteria: Vec<String> = plan
        .acceptance_criteria
        .iter()
        .map(|c| format!("- {} ({}{}): {}", c.id, c.kind, if c.target.is_empty() { String::new() } else { format!(": {}", c.target) }, c.description))
        .collect();
    let parts = [
        format!("Goal of the whole plan: {}", plan.goal),
        format!("You are doing step {} of {}: {} (id {}).\n{}", index + 1, plan.steps.len(), step.name, step.step_id, step.description),
        if done.is_empty() { String::new() } else { format!("Already done:\n{}", done.join("\n")) },
        if later.is_empty() { String::new() } else { format!("Later steps — do NOT do these now:\n{}", later.join("\n")) },
        format!("The plan will be accepted when:\n{}", criteria.join("\n")),
        extra.to_owned(),
    ];
    parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n")
}

pub struct StepExecutor<'a> {
    run_id: String,
    project: PathBuf,
    pub plan: Mutex<Plan>,
    pub candidates: Mutex<Vec<Runnable>>,
    ledger: &'a Ledger,
    checkpoints: &'a dyn Checkpoints,
    cfg: Config,
    pub current: Mutex<Option<Runnable>>,
    pub packet: Mutex<Option<HandoffPacket>>,
    out: Mutex<ExecResult>,
    /// task id → the worker running it, while it runs.
    inflight: Mutex<HashMap<String, Arc<dyn Worker>>>,
}

fn min_opt<T: PartialOrd + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if y < x { y } else { x }),
        (x, None) => x,
        (None, y) => y,
    }
}

impl<'a> StepExecutor<'a> {
    pub fn new(run_id: &str, project: &Path, plan: Plan, candidates: Vec<Runnable>, ledger: &'a Ledger, checkpoints: &'a dyn Checkpoints, mut cfg: Config) -> Self {
        cfg.max_parallel = cfg.max_parallel.max(1);
        let packet = cfg.start_packet.take();
        Self {
            run_id: run_id.into(),
            project: project.to_owned(),
            plan: Mutex::new(plan),
            candidates: Mutex::new(candidates),
            ledger,
            checkpoints,
            cfg,
            current: Mutex::new(None),
            packet: Mutex::new(packet),
            out: Mutex::new(ExecResult { status: "completed".into(), ..Default::default() }),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    // ------------------------------------------------------------------ plan access

    fn step(&self, index: usize) -> Step {
        self.plan.lock().unwrap().steps[index].clone()
    }

    fn update_step<R>(&self, index: usize, f: impl FnOnce(&mut Step) -> R) -> R {
        f(&mut self.plan.lock().unwrap().steps[index])
    }

    fn out<R>(&self, f: impl FnOnce(&mut ExecResult) -> R) -> R {
        f(&mut self.out.lock().unwrap())
    }

    fn emit(&self, kind: &str, payload: Value) {
        if let Some(listener) = &self.cfg.on_event {
            let mut event = json!({"kind": kind, "run_id": self.run_id});
            if let (Some(e), Some(p)) = (event.as_object_mut(), payload.as_object()) {
                e.extend(p.clone());
            }
            listener(event);
        }
    }

    // ------------------------------------------------------------------ choosing

    fn level(&self, c: &Runnable) -> &'static str {
        self.ledger.level(&c.candidate.family, c.candidate.model.as_deref())
    }

    fn pool_id(&self, c: &Runnable) -> String {
        self.ledger.pool_for(&c.candidate.family, c.candidate.model.as_deref()).pool_id
    }

    fn headroom(&self, c: &Runnable) -> f64 {
        let pool = self.ledger.pool_for(&c.candidate.family, c.candidate.model.as_deref());
        self.ledger.state(&pool).pct
    }

    /// The worker for the next attempt: `ok` pools before `soft` ones; within a level, `high` complexity takes the
    /// highest tier, `low` the lowest unit price (ties: most pool headroom, then list order), `medium` the first.
    pub fn pick(&self, exclude_pools: &HashSet<String>, complexity: &str, candidates: &[Runnable]) -> Option<Runnable> {
        let usable: Vec<&Runnable> = candidates.iter().filter(|c| !exclude_pools.contains(&self.pool_id(c))).collect();
        if self.cfg.strongest_first {
            return usable.into_iter().find(|c| matches!(self.level(c), "ok" | "soft")).cloned();
        }
        for wanted in ["ok", "soft"] {
            let at_level: Vec<&Runnable> = usable.iter().copied().filter(|c| self.level(c) == wanted).collect();
            if at_level.is_empty() {
                continue;
            }
            if !matches!(complexity, "high" | "low") || self.cfg.profile.is_empty() {
                return Some(at_level[0].clone());
            }
            let key = |position: usize, c: &Runnable| {
                let (tier, price) = self.cfg.profile.get(&c.key()).copied().unwrap_or((1, f64::INFINITY));
                let primary = if complexity == "high" { -(tier as f64) } else { price };
                (primary, self.headroom(c), position)
            };
            let best = at_level.iter().enumerate().min_by(|(ia, a), (ib, b)| {
                let (ka, kb) = (key(*ia, a), key(*ib, b));
                ka.0.total_cmp(&kb.0).then(ka.1.total_cmp(&kb.1)).then(ka.2.cmp(&kb.2))
            });
            return best.map(|(_, c)| (*c).clone());
        }
        None
    }

    // ------------------------------------------------------------------ the person's controls

    /// Stops every worker call in flight (a request to pause or cancel *now*): each worker kills its process group and
    /// reports `cancelled`; the step is rolled back and the request is honoured where that result lands.
    pub async fn stop_inflight(&self) {
        let workers: Vec<(String, Arc<dyn Worker>)> = self.inflight.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (task_id, worker) in workers {
            worker.cancel(&task_id).await;
        }
    }

    fn requested(&self) -> Option<String> {
        self.cfg.controls.as_ref().and_then(|c| c.pending())
    }

    fn fail_with(&self, ctx: &mut StepCtx, reason: impl Into<String>) {
        let reason = reason.into();
        ctx.reason = reason.clone();
        self.out(|o| o.reason = reason);
    }

    fn user_pause(&self, index: usize, ctx: &mut StepCtx, failures: &[String]) -> &'static str {
        let step = self.step(index);
        let packet = self.packet_for(index, &step, "user_pause", ctx.current.as_ref(), None, failures, false, None);
        ctx.last_packet = Some(packet.clone());
        self.out(|o| o.last_packet = Some(packet.clone()));
        self.fail_with(ctx, "paused by the person");
        self.emit("paused", json!({"step_id": step.step_id, "handoff_id": packet.handoff_id, "reason": "user_pause"}));
        "paused"
    }

    // ------------------------------------------------------------------ the loop

    pub async fn run(&self) -> ExecResult {
        self.out(|o| *o = ExecResult { status: "completed".into(), ..Default::default() });
        if self.plan.lock().unwrap().steps.iter().any(|s| !s.depends_on.is_empty()) {
            self.run_waves().await;
        } else {
            // Sequential: one context carries the worker, its note and the retry rotation from step to step.
            let mut ctx = StepCtx::new(self.candidates.lock().unwrap().clone(), self.current.lock().unwrap().clone(), self.packet.lock().unwrap().clone());
            let count = self.plan.lock().unwrap().steps.len();
            for index in 0..count {
                if matches!(self.step(index).status.as_str(), "completed" | "skipped") {
                    continue;
                }
                let verdict = self.run_step(index, &mut ctx).await;
                *self.candidates.lock().unwrap() = ctx.candidates.clone();
                *self.current.lock().unwrap() = ctx.current.clone();
                *self.packet.lock().unwrap() = ctx.packet.clone();
                if verdict != "completed" {
                    self.out(|o| o.status = verdict.to_owned());
                    break;
                }
            }
        }
        self.out(|o| o.clone())
    }

    async fn run_waves(&self) {
        let mut start_packet = self.packet.lock().unwrap().clone();
        loop {
            let (done, pending): (HashSet<String>, Vec<Step>) = {
                let plan = self.plan.lock().unwrap();
                let done = plan.steps.iter().filter(|s| matches!(s.status.as_str(), "completed" | "skipped")).map(|s| s.step_id.clone()).collect();
                let pending = plan.steps.iter().filter(|s| !matches!(s.status.as_str(), "completed" | "skipped")).cloned().collect();
                (done, pending)
            };
            if pending.is_empty() {
                return;
            }
            if let Some(asked) = self.requested() {
                // Between waves nothing runs, so the request is honoured as it stands.
                let (status, reason) = if asked == "pause" { ("paused", "paused by the person") } else { ("cancelled", "cancelled by the person") };
                self.out(|o| (o.status, o.reason) = (status.into(), reason.into()));
                return;
            }
            let wave = form_wave(&pending, &done, self.cfg.max_parallel);
            if wave.is_empty() {
                let ids: Vec<&str> = pending.iter().map(|s| s.step_id.as_str()).collect();
                self.out(|o| {
                    o.status = "failed".into();
                    o.reason = format!("dependency deadlock: no pending step has all of its depends_on completed ({})", ids.join(", "));
                });
                self.emit("deadlock", json!({"pending": ids}));
                return;
            }
            self.emit("wave_start", json!({"steps": wave.iter().map(|s| s.step_id.clone()).collect::<Vec<_>>()}));
            let index_of: HashMap<String, usize> = self.plan.lock().unwrap().steps.iter().enumerate().map(|(i, s)| (s.step_id.clone(), i)).collect();
            let mut contexts: Vec<StepCtx> = Vec::new();
            for step in &wave {
                let siblings: Vec<String> = wave.iter().filter(|o| o.step_id != step.step_id).flat_map(|o| o.file_scope.clone()).collect();
                let mut ctx = StepCtx::new(self.candidates.lock().unwrap().clone(), self.current.lock().unwrap().clone(), start_packet.take());
                ctx.scoped = wave.len() > 1;
                ctx.sibling_scope = siblings;
                ctx.isolated = self.cfg.isolate && wave.len() > 1 && self.cfg.isolation.is_some();
                contexts.push(ctx);
            }
            let verdicts: Vec<&'static str> = {
                let runs = wave.iter().zip(contexts.iter_mut()).map(|(s, c)| self.run_step(index_of[&s.step_id], c));
                futures::future::join_all(runs).await
            };
            if let Some(last) = contexts.iter().rev().find_map(|c| c.current.clone()) {
                *self.current.lock().unwrap() = Some(last);
            }
            *self.packet.lock().unwrap() = None;
            for (ctx, verdict) in contexts.iter().zip(&verdicts) {
                if *verdict != "completed" {
                    self.out(|o| {
                        o.status = (*verdict).into();
                        if !ctx.reason.is_empty() {
                            o.reason = ctx.reason.clone();
                        }
                        if ctx.last_packet.is_some() {
                            o.last_packet = ctx.last_packet.clone();
                        }
                    });
                    *self.packet.lock().unwrap() = ctx.packet.clone();
                    return;
                }
            }
        }
    }

    fn envelope(&self) -> cordata_ledger::EnvelopeState {
        let budget = self.plan.lock().unwrap().budget_envelope.clone();
        self.ledger.envelope(&self.run_id, budget.get("max_usd").and_then(Value::as_f64), budget.get("max_tokens").and_then(Value::as_i64))
    }

    fn record(&self, step_id: &str, candidate: &Runnable, result: &WorkerResult) {
        let model = candidate.candidate.model.as_deref().or(result.model.as_deref());
        let row = self.ledger.record_result(&self.run_id, Some(step_id), &candidate.candidate.family, model, &result.ledger_outcome(), Some("execute"), Some(&candidate.key()));
        if let Some(listener) = &self.cfg.on_usage {
            listener(json!({"run_id": self.run_id, "role": "execute", "worker": candidate.key(), "step_id": step_id, "pool_id": row.pool_id,
                            "tokens_in": result.tokens_in, "tokens_out": result.tokens_out, "cost_usd": result.cost_usd}));
        }
    }

    async fn run_step(&self, index: usize, ctx: &mut StepCtx) -> &'static str {
        let verdict = self.attempts(index, ctx).await;
        if let (Some(sandbox), Some(isolation)) = (ctx.sandbox.take(), &self.cfg.isolation) {
            isolation.discard(&sandbox);
        }
        verdict
    }

    /// (Re)make an isolated step's sandbox from the primary tree as it is now; on failure, run it shared instead.
    fn fresh_sandbox(&self, step: &Step, ctx: &mut StepCtx) {
        let Some(isolation) = &self.cfg.isolation else { return };
        if let Some(old) = ctx.sandbox.take() {
            isolation.discard(&old);
        }
        let label: String = step.step_id.replace('/', "-").chars().take(24).collect();
        match isolation.create(&self.project, &label, &step.file_scope) {
            Ok(sb) => {
                self.emit("sandbox", json!({"step_id": step.step_id, "root": sb.root, "id": sb.id}));
                ctx.sandbox = Some(sb);
            }
            Err(error) => {
                ctx.isolated = false;
                self.emit("sandbox", json!({"step_id": step.step_id, "root": null, "error": error}));
            }
        }
    }

    /// Throw an attempt's work away: a fresh sandbox for an isolated step, the checkpoint for a shared one.
    fn undo(&self, step: &Step, ctx: &mut StepCtx) {
        if ctx.isolated { self.fresh_sandbox(step, ctx) } else { self.rollback(step, ctx.scoped) }
    }

    async fn attempts(&self, index: usize, ctx: &mut StepCtx) -> &'static str {
        let initial = self.step(index);
        if let (Some(rerank), 0) = (&self.cfg.rerank, initial.attempts) {
            let ordered = rerank(&initial, &ctx.candidates);
            let sorted_keys = |v: &[Runnable]| {
                let mut k: Vec<String> = v.iter().map(Runnable::key).collect();
                k.sort();
                k
            };
            if sorted_keys(&ordered) == sorted_keys(&ctx.candidates) {
                ctx.candidates = ordered;
            }
        }
        if ctx.isolated {
            self.fresh_sandbox(&initial, ctx);
        }
        let snap = self.checkpoints.take(&self.project, &self.run_id, &initial.step_id, &format!("step {}", index + 1));
        self.update_step(index, |s| {
            s.checkpoint_hash = if snap.ok { snap.id.clone() } else { None };
            if !snap.ok {
                s.rollback_policy = "keep_partial".into();
            }
        });
        self.emit("step_start", json!({"step_id": initial.step_id, "index": index, "checkpoint": self.step(index).checkpoint_hash}));
        let mut failures: Vec<String> = Vec::new();
        let mut excluded: HashSet<String> = HashSet::new();
        loop {
            match self.requested().as_deref() {
                Some("pause") => {
                    self.update_step(index, |s| if s.status == "running" { s.status = "pending".into() });
                    return self.user_pause(index, ctx, &failures);
                }
                Some("cancel") => {
                    self.update_step(index, |s| if s.status == "running" { s.status = "pending".into() });
                    self.fail_with(ctx, "cancelled by the person");
                    return "cancelled";
                }
                _ => {}
            }
            let envelope = self.envelope();
            let step = self.step(index);
            if envelope.exhausted {
                self.update_step(index, |s| if s.status == "running" { s.status = "failed".into() });
                self.fail_with(ctx, format!("the plan's budget envelope is spent (${:.4}, {} tokens)", envelope.spent_usd, envelope.spent_tokens));
                self.emit("budget_exhausted", json!({"step_id": step.step_id, "envelope": envelope}));
                return "budget_exhausted";
            }
            let Some(candidate) = self.pick(&excluded, &step.complexity, &ctx.candidates) else {
                let reason = self.out(|o| o.handoffs.last().map_or("worker_unavailable".to_owned(), |h| h.reason.clone()));
                let packet = self.packet_for(index, &step, &reason, ctx.current.as_ref(), None, &failures, false, None);
                ctx.last_packet = Some(packet.clone());
                self.out(|o| o.last_packet = Some(packet.clone()));
                self.fail_with(ctx, "no worker has quota left; the run is paused at a safe point");
                self.emit("paused", json!({"step_id": step.step_id, "handoff_id": packet.handoff_id}));
                return "paused";
            };
            if ctx.packet.is_none() && !self.out(|o| o.workers_used.is_empty())
                && let Some(prev) = ctx.current.as_ref().filter(|p| p.key() != candidate.key()) {
                    // A switch at a step boundary (soft line, or the previous worker's pool went away): leave a note.
                    let reason = if self.level(prev) == "soft" { "quota_soft_limit_reached" } else { "worker_unavailable" };
                    let packet = self.packet_for(index, &step, reason, Some(prev), Some(&candidate), &failures, false, None);
                    ctx.packet = Some(packet.clone());
                    self.out(|o| o.handoffs.push(packet));
                }
            ctx.current = Some(candidate.clone());
            self.out(|o| {
                if !o.workers_used.contains(&candidate.key()) {
                    o.workers_used.push(candidate.key());
                }
            });
            let allowance = self.ledger.step_allowance(&candidate.candidate.family, candidate.candidate.model.as_deref());
            let budget_usd = min_opt(if let Allowance::Usd(v) = allowance { Some(v) } else { None }, envelope.remaining_usd);
            let budget_tokens = min_opt(if let Allowance::Tokens(v) = allowance { Some(v) } else { None }, envelope.remaining_tokens);
            let attempt = self.update_step(index, |s| {
                s.status = "running".into();
                s.worker = Some(candidate.key());
                s.attempts += 1;
                s.attempts
            });
            let root = match (&ctx.sandbox, ctx.isolated) {
                (Some(sb), true) => sb.root.to_string_lossy().into_owned(),
                _ => self.project.to_string_lossy().into_owned(),
            };
            let failure_note = if failures.is_empty() { String::new() } else { format!("\n\nEarlier attempts at this step failed:\n- {}", failures.join("\n- ")) };
            let step_now = self.step(index);
            let mut task = TaskPacket::new(
                &format!("{}:{}:{attempt}", self.run_id, step.step_id),
                "execute",
                &step_prompt(&self.plan.lock().unwrap(), index, &format!("{}{failure_note}", self.cfg.extra_prompt)),
                &root,
            );
            task.file_scope = step.file_scope.clone();
            task.budget_usd = budget_usd;
            task.budget_tokens = budget_tokens;
            task.timeout_s = self.cfg.timeout_s;
            task.model = candidate.candidate.model.clone();
            task.effort = candidate.candidate.effort.clone();
            task.checkpoint_hash = step_now.checkpoint_hash.clone();
            task.handoff_context = ctx.packet.as_ref().map(HandoffPacket::context);
            let pool_id = self.pool_id(&candidate);
            self.emit(
                "step_dispatch",
                json!({"step_id": step.step_id, "worker": candidate.key(), "pool_id": pool_id, "complexity": step.complexity, "budget_usd": budget_usd,
                       "budget_tokens": task.budget_tokens, "handoff": ctx.packet.as_ref().map(|p| p.handoff_id.clone())}),
            );
            let sink: EventSink = Arc::new(|_| {});
            let task_id = task.task_id.clone();
            self.inflight.lock().unwrap().insert(task_id.clone(), candidate.worker.clone());
            let mut result = candidate.worker.run(task, sink).await;
            self.inflight.lock().unwrap().remove(&task_id);
            self.record(&step.step_id, &candidate, &result);
            if !ctx.sibling_scope.is_empty() {
                result.out_of_scope.retain(|p| !scope::matches(p, &ctx.sibling_scope, &root));
                result.files_changed.retain(|p| !scope::matches(p, &ctx.sibling_scope, &root));
            }
            self.out(|o| {
                o.results.entry(step.step_id.clone()).or_default().push(json!({
                    "worker": candidate.key(), "status": result.status, "error_message": result.error_message, "tokens_in": result.tokens_in,
                    "tokens_out": result.tokens_out, "cost_usd": result.cost_usd, "files_changed": result.files_changed,
                    "out_of_scope": result.out_of_scope, "rate_limited": result.rate_limited,
                }));
            });
            self.emit(
                "step_result",
                json!({"step_id": step.step_id, "worker": candidate.key(), "status": result.status, "cost_usd": (result.cost_usd * 1e4).round() / 1e4,
                       "tokens_in": result.tokens_in, "tokens_out": result.tokens_out, "duration_s": (result.duration_s * 10.0).round() / 10.0,
                       "error": result.error_message, "files": result.files_changed.iter().take(50).collect::<Vec<_>>()}),
            );

            if result.status == "completed" && result.out_of_scope.is_empty() && ctx.isolated
                && let (Some(sandbox), Some(isolation)) = (&ctx.sandbox, &self.cfg.isolation) {
                    let taken = isolation.harvest(sandbox, &result.files_changed, &step.file_scope);
                    self.emit("harvest", json!({"step_id": step.step_id, "written": taken.written.iter().take(50).collect::<Vec<_>>(), "deleted": taken.deleted.iter().take(50).collect::<Vec<_>>(),
                                                "conflicts": taken.conflicts.iter().take(50).collect::<Vec<_>>(), "skipped": taken.skipped.iter().take(50).collect::<Vec<_>>()}));
                    self.out(|o| {
                        o.harvested.extend(taken.written.iter().chain(&taken.deleted).cloned());
                        o.conflicts.extend(taken.conflicts.iter().cloned());
                    });
                    if !taken.conflicts.is_empty() {
                        // Someone changed these in the primary tree while the step ran: never overwrite, retry on top.
                        result.status = "failed".into();
                        result.error_message = Some(format!("changed in the project while this step ran (not overwritten): {}", taken.conflicts.iter().take(10).cloned().collect::<Vec<_>>().join(", ")));
                    }
                }
            if result.status == "completed" && result.out_of_scope.is_empty() {
                self.update_step(index, |s| s.status = "completed".into());
                ctx.packet = None;
                return "completed";
            }
            if result.status == "cancelled" {
                let step = self.step(index);
                self.undo(&step, ctx);
                if self.requested().as_deref() == Some("pause") {
                    // Stopped mid-call for a pause: the step goes back to pending and runs again on resume.
                    self.update_step(index, |s| s.status = "pending".into());
                    return self.user_pause(index, ctx, &failures);
                }
                self.update_step(index, |s| s.status = "failed".into());
                let why = if self.requested().as_deref() == Some("cancel") { "cancelled by the person" } else { "cancelled" };
                self.fail_with(ctx, why);
                return "cancelled";
            }
            if result.status == "quota_limit" {
                let reason = if result.rate_limited { "provider_rate_limit" } else { "quota_hard_limit_reached" };
                let step = self.step(index);
                // An isolated step's partial work lives in a sandbox that is about to go: nothing to keep.
                let keep = step.rollback_policy == "keep_partial" && !ctx.isolated;
                if !keep {
                    self.undo(&step, ctx);
                }
                let envelope = self.envelope();
                if envelope.exhausted && !result.rate_limited {
                    // The cut-off was the plan's own envelope, not the pool: no other worker may spend more on it.
                    self.update_step(index, |s| s.status = if keep { "partial" } else { "pending" }.into());
                    self.fail_with(ctx, format!("the plan's budget envelope is spent (${:.4}, {} tokens)", envelope.spent_usd, envelope.spent_tokens));
                    self.emit("budget_exhausted", json!({"step_id": step.step_id, "envelope": envelope}));
                    return "budget_exhausted";
                }
                failures.push(format!("{} stopped: {}", candidate.key(), result.error_message.clone().unwrap_or_else(|| reason.to_owned())));
                excluded.insert(self.pool_id(&candidate));
                let next = self.pick(&excluded, &step.complexity, &ctx.candidates);
                let packet = self.packet_for(index, &step, reason, Some(&candidate), next.as_ref(), &failures, keep, Some(&result));
                ctx.packet = Some(packet.clone());
                self.out(|o| o.handoffs.push(packet.clone()));
                self.emit("handoff", json!({"step_id": step.step_id, "handoff_id": packet.handoff_id, "reason": reason, "from_worker": candidate.key(), "to_worker": next.as_ref().map(Runnable::key)}));
                self.update_step(index, |s| s.status = if keep { "partial" } else { "pending" }.into());
                continue;
            }
            // failed, or completed with writes outside the step's scope
            let mut why = result.error_message.clone().unwrap_or_else(|| "failed".into());
            if !result.out_of_scope.is_empty() {
                why = format!("wrote outside the step's scope: {}", result.out_of_scope.iter().take(10).cloned().collect::<Vec<_>>().join(", "));
            }
            failures.push(format!("{}: {why}", candidate.key()));
            let step = self.step(index);
            self.undo(&step, ctx);
            if failures.len() >= self.cfg.max_attempts {
                self.update_step(index, |s| s.status = "failed".into());
                self.fail_with(ctx, format!("step {} failed {} times: {}", step.step_id, failures.len(), failures.last().cloned().unwrap_or_default()));
                return "failed";
            }
            // try another candidate: prefer a different worker for the retry
            let others: Vec<Runnable> = ctx.candidates.iter().filter(|c| c.key() != candidate.key()).cloned().collect();
            if !others.is_empty() {
                ctx.candidates = others.into_iter().chain([candidate.clone()]).collect();
            }
            let packet = self.packet_for(index, &step, "worker_failed", Some(&candidate), None, &failures, false, Some(&result));
            ctx.packet = Some(packet.clone());
            self.out(|o| o.handoffs.push(packet));
        }
    }

    // ------------------------------------------------------------------ helpers

    /// Restore the step's checkpoint. In a wave of several steps (`scoped`) only the step's own scope goes back — the
    /// other steps are writing the rest of the project — and a scope that cannot be narrowed is not restored.
    fn rollback(&self, step: &Step, scoped: bool) {
        let Some(hash) = &step.checkpoint_hash else { return };
        let report = if scoped {
            let Some(paths) = diff_paths(&step.file_scope) else {
                self.emit("rollback", json!({"step_id": step.step_id, "checkpoint": hash, "ok": false, "files": [], "skipped": true}));
                return;
            };
            self.checkpoints.restore(&self.project, hash, Some(&paths), &self.run_id)
        } else {
            self.checkpoints.restore(&self.project, hash, None, &self.run_id)
        };
        self.emit("rollback", json!({"step_id": step.step_id, "checkpoint": hash, "ok": report.ok, "files": report.files.iter().take(50).collect::<Vec<_>>()}));
    }

    #[allow(clippy::too_many_arguments)]
    fn packet_for(&self, index: usize, step: &Step, reason: &str, source: Option<&Runnable>, target: Option<&Runnable>, failures: &[String], keep: bool, result: Option<&WorkerResult>) -> HandoffPacket {
        let (mut patch, mut stat) = (String::new(), String::new());
        let touched = result.map(|r| r.files_changed.clone()).unwrap_or_default();
        if let (true, Some(hash)) = (keep, &step.checkpoint_hash) {
            (patch, stat) = self.checkpoints.diff_since(&self.project, hash, diff_paths(&step.file_scope).as_deref());
        }
        let plan = self.plan.lock().unwrap();
        let packet = handoff::build(Build {
            session_id: self.run_id.clone(),
            reason: reason.into(),
            plan_id: plan.plan_id.clone(),
            step_index: index as i64,
            step_id: step.step_id.clone(),
            step_status: if keep && !patch.is_empty() { "partial" } else { "pending" }.into(),
            checkpoint_hash: step.checkpoint_hash.clone(),
            from_worker: source.map_or("none".to_owned(), Runnable::key),
            to_worker: target.map(Runnable::key),
            diff_stat: if stat.is_empty() && keep { result.map(|r| r.diff_stat.clone()).unwrap_or_default() } else { stat },
            diff_patch: patch,
            files_touched: if keep { touched } else { Vec::new() },
            failed_attempts: failures.to_vec(),
            open_assumptions: plan.assumptions.clone(),
            completed_steps: plan.steps.iter().filter(|s| s.status == "completed").map(|s| s.step_id.clone()).collect(),
            ..Default::default()
        })
        .expect("executor only uses known handoff reasons");
        handoff::save(self.ledger, &packet);
        packet
    }
}

/// Scope entries as git pathspecs: a glob is widened to its literal directory; one at the project root means "the
/// whole project" (`None`).
pub fn diff_paths(file_scope: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for entry in file_scope {
        let mut clean = scope::normalize(entry).trim_end_matches('/').to_owned();
        if clean.contains(['*', '?', '[']) {
            clean = scope::literal_prefix(&clean);
            if clean.is_empty() {
                return None;
            }
        }
        if !clean.is_empty() {
            out.push(clean);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The literal prefixes a step may write under; `None` when a glob at the root opens the whole project. An empty
/// scope (a read-only step) writes nowhere.
pub fn scope_prefixes(file_scope: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for entry in file_scope {
        let mut clean = scope::normalize(entry).trim_end_matches('/').to_owned();
        if clean.contains(['*', '?', '[']) {
            clean = scope::literal_prefix(&clean).trim_end_matches('/').to_owned();
            if clean.is_empty() {
                return None;
            }
        }
        if !clean.is_empty() {
            out.push(clean);
        }
    }
    Some(out)
}

pub fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    let (Some(pa), Some(pb)) = (scope_prefixes(a), scope_prefixes(b)) else { return true }; // a root-wide glob overlaps everything
    pa.iter().any(|x| pb.iter().any(|y| x == y || x.starts_with(&format!("{y}/")) || y.starts_with(&format!("{x}/"))))
}

/// Pending steps whose dependencies are all done, taken in plan order while their scopes stay pairwise disjoint.
pub fn form_wave(pending: &[Step], done: &HashSet<String>, max_parallel: usize) -> Vec<Step> {
    let mut wave: Vec<Step> = Vec::new();
    for step in pending {
        if wave.len() >= max_parallel {
            break;
        }
        if !step.depends_on.iter().all(|d| done.contains(d)) || wave.iter().any(|o| scopes_overlap(&step.file_scope, &o.file_scope)) {
            continue;
        }
        wave.push(step.clone());
    }
    wave
}

/// Whole seconds since the epoch, for task ids.
pub fn unix_seconds() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
