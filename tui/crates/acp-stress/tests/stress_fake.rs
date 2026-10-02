//! Runs the real `acp-stress` binary against the scripted fake agent: one
//! JSONL line per turn, sane latencies, a deliberately broken scenario is
//! reported (not hung on), and no backend processes are left behind.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn fake_agent_copy(tag: &str) -> PathBuf {
    // A uniquely named copy lets the orphan check `pgrep` for exactly this run.
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../acp-core/tests/fixtures/fake_agent.py");
    let dest =
        std::env::temp_dir().join(format!("acp_stress_fake_{tag}_{}.py", std::process::id()));
    std::fs::copy(source, &dest).unwrap();
    dest
}

fn orphans(script: &PathBuf) -> usize {
    let out = Command::new("pgrep")
        .arg("-f")
        .arg(script)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    lines: Vec<Value>,
}

fn run(tag: &str, scenarios: &str, extra: &[&str]) -> (Run, PathBuf) {
    let script = fake_agent_copy(tag);
    let scenario_file =
        std::env::temp_dir().join(format!("acp_stress_{tag}_{}.json", std::process::id()));
    let out_file =
        std::env::temp_dir().join(format!("acp_stress_{tag}_{}.jsonl", std::process::id()));
    std::fs::write(&scenario_file, scenarios).unwrap();
    let _ = std::fs::remove_file(&out_file);

    let output = Command::new(env!("CARGO_BIN_EXE_acp-stress"))
        .arg(&scenario_file)
        .args([
            "--server-script",
            script.to_str().unwrap(),
            "--out",
            out_file.to_str().unwrap(),
        ])
        .args(extra)
        .output()
        .unwrap();
    let lines = std::fs::read_to_string(&out_file)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (
        Run {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            lines,
        },
        script,
    )
}

#[test]
fn concurrent_mixed_scenarios_produce_one_sane_line_per_turn() {
    let (run, script) = run(
        "mixed",
        r#"{"scenarios": [
            {"name": "plain", "prompts": ["pong", "pong"], "mode": "auto", "repeat": 4},
            {"name": "gated-allow", "prompts": ["shell: echo hi"], "mode": "build", "approval_policy": "auto_allow", "repeat": 3},
            {"name": "gated-script", "prompts": ["shell: echo x"], "approval_policy": {"shell": "reject"}},
            {"name": "tuned", "prompts": ["pong"], "model": "fake-model-b", "temperature": "creative", "mode": "plan"}
        ]}"#,
        &["--concurrency", "6"],
    );
    assert!(run.status.success(), "stdout:\n{}", run.stdout);

    // 4*2 + 3*1 + 1 + 1
    assert_eq!(run.lines.len(), 13, "one JSONL line per prompt-turn");
    for line in &run.lines {
        assert_eq!(line["stop_reason"], "end_turn", "{line}");
        assert!(line["error"].is_null(), "{line}");
        let latency = line["latency_ms"].as_u64().unwrap();
        assert!(latency < 10_000, "implausible latency {latency}");
        assert!(line["ttft_ms"].as_u64().unwrap() <= latency);
        assert!(line["session_id"].is_string());
    }

    let allowed: Vec<_> = run
        .lines
        .iter()
        .filter(|l| l["scenario"].as_str().unwrap().starts_with("gated-allow"))
        .collect();
    assert_eq!(allowed.len(), 3);
    for line in allowed {
        assert_eq!(line["permission_requests"], 1);
        assert_eq!(line["permissions"][0]["decision"], "allow");
        assert_eq!(line["tool_calls"][0]["status"], "completed");
    }
    let scripted = run
        .lines
        .iter()
        .find(|l| l["scenario"] == "gated-script")
        .unwrap();
    assert_eq!(scripted["permissions"][0]["decision"], "reject");
    assert_eq!(scripted["tool_calls"][0]["status"], "failed");

    // auto mode: the backend never asks, so the policy is never consulted.
    assert!(
        run.lines
            .iter()
            .filter(|l| l["scenario"].as_str().unwrap().starts_with("plain"))
            .all(|l| l["permission_requests"] == 0)
    );

    for needle in [
        "scenarios: 9 (0 errored)",
        "turns: 13 (0 unhealthy)",
        "end_turn=13",
        "p50",
        "p95",
        "most common: shell",
    ] {
        assert!(
            run.stdout.contains(needle),
            "missing {needle:?} in\n{}",
            run.stdout
        );
    }
    assert_eq!(
        orphans(&script),
        0,
        "no backend processes may outlive the run"
    );
}

#[test]
fn broken_scenarios_are_reported_not_hung_on() {
    let (run, script) = run(
        "broken",
        r#"[
            {"name": "healthy", "prompts": ["pong"], "mode": "auto"},
            {"name": "crashes", "prompts": ["pong", "crash", "pong"], "mode": "auto"},
            {"name": "bad-model", "prompts": ["pong"], "model": "no-such-model"},
            {"name": "errors", "prompts": ["error", "pong"]}
        ]"#,
        &["--concurrency", "4"],
    );
    assert!(
        !run.status.success(),
        "any errored session must fail the run"
    );

    let by = |scenario: &str| -> Vec<&Value> {
        run.lines
            .iter()
            .filter(|l| l["scenario"] == scenario)
            .collect()
    };
    assert!(by("healthy")[0]["error"].is_null());

    // crash mid-turn: the crashing turn has the error; the turn after it is "skipped".
    let crashes = by("crashes");
    assert_eq!(
        crashes.len(),
        3,
        "a line for every prompt even after the session dies"
    );
    assert!(crashes[0]["error"].is_null());
    assert!(crashes[1]["error"].is_string());
    assert!(crashes[2]["error"].as_str().unwrap().starts_with("skipped"));

    // unavailable model is caught client-side before any prompt runs
    let bad = by("bad-model");
    assert!(
        bad[0]["error"].as_str().unwrap().contains("not offered"),
        "{:?}",
        bad[0]
    );

    // a failed prompt is a turn error but doesn't kill the session
    let errors = by("errors");
    assert!(
        errors[0]["error"]
            .as_str()
            .unwrap()
            .contains("fake agent exploded")
    );
    assert!(errors[1]["error"].is_null() && errors[1]["stop_reason"] == "end_turn");

    assert!(
        run.stdout.contains("problems:") && run.stdout.contains("crashes:"),
        "{}",
        run.stdout
    );
    assert_eq!(orphans(&script), 0);
}

#[test]
fn a_hung_turn_times_out_and_is_cancelled_instead_of_blocking_the_batch() {
    let started = std::time::Instant::now();
    let (run, script) = run(
        "timeout",
        r#"[{"name": "slow", "prompts": ["slow"], "mode": "auto"}, {"name": "fast", "prompts": ["pong"], "mode": "auto"}]"#,
        &["--turn-timeout", "1"],
    );
    assert!(started.elapsed().as_secs() < 30);
    assert!(!run.status.success());
    let slow = run.lines.iter().find(|l| l["scenario"] == "slow").unwrap();
    assert!(
        slow["error"].as_str().unwrap().contains("timed out"),
        "{slow}"
    );
    assert_eq!(slow["stop_reason"], "cancelled");
    let fast = run.lines.iter().find(|l| l["scenario"] == "fast").unwrap();
    assert!(fast["error"].is_null());
    assert_eq!(orphans(&script), 0);
}

#[test]
fn unusable_backend_fails_the_run() {
    // An undiscoverable backend is a hard, non-zero failure (unexpected stop reasons are covered in report.rs).
    let (run, _script) = run(
        "badstart",
        r#"[{"name": "x", "prompts": ["pong"], "mode": "auto"}]"#,
        &["--startup-timeout", "5", "--backend-root", "/nonexistent"],
    );
    assert!(!run.status.success());
}
