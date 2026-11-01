//! The real `cordata-run` binary driving the real Rust orchestrator against a fake `claude` CLI (a shell script that answers
//! as planner, executor and auditor by reading its prompt). Skipped where `sandbox-exec` cannot be applied.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use cordata_orchestrator::workers::cli::seatbelt_works;
use serde_json::{Value, json};

const FAKE_CLAUDE: &str = r##"#!/bin/bash
PROMPT="$(cat)"
echo "$@" >> "@DIR@/fake.log"
if [[ "$PROMPT" == *"You are the PLANNER"* ]]; then
  [[ -e "@DIR@/slow" ]] && sleep 30
  cat <<'STREAM'
{"type":"result","subtype":"success","total_cost_usd":0.01,"structured_output":{"goal":"write the files","acceptance_criteria":[{"id":"c1","type":"regex_match","description":"a has its content","target":"calc/a.txt::ok a"},{"id":"c2","type":"file_exists","description":"b exists","target":"calc/b.txt"}],"steps":[{"step_id":"s1","name":"write a","description":"write a","file_scope":["calc/a.txt"]},{"step_id":"s2","name":"write b","description":"write b","file_scope":["calc/b.txt"]}],"assumptions":["none"]}}
STREAM
elif [[ "$PROMPT" == *"You are the EXECUTOR"* ]]; then
  FILES="$(echo "$PROMPT" | sed -n 's/^Files you may change (project-relative): //p')"
  for f in ${FILES//,/ }; do mkdir -p "$(dirname "$f")"; printf 'ok %s' "$(basename "$f" .txt)" > "$f"; done
  echo '{"type":"result","subtype":"success","result":"done","total_cost_usd":0.02}'
elif [[ "$PROMPT" == *"You are the AUDITOR"* ]]; then
  echo '{"type":"result","subtype":"success","total_cost_usd":0.01,"structured_output":{"verdict":"accept","items":[]}}'
else
  echo '{"type":"result","subtype":"success","result":"ok","total_cost_usd":0.0}'
fi
"##;

struct Env {
    dir: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
    claude: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(project.join("calc")).unwrap();
    std::fs::write(project.join("calc/base.txt"), "base\n").unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let claude = dir.path().join("claude");
    std::fs::write(&claude, FAKE_CLAUDE.replace("@DIR@", dir.path().to_str().unwrap())).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    Env { dir, project, home, claude }
}

impl Env {
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cordata-run"));
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .env("HOME", self.dir.path())
            .env("CORDATA_HOME", &self.home)
            .env("CORDATA_CLAUDE_BIN", &self.claude)
            .env("TMPDIR", std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()));
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn run_ok(&self, extra: &[&str]) -> (Output, Vec<Value>) {
        let mut args = vec!["run", "write the files", "--cwd", self.project.to_str().unwrap(), "--allow-direct", "--workers", "claude_code", "--json"];
        args.extend(extra);
        let out = self.run(&args);
        let events = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect();
        (out, events)
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.project.join(rel)).ok()
    }
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn states(events: &[Value]) -> Vec<String> {
    events.iter().filter(|e| e["kind"] == "state").map(|e| e["state"].as_str().unwrap().to_owned()).collect()
}

#[test]
fn a_native_run_plans_executes_audits_and_can_be_read_back() {
    if !seatbelt_works() {
        return eprintln!("skipped: sandbox-exec cannot be applied here");
    }
    let e = env();
    let (out, events) = e.run_ok(&["--run-id", "run-e2e"]);
    assert_eq!(out.status.code(), Some(0), "stdout: {}\nstderr: {}", text(&out), String::from_utf8_lossy(&out.stderr));
    assert_eq!(states(&events), ["plan", "execute", "harden", "audit", "done"]);
    assert_eq!(e.read("calc/a.txt").unwrap(), "ok a");
    assert_eq!(e.read("calc/b.txt").unwrap(), "ok b");
    let kinds: Vec<&str> = events.iter().map(|ev| ev["kind"].as_str().unwrap()).collect();
    for k in ["dispatch", "usage", "step_dispatch", "step_result"] {
        assert!(kinds.contains(&k), "{k} missing in {kinds:?}");
    }
    // The fake CLI was invoked the way the Python worker invokes it.
    let log = std::fs::read_to_string(e.dir.path().join("fake.log")).unwrap();
    assert!(log.lines().all(|l| l.starts_with("-p --output-format stream-json --verbose --permission-mode")), "{log}");
    assert!(log.contains("--permission-mode plan") && log.contains("--permission-mode acceptEdits") && log.contains("--json-schema"));

    // status: JSON and text; the text equals what Python's renderer makes of the same data.
    let status = e.run(&["status", "--json"]);
    let data: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(data["runs"][0]["run_id"], "run-e2e");
    assert_eq!(data["runs"][0]["state"], "done");
    assert_eq!(data["pools"][0]["pool_id"], "claude_code");
    assert!(data["runs"][0]["usage"]["calls"].as_i64().unwrap() >= 4);
    let rendered = text(&e.run(&["status"]));
    assert!(rendered.starts_with("╭── Quota Pools ") && rendered.contains("run-e2e done: write the files") && rendered.contains("🟢 claude_code"), "{rendered}");

    // show: JSON is the record; text lists the plan, history, audit and usage.
    let shown = text(&e.run(&["show", "run-e2e"]));
    assert!(shown.starts_with("run-e2e  done  write the files\n\n# Plan plan-") && shown.contains("Audit: passed, verdict accept") && shown.contains("Conformance rounds: 0") && shown.contains("Usage:\n  role"), "{shown}");
    let record: Value = serde_json::from_slice(&e.run(&["show", "run-e2e", "--json"]).stdout).unwrap();
    assert_eq!(record["state"], "done");
    assert_eq!(record["plan"]["steps"][1]["status"], "completed");
    let missing = e.run(&["show", "run-nope"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no run run-nope"));

    // The same ledger, read by the Python tools: it renders what Rust reports.
    if let Some(py) = python(&e) {
        let want = py_run(&py, &e, "from orchestrator.status import render\nimport json,sys\nprint(render(json.load(sys.stdin), marks=True))", Some(&data));
        assert_eq!(rendered.trim_end(), want.trim_end());
        let py_show = py_cli(&py, &e, &["show", "run-e2e"]);
        assert_eq!(shown, py_show, "`show` prints what Python's prints");
    }
}

fn python(e: &Env) -> Option<PathBuf> {
    let _ = e;
    let backend = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../backend").canonicalize().ok()?;
    let py = backend.join(".venv/bin/python");
    py.exists().then_some(py)
}

fn py_run(py: &Path, e: &Env, code: &str, stdin: Option<&Value>) -> String {
    let backend = py.parent().unwrap().parent().unwrap().parent().unwrap();
    let mut child = Command::new(py)
        .current_dir(backend)
        .env("PYTHONPATH", backend)
        .env("CORDATA_HOME", &e.home)
        .env("CORDATA_STATE_DB", e.home.join("state.db"))
        .args(["-c", code])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), stdin.map_or(String::new(), Value::to_string).as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn py_cli(py: &Path, e: &Env, args: &[&str]) -> String {
    let backend = py.parent().unwrap().parent().unwrap().parent().unwrap();
    let out = Command::new(py)
        .current_dir(backend)
        .env("PYTHONPATH", backend)
        .env("CORDATA_HOME", &e.home)
        .env("CORDATA_STATE_DB", e.home.join("state.db"))
        .args(["-m", "orchestrator"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn a_pause_request_stops_the_run_and_resume_finishes_it() {
    if !seatbelt_works() {
        return eprintln!("skipped: sandbox-exec cannot be applied here");
    }
    let e = env();
    let control_dir = e.home.join("orchestrator/run-pause");
    std::fs::create_dir_all(&control_dir).unwrap();
    std::fs::write(control_dir.join("control.ndjson"), format!("{}\n", json!({"cmd": "pause", "by": "test"}))).unwrap();
    let (out, events) = e.run_ok(&["--run-id", "run-pause"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(states(&events).ends_with(&["paused".to_owned()]), "{:?}", states(&events));
    assert_eq!(e.read("calc/a.txt"), None);
    let record: Value = serde_json::from_slice(&e.run(&["show", "run-pause", "--json"]).stdout).unwrap();
    assert_eq!((record["state"].as_str(), record["resume_state"].as_str()), (Some("paused"), Some("plan")));

    // The request is answered; clear the file, then resume.
    std::fs::write(control_dir.join("control.ndjson"), "").unwrap();
    let resumed = e.run(&["resume", "run-pause", "--allow-direct"]);
    assert_eq!(resumed.status.code(), Some(0), "{} {}", text(&resumed), String::from_utf8_lossy(&resumed.stderr));
    assert!(text(&resumed).contains("run-pause: done"));
    assert_eq!(e.read("calc/b.txt").unwrap(), "ok b");
    assert_eq!(e.run(&["resume", "run-pause"]).status.code(), Some(0), "a finished run just reports itself");
    assert_eq!(e.run(&["resume", "run-nope"]).status.code(), Some(2));
}

#[test]
fn ctrl_c_stops_the_run_where_it_is_and_records_it() {
    if !seatbelt_works() {
        return eprintln!("skipped: sandbox-exec cannot be applied here");
    }
    let e = env();
    std::fs::write(e.dir.path().join("slow"), "").unwrap();
    let mut child = e
        .command()
        .args(["run", "slow goal", "--cwd", e.project.to_str().unwrap(), "--allow-direct", "--workers", "claude_code", "--run-id", "run-int"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    // Wait until the planner is running (its fake CLI has logged an invocation), then interrupt.
    while !e.dir.path().join("fake.log").exists() && started.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    // SAFETY: signalling our own child process.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(1));
    assert!(started.elapsed() < Duration::from_secs(25), "stopped, not waited for the 30s sleep");
    let record: Value = serde_json::from_slice(&e.run(&["show", "run-int", "--json"]).stdout).unwrap();
    assert_eq!(record["state"], "cancelled");
    assert!(record["reason"].as_str().unwrap().starts_with("cancelled — the run stopped where it was"));
}

#[test]
fn an_empty_ledger_reports_zero_not_negative_zero() {
    let e = env();
    let rendered = text(&e.run(&["status"]));
    assert!(rendered.contains("claude_code") && rendered.contains("   0%  ok        0/20 usd") && !rendered.contains("-0"), "{rendered}");
    let data: Value = serde_json::from_slice(&e.run(&["status", "--json"]).stdout).unwrap();
    assert_eq!(data["pools"][0]["used"].to_string(), "0.0");
    assert_eq!(data["runs"], json!([]));
    if let Some(py) = python(&e) {
        assert_eq!(rendered.trim_end(), py_run(&py, &e, "from orchestrator.status import render\nimport json,sys\nprint(render(json.load(sys.stdin), marks=True))", Some(&data)).trim_end());
    }
}
