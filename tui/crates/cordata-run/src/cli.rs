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
use cordata_orchestrator::machine::{self, Machine, RunConfig, RunRecord, World};
use cordata_orchestrator::plan::Plan;
use cordata_orchestrator::research::FileResearch;
use cordata_orchestrator::synthesizer::{CowIsolation, FileSynthesis};
use serde_json::Value;

use crate::config;
use crate::controls::FileControls;
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
    pub no_research: bool,
    pub research: bool,
    pub no_conformance: bool,
    pub no_tier2: bool,
    pub no_screen: bool,
    pub json: bool,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
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
    c.tier2 &= !args.no_tier2;
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

fn finish(record: &RunRecord, as_json: bool) -> i32 {
    if !as_json {
        println!("\n{}: {}{}", record.run_id, record.state, if record.reason.is_empty() { String::new() } else { format!(" — {}", record.reason) });
        for w in &record.warnings {
            println!("  warning: {w}");
        }
    }
    i32::from(record.state != "done")
}

pub async fn run(args: RunArgs) -> i32 {
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
    let (specs, dropped) = config::live_specs(config::specs(&requested), &environ);
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
    let controls = args.run_id.as_ref().map(|id| Arc::new(FileControls::new(Some(machine::artifacts_dir(&home, id).join("control.ndjson")))) as Arc<dyn machine::RunControls>);
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
    let mut machine = Machine::new(&args.goal, &cwd.to_string_lossy(), config, world, args.run_id.as_deref(), None, args.session_id.as_deref());
    machine.record.warnings.extend(notes);
    let record = drive(machine).await;
    finish(&record, args.json)
}

pub async fn resume(run_id: &str, workers: Option<String>, allow_direct: bool, max_usd: Option<f64>, as_json: bool) -> i32 {
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
    let Some(record) = machine::load(&probe, run_id) else {
        eprintln!("no run {run_id}");
        return 2;
    };
    drop(probe);
    let cwd = PathBuf::from(&record.cwd);
    let settings = settings_for(&cwd);
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
    let (specs, _) = config::live_specs(config::specs(&families), &environ);
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
    let controls = Arc::new(FileControls::new(Some(machine::artifacts_dir(&home, run_id).join("control.ndjson")))) as Arc<dyn machine::RunControls>;
    let mut run_config = config::run_config(&settings);
    if let Some(v) = max_usd {
        run_config.max_usd = v;
    }
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
        home,
    };
    let record = {
        let machine = Machine::with_record(record, run_config, world);
        if machine::TERMINAL.contains(&machine.record.state.as_str()) { machine.record.clone() } else { drive(machine).await }
    };
    println!("{}: {}{}", record.run_id, record.state, if record.reason.is_empty() { String::new() } else { format!(" — {}", record.reason) });
    i32::from(record.state != "done")
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

pub fn show_cmd(run_id: &str, as_json: bool) -> i32 {
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
