//! The evidence receipt every `cordata run` leaves in its run dir (`receipt.json`): what the deterministic criteria
//! said, what changed, what it cost, how long it took and what the audit found. Built from the record, the ledger and
//! the checkpoint store; no model writes it.

use std::path::Path;
use std::time::Duration;

use cordata_ledger::Ledger;
use cordata_orchestrator::checkpoints::Checkpoints;
use cordata_orchestrator::machine::{RunRecord, artifacts_dir};
use serde_json::{Value, json};

pub fn build(record: &RunRecord, ledger: &Ledger, checkpoints: &dyn Checkpoints, wall: Duration) -> Value {
    let audit = record.audit.clone().unwrap_or(Value::Null);
    let criteria: Vec<Value> = audit.get("checks").and_then(Value::as_array).into_iter().flatten()
        .map(|c| json!({"id": c.get("id"), "kind": c.get("kind"), "ok": c.get("ok")})).collect();
    let diffstat = record.start_checkpoint.as_deref().map_or_else(String::new, |id| checkpoints.diff_since(Path::new(&record.cwd), id, None).1);
    json!({
        "run_id": record.run_id, "goal": record.goal, "state": record.state, "reason": record.reason,
        "criteria": criteria, "criteria_ok": audit.get("tier1_ok").and_then(Value::as_bool),
        "diffstat": diffstat.trim(), "cost_usd": ledger.envelope(&record.run_id, None, None).spent_usd,
        "wall_seconds": wall.as_secs(), "audit_verdict": audit.get("verdict"), "audit_findings": audit.get("failures").cloned().unwrap_or(json!([])),
        "warnings": record.warnings,
    })
}

/// Writes `receipt.json` (0600) to the run dir and returns the short summary to print.
pub fn write(home: &Path, receipt: &Value) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = artifacts_dir(home, receipt["run_id"].as_str().unwrap_or_default());
    let path = dir.join("receipt.json");
    if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&path, format!("{receipt:#}\n")).is_ok() {
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let list = |k: &str| receipt[k].as_array().map_or(0, Vec::len);
    let passed = receipt["criteria"].as_array().map_or(0, |c| c.iter().filter(|c| c["ok"] == true).count());
    let files = receipt["diffstat"].as_str().and_then(|s| s.lines().last()).filter(|l| !l.is_empty()).unwrap_or("no changes");
    format!("receipt: criteria {passed}/{} · {files} · ${:.4} · {}s · {} audit finding(s) — {}",
        list("criteria"), receipt["cost_usd"].as_f64().unwrap_or(0.0), receipt["wall_seconds"], list("audit_findings"), path.display())
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
        assert!(summary.starts_with("receipt: criteria 1/2 · no changes · $0.0000 · 42s · 1 audit finding(s)"), "{summary}");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(artifacts_dir(dir.path(), "run-r").join("receipt.json")).unwrap()).unwrap();
        assert_eq!((saved["criteria_ok"].clone(), saved["audit_findings"].clone()), (json!(false), json!(["c2 (test): exit 1"])));
    }
}
