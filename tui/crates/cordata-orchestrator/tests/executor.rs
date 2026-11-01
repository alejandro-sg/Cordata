//! The EXECUTE phase against scripted workers and a copy-based checkpoint store — the scenarios of
//! `backend/_quota_ledger_smoke_test.py` (fault injection at step 2 of 5, keep_partial, the soft line, the envelope)
//! plus retries, out-of-scope writes, pausing and parallel waves.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cordata_ledger::{Ledger, Outcome, Pool};
use cordata_orchestrator::executor::{Config, Controls, ExecResult, Runnable, StepExecutor, form_wave};
use cordata_orchestrator::handoff;
use cordata_orchestrator::plan::{self, Options, Plan};
use cordata_orchestrator::testkit::{CopyCheckpoints, ScriptedWorker, runnable};
use cordata_orchestrator::worker::{TaskPacket, WorkerResult};
use serde_json::{Value, json};

struct Env {
    _dir: tempfile::TempDir,
    project: PathBuf,
    store: PathBuf,
    ledger: Ledger,
    events: Arc<Mutex<Vec<Value>>>,
}

fn env(pools: Vec<Pool>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(project.join("calc")).unwrap();
    std::fs::write(project.join("calc/base.txt"), "base\n").unwrap();
    let ledger = Ledger::open(&dir.path().join("state.db"), pools).unwrap();
    Env { project, store: dir.path().join("cp"), ledger, events: Arc::default(), _dir: dir }
}

fn draft(n: usize, policy: &str) -> Value {
    let steps: Vec<Value> = (1..=n)
        .map(|i| json!({"step_id": format!("s{i}"), "name": format!("step {i}"), "description": "write the file", "file_scope": [format!("calc/s{i}.txt")], "rollback_policy": policy}))
        .collect();
    json!({"goal": "write files", "acceptance_criteria": [{"id": "c1", "type": "file_exists", "description": "exists", "target": "calc/s1.txt"}], "steps": steps, "assumptions": ["py3.12"]})
}

fn plan_of(n: usize, policy: &str, max_usd: f64) -> Plan {
    plan::from_draft(&draft(n, policy), "/nonexistent", Options { max_usd, plan_id: Some("plan-t".into()), ..Default::default() }, None).unwrap()
}

fn step_of(task: &TaskPacket) -> String {
    task.task_id.split(':').nth(1).unwrap().to_owned()
}

fn write(task: &TaskPacket, step: &str, text: &str) {
    std::fs::write(Path::new(&task.cwd).join(format!("calc/{step}.txt")), text).unwrap();
}

/// A worker that completes every step by writing `ok <step>`, except where `misbehave` says otherwise.
fn worker(id: &str, tokens: i64, cost: f64, misbehave: impl Fn(&str, usize) -> Option<WorkerResult> + Send + Sync + 'static) -> Arc<ScriptedWorker> {
    let id_owned = id.to_owned();
    ScriptedWorker::new(id, move |task, n| {
        let step = step_of(task);
        if let Some(r) = misbehave(&step, n) {
            return r;
        }
        write(task, &step, &format!("ok {step}"));
        let mut r = WorkerResult::new(&id_owned, "completed");
        (r.tokens_in, r.cost_usd, r.files_changed) = (tokens, cost, vec![format!("calc/{step}.txt")]);
        r
    })
}

fn limited(id: &str, rate_limited: bool) -> WorkerResult {
    let mut r = WorkerResult::new(id, "quota_limit");
    (r.rate_limited, r.error_message) = (rate_limited, Some("limit".into()));
    r
}

fn exec<'a>(env: &'a Env, plan: Plan, candidates: Vec<Runnable>, cp: &'a CopyCheckpoints, mut cfg: Config) -> StepExecutor<'a> {
    let events = env.events.clone();
    cfg.on_event = Some(Box::new(move |e| events.lock().unwrap().push(e)));
    StepExecutor::new("run-t", &env.project, plan, candidates, &env.ledger, cp, cfg)
}

fn kinds(env: &Env, kind: &str) -> Vec<Value> {
    env.events.lock().unwrap().iter().filter(|e| e["kind"] == kind).cloned().collect()
}

fn read(env: &Env, file: &str) -> Option<String> {
    std::fs::read_to_string(env.project.join(file)).ok()
}

fn two_pools(limit_a: f64) -> Vec<Pool> {
    vec![Pool::new("pa", "claude_code").limit(limit_a), Pool::new("pb", "antigravity").limit(100_000.0)]
}

#[tokio::test]
async fn a_provider_limit_at_step_2_of_5_rolls_back_hands_off_and_finishes_elsewhere() {
    let e = env(two_pools(100_000.0));
    let project = e.project.clone();
    let a = ScriptedWorker::new("a", move |task, _| {
        let step = step_of(task);
        if step == "s2" {
            std::fs::write(project.join("calc/s2.txt"), "half").unwrap();
            return limited("a", true);
        }
        write(task, &step, &format!("ok {step}"));
        let mut r = WorkerResult::new("a", "completed");
        (r.tokens_in, r.files_changed) = (10, vec![format!("calc/{step}.txt")]);
        r
    });
    let b = worker("b", 10, 0.0, |_, _| None);
    let cp = CopyCheckpoints::new(&e.store);
    let cands = vec![runnable(&a, "sonnet", "claude_code", "anthropic"), runnable(&b, "gemini", "antigravity", "google")];
    let ex = exec(&e, plan_of(5, "rollback_checkpoint", 2.0), cands, &cp, Config::default());
    let out = ex.run().await;

    assert_eq!(out.status, "completed", "{}", out.reason);
    let plan = ex.plan.lock().unwrap().clone();
    let workers: Vec<&str> = plan.steps.iter().map(|s| s.worker.as_deref().unwrap()).collect();
    assert_eq!(workers, ["a:sonnet", "b:gemini", "b:gemini", "b:gemini", "b:gemini"], "step 1 on the first worker; 2–5 once its pool was spent");
    let rollbacks = kinds(&e, "rollback");
    assert_eq!((rollbacks.len(), rollbacks[0]["step_id"].clone(), rollbacks[0]["ok"].clone()), (1, json!("s2"), json!(true)));
    assert_eq!(out.handoffs.len(), 1);
    let packet = &out.handoffs[0];
    assert_eq!((packet.reason.as_str(), packet.step_id.as_str(), packet.step_status.as_str()), ("provider_rate_limit", "s2", "pending"));
    assert_eq!(packet.completed_steps, ["s1"]);
    assert_eq!(packet.checkpoint_hash, plan.steps[1].checkpoint_hash);
    assert_eq!(packet.to_worker.as_deref(), Some("b:gemini"));
    let resumed: Vec<TaskPacket> = b.calls.lock().unwrap().iter().filter(|t| t.task_id.starts_with("run-t:s2:")).cloned().collect();
    assert_eq!(resumed[0].handoff_context.as_ref().unwrap()["reason"], "provider_rate_limit", "the second worker got step 2 with the packet");
    assert!(handoff::load(&e.ledger, &packet.handoff_id).is_some(), "the packet is in state.db");
    assert_eq!(e.ledger.state_of("pa").level, "exhausted");
    assert_eq!(e.ledger.state_of("pb").level, "ok");
    for i in 1..=5 {
        assert_eq!(read(&e, &format!("calc/s{i}.txt")).unwrap(), format!("ok s{i}"), "no half-written file survives");
    }
    assert_eq!(read(&e, "calc/base.txt").unwrap(), "base\n");
}

#[tokio::test]
async fn keep_partial_carries_the_partial_diff_to_the_next_worker() {
    let e = env(two_pools(100_000.0));
    let project = e.project.clone();
    let a = ScriptedWorker::new("a", move |task, _| {
        let step = step_of(task);
        std::fs::write(project.join(format!("calc/{step}.txt")), "half done\n").unwrap();
        let mut r = limited("a", true);
        r.files_changed = vec![format!("calc/{step}.txt")];
        r
    });
    let b = worker("b", 10, 0.0, |_, _| None);
    let cp = CopyCheckpoints::new(&e.store);
    let cands = vec![runnable(&a, "sonnet", "claude_code", "anthropic"), runnable(&b, "gemini", "antigravity", "google")];
    let ex = exec(&e, plan_of(2, "keep_partial", 2.0), cands, &cp, Config::default());
    let out = ex.run().await;
    assert_eq!(out.status, "completed", "{}", out.reason);
    let packet = &out.handoffs[0];
    assert_eq!(packet.step_status, "partial");
    assert!(packet.diff_patch.contains("half done") && packet.diff_patch.contains("calc/s1.txt"), "{}", packet.diff_patch);
    assert!(kinds(&e, "rollback").is_empty(), "partial work is kept, not rolled back");
    let seen = b.calls.lock().unwrap()[0].handoff_context.clone().unwrap();
    assert!(seen["diff_patch"].as_str().unwrap().contains("half done"));
    assert_eq!(read(&e, "calc/s1.txt").unwrap(), "ok s1", "the next worker finished the step");
}

#[tokio::test]
async fn when_every_pool_is_spent_the_run_pauses_with_a_packet() {
    let e = env(two_pools(100_000.0));
    let a = worker("a", 1, 0.0, |_, _| Some(limited("a", true)));
    let b = worker("b", 1, 0.0, |_, _| Some(limited("b", true)));
    let cp = CopyCheckpoints::new(&e.store);
    let cands = vec![runnable(&a, "m", "claude_code", "anthropic"), runnable(&b, "m", "antigravity", "google")];
    let ex = exec(&e, plan_of(3, "rollback_checkpoint", 2.0), cands, &cp, Config::default());
    let out = ex.run().await;
    assert_eq!(out.status, "paused");
    assert!(out.reason.contains("no worker has quota left"));
    let last = out.last_packet.expect("a packet to resume from");
    assert_eq!(last.reason, "provider_rate_limit", "the reason of the last handoff carries over");
    assert_eq!(out.handoffs.len(), 2);
    assert_eq!(ex.plan.lock().unwrap().steps[0].status, "pending", "the step is resumable");
    assert_eq!(read(&e, "calc/s1.txt"), None);
}

#[tokio::test]
async fn the_soft_line_at_a_step_boundary_switches_pools_with_a_note() {
    let e = env(two_pools(100.0));
    // Step 1 spends 85 of 100 tokens: past the soft line, not the hard one.
    let a = worker("a", 85, 0.0, |_, _| None);
    let b = worker("b", 5, 0.0, |_, _| None);
    let cp = CopyCheckpoints::new(&e.store);
    let cands = vec![runnable(&a, "m", "claude_code", "anthropic"), runnable(&b, "m", "antigravity", "google")];
    let ex = exec(&e, plan_of(2, "rollback_checkpoint", 2.0), cands, &cp, Config::default());
    let out = ex.run().await;
    assert_eq!(out.status, "completed", "{}", out.reason);
    assert_eq!(out.workers_used, ["a:m", "b:m"]);
    assert_eq!(out.handoffs.len(), 1);
    assert_eq!(out.handoffs[0].reason, "quota_soft_limit_reached");
    assert_eq!(out.handoffs[0].completed_steps, ["s1"]);
    assert!(b.calls.lock().unwrap()[0].handoff_context.is_some());
}

#[tokio::test]
async fn the_hard_line_bounds_a_step_by_what_the_pool_has_left() {
    let e = env(two_pools(100.0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let a = ScriptedWorker::new("a", move |task, _| {
        sink.lock().unwrap().push((task.budget_tokens, task.budget_usd));
        let step = step_of(task);
        write(task, &step, "ok");
        WorkerResult::new("a", "completed")
    });
    let cp = CopyCheckpoints::new(&e.store);
    let ex = exec(&e, plan_of(1, "rollback_checkpoint", 2.0), vec![runnable(&a, "m", "claude_code", "anthropic")], &cp, Config::default());
    ex.run().await;
    assert_eq!(seen.lock().unwrap()[0], (Some(95), Some(2.0)), "95% of the pool's cap; the envelope's dollars");
}

#[tokio::test]
async fn a_spent_envelope_stops_the_run_instead_of_spending_elsewhere() {
    let e = env(two_pools(100_000.0));
    let a = worker("a", 10, 1.0, |_, _| None);
    let b = worker("b", 10, 1.0, |_, _| None);
    let cp = CopyCheckpoints::new(&e.store);
    let cands = vec![runnable(&a, "m", "claude_code", "anthropic"), runnable(&b, "m", "antigravity", "google")];
    let ex = exec(&e, plan_of(3, "rollback_checkpoint", 0.5), cands, &cp, Config::default());
    let out = ex.run().await;
    assert_eq!(out.status, "budget_exhausted");
    assert!(out.reason.contains("budget envelope is spent"));
    assert_eq!(ex.plan.lock().unwrap().steps[0].status, "completed");
    assert_eq!(ex.plan.lock().unwrap().steps[1].status, "pending");
    assert_eq!(kinds(&e, "budget_exhausted").len(), 1);
}

#[tokio::test]
async fn a_failing_step_rotates_workers_then_fails_after_max_attempts() {
    let e = env(two_pools(100_000.0));
    let fail = |id: &'static str| worker(id, 1, 0.0, move |_, _| {
        let mut r = WorkerResult::new(id, "failed");
        r.error_message = Some("tests red".into());
        Some(r)
    });
    let (a, b) = (fail("a"), fail("b"));
    let cp = CopyCheckpoints::new(&e.store);
    let cands = vec![runnable(&a, "m", "claude_code", "anthropic"), runnable(&b, "m", "antigravity", "google")];
    let ex = exec(&e, plan_of(2, "rollback_checkpoint", 2.0), cands, &cp, Config::default());
    let out = ex.run().await;
    assert_eq!(out.status, "failed");
    assert!(out.reason.starts_with("step s1 failed 3 times: "), "{}", out.reason);
    assert_eq!((a.calls.lock().unwrap().len(), b.calls.lock().unwrap().len()), (2, 1), "a, then b, then a again");
    assert_eq!(out.handoffs.len(), 2, "each non-final failure leaves a worker_failed note");
    assert!(out.handoffs.iter().all(|h| h.reason == "worker_failed"));
    assert_eq!(ex.plan.lock().unwrap().steps[0].status, "failed");
    let second = b.calls.lock().unwrap()[0].clone();
    assert!(second.prompt.contains("Earlier attempts at this step failed:\n- a:m: tests red"));
    assert_eq!(out.results["s1"].len(), 3);
}

#[tokio::test]
async fn writes_outside_the_scope_fail_the_step_and_are_rolled_back() {
    let e = env(two_pools(100_000.0));
    let project = e.project.clone();
    let bad = ScriptedWorker::new("bad", move |task, n| {
        let step = step_of(task);
        if n == 1 {
            std::fs::write(project.join("calc/stray.txt"), "oops").unwrap();
            let mut r = WorkerResult::new("bad", "completed");
            r.out_of_scope = vec!["calc/stray.txt".into()];
            return r;
        }
        write(task, &step, "ok");
        WorkerResult::new("bad", "completed")
    });
    let cp = CopyCheckpoints::new(&e.store);
    let ex = exec(&e, plan_of(1, "rollback_checkpoint", 2.0), vec![runnable(&bad, "m", "claude_code", "anthropic")], &cp, Config::default());
    let out = ex.run().await;
    assert_eq!(out.status, "completed", "{}", out.reason);
    assert_eq!(read(&e, "calc/stray.txt"), None, "the stray file was rolled back");
    assert!(bad.calls.lock().unwrap()[1].prompt.contains("wrote outside the step's scope: calc/stray.txt"));
}

struct PauseAfter(Arc<Mutex<Vec<Value>>>);
impl Controls for PauseAfter {
    fn pending(&self) -> Option<String> {
        self.0.lock().unwrap().iter().any(|e| e["kind"] == "step_result").then(|| "pause".to_owned())
    }
}

#[tokio::test]
async fn a_pause_request_stops_at_the_next_safe_point_with_a_resume_packet() {
    let e = env(two_pools(100_000.0));
    let a = worker("a", 1, 0.0, |_, _| None);
    let cp = CopyCheckpoints::new(&e.store);
    let cfg = Config { controls: Some(Arc::new(PauseAfter(e.events.clone()))), ..Default::default() };
    let ex = exec(&e, plan_of(3, "rollback_checkpoint", 2.0), vec![runnable(&a, "m", "claude_code", "anthropic")], &cp, cfg);
    let out = ex.run().await;
    assert_eq!(out.status, "paused");
    assert_eq!(out.reason, "paused by the person");
    assert_eq!(out.last_packet.unwrap().reason, "user_pause");
    let statuses: Vec<String> = ex.plan.lock().unwrap().steps.iter().map(|s| s.status.clone()).collect();
    assert_eq!(statuses, ["completed", "pending", "pending"]);
    // Resuming with a fresh executor on the same plan finishes the rest.
    let plan = ex.plan.lock().unwrap().clone();
    let again = exec(&e, plan, vec![runnable(&a, "m", "claude_code", "anthropic")], &cp, Config::default());
    assert_eq!(again.run().await.status, "completed");
    assert_eq!(read(&e, "calc/s3.txt").unwrap(), "ok s3");
}

#[tokio::test]
async fn complexity_routes_high_steps_to_the_strongest_tier_and_low_to_the_cheapest() {
    let e = env(vec![Pool::new("pa", "claude_code").limit(100_000.0), Pool::new("pb", "antigravity").limit(100_000.0)]);
    let strong = worker("strong", 1, 0.0, |_, _| None);
    let cheap = worker("cheap", 1, 0.0, |_, _| None);
    let cp = CopyCheckpoints::new(&e.store);
    let mut draft = draft(3, "rollback_checkpoint");
    draft["steps"][0]["complexity"] = json!("high");
    draft["steps"][1]["complexity"] = json!("low");
    let plan = plan::from_draft(&draft, "/x", Options::default(), None).unwrap();
    let profile = HashMap::from([("cheap:m".to_owned(), (0, 0.08)), ("strong:m".to_owned(), (2, 1.0))]);
    let cands = vec![runnable(&cheap, "m", "antigravity", "google"), runnable(&strong, "m", "claude_code", "anthropic")];
    let ex = exec(&e, plan, cands, &cp, Config { profile, ..Default::default() });
    let out = ex.run().await;
    assert_eq!(out.status, "completed", "{}", out.reason);
    let workers: Vec<String> = ex.plan.lock().unwrap().steps.iter().map(|s| s.worker.clone().unwrap()).collect();
    assert_eq!(workers, ["strong:m", "cheap:m", "cheap:m"], "high → strongest, low → cheapest, medium → first usable");
}

#[tokio::test]
async fn independent_steps_run_as_a_wave_and_dependents_wait() {
    let e = env(two_pools(100_000.0));
    let a = worker("a", 1, 0.0, |_, _| None);
    let cp = CopyCheckpoints::new(&e.store);
    let mut d = draft(3, "rollback_checkpoint");
    d["steps"][2]["depends_on"] = json!(["s1", "s2"]);
    let plan = plan::from_draft(&d, "/x", Options::default(), None).unwrap();
    let ex = exec(&e, plan, vec![runnable(&a, "m", "claude_code", "anthropic")], &cp, Config::default());
    let out = ex.run().await;
    assert_eq!(out.status, "completed", "{}", out.reason);
    let waves: Vec<Value> = kinds(&e, "wave_start").iter().map(|w| w["steps"].clone()).collect();
    assert_eq!(waves, [json!(["s1", "s2"]), json!(["s3"])]);
    for i in 1..=3 {
        assert_eq!(read(&e, &format!("calc/s{i}.txt")).unwrap(), format!("ok s{i}"));
    }
}

#[tokio::test]
async fn a_failed_step_in_a_wave_rolls_back_only_its_own_scope() {
    let e = env(two_pools(100_000.0));
    let a = worker("a", 1, 0.0, |step, _| {
        (step == "s2").then(|| {
            let mut r = WorkerResult::new("a", "failed");
            r.error_message = Some("nope".into());
            r
        })
    });
    let cp = CopyCheckpoints::new(&e.store);
    let mut d = draft(3, "rollback_checkpoint");
    d["steps"][2]["depends_on"] = json!(["s1"]);
    let plan = plan::from_draft(&d, "/x", Options::default(), None).unwrap();
    let ex = exec(&e, plan, vec![runnable(&a, "m", "claude_code", "anthropic")], &cp, Config { max_attempts: 1, ..Default::default() });
    let out = ex.run().await;
    assert_eq!(out.status, "failed");
    assert_eq!(read(&e, "calc/s1.txt").unwrap(), "ok s1", "the sibling's work survives the other step's rollback");
}

#[tokio::test]
async fn a_dependency_on_nothing_that_can_finish_is_a_reported_deadlock() {
    let e = env(two_pools(100_000.0));
    let a = worker("a", 1, 0.0, |_, _| None);
    let cp = CopyCheckpoints::new(&e.store);
    let mut plan = plan_of(2, "rollback_checkpoint", 2.0);
    plan.steps[0].depends_on = vec!["s2".into()];
    plan.steps[1].depends_on = vec!["s1".into()];
    let ex = exec(&e, plan, vec![runnable(&a, "m", "claude_code", "anthropic")], &cp, Config::default());
    let out = ex.run().await;
    assert_eq!(out.status, "failed");
    assert!(out.reason.starts_with("dependency deadlock"), "{}", out.reason);
    assert_eq!(kinds(&e, "deadlock").len(), 1);
}

#[tokio::test]
async fn an_unprotected_step_keeps_partial_work_rather_than_claiming_a_rollback() {
    struct NoSnapshots;
    impl cordata_orchestrator::checkpoints::Checkpoints for NoSnapshots {
        fn take(&self, _: &Path, _: &str, _: &str, _: &str) -> cordata_orchestrator::checkpoints::Snapshot {
            cordata_orchestrator::checkpoints::Snapshot { ok: false, id: None, reason: "disk full".into() }
        }
        fn restore(&self, _: &Path, _: &str, _: Option<&[String]>, _: &str) -> cordata_orchestrator::checkpoints::RestoreReport {
            panic!("nothing to restore");
        }
        fn diff_since(&self, _: &Path, _: &str, _: Option<&[String]>) -> (String, String) {
            (String::new(), String::new())
        }
    }
    let e = env(two_pools(100_000.0));
    let a = worker("a", 1, 0.0, |_, _| None);
    let ex = StepExecutor::new("run-t", &e.project, plan_of(1, "rollback_checkpoint", 2.0), vec![runnable(&a, "m", "claude_code", "anthropic")], &e.ledger, &NoSnapshots, Config::default());
    let out: ExecResult = ex.run().await;
    assert_eq!(out.status, "completed");
    let step = ex.plan.lock().unwrap().steps[0].clone();
    assert_eq!((step.rollback_policy.as_str(), step.checkpoint_hash), ("keep_partial", None));
}

#[test]
fn waves_take_disjoint_scopes_in_plan_order_up_to_the_limit() {
    let plan = plan_of(4, "rollback_checkpoint", 2.0);
    let mut steps = plan.steps.clone();
    steps[1].file_scope = vec!["calc/".into()]; // overlaps s2's dir with s1's file
    steps[0].file_scope = vec!["calc/s1.txt".into()];
    let wave = form_wave(&steps, &HashSet::new(), 3);
    assert_eq!(wave.iter().map(|s| s.step_id.as_str()).collect::<Vec<_>>(), ["s1", "s3", "s4"], "s2 overlaps s1 and waits");
    assert_eq!(form_wave(&steps, &HashSet::new(), 1).len(), 1);
    let _ = Outcome::default();
}

#[tokio::test]
async fn the_fault_injection_scenario_also_holds_on_the_real_shadow_git_store() {
    use cordata_orchestrator::checkpoints::GitCheckpoints;
    let e = env(two_pools(100_000.0));
    let project = e.project.clone();
    let a = ScriptedWorker::new("a", move |task, _| {
        let step = step_of(task);
        if step == "s2" {
            std::fs::write(project.join("calc/s2.txt"), "half").unwrap();
            std::fs::write(project.join("calc/stray.txt"), "debris").unwrap();
            let mut r = limited("a", true);
            r.files_changed = vec!["calc/s2.txt".into()];
            return r;
        }
        write(task, &step, &format!("ok {step}"));
        let mut r = WorkerResult::new("a", "completed");
        (r.tokens_in, r.files_changed) = (10, vec![format!("calc/{step}.txt")]);
        r
    });
    let b = worker("b", 10, 0.0, |_, _| None);
    let git = GitCheckpoints::new(cordata_checkpoints::Store::new(e.store.parent().unwrap().join("shadow-home")));
    let cands = vec![runnable(&a, "sonnet", "claude_code", "anthropic"), runnable(&b, "gemini", "antigravity", "google")];
    let events = e.events.clone();
    let cfg = Config { on_event: Some(Box::new(move |ev| events.lock().unwrap().push(ev))), ..Default::default() };
    let ex = StepExecutor::new("run-git", &e.project, plan_of(3, "rollback_checkpoint", 2.0), cands, &e.ledger, &git, cfg);
    let out = ex.run().await;
    assert_eq!(out.status, "completed", "{}", out.reason);
    assert!(kinds(&e, "rollback").iter().all(|r| r["ok"] == true) && kinds(&e, "rollback").len() == 1);
    assert_eq!(read(&e, "calc/stray.txt"), None, "the debris the failed step left was removed by the real restore");
    for i in 1..=3 {
        assert_eq!(read(&e, &format!("calc/s{i}.txt")).unwrap(), format!("ok s{i}"));
    }
    assert_eq!(read(&e, "calc/base.txt").unwrap(), "base\n");
    // The same history the Python `checkpoints.turns` would show: one turn per step.
    let turns = cordata_checkpoints::Store::new(e.store.parent().unwrap().join("shadow-home")).turns(&e.project, 10);
    assert_eq!(turns.iter().filter(|t| t.tools == ["orchestrator"]).count(), 3);

    // keep_partial carries the real diff of the half-written file.
    let e2 = env(two_pools(100_000.0));
    let project2 = e2.project.clone();
    let a2 = ScriptedWorker::new("a", move |task, _| {
        std::fs::write(project2.join(format!("calc/{}.txt", step_of(task))), "half done\n").unwrap();
        limited("a", true)
    });
    let b2 = worker("b", 10, 0.0, |_, _| None);
    let git2 = GitCheckpoints::new(cordata_checkpoints::Store::new(e2.store.parent().unwrap().join("shadow-home")));
    let cands = vec![runnable(&a2, "sonnet", "claude_code", "anthropic"), runnable(&b2, "gemini", "antigravity", "google")];
    let ex = StepExecutor::new("run-git2", &e2.project, plan_of(1, "keep_partial", 2.0), cands, &e2.ledger, &git2, Config::default());
    let out = ex.run().await;
    assert_eq!(out.status, "completed", "{}", out.reason);
    let packet = &out.handoffs[0];
    assert_eq!(packet.step_status, "partial");
    assert!(packet.diff_patch.contains("+++ b/calc/s1.txt") && packet.diff_patch.contains("+half done"), "{}", packet.diff_patch);
    assert!(packet.diff_stat.contains("1 file changed"), "{}", packet.diff_stat);
}
