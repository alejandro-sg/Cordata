//! The state machine end to end against scripted workers: one agent answers as planner, executor, hardener, auditor
//! and conformance checker according to the task it is given, over a real shadow-git checkpoint store.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cordata_checkpoints::Store;
use cordata_ledger::{Ledger, Pool};
use cordata_orchestrator::audit::{AuditEnv, PatternGuard};
use cordata_orchestrator::checkpoints::GitCheckpoints;
use cordata_orchestrator::machine::{self, ControlRequest, Machine, Research, RunConfig, RunControls, StaticRoster, Synthesis, Verified, World};
use cordata_orchestrator::testkit::{ScriptedWorker, runnable};
use cordata_orchestrator::worker::{TaskPacket, WorkerResult};
use serde_json::{Value, json};

/// Answers the machine's calls by role / label. Queues let a test script successive verdicts.
#[derive(Default)]
struct Script {
    /// Draft plans, consumed one per planning call (the last repeats).
    drafts: VecDeque<Value>,
    audits: VecDeque<Value>,
    conformances: VecDeque<Value>,
    /// Executor steps that fail (by step id), and for how many calls.
    fail_steps: HashMap<String, usize>,
    /// Hardener behaviour.
    harden_writes_outside: bool,
    prompts: Vec<(String, String)>,
}

struct Rig {
    _dir: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
    ledger: Ledger,
    checkpoints: GitCheckpoints,
    script: Arc<Mutex<Script>>,
    events: Arc<Mutex<Vec<Value>>>,
    roster: StaticRoster,
}

fn label_of(task: &TaskPacket) -> String {
    task.task_id.split(':').nth(1).unwrap_or("").to_owned()
}

fn draft(files: &[&str]) -> Value {
    let steps: Vec<Value> = files.iter().enumerate().map(|(i, f)| json!({"step_id": format!("s{}", i + 1), "name": format!("write {f}"), "description": "write it", "file_scope": [format!("calc/{f}.txt")]})).collect();
    let criteria: Vec<Value> = files.iter().map(|f| json!({"id": format!("c-{f}"), "type": "regex_match", "description": format!("{f} has its content"), "target": format!("calc/{f}.txt::ok {f}")})).collect();
    json!({"goal": "write the files", "acceptance_criteria": criteria, "steps": steps, "assumptions": ["py3.12"]})
}

fn agent(script: Arc<Mutex<Script>>, id: &'static str, project: PathBuf) -> Arc<ScriptedWorker> {
    ScriptedWorker::new(id, move |task, _| {
        let label = label_of(task);
        let mut s = script.lock().unwrap();
        s.prompts.push((label.clone(), task.prompt.clone()));
        let mut r = WorkerResult::new(id, "completed");
        r.tokens_in = 10;
        match (task.role.as_str(), label.as_str()) {
            ("plan", "plan") => {
                let d = if s.drafts.len() > 1 { s.drafts.pop_front() } else { s.drafts.front().cloned() };
                r.structured = d;
            }
            ("plan", "research") => r.structured = Some(json!({"findings": []})),
            ("plan", "conformance") => r.structured = s.conformances.pop_front(),
            ("audit", _) => r.structured = s.audits.pop_front(),
            ("harden", _) => {
                if s.harden_writes_outside {
                    std::fs::write(project.join("calc/stray.txt"), "x").unwrap();
                    r.out_of_scope = vec!["calc/stray.txt".into()];
                }
            }
            _ => {
                // execute: write "ok <stem>" into every file of the step's scope.
                let step = label.clone();
                let step_id = task.task_id.split(':').nth(1).unwrap_or("").to_owned();
                let failing = task.prompt.contains(&format!("(id {step})")) && s.fail_steps.get(&step_id).is_some_and(|n| *n > 0);
                if failing {
                    *s.fail_steps.get_mut(&step_id).unwrap() -= 1;
                    r.status = "failed".into();
                    r.exit_code = 1;
                    r.error_message = Some("boom".into());
                    return r;
                }
                for entry in &task.file_scope {
                    let stem = Path::new(entry).file_stem().unwrap().to_string_lossy().into_owned();
                    let path = Path::new(&task.cwd).join(entry);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, format!("ok {stem}")).unwrap();
                    r.files_changed.push(entry.clone());
                }
            }
        }
        r
    })
}

fn rig(script: Script) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(project.join("calc")).unwrap();
    std::fs::write(project.join("calc/base.txt"), "base\n").unwrap();
    let home = dir.path().join("home");
    let ledger = Ledger::open(&dir.path().join("state.db"), vec![Pool::new("cc", "claude_code").limit(1_000_000.0), Pool::new("agy", "antigravity").limit(1_000_000.0)]).unwrap();
    let checkpoints = GitCheckpoints::new(Store::new(home.join("cp")));
    let script = Arc::new(Mutex::new(script));
    let project_real = std::fs::canonicalize(&project).unwrap();
    // anthropic: planner/executor; google: hardener/auditor (a different model family than the executor).
    let anth = agent(script.clone(), "anth", project_real.clone());
    let goog = agent(script.clone(), "goog", project_real);
    let mut roster = StaticRoster::default();
    let (a, g) = (runnable(&anth, "sonnet", "claude_code", "anthropic"), runnable(&goog, "gemini", "antigravity", "google"));
    roster.roles.insert("plan".into(), vec![a.clone()]);
    roster.roles.insert("execute".into(), vec![a.clone()]);
    roster.roles.insert("harden".into(), vec![g.clone(), a.clone()]);
    roster.roles.insert("audit".into(), vec![g, a]);
    Rig { project, home, ledger, checkpoints, script, events: Arc::default(), roster, _dir: dir }
}

impl Rig {
    fn world<'a>(&'a mut self, controls: Option<Arc<dyn RunControls>>, research: Option<&'a dyn Research>, synthesis: Option<&'a dyn Synthesis>) -> World<'a> {
        let events = self.events.clone();
        World {
            roster: &mut self.roster,
            ledger: &self.ledger,
            checkpoints: &self.checkpoints,
            audit_env: AuditEnv { checkpoints: &self.checkpoints, guard: &PatternGuard, environ: std::env::vars().collect() },
            research,
            synthesis,
            on_event: Some(Arc::new(move |e| events.lock().unwrap().push(e))),
            controls,
            isolation: None,
            home: self.home.clone(),
        }
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.project.join(rel)).ok()
    }
}

fn config() -> RunConfig {
    RunConfig { max_usd: 5.0, ..Default::default() }
}

fn states(record: &machine::RunRecord) -> Vec<String> {
    record.history.iter().map(|h| h["state"].as_str().unwrap().to_owned()).collect()
}

fn accept() -> Value {
    json!({"verdict": "accept", "items": []})
}

#[tokio::test]
async fn the_happy_path_plans_executes_hardens_audits_and_records_everything() {
    let mut r = rig(Script { drafts: [draft(&["a", "b"])].into(), audits: [accept()].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let (record, plan) = {
        let world = r.world(None, None, None);
        let mut m = Machine::new("write the files", &project, config(), world, Some("run-happy"), None, None);
        m.run().await;
        (m.record.clone(), m.plan.clone().unwrap())
    };
    assert_eq!(record.state, "done", "{}", record.reason);
    assert_eq!(states(&record), ["plan", "execute", "harden", "audit", "done"]);
    assert_eq!(record.history.last().unwrap()["verdict"], "accept");
    assert_eq!(r.read("calc/a.txt").unwrap(), "ok a");
    assert_eq!(r.read("calc/b.txt").unwrap(), "ok b");
    assert!(plan.steps.iter().all(|s| s.status == "completed"));
    assert_eq!(record.workers["plan"], ["anth:sonnet"]);
    assert_eq!(record.workers["harden"], ["goog:gemini"], "the reviewer is from a different model family than the executor");
    assert_eq!(record.workers["audit"], ["goog:gemini"]);
    assert_eq!(record.audit.as_ref().unwrap()["tier1_ok"], true);
    assert!(record.start_checkpoint.is_some());

    // Persisted, loadable, resumable by id; artifacts written privately.
    let loaded = machine::load(&r.ledger, "run-happy").unwrap();
    assert_eq!((loaded.state.as_str(), loaded.goal.as_str()), ("done", "write the files"));
    assert_eq!(machine::recent(&r.ledger, 5).len(), 1);
    let dir = machine::artifacts_dir(&r.home, "run-happy");
    let walk = std::fs::read_to_string(dir.join("walkthrough.md")).unwrap();
    assert!(walk.contains("**Outcome:** done") && walk.contains("[completed] **write a**") && walk.contains("deterministic checks: passed; reviewer verdict: accept"), "{walk}");
    assert!(dir.join("plan.md").exists() && !dir.join("research.md").exists());
    assert!(!record.usage.is_empty() && record.usage.iter().any(|u| u["role"] == "execute"));
    let kinds: Vec<String> = r.events.lock().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_owned()).collect();
    assert!(kinds.contains(&"dispatch".to_owned()) && kinds.contains(&"usage".to_owned()) && kinds.contains(&"step_result".to_owned()), "{kinds:?}");
    assert!(!r.ledger.recent_events("orchestrator", 50).is_empty());
    // Rewards: every role that touched the change earned the verdict.
    let credited: Vec<&str> = r.roster.observed.iter().map(|o| o.0.as_str()).collect();
    for role in ["plan", "execute", "harden", "audit"] {
        assert!(credited.contains(&role), "{role} not credited: {credited:?}");
    }
    assert!(r.roster.observed.iter().all(|o| o.3.ok && o.3.reward == Some(1.0) && o.3.final_));
}

#[tokio::test]
async fn harden_is_skipped_when_no_reviewer_from_another_family_exists() {
    let mut r = rig(Script { drafts: [draft(&["a"])].into(), audits: [accept()].into(), ..Default::default() });
    let a = r.roster.roles["execute"][0].clone();
    r.roster.roles.insert("harden".into(), vec![a.clone()]);
    r.roster.roles.insert("audit".into(), vec![a]);
    let project = r.project.to_string_lossy().into_owned();
    let record = {
        let world = r.world(None, None, None);
        let mut m = Machine::new("g", &project, config(), world, Some("run-skip"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "done");
    assert!(record.warnings.iter().any(|w| w.starts_with("harden skipped: no reviewer from a model family other than the executors' (anthropic)")), "{:?}", record.warnings);
    assert!(record.workers["harden"].is_empty());
}

#[tokio::test]
async fn a_hardener_that_writes_outside_the_scope_is_rolled_back() {
    let mut r = rig(Script { drafts: [draft(&["a"])].into(), audits: [accept()].into(), harden_writes_outside: true, ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let record = {
        let world = r.world(None, None, None);
        let mut m = Machine::new("g", &project, config(), world, Some("run-harden"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "done");
    assert!(record.warnings.iter().any(|w| w.contains("harden by goog:gemini discarded: wrote outside the scope: calc/stray.txt")));
    assert_eq!(r.read("calc/stray.txt"), None, "the hardener's stray file was restored away");
    assert_eq!(r.read("calc/a.txt").unwrap(), "ok a", "the executor's work survives the hardener's rollback");
}

#[tokio::test]
async fn drift_gets_a_fix_pass_then_the_audit_runs_again() {
    let drift = json!({"verdict": "drift", "items": ["a is missing the footer"]});
    let mut r = rig(Script { drafts: [draft(&["a"])].into(), audits: [drift, accept()].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let record = {
        let world = r.world(None, None, None);
        let mut m = Machine::new("g", &project, config(), world, Some("run-drift"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "done", "{}", record.reason);
    assert_eq!(record.drift_retries, 1);
    assert_eq!(states(&record), ["plan", "execute", "harden", "audit", "execute", "audit", "done"]);
    let fix_prompt = r.script.lock().unwrap().prompts.iter().find(|(_, p)| p.contains("Fix what the audit found")).map(|(_, p)| p.clone()).expect("a fix step ran");
    assert!(fix_prompt.contains("a is missing the footer"), "{fix_prompt}");
}

#[tokio::test]
async fn a_replan_goes_back_to_the_start_checkpoint_and_plans_again_with_the_reasons() {
    let replan = json!({"verdict": "replan", "items": ["wrong files entirely"]});
    let mut r = rig(Script { drafts: [draft(&["a"]), draft(&["b"])].into(), audits: [replan, accept()].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let record = {
        let world = r.world(None, None, None);
        let mut m = Machine::new("g", &project, config(), world, Some("run-replan"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "done", "{}", record.reason);
    assert_eq!(record.replans, 1);
    assert_eq!(r.read("calc/a.txt"), None, "the first plan's work was restored away");
    assert_eq!(r.read("calc/b.txt").unwrap(), "ok b");
    let plans: Vec<String> = r.script.lock().unwrap().prompts.iter().filter(|(l, _)| l == "plan").map(|(_, p)| p.clone()).collect();
    assert_eq!(plans.len(), 2);
    assert!(plans[1].contains("A previous plan did not work out") && plans[1].contains("wrong files entirely"));
    assert!(record.warnings.iter().any(|w| w.starts_with("work discarded by a restore (the auditor asked for a new plan) was saved to ")));
    let patches: Vec<_> = std::fs::read_dir(machine::artifacts_dir(&r.home, "run-replan")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".patch")).collect();
    assert_eq!(patches.len(), 1);
    assert!(std::fs::read_to_string(patches[0].path()).unwrap().contains("+ok a"));
}

#[tokio::test]
async fn failing_every_bound_ends_failed_and_leaves_the_tree_as_it_found_it() {
    // The deterministic checks can never pass (the criterion expects a file nothing writes): drift retries, a replan, then failure.
    let mut d = draft(&["a"]);
    d["acceptance_criteria"] = json!([{"id": "never", "type": "file_exists", "description": "x", "target": "calc/never.txt"}]);
    let mut r = rig(Script { drafts: [d].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let record = {
        let world = r.world(None, None, None);
        let mut m = Machine::new("g", &project, config(), world, Some("run-fail"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "failed");
    assert!(record.reason.starts_with("drift persisted after the retries: never (file_exists): missing"), "{}", record.reason);
    assert_eq!((record.drift_retries, record.replans), (2, 1), "two fix passes, a replan (which resets the counter), two more fix passes, then failure");
    assert_eq!(r.read("calc/a.txt"), None, "a failed run restores the project");
    assert_eq!(r.read("calc/base.txt").unwrap(), "base\n");
    assert_eq!(record.history.last().unwrap()["restored"], true);
    // Failure debits the roles that touched the change.
    assert!(r.roster.observed.iter().any(|o| o.0 == "execute" && !o.3.ok && o.3.reward == Some(-1.0)), "a failed deterministic check is the harshest reward: {:?}", r.roster.observed);
}

#[tokio::test]
async fn an_unplannable_goal_fails_with_the_planners_reasons() {
    let bad = json!({"goal": "x", "acceptance_criteria": [{"id": "c", "type": "command", "description": "d"}], "steps": [{"step_id": "s", "name": "n", "description": "d", "file_scope": []}]});
    let mut r = rig(Script { drafts: [bad].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let record = {
        let world = r.world(None, None, None);
        let mut m = Machine::new("g", &project, config(), world, Some("run-noplan"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "failed");
    assert!(record.reason.starts_with("no usable plan: anth:sonnet: the plan was refused: ") && record.reason.contains("declares no file_scope"), "{}", record.reason);
}

#[tokio::test]
async fn a_step_that_defeats_every_attempt_is_retried_with_a_note_keeping_finished_steps() {
    let mut r = rig(Script { drafts: [draft(&["a", "b"])].into(), audits: [accept()].into(), fail_steps: [("s2".to_owned(), 3)].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let record = {
        let world = r.world(None, None, None);
        let mut m = Machine::new("g", &project, config(), world, Some("run-recover"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "done", "{}", record.reason);
    assert_eq!(record.recoveries, 1);
    assert_eq!(record.replans, 0, "no replan: the finished step was kept");
    assert!(record.history.iter().any(|h| h["state"] == "execute" && h["recovery"] == 1 && h["step"] == "s2"));
    let recovery_prompt = r.script.lock().unwrap().prompts.iter().rev().find(|(_, p)| p.contains("Work in several SMALL edits")).map(|(_, p)| p.clone());
    assert!(recovery_prompt.is_some(), "the recovery note reached the worker");
    assert_eq!(r.read("calc/b.txt").unwrap(), "ok b");
}

#[tokio::test]
async fn a_spent_envelope_stops_the_run_before_the_next_phase() {
    let mut r = rig(Script { drafts: [draft(&["a"])].into(), audits: [accept()].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let record = {
        let world = r.world(None, None, None);
        let mut m = Machine::new("g", &project, RunConfig { max_tokens: Some(5), ..config() }, world, Some("run-budget"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "budget_exhausted");
    assert_eq!(record.reason, "the plan's budget envelope is spent");
    assert!(!record.usage.is_empty());
}

struct Controls(Mutex<Option<ControlRequest>>);

impl RunControls for Controls {
    fn pending(&self) -> Option<ControlRequest> {
        self.0.lock().unwrap().clone()
    }
    fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
}

#[tokio::test]
async fn a_pause_stops_at_the_next_safe_point_and_a_resume_finishes_the_run() {
    let mut r = rig(Script { drafts: [draft(&["a"])].into(), audits: [accept()].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let controls = Arc::new(Controls(Mutex::new(None)));
    let events = r.events.clone();
    let c2 = controls.clone();
    // Ask for a pause as soon as the plan is made (when execution is announced).
    let trigger: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(move |e: Value| {
        if e["kind"] == "state" && e["state"] == "execute" {
            *c2.0.lock().unwrap() = Some(ControlRequest { kind: "pause".into(), by: "me".into(), restore: false });
        }
        events.lock().unwrap().push(e);
    });
    let paused = {
        let mut world = r.world(Some(controls.clone()), None, None);
        world.on_event = Some(trigger);
        let mut m = Machine::new("g", &project, config(), world, Some("run-pause"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(paused.state, "paused");
    assert_eq!((paused.reason.as_str(), paused.resume_state.as_deref()), ("paused by the person", Some("execute")));
    assert_eq!(paused.history.last().unwrap()["by"], "me");
    assert!(controls.pending().is_none(), "the request was consumed");
    assert_eq!(r.read("calc/a.txt"), None, "nothing executed yet");

    let finished = {
        let world = r.world(None, None, None);
        machine::resume("run-pause", config(), world).await.unwrap()
    };
    assert_eq!(finished.state, "done", "{}", finished.reason);
    assert!(finished.history.iter().any(|h| h["resumed"] == true));
    assert_eq!(r.read("calc/a.txt").unwrap(), "ok a");
    // A terminal run comes back as it is.
    let again = {
        let world = r.world(None, None, None);
        machine::resume("run-pause", config(), world).await.unwrap()
    };
    assert_eq!(again.state, "done");
    assert!(machine::resume("nope", config(), r.world(None, None, None)).await.is_none());
}

#[tokio::test]
async fn a_cancel_with_restore_saves_a_patch_and_puts_the_project_back() {
    let mut r = rig(Script { drafts: [draft(&["a"])].into(), audits: [accept()].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let controls = Arc::new(Controls(Mutex::new(None)));
    let (c2, events) = (controls.clone(), r.events.clone());
    // Cancel (with restore) once the work is done and the run is about to harden.
    let trigger: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(move |e: Value| {
        if e["kind"] == "state" && e["state"] == "harden" {
            *c2.0.lock().unwrap() = Some(ControlRequest { kind: "cancel".into(), by: String::new(), restore: true });
        }
        events.lock().unwrap().push(e);
    });
    let record = {
        let mut world = r.world(Some(controls), None, None);
        world.on_event = Some(trigger);
        let mut m = Machine::new("g", &project, config(), world, Some("run-cancel"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "cancelled");
    assert_eq!(record.reason, "cancelled by the person");
    assert_eq!(record.history.last().unwrap()["restored"], true);
    assert_eq!(r.read("calc/a.txt"), None);
    assert!(record.warnings.iter().any(|w| w.starts_with("work discarded by a restore (cancelled with restore)")));
    assert!(r.roster.observed.is_empty(), "a cancel credits no one");
}

struct FakeResearch;

impl Research for FakeResearch {
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn prompt(&self, goal: &str, feedback: &[String]) -> String {
        format!("research {goal} {}", feedback.join(","))
    }
    fn verify(&self, structured: Option<&Value>, _project: &str) -> Verified {
        let findings = structured.and_then(|s| s.get("findings")).cloned().unwrap_or(json!([]));
        Verified { brief: json!({"findings": [{"id": "f1", "claim": "calc is where it lives"}], "raw": findings}), refused: false, reasons: vec![], dropped: vec![] }
    }
    fn render(&self, brief: &Value) -> String {
        format!("BRIEF[{}]", brief["findings"][0]["id"].as_str().unwrap_or("?"))
    }
}

#[tokio::test]
async fn the_research_loop_grounds_the_plan_and_conformance_gaps_buy_a_fix_round() {
    let mut d = draft(&["a"]);
    d["steps"][0]["grounded_in"] = json!(["f1"]);
    let gaps = json!({"verdict": "gaps", "gaps": [{"ref": "f1", "missing": "the footer", "severity": "high"}], "step_scores": [{"step_id": "s1", "cohesion": 0.9}]});
    let mut r = rig(Script { drafts: [d].into(), conformances: [gaps, accept_c()].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let research = FakeResearch;
    let record = {
        let world = r.world(None, Some(&research), None);
        let mut m = Machine::new("g", &project, RunConfig { research: true, ..config() }, world, Some("run-research"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "done", "{}", record.reason);
    assert_eq!(states(&record), ["research", "plan", "execute", "harden", "audit", "conformance", "execute", "harden", "audit", "conformance", "done"]);
    assert_eq!(record.rounds, 1);
    assert_eq!(record.planner.as_deref(), Some("anth:sonnet"));
    let plan_prompt = r.script.lock().unwrap().prompts.iter().find(|(l, _)| l == "plan").map(|(_, p)| p.clone()).unwrap();
    assert!(plan_prompt.contains("BRIEF[f1]") && plan_prompt.contains("cite only these ids"));
    let audit = record.audit.as_ref().unwrap();
    assert_eq!(audit["conformance"], "accept");
    assert_eq!(audit["step_scores"], json!([{"step_id": "s1", "cohesion": 0.95}]), "the scores of the last conformance pass are kept");
    assert!(machine::artifacts_dir(&r.home, "run-research").join("research.md").exists());
    assert_eq!(std::fs::read_to_string(machine::artifacts_dir(&r.home, "run-research").join("research.md")).unwrap(), "BRIEF[f1]");
    let gap_prompt = r.script.lock().unwrap().prompts.iter().find(|(_, p)| p.contains("Close the conformance gaps")).map(|(_, p)| p.clone()).unwrap();
    assert!(gap_prompt.contains("f1: the footer (high)"));
    assert!(r.script.lock().unwrap().prompts.iter().all(|(l, _)| l != "audit"), "conformance replaces the tier-2 reviewer in the loop");
}

fn accept_c() -> Value {
    json!({"verdict": "accept", "gaps": [], "step_scores": [{"step_id": "s1", "cohesion": 0.95}, "junk"]})
}

#[tokio::test]
async fn an_unusable_conformance_answer_accepts_on_the_deterministic_checks() {
    let mut d = draft(&["a"]);
    d["steps"][0]["grounded_in"] = json!(["f1"]);
    let mut r = rig(Script { drafts: [d].into(), conformances: [json!({"verdict": "gaps", "gaps": []})].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let research = FakeResearch;
    let record = {
        let world = r.world(None, Some(&research), None);
        let mut m = Machine::new("g", &project, RunConfig { research: true, ..config() }, world, Some("run-conf-bad"), None, None);
        m.run().await;
        m.record.clone()
    };
    assert_eq!(record.state, "done");
    assert_eq!(record.history.last().unwrap()["verdict"], "accept(tier1)");
    assert!(record.warnings.iter().any(|w| w.contains("conformance check by anth:sonnet unavailable (no usable verdict in the answer)")));
}

struct Problems(Mutex<Vec<Vec<String>>>);

impl Synthesis for Problems {
    fn check(&self, _project: &str, _files: &[String]) -> Vec<String> {
        let mut queue = self.0.lock().unwrap();
        if queue.len() > 1 { queue.remove(0) } else { queue.first().cloned().unwrap_or_default() }
    }
}

#[tokio::test]
async fn synthesis_problems_get_a_bounded_fix_pass_and_remaining_ones_become_warnings() {
    // Two independent steps run as a wave; without an Isolation nothing is harvested, so SYNTHESIZE is entered through
    // the machine's own transition: drive the phase directly from a record that is already in it.
    let mut r = rig(Script { drafts: [draft(&["a"])].into(), audits: [accept()].into(), ..Default::default() });
    let project = r.project.to_string_lossy().into_owned();
    let synth = Problems(Mutex::new(vec![vec!["a.py imports b.helper, which does not exist".into()], vec!["still wrong".into()]]));
    let record = {
        let world = r.world(None, None, Some(&synth));
        let mut m = Machine::new("g", &project, config(), world, Some("run-synth"), None, None);
        m.run().await; // a normal run to get a plan and a start checkpoint
        let mut again = m.record.clone();
        again.state = "synthesize".into();
        again.synthesis = Some(json!({"harvested": ["calc/a.txt"], "conflicts": [], "problems": []}));
        again.synth_fixes = 0;
        again
    };
    let world = r.world(None, None, Some(&synth));
    let mut m = Machine::with_record(record, config(), world);
    m.run().await;
    assert_eq!(m.record.synth_fixes, 1, "bounded by max_synth_fixes");
    assert!(m.record.warnings.iter().any(|w| w.starts_with("synthesis: 1 problem(s) left for harden/audit: still wrong")), "{:?}", m.record.warnings);
    assert_eq!(m.record.state, "done");
    let fix_prompt = r.script.lock().unwrap().prompts.iter().find(|(_, p)| p.contains("Make the parallel steps fit together")).map(|(_, p)| p.clone()).unwrap();
    assert!(fix_prompt.contains("a.py imports b.helper"));
}
