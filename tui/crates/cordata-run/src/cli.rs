//! The commands: `run`, `status`, `show`, `resume`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cordata_checkpoints::Store;
use cordata_doctor::Settings;
use cordata_ledger::Ledger;
use cordata_orchestrator::audit::{AuditEnv, PatternGuard};
use cordata_orchestrator::bandit::Bandit;
use cordata_orchestrator::checkpoints::GitCheckpoints;
use cordata_orchestrator::dispatch::Mode;
use cordata_orchestrator::evolution::{EvolutionConfig, EvolutionTarget, run_evolution_cycle};
use cordata_orchestrator::machine::{self, Machine, RunConfig, RunRecord, World};
use cordata_orchestrator::plan::Plan;
use cordata_orchestrator::research::FileResearch;
use cordata_orchestrator::synthesizer::{CowIsolation, FileSynthesis};
use serde_json::{json, Value};

use crate::config;
use crate::controls::{self, FileControls};
use crate::local_time;
use crate::roster::{LearnedRoster, bandit_path};
use crate::status;

/// Everything `run` can be told on the command line.
#[derive(Debug, Clone, Default)]
pub struct RunArgs {
    pub goal: String,
    pub cwd: PathBuf,
    pub workers: Option<String>,
    pub dispatch: Option<String>,
    pub allow_direct: bool,
    pub max_usd: Option<f64>,
    pub max_rounds: Option<i64>,
    pub max_parallel: Option<usize>,
    pub no_harden: bool,
    pub review_plan: bool,
    pub no_research: bool,
    pub research: bool,
    pub no_conformance: bool,
    pub no_tier2: bool,
    pub no_step_check: bool,
    pub no_screen: bool,
    pub json: bool,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub land: Option<String>,
    pub detach: bool,
}

/// Exit status of `run`/`resume` when no worker can run (`EX_CONFIG`): nothing has been done at that point.
pub const NO_NATIVE_WORKER: i32 = 78;

/// Exit status of `run`/`resume` when the coding CLIs have no egress (no vault, no `--allow-direct`): also `EX_CONFIG`.
pub const NO_EGRESS: i32 = 78;

/// Does the roster have a native worker whose CLI is installed?
fn has_native_worker(families: &[String], environ: &HashMap<String, String>) -> bool {
    config::specs(families).iter().any(|s| config::family_available(&s.candidate.family, environ))
}

pub fn cordata_home(environ: &HashMap<String, String>) -> PathBuf {
    environ.get("CORDATA_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(environ.get("HOME").cloned().unwrap_or_default()).join(".cordata"))
}

/// Python's `str()` of a JSON value, for the `k=v` event lines.
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        other => other.to_string(),
    }
}

fn kv(event: &Value, skip: &[&str]) -> String {
    event.as_object().into_iter().flatten().filter(|(k, _)| !skip.contains(&k.as_str())).map(|(k, v)| format!(" {k}={}", py_str(v))).collect()
}

/// `usage 4 call(s) · 12k tokens · $0.0310 — plan 1, execute 2, audit 1` — one line for a `usage` event.
pub fn usage_summary(rows: &[Value]) -> String {
    let totals = status::usage_totals(rows);
    let mut by_role: Vec<(String, i64)> = Vec::new();
    for row in rows {
        let role = row.get("role").map_or("None".to_owned(), py_str);
        let calls = row.get("calls").and_then(Value::as_i64).unwrap_or(0);
        match by_role.iter_mut().find(|(r, _)| *r == role) {
            Some((_, n)) => *n += calls,
            None => by_role.push((role, calls)),
        }
    }
    let roles = by_role.iter().map(|(r, n)| format!("{r} {n}")).collect::<Vec<_>>().join(", ");
    format!(
        "usage {} call(s) · {} tokens · ${:.4}{}",
        totals["calls"],
        status::compact(totals["tokens"].as_f64().unwrap_or(0.0)),
        totals["cost_usd"].as_f64().unwrap_or(0.0),
        if roles.is_empty() { String::new() } else { format!(" — {roles}") }
    )
}

/// The event listener for a run on a terminal: one line per state change or worker dispatch, or NDJSON.
pub fn printer(as_json: bool) -> machine::EventListener {
    // A usage event fires on every pod update; a line that says what the last one said is noise.
    let last_usage = std::sync::Mutex::new(String::new());
    Arc::new(move |event: Value| {
        let kind = event.get("kind").and_then(Value::as_str).unwrap_or("");
        if as_json {
            println!("{event}");
        } else if kind == "usage" {
            let pods = event.get("pods").and_then(Value::as_array).cloned().unwrap_or_default();
            let line = usage_summary(&pods);
            let mut last = last_usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if *last != line {
                println!("  {line}");
                *last = line;
            }
        } else if kind == "state" {
            println!("[{}]{}", event.get("state").and_then(Value::as_str).unwrap_or(""), kv(&event, &["kind", "run_id", "state", "ts"]));
        } else if ["step_dispatch", "step_result", "handoff", "rollback", "paused", "budget_exhausted"].contains(&kind) {
            println!("  {kind}{}", kv(&event, &["kind", "run_id"]));
        }
    })
}

fn usage_table(rows: &[Value]) -> Vec<String> {
    let header = ["role", "worker", "calls", "tokens in", "tokens out", "cost"].map(String::from);
    let int = |r: &Value, k: &str| r.get(k).and_then(Value::as_i64).unwrap_or(0).to_string();
    let body: Vec<[String; 6]> = rows
        .iter()
        .map(|r| {
            [
                r.get("role").map_or("None".into(), py_str),
                r.get("worker").map_or("None".into(), py_str),
                int(r, "calls"),
                int(r, "tokens_in"),
                int(r, "tokens_out"),
                format!("${:.4}", r.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0)),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..6).map(|i| std::iter::once(&header).chain(body.iter()).map(|row| row[i].chars().count()).max().unwrap_or(0)).collect();
    let fmt = |row: &[String; 6]| {
        let cells: Vec<String> = row.iter().enumerate().map(|(i, c)| if i < 2 { format!("{c:<w$}", w = widths[i]) } else { format!("{c:>w$}", w = widths[i]) }).collect();
        format!("  {}", cells.join("  "))
    };
    std::iter::once(fmt(&header)).chain(body.iter().map(fmt)).collect()
}

pub fn settings_for(cwd: &Path) -> Settings {
    Settings::load(cwd)
}

fn open_ledger(settings: &Settings) -> Result<Ledger, String> {
    Ledger::open_default(config::pools(settings)).map_err(|e| format!("cannot open the ledger: {e}"))
}

pub fn run_args_to_config(args: &RunArgs, base: RunConfig) -> RunConfig {
    let mut c = base;
    if let Some(v) = args.max_usd {
        c.max_usd = v;
    }
    c.harden &= !args.no_harden;
    c.review_plan |= args.review_plan;
    c.tier2 &= !args.no_tier2;
    c.step_check &= !args.no_step_check;
    c.screen &= !args.no_screen;
    if let Some(v) = args.max_rounds {
        c.max_rounds = v.max(0);
    }
    if let Some(v) = args.max_parallel {
        c.max_parallel = v.max(1);
    }
    c.conformance &= !args.no_conformance;
    if args.no_research {
        c.research = false;
    }
    if args.research {
        c.research = true;
    }
    if let Some(v) = &args.land {
        c.land = v.clone();
    }
    c
}

/// What the ledger's `orch_outcome` events say each past decision earned, oldest first.
fn logged_outcomes(ledger: &Ledger) -> Vec<cordata_orchestrator::dispatch::Logged> {
    let mut rows: Vec<cordata_orchestrator::dispatch::Logged> = ledger
        .recent_events("orch_outcome", 2000)
        .iter()
        .filter_map(|e| {
            let p = e.get("payload").unwrap_or(e);
            Some(cordata_orchestrator::dispatch::Logged {
                role: p.get("role")?.as_str()?.to_owned(),
                task_class: p.get("task_class").and_then(Value::as_str).unwrap_or("").to_owned(),
                arm: p.get("arm")?.as_str()?.to_owned(),
                ok: p.get("ok").and_then(Value::as_bool).unwrap_or(false),
                cost: p.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
            })
        })
        .collect();
    rows.reverse();
    rows
}

fn mode_of(name: &str, notes: &mut Vec<String>) -> Mode {
    match name {
        "off" => Mode::Off,
        "live" => Mode::Live,
        "shadow" => Mode::Shadow,
        other => {
            notes.push(format!("dispatch mode '{other}' is not available natively; using shadow"));
            Mode::Shadow
        }
    }
}

/// Drives `machine` to its end, or stops it where it is on Ctrl+C (the work stays).
async fn drive(mut machine: Machine<'_>) -> RunRecord {
    tokio::select! {
        _ = machine.run() => {}
        _ = tokio::signal::ctrl_c() => machine.interrupted(),
    }
    machine.record.clone()
}

fn finish(record: &RunRecord, as_json: bool, receipt: &str) -> i32 {
    if !as_json {
        // A run held for approval shows what it expects to build, so the decision is made on this screen.
        let held = record.history.last().and_then(|h| h.get("reason")).and_then(Value::as_str) == Some("plan_review");
        if let (true, Some(plan)) = (held && record.state == "paused", record.plan.as_ref().and_then(|p| Plan::from_json(p).ok())) {
            println!("\n{}", plan.render());
        }
        println!("\n{}: {}{}", record.run_id, record.state, if record.reason.is_empty() { String::new() } else { format!(" — {}", record.reason) });
        for w in &record.warnings {
            println!("  warning: {w}");
        }
        if !receipt.is_empty() {
            println!("{receipt}");
        }
    }
    i32::from(record.state != "done")
}

pub async fn run(args: RunArgs) -> i32 {
    let started = std::time::Instant::now();
    // Sandboxes a crashed run left behind; a day untouched is long past any live step.
    cordata_orchestrator::synthesizer::sweep(std::time::Duration::from_secs(24 * 3600), None);
    let environ: HashMap<String, String> = std::env::vars().collect();
    let cwd = std::fs::canonicalize(&args.cwd).unwrap_or_else(|_| args.cwd.clone());
    let settings = settings_for(&cwd);
    let mut environ = environ;
    if args.allow_direct {
        environ.insert("CORDATA_VAULT_DIRECT".into(), "1".into());
    }
    let requested: Vec<String> = match &args.workers {
        Some(list) => list.split(',').map(|f| f.trim().to_owned()).filter(|f| config::KNOWN_FAMILIES.contains(&f.as_str())).collect(),
        None => config::enabled_families(&settings),
    };
    let mut notes = Vec::new();
    if config::setting(&settings, "orchestrator.pools").is_some() {
        let bad = cordata_ledger::bad_pool_regexes(config::setting(&settings, "orchestrator.pools"));
        if !bad.is_empty() {
            notes.push(format!("orchestrator.pools: {} dropped, the regex does not compile; its models run under the default cap", bad.join(", ")));
        }
        let pooled = config::pools(&settings);
        let added: Vec<String> = cordata_ledger::default_pools().into_iter().filter(|d| requested.contains(&d.family) && !pooled.iter().any(|p| p.pool_id == d.pool_id)).map(|d| d.pool_id).collect();
        if !added.is_empty() {
            notes.push(format!("orchestrator.pools does not name {}: what it leaves out runs under those default caps (pass --workers to leave a family out)", added.join(", ")));
        }
    }
    let skipped = config::unsupported(&requested);
    if !skipped.is_empty() {
        notes.push(format!("{} has no worker and is left out of this run", skipped.join(", ")));
    }
    if !has_native_worker(&requested, &environ) {
        eprintln!("cordata-run: no worker is available for {} (claude or agy must be installed)", if requested.is_empty() { "an empty roster".to_owned() } else { requested.join(", ") });
        return NO_NATIVE_WORKER;
    }
    // Every worker shares one egress: refuse here, not after research and planning have each tried every worker.
    if let Err(reason) = config::egress_preflight(&settings, &environ) {
        eprintln!("cordata-run: {reason}");
        return NO_EGRESS;
    }
    let ledger = match open_ledger(&settings) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let home = cordata_home(&environ);
    let (specs, dropped) = config::live_specs(config::only_models(config::specs(&requested), &settings), &environ);
    notes.extend(dropped);
    let wanted = args.dispatch.clone().unwrap_or_else(|| config::setting(&settings, "orchestrator.dispatch").and_then(Value::as_str).unwrap_or("auto").to_owned());
    let mode = if wanted == "auto" {
        // Learned routing earns the right to decide: it acts once replay says it does as well as the default.
        let (mode, note) = cordata_orchestrator::dispatch::graduate(&logged_outcomes(&ledger), &specs, config::setting_f64(&settings, "orchestrator.lambda", 0.3));
        notes.push(note);
        mode
    } else {
        mode_of(&wanted, &mut notes)
    };
    let env_for_avail = environ.clone();
    let mut roster = LearnedRoster::new(
        specs,
        Bandit::open(&bandit_path(&home, &cwd)),
        &ledger,
        config::workers(&settings, &environ),
        mode,
        config::setting_f64(&settings, "orchestrator.lambda", 0.3),
        config::setting_f64(&settings, "orchestrator.mu", 0.5),
        move |spec| config::family_available(&spec.candidate.family, &env_for_avail),
    );
    let checkpoints = GitCheckpoints::new(Store::new(&home));
    // Always a known id, so a front door can find this run's control file to pause it.
    let run_id = args.run_id.clone().unwrap_or_else(|| format!("run-{}", machine::short_id()));
    let controls = Some(Arc::new(FileControls::new(Some(machine::artifacts_dir(&home, &run_id).join("control.ndjson")))) as Arc<dyn machine::RunControls>);
    let world = World {
        roster: &mut roster,
        ledger: &ledger,
        checkpoints: &checkpoints,
        audit_env: AuditEnv { checkpoints: &checkpoints, guard: &PatternGuard, environ: environ.clone() },
        research: Some(&FileResearch),
        synthesis: Some(&FileSynthesis),
        isolation: Some(Arc::new(CowIsolation::default())),
        on_event: Some(printer(args.json)),
        controls,
        home: home.clone(),
    };
    let config = run_args_to_config(&args, config::run_config(&settings));
    let mut machine = Machine::new(&args.goal, &cwd.to_string_lossy(), config, world, Some(run_id.as_str()), None, args.session_id.as_deref());
    machine.record.warnings.extend(notes);
    let record = drive(machine).await;
    let summary = crate::receipt::write(&home, &crate::receipt::build(&record, &ledger, &checkpoints, started.elapsed()));
    finish(&record, args.json, &summary)
}

pub async fn resume(run_id: &str, workers: Option<String>, allow_direct: bool, max_usd: Option<f64>, allow_weaker: bool, direction: Option<String>, as_json: bool) -> i32 {
    let started = std::time::Instant::now();
    let mut environ: HashMap<String, String> = std::env::vars().collect();
    if allow_direct {
        environ.insert("CORDATA_VAULT_DIRECT".into(), "1".into());
    }
    let probe = match Ledger::open_default(Vec::new()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot open the ledger: {e}");
            return 2;
        }
    };
    let Some(mut record) = machine::load(&probe, run_id) else {
        eprintln!("no run {run_id}");
        return 2;
    };
    drop(probe);
    // A run that is still going and has a step parked on a question takes the answer where it is: starting a second
    // process on the same run would fight the first over its files.
    if let Some(text) = direction.as_deref()
        && let Some(step) = live_parked_step(&record)
    {
        let home = cordata_home(&std::env::vars().collect());
        return match controls::answer(&machine::artifacts_dir(&home, run_id), &step, text.trim(), "person") {
            Ok(()) => {
                eprintln!("cordata-run: {run_id} is still going and step {step} is waiting; your answer was sent to it");
                0
            }
            Err(e) => {
                eprintln!("cordata-run: cannot reach {run_id}: {e}");
                2
            }
        };
    }
    if let Some(text) = direction.as_deref() && !apply_direction(&mut record, text) {
        if direction.as_deref().is_some_and(|d| d.trim().is_empty()) {
            eprintln!("cordata-run: the direction is empty");
        } else {
            eprintln!("cordata-run: {run_id} has no planned step to give the direction to yet (it stopped before planning finished); resume without --direction and it plans first");
        }
        return 2;
    }
    let cwd = PathBuf::from(&record.cwd);
    let settings = settings_for(&cwd);
    let mut run_config = config::run_config(&settings);
    // The limits a run was started with are its own: resuming must not quietly swap them for the settings' defaults.
    if let Some(then) = &record.config {
        (run_config.max_usd, run_config.max_rounds) = (then.max_usd, then.max_rounds);
    }
    if let Some(v) = max_usd {
        run_config.max_usd = v;
    }
    let drift = machine::config_drift(&record, &run_config);
    if !drift.weakened.is_empty() && !allow_weaker && !machine::TERMINAL.contains(&record.state.as_str()) {
        eprintln!(
            "cordata-run: {run_id} was started with {} on and these settings turn it off. Fix the settings, or pass --allow-weaker-guards to continue anyway.",
            drift.weakened.join(", ")
        );
        return 2;
    }
    let ledger = match open_ledger(&settings) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let home = cordata_home(&environ);
    let families = match workers.as_deref() {
        Some(w) => w.split(',').map(str::trim).filter(|f| config::KNOWN_FAMILIES.contains(f)).map(str::to_owned).collect(),
        None => config::enabled_families(&settings),
    };
    if !machine::TERMINAL.contains(&record.state.as_str()) && !has_native_worker(&families, &environ) {
        eprintln!("cordata-run: no worker is available to resume {run_id}");
        return NO_NATIVE_WORKER;
    }
    if !machine::TERMINAL.contains(&record.state.as_str()) && let Err(reason) = config::egress_preflight(&settings, &environ) {
        eprintln!("cordata-run: {reason}");
        return NO_EGRESS;
    }
    let (specs, _) = config::live_specs(config::only_models(config::specs(&families), &settings), &environ);
    let env_for_avail = environ.clone();
    let mut roster = LearnedRoster::new(
        specs,
        Bandit::open(&bandit_path(&home, &cwd)),
        &ledger,
        config::workers(&settings, &environ),
        Mode::Shadow,
        config::setting_f64(&settings, "orchestrator.lambda", 0.3),
        config::setting_f64(&settings, "orchestrator.mu", 0.5),
        move |spec| config::family_available(&spec.candidate.family, &env_for_avail),
    );
    let checkpoints = GitCheckpoints::new(Store::new(&home));
    // Nothing is running, so every request already in the control file was answered by the pause or stop that ended the
    // last process. Replaying a stale `pause` would stop the resumed run at its first safe point.
    let control_file = machine::artifacts_dir(&home, run_id).join("control.ndjson");
    if control_file.exists() {
        let _ = std::fs::write(&control_file, "");
    }
    let controls = Arc::new(FileControls::new(Some(control_file))) as Arc<dyn machine::RunControls>;
    let world = World {
        roster: &mut roster,
        ledger: &ledger,
        checkpoints: &checkpoints,
        audit_env: AuditEnv { checkpoints: &checkpoints, guard: &PatternGuard, environ },
        research: Some(&FileResearch),
        synthesis: Some(&FileSynthesis),
        isolation: Some(Arc::new(CowIsolation::default())),
        on_event: Some(printer(as_json)),
        controls: Some(controls),
        home: home.clone(),
    };
    let finished = machine::TERMINAL.contains(&record.state.as_str());
    let record = {
        let machine = Machine::with_record(record, run_config, world);
        if finished { machine.record.clone() } else { drive(machine).await }
    };
    // An already finished run keeps the receipt it wrote when it finished.
    let summary = if finished { String::new() } else { crate::receipt::write(&home, &crate::receipt::build(&record, &ledger, &checkpoints, started.elapsed())) };
    finish(&record, as_json, &summary)
}

pub fn status_cmd(as_json: bool) -> i32 {
    let ledger = match Ledger::open_default(config::pools(&settings_for(&std::env::current_dir().unwrap_or_default()))) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot open the ledger: {e}");
            return 2;
        }
    };
    let data = status::status_data(&ledger, 6);
    if as_json {
        println!("{}", serde_json::to_string_pretty(&data).expect("serializes"));
    } else {
        println!("{}", status::render(&data, true));
    }
    0
}

/// The step of a live run that is parked on a question and still inside its wait: where an answer can go to the run.
pub fn live_parked_step(record: &machine::RunRecord) -> Option<String> {
    if machine::TERMINAL.contains(&record.state.as_str()) || record.state == "paused" {
        return None;
    }
    let now = cordata_checkpoints::now();
    record.plan.as_ref()?.get("steps")?.as_array()?.iter().find_map(|s| {
        let waiting = s.get("status").and_then(Value::as_str) == Some("blocked") && s.get("blocked_until").and_then(Value::as_f64).is_some_and(|until| until > now);
        waiting.then(|| s.get("step_id").and_then(Value::as_str).map(str::to_owned)).flatten()
    })
}

/// Puts the person's words on the step that runs next, in the plan the worker is given, and notes it on the run.
/// The text is theirs, typed in the window, so it rides the plan (trusted) and not the fenced worker data.
pub fn apply_direction(record: &mut machine::RunRecord, text: &str) -> bool {
    let text: String = text.trim().chars().take(2000).collect();
    if text.is_empty() {
        return false;
    }
    let Some(steps) = record.plan.as_mut().and_then(|p| p.get_mut("steps")).and_then(Value::as_array_mut) else { return false };
    // A step parked on a question is the one the person is answering; else the first not yet completed.
    let parked = steps.iter().position(|s| s.get("status").and_then(Value::as_str) == Some("blocked") && s.get("blocked_reason").is_some_and(|r| !r.is_null()));
    let Some(at) = parked.or_else(|| steps.iter().position(|s| s.get("status").and_then(Value::as_str) != Some("completed"))) else { return false };
    let step = &mut steps[at];
    if parked.is_some() {
        step["status"] = Value::String("pending".into());
        step["blocked_reason"] = Value::Null;
        step["blocked_until"] = Value::Null;
    }
    let id = step.get("step_id").and_then(Value::as_str).unwrap_or("").to_owned();
    let before = step.get("description").and_then(Value::as_str).unwrap_or("").to_owned();
    step["description"] = Value::String(format!("{before}\n\nDirection from the person (follow it where it differs from the above): {text}"));
    record.warnings.push(format!("direction added by the person for step {id}"));
    true
}

pub fn show_cmd(run_id: &str, as_json: bool, step: Option<&str>, attempt: Option<usize>, follow: bool, timeline: bool) -> i32 {
    if timeline {
        return show_timeline(run_id, as_json);
    }
    if let Some(step_id) = step {
        return show_step_tape(run_id, step_id, attempt, follow, as_json);
    }
    let Ok(ledger) = Ledger::open_default(Vec::new()) else {
        eprintln!("cannot open the ledger");
        return 2;
    };
    let Some(record) = machine::load(&ledger, run_id) else {
        eprintln!("no run {run_id}");
        return 2;
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&record.to_json()).expect("serializes"));
        return 0;
    }
    println!("{}  {}  {}\n", record.run_id, record.state, record.goal);
    if let Some(research) = &record.research {
        println!("Research: {} verified finding(s)\n", research.get("findings").and_then(Value::as_array).map_or(0, Vec::len));
    }
    if let Some(plan) = record.plan.as_ref().and_then(|p| Plan::from_json(p).ok()) {
        println!("{}", plan.render());
    }
    for entry in &record.history {
        println!("  {} {}", local_time(entry.get("ts").and_then(Value::as_f64).unwrap_or(0.0), "%H:%M:%S"), entry.get("state").and_then(Value::as_str).unwrap_or(""));
    }
    if let Some(audit) = &record.audit {
        let verdict = audit.get("verdict").and_then(Value::as_str).filter(|v| !v.is_empty());
        println!("\nAudit: {}{}", if audit.get("tier1_ok").and_then(Value::as_bool).unwrap_or(false) { "passed" } else { "failed" }, verdict.map_or(String::new(), |v| format!(", verdict {v}")));
        for failure in audit.get("failures").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            println!("  - {}", failure.lines().next().unwrap_or(""));
        }
    }
    println!("\nConformance rounds: {}", record.rounds);
    if !record.usage.is_empty() {
        println!("\nUsage:");
        for line in usage_table(&record.usage) {
            println!("{line}");
        }
    }
    0
}

pub fn show_step_tape(run_id: &str, step_id: &str, attempt: Option<usize>, follow: bool, as_json: bool) -> i32 {
    let home = cordata_home(&std::env::vars().collect());
    let art_dir = machine::artifacts_dir(&home, run_id);
    let path = cordata_orchestrator::tape::tape_path(&art_dir, step_id, attempt);
    if !path.exists() && !follow {
        eprintln!("no tape found for step {step_id} in run {run_id} at {}", path.display());
        return 2;
    }

    if !follow {
        match cordata_orchestrator::tape::read_tape(&path) {
            Ok(events) => {
                if as_json {
                    println!("{}", serde_json::to_string_pretty(&events).unwrap());
                } else {
                    for ev in &events {
                        print_tape_event(ev);
                    }
                }
                0
            }
            Err(e) => {
                eprintln!("error reading tape: {e}");
                2
            }
        }
    } else {
        use std::io::{BufRead, BufReader, Seek, SeekFrom};
        let mut pos = 0u64;
        let mut finished = false;
        println!("== Tailing step {step_id} (run {run_id}) ==");
        while !finished {
            if path.exists()
                && let Ok(mut file) = std::fs::File::open(&path)
            {
                {
                    let _ = file.seek(SeekFrom::Start(pos));
                    let mut reader = BufReader::new(file);
                    let mut line = String::new();
                    while let Ok(n) = reader.read_line(&mut line) {
                        if n == 0 {
                            break;
                        }
                        pos += n as u64;
                        if let Ok(val) = serde_json::from_str::<Value>(&line) {
                            if as_json {
                                println!("{}", line.trim());
                            } else {
                                print_tape_event(&val);
                            }
                            if val.get("kind").and_then(Value::as_str) == Some("result") {
                                finished = true;
                                break;
                            }
                        }
                        line.clear();
                    }
                }
            }
            if !finished {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        0
    }
}

fn print_tape_event(ev: &Value) {
    let kind = ev.get("kind").and_then(Value::as_str).unwrap_or("event");
    let ts = ev.get("at").and_then(Value::as_f64).unwrap_or(0.0);
    let time_str = local_time(ts, "%H:%M:%S");
    let payload = ev.get("payload").unwrap_or(&Value::Null);

    match kind {
        "prompt" => {
            let role = payload.get("role").and_then(Value::as_str).unwrap_or("worker");
            let model = payload.get("model").and_then(Value::as_str).unwrap_or("default");
            println!("{time_str} [prompt] role={role} model={model}");
        }
        "tool_use" => {
            let tool = payload.get("tool").or_else(|| payload.get("name")).and_then(Value::as_str).unwrap_or("tool");
            let args = payload.get("args").or_else(|| payload.get("input")).unwrap_or(&Value::Null);
            println!("{time_str} [tool_use] {tool}: {args}");
        }
        "text" => {
            let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
            for line in text.lines() {
                println!("{time_str} [text] {line}");
            }
        }
        "progress" => {
            println!("{time_str} [progress] {payload}");
        }
        "result" => {
            let status = payload.get("status").and_then(Value::as_str).unwrap_or("unknown");
            let cost = payload.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
            let in_tok = payload.get("tokens_in").and_then(Value::as_i64).unwrap_or(0);
            let out_tok = payload.get("tokens_out").and_then(Value::as_i64).unwrap_or(0);
            println!("{time_str} [result] status={status} in={in_tok} out={out_tok} cost=${cost:.4}");
        }
        "omitted" => {
            let count = ev.get("omitted_count").and_then(Value::as_i64).unwrap_or(0);
            println!("{time_str} ... [omitted {count} intermediate lines] ...");
        }
        other => {
            println!("{time_str} [{other}] {payload}");
        }
    }
}

pub fn show_timeline(run_id: &str, as_json: bool) -> i32 {
    let home = cordata_home(&std::env::vars().collect());
    let art_dir = machine::artifacts_dir(&home, run_id);
    let steps_dir = art_dir.join("steps");
    let trace_id = cordata_orchestrator::tape::trace_id_for_run(run_id);
    let mut spans = Vec::new();

    spans.push(json!({
        "name": "invoke_agent",
        "span_id": trace_id[..16].to_string(),
        "trace_id": trace_id,
        "parent_span_id": null,
        "kind": "agent",
    }));

    if let Ok(entries) = std::fs::read_dir(&steps_dir) {
        let mut sorted_entries: Vec<_> = entries.flatten().collect();
        sorted_entries.sort_by_key(|e| e.file_name());
        for entry in sorted_entries {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("ndjson")
                && let Ok(events) = cordata_orchestrator::tape::read_tape(&path)
                && let Some(first) = events.first()
            {
                {
                    {
                        let step_id = first.get("step_id").and_then(Value::as_str).unwrap_or("step");
                        let attempt = first.get("attempt").and_then(Value::as_i64).unwrap_or(1);
                        let span_id = first.get("span_id").and_then(Value::as_str).unwrap_or("").to_string();
                        let start_at = first.get("at").and_then(Value::as_f64).unwrap_or(0.0);
                        let last = events.last().unwrap_or(first);
                        let end_at = last.get("at").and_then(Value::as_f64).unwrap_or(start_at);
                        let dur = (end_at - start_at).max(0.0);
                        let status = last.get("payload").and_then(|p| p.get("status")).and_then(Value::as_str).unwrap_or("completed");

                        spans.push(json!({
                            "name": format!("step:{step_id}:attempt_{attempt}"),
                            "span_id": span_id,
                            "trace_id": trace_id,
                            "parent_span_id": trace_id[..16].to_string(),
                            "duration_s": (dur * 100.0).round() / 100.0,
                            "status": status,
                            "event_count": events.len(),
                        }));
                    }
                }
            }
        }
    }

    if as_json {
        println!("{}", serde_json::to_string_pretty(&spans).unwrap());
        return 0;
    }

    println!("Timeline for run {run_id} (trace: {trace_id}):");
    println!("  [0.00s] ◉ invoke_agent");
    for span in &spans[1..] {
        let name = span.get("name").and_then(Value::as_str).unwrap_or("");
        let dur = span.get("duration_s").and_then(Value::as_f64).unwrap_or(0.0);
        let status = span.get("status").and_then(Value::as_str).unwrap_or("");
        let count = span.get("event_count").and_then(Value::as_i64).unwrap_or(0);
        println!("    ├─ ▷ {name:<24} ({dur:5.2}s) [{status}] {count} events");
    }
    0
}

pub fn export_cmd(run_id: &str, _otlp: bool) -> i32 {
    let home = cordata_home(&std::env::vars().collect());
    let art_dir = machine::artifacts_dir(&home, run_id);
    let trace_id = cordata_orchestrator::tape::trace_id_for_run(run_id);
    let root_span_id = trace_id[..16].to_string();

    let mut otlp_spans = Vec::new();
    otlp_spans.push(json!({
        "traceId": trace_id,
        "spanId": root_span_id,
        "name": "invoke_agent",
        "kind": 1,
        "attributes": [
            {"key": "gen_ai.system", "value": {"stringValue": "cordata"}},
            {"key": "gen_ai.operation.name", "value": {"stringValue": "orchestrator_run"}},
        ]
    }));

    let steps_dir = art_dir.join("steps");
    if let Ok(entries) = std::fs::read_dir(&steps_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("ndjson")
                && let Ok(events) = cordata_orchestrator::tape::read_tape(&path)
            {
                {
                    for ev in events {
                        let kind = ev.get("kind").and_then(Value::as_str).unwrap_or("");
                        let span_id = ev.get("span_id").and_then(Value::as_str).unwrap_or(&root_span_id);
                        if matches!(kind, "tool_use" | "result" | "prompt") {
                            otlp_spans.push(json!({
                                "traceId": trace_id,
                                "spanId": span_id,
                                "parentSpanId": root_span_id,
                                "name": format!("execute_{kind}"),
                                "kind": 3,
                                "attributes": [
                                    {"key": "gen_ai.system", "value": {"stringValue": "cordata"}},
                                    {"key": "gen_ai.event.kind", "value": {"stringValue": kind}},
                                ]
                            }));
                        }
                    }
                }
            }
        }
    }

    let otlp = json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [
                    {"key": "service.name", "value": {"stringValue": "cordata"}}
                ]
            },
            "scopeSpans": [{
                "scope": {"name": "cordata-orchestrator", "version": "0.5.0"},
                "spans": otlp_spans
            }]
        }]
    });
    println!("{}", serde_json::to_string_pretty(&otlp).unwrap());
    0
}

pub fn evolve_cmd(target: &str, threshold: f64, dry_run: bool, as_json: bool, cwd: Option<PathBuf>, mutate_cmd: Option<String>, propose: Option<String>, propose_usd: f64, propose_workers: Option<String>) -> i32 {
    let target: EvolutionTarget = target.parse().unwrap_or(EvolutionTarget::Repomap);
    let project_root = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let config = EvolutionConfig {
        target,
        threshold,
        dry_run,
        project_root,
        mutate_cmd,
        propose: propose.and_then(|d| {
            let exe = std::env::current_exe().ok()?;
            Some(cordata_orchestrator::evolution::ModelProposal { exe, direction: (!d.trim().is_empty()).then_some(d), max_usd: propose_usd, workers: propose_workers.clone() })
        }),
    };
    if !as_json {
        println!("Cordata Recursive Self-Improvement Engine (RSI)");
        println!("Target: {} · Threshold: {:+.4} · Dry run: {}\n", config.target, config.threshold, config.dry_run);
    }

    match run_evolution_cycle(config) {
        Ok(report) => {
            if as_json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
                return if report.promoted { 0 } else { 1 };
            }
            println!("Baseline Fitness:  {:.4} ({})", report.baseline_fitness.primary, report.baseline_fitness.details);
            if let Some(cand) = &report.candidate_fitness {
                println!("Candidate Fitness: {:.4} ({})", cand.primary, cand.details);
                println!("Delta Fitness:     {:+.4} (latency: {:.1}ms)", report.delta_fitness, cand.latency_ms);
            }
            println!("\nIron Gate Results:");
            for gate in &report.gates {
                let mark = if gate.passed { "✓ PASS" } else { "✗ FAIL" };
                println!("  Gate {}: {:<30} [{}] — {}", gate.gate_number, gate.name, mark, gate.output);
            }
            println!();
            if report.promoted {
                println!("PROMOTED: Candidate passed all 4 Iron Gates and improved fitness by {:+.4}!", report.delta_fitness);
                0
            } else {
                println!("NOT PROMOTED: Candidate did not satisfy all 4 Iron Gates (or dry-run requested).");
                1
            }
        }
        Err(e) => {
            eprintln!("Evolution cycle error: {e}");
            2
        }
    }
}

#[cfg(test)]
mod direction_tests {
    use super::*;
    use serde_json::json;

    fn record_with(steps: Value) -> machine::RunRecord {
        let mut r = machine::RunRecord::from_json(&json!({"run_id": "run-t", "goal": "g", "cwd": "/p"})).expect("a record");
        r.plan = Some(json!({"plan_id": "p", "goal": "g", "steps": steps}));
        r
    }

    #[test]
    fn a_direction_goes_on_the_first_step_not_yet_completed() {
        let mut r = record_with(json!([
            {"step_id": "s1", "name": "a", "description": "do a", "status": "completed"},
            {"step_id": "s2", "name": "b", "description": "do b", "status": "pending"},
        ]));
        assert!(apply_direction(&mut r, "  keep it small  "));
        let steps = r.plan.as_ref().unwrap()["steps"].as_array().unwrap().clone();
        assert_eq!(steps[0]["description"], "do a", "a finished step is left alone");
        let d = steps[1]["description"].as_str().unwrap();
        assert!(d.starts_with("do b\n\nDirection from the person") && d.ends_with("keep it small"));
        assert_eq!(r.warnings, vec!["direction added by the person for step s2".to_owned()]);
    }

    #[test]
    fn a_direction_answers_the_step_parked_on_a_question_before_any_other() {
        let mut r = record_with(json!([
            {"step_id": "s1", "name": "a", "description": "do a", "status": "pending"},
            {"step_id": "s2", "name": "b", "description": "do b", "status": "blocked", "blocked_reason": "asks: sqlite or postgres?", "blocked_until": 5.0},
        ]));
        assert!(apply_direction(&mut r, "use sqlite"));
        let steps = r.plan.as_ref().unwrap()["steps"].as_array().unwrap().clone();
        assert_eq!(steps[0]["description"], "do a", "the first unfinished step is not the one that asked");
        assert!(steps[1]["description"].as_str().unwrap().ends_with("use sqlite"));
        assert_eq!((steps[1]["status"].clone(), steps[1]["blocked_reason"].clone()), (json!("pending"), Value::Null));
        assert_eq!(r.warnings, vec!["direction added by the person for step s2".to_owned()]);
    }

    #[test]
    fn a_live_run_waiting_on_a_question_takes_the_answer_through_its_control_file() {
        let in_future = cordata_checkpoints::now() + 300.0;
        let mut r = record_with(json!([{"step_id": "s1", "name": "a", "description": "x", "status": "blocked", "blocked_reason": "asks: q?", "blocked_until": in_future}]));
        r.state = "execute".into();
        assert_eq!(live_parked_step(&r).as_deref(), Some("s1"));
        r.state = "paused".into();
        assert_eq!(live_parked_step(&r), None, "a paused run has no process to answer: resume it with the direction");
        r.state = "execute".into();
        r.plan.as_mut().unwrap()["steps"][0]["blocked_until"] = json!(1.0);
        assert_eq!(live_parked_step(&r), None, "its wait is over: the run moved on");
        let dir = tempfile::tempdir().unwrap();
        controls::answer(dir.path(), "s1", "use sqlite", "person").unwrap();
        let file = FileControls::new(Some(dir.path().join("control.ndjson")));
        let req = machine::RunControls::pending(&file).unwrap();
        assert_eq!((req.kind.as_str(), req.step.as_deref(), req.text.as_deref()), ("answer", Some("s1"), Some("use sqlite")));
    }

    #[test]
    fn an_empty_direction_or_a_finished_plan_changes_nothing() {
        let mut r = record_with(json!([{"step_id": "s1", "name": "a", "description": "x", "status": "completed"}]));
        assert!(!apply_direction(&mut r, "go on"), "no step left");
        let mut r = record_with(json!([{"step_id": "s1", "name": "a", "description": "x", "status": "pending"}]));
        assert!(!apply_direction(&mut r, "   "));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn show_step_tape_and_timeline_read_from_tape() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let run_id = "run-tape-test";
        let art_dir = machine::artifacts_dir(home, run_id);
        let tape = cordata_orchestrator::tape::StepTape::new(&art_dir, run_id, "s1", 1, None, None);
        tape.record_prompt("Build feature", "t1", Some("claude"), "execute");
        tape.record_worker_event("tool_use", &json!({"tool": "read_file", "path": "main.rs"}));
        tape.record_result("completed", &json!({"cost_usd": 0.01, "tokens_in": 100, "tokens_out": 50}));

        let tape_path = cordata_orchestrator::tape::tape_path(&art_dir, "s1", Some(1));
        assert!(tape_path.exists());
        let events = cordata_orchestrator::tape::read_tape(&tape_path).unwrap();
        assert_eq!(events.len(), 3);
    }
}
