//! The evidence receipt every `cordata run` leaves in its run dir (`receipt.json`): what the deterministic criteria
//! said, what changed, what it cost, how long it took and what the audit found. Built from the record, the ledger and
//! the checkpoint store; no model writes it.

use std::path::Path;
use std::time::Duration;

use cordata_ledger::Ledger;
use cordata_orchestrator::checkpoints::Checkpoints;
use cordata_orchestrator::machine::{RunRecord, artifacts_dir};
use cordata_orchestrator::plan::Plan;
use cordata_orchestrator::scope;
use serde_json::{Value, json};

pub fn build(record: &RunRecord, ledger: &Ledger, checkpoints: &dyn Checkpoints, wall: Duration) -> Value {
    let audit = record.audit.clone().unwrap_or(Value::Null);
    let criteria: Vec<Value> = audit.get("checks").and_then(Value::as_array).into_iter().flatten()
        .map(|c| json!({"id": c.get("id"), "kind": c.get("kind"), "ok": c.get("ok")})).collect();
    // An empty stat is "no changes" only when the store can say nothing changed; otherwise it is unknown (null).
    let project = Path::new(&record.cwd);
    let diffstat = record.start_checkpoint.as_deref().and_then(|id| match checkpoints.diff_since(project, id, None).1.trim() {
        "" => checkpoints.changes_since(project, id).filter(Vec::is_empty).map(|_| String::new()),
        stat => Some(stat.to_owned()),
    });
    let changed = record.start_checkpoint.as_deref().and_then(|id| checkpoints.changes_since(project, id));
    // The ledger reports a failed query as an exhausted envelope; with no limits asked for, that can mean nothing else.
    let spent = ledger.envelope(&record.run_id, None, None);
    json!({
        "run_id": record.run_id, "goal": record.goal, "state": record.state, "reason": record.reason,
        "criteria": criteria, "criteria_ok": audit.get("tier1_ok").and_then(Value::as_bool),
        "diffstat": diffstat, "cost_usd": (!spent.exhausted).then_some(spent.spent_usd),
        "wall_seconds": wall.as_secs(), "audit_verdict": audit.get("verdict"), "audit_findings": audit.get("failures").cloned().unwrap_or(json!([])),
        "warnings": record.warnings,
        "logic": logic(record, changed.as_deref()),
    })
}

/// The run as a logic diff: for each step of the plan, the behavior it was to bring, why, how it ended, and the changed
/// files that fall inside its scope. Read straight from the plan and the checkpoint store; no model writes it. A step
/// that did not say its behavior falls back to its description; `files` is null when the store could not say what changed.
fn logic(record: &RunRecord, changed: Option<&[String]>) -> Vec<Value> {
    let Some(plan) = record.plan.as_ref().and_then(|p| Plan::from_json(p).ok()) else { return Vec::new() };
    plan.steps.iter().map(|step| {
        let files = changed.map(|all| all.iter().filter(|f| !step.file_scope.is_empty() && scope::matches(f, &step.file_scope, &record.cwd)).cloned().collect::<Vec<_>>());
        json!({
            "step_id": step.step_id, "name": step.name, "status": step.status,
            "behavior": if step.behavior.is_empty() { &step.description } else { &step.behavior },
            "because": step.because, "files": files,
        })
    }).collect()
}

/// Writes `receipt.json` (0600) to the run dir and returns the short summary to print.
pub fn write(home: &Path, receipt: &Value) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = artifacts_dir(home, receipt["run_id"].as_str().unwrap_or_default());
    let path = dir.join("receipt.json");
    let saved = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, format!("{receipt:#}\n")))
        .and_then(|()| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)));
    if let Err(e) = saved {
        return format!("receipt: could not write {}: {e}", path.display());
    }
    let list = |k: &str| receipt[k].as_array().map_or(0, Vec::len);
    let passed = receipt["criteria"].as_array().map_or(0, |c| c.iter().filter(|c| c["ok"] == true).count());
    let files = match receipt["diffstat"].as_str() {
        Some("") => "no changes",
        Some(stat) => stat,
        None => "changes unknown",
    };
    let cost = receipt["cost_usd"].as_f64().map_or_else(|| "cost unknown".to_owned(), |c| format!("${c:.4}"));
    format!("receipt: criteria {passed}/{} · {files} · {cost} · {}s · {} audit finding(s) — {}",
        list("criteria"), receipt["wall_seconds"], list("audit_findings"), path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordata_orchestrator::testkit::CopyCheckpoints;

    #[test]
    fn a_receipt_records_criteria_cost_time_and_findings_in_the_run_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("state.db"), Vec::new()).unwrap();
        let mut record: RunRecord = serde_json::from_value(json!({"run_id": "run-r", "goal": "g", "cwd": dir.path()})).unwrap();
        record.state = "failed".into();
        record.audit = Some(json!({"tier1_ok": false, "verdict": "", "failures": ["c2 (test): exit 1"],
            "checks": [{"id": "c1", "kind": "file_exists", "ok": true}, {"id": "c2", "kind": "test", "ok": false}]}));
        let cp = CopyCheckpoints::new(&dir.path().join("cp"));
        let receipt = build(&record, &ledger, &cp, Duration::from_secs(42));
        let summary = write(dir.path(), &receipt);
        assert!(summary.starts_with("receipt: criteria 1/2 · changes unknown · $0.0000 · 42s · 1 audit finding(s)"), "{summary}");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(artifacts_dir(dir.path(), "run-r").join("receipt.json")).unwrap()).unwrap();
        assert_eq!((saved["criteria_ok"].clone(), saved["audit_findings"].clone()), (json!(false), json!(["c2 (test): exit 1"])));
    }

    #[test]
    fn the_logic_diff_pairs_each_step_with_what_it_brought_and_the_files_in_its_scope() {
        let dir = tempfile::tempdir().unwrap();
        let plan = json!({"plan_id": "p", "goal": "g", "acceptance_criteria": [], "steps": [
            {"step_id": "s1", "name": "Close full slots", "description": "d1", "file_scope": ["site/order.js"], "status": "completed",
             "behavior": "A full slot is greyed out", "because": "customers picked full slots"},
            {"step_id": "s2", "name": "Docs", "description": "write the docs", "file_scope": ["docs/**"], "status": "failed"}]});
        let record: RunRecord = serde_json::from_value(json!({"run_id": "r", "goal": "g", "cwd": dir.path(), "plan": plan})).unwrap();
        let changed = ["site/order.js".to_owned(), "README.md".to_owned()];
        let out = logic(&record, Some(&changed));
        assert_eq!(out[0]["behavior"], "A full slot is greyed out");
        assert_eq!((out[0]["because"].clone(), out[0]["files"].clone()), (json!("customers picked full slots"), json!(["site/order.js"])));
        assert_eq!((out[1]["behavior"].clone(), out[1]["files"].clone()), (json!("write the docs"), json!([])));
        assert_eq!(logic(&record, None)[0]["files"], Value::Null, "unknown changes stay unknown");
    }
}
