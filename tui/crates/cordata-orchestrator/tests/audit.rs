//! Tier 1's commands run for real under the audit Seatbelt profile (skipped where `sandbox-exec` cannot be applied).

use std::collections::HashMap;

use cordata_orchestrator::audit::{PatternGuard, audit_profile, run_command};
use cordata_orchestrator::workers::cli::seatbelt_works;

fn environ() -> HashMap<String, String> {
    ["PATH", "HOME", "TMPDIR"].iter().filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_owned(), v))).collect()
}

fn project() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap().to_string_lossy().into_owned();
    (dir, root)
}

#[tokio::test]
async fn exit_codes_are_compared_with_the_expected_one() {
    if !seatbelt_works() {
        return eprintln!("skipped: sandbox-exec cannot be applied here");
    }
    let (_d, root) = project();
    let (ok, detail) = run_command(&root, "echo hi", &PatternGuard, 20.0, 0, &environ()).await;
    assert!(ok && detail.starts_with("exit 0 (expected 0) in "), "{detail}");
    let (ok, detail) = run_command(&root, "echo out; echo err >&2; exit 3", &PatternGuard, 20.0, 0, &environ()).await;
    assert!(!ok && detail.contains("exit 3 (expected 0)") && detail.ends_with("\nout\nerr"), "{detail:?}");
    let (ok, _) = run_command(&root, "exit 3", &PatternGuard, 20.0, 3, &environ()).await;
    assert!(ok, "a non-zero exit can be the expected one");
}

#[tokio::test]
async fn the_command_cannot_write_the_project_except_tool_caches_or_reach_a_remote_host() {
    if !seatbelt_works() {
        return eprintln!("skipped: sandbox-exec cannot be applied here");
    }
    let (_d, root) = project();
    std::fs::write(format!("{root}/.env"), "SECRET=1\n").unwrap();
    std::fs::create_dir(format!("{root}/src")).unwrap();
    let (ok, _) = run_command(&root, "echo x > new.txt", &PatternGuard, 20.0, 0, &environ()).await;
    assert!(!ok && !std::path::Path::new(&format!("{root}/new.txt")).exists(), "project writes are denied");
    let (ok, detail) = run_command(&root, "mkdir -p .pytest_cache && echo x > .pytest_cache/v && mkdir -p src/__pycache__ && echo x > src/__pycache__/m.pyc", &PatternGuard, 20.0, 0, &environ()).await;
    assert!(ok, "tool caches may be written: {detail}");
    let (ok, _) = run_command(&root, "cat .env", &PatternGuard, 20.0, 0, &environ()).await;
    assert!(!ok, ".env is unreadable");
    let (ok, detail) = run_command(&root, "/usr/bin/curl -sS -m 3 http://93.184.216.34/ -o /dev/null", &PatternGuard, 20.0, 0, &environ()).await;
    assert!(!ok, "outbound network is denied: {detail}");
}

#[tokio::test]
async fn destructive_commands_are_refused_and_runaways_time_out() {
    if !seatbelt_works() {
        return eprintln!("skipped: sandbox-exec cannot be applied here");
    }
    let (_d, root) = project();
    let (ok, detail) = run_command(&root, "sudo rm -rf /", &PatternGuard, 20.0, 0, &environ()).await;
    assert!(!ok && detail.starts_with("refused to run (Laya Guard 0.95: "), "{detail}");
    let started = std::time::Instant::now();
    let (ok, detail) = run_command(&root, "sleep 30", &PatternGuard, 1.0, 0, &environ()).await;
    assert!(!ok && detail == "timed out after 1s" && started.elapsed().as_secs() < 10, "{detail}");
    let secret = format!("sk-ant-api03-{}", "Z".repeat(40));
    let (_, detail) = run_command(&root, &format!("echo {secret}; exit 1"), &PatternGuard, 20.0, 0, &environ()).await;
    assert!(detail.contains("[REDACTED]") && !detail.contains(&secret), "{detail}");
}

#[test]
fn the_profile_is_the_pythons_for_the_same_inputs() {
    let p = audit_profile("/nonexistent/proj", "/Users/x");
    assert!(p.starts_with("(version 1)\n(allow default)\n(deny network-outbound)\n(allow network-outbound (remote ip \"localhost:*\") (remote unix-socket))\n(deny file-write* (subpath \"/nonexistent/proj\"))\n"));
}
