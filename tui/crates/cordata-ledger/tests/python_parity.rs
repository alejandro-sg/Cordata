//! Rust and Python share one `state.db`: rows written by this crate must be read identically by
//! `backend/orchestrator/quota_ledger.py`, and the other way round. Skipped when the backend venv is absent.

use std::path::PathBuf;
use std::process::Command;

use cordata_ledger::{Ledger, Outcome, Pool, Unit};

fn python() -> Option<(PathBuf, PathBuf)> {
    let backend = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../backend").canonicalize().ok()?;
    let py = backend.join(".venv/bin/python");
    py.exists().then_some((py, backend))
}

fn run_py(db: &std::path::Path, code: &str) -> String {
    let (py, backend) = python().unwrap();
    let out = Command::new(py)
        .current_dir(&backend)
        .env("CORDATA_STATE_DB", db)
        .env("CORDATA_HOME", db.parent().unwrap())
        .args(["-c", code])
        .output()
        .unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn python_reads_what_rust_wrote_and_vice_versa() {
    if python().is_none() {
        eprintln!("skipped: backend/.venv not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let pools = vec![Pool::new("cc", "claude_code").unit(Unit::Usd).limit(10.0).weekly(40.0)];
    let ledger = Ledger::open(&db, pools).unwrap();
    ledger.record_result("run1", Some("s1"), "claude_code", Some("sonnet"), &Outcome { cost_usd: 8.2, tokens_in: 7, ..Default::default() }, Some("execute"), Some("w"));

    let seen = run_py(
        &db,
        "import json\nfrom orchestrator.quota_ledger import Pool, QuotaLedger\n\
         l = QuotaLedger([Pool('cc','claude_code','.*',unit='usd',window_limit=10.0,weekly_limit=40.0)])\n\
         s = l.state('cc')\nprint(json.dumps({'level': s.level, 'pct': s.pct, 'used': s.used, 'to_hard': s.to_hard, 'usage': l.run_usage('run1')}))",
    );
    let seen: serde_json::Value = serde_json::from_str(&seen).unwrap();
    let mine = ledger.state_of("cc");
    assert_eq!(seen["level"], mine.level);
    assert_eq!(seen["pct"], mine.pct);
    assert_eq!(seen["used"], mine.used);
    assert!((seen["to_hard"].as_f64().unwrap() - mine.to_hard.unwrap()).abs() < 1e-6);
    assert_eq!(seen["usage"][0]["worker"], "w");

    // Python writes; Rust reads the same total.
    run_py(
        &db,
        "from orchestrator.quota_ledger import Pool, QuotaLedger, UsageRow\n\
         l = QuotaLedger([Pool('cc','claude_code','.*',unit='usd',window_limit=10.0,weekly_limit=40.0)])\n\
         l.record(UsageRow('run1','cc','sonnet',cost_usd=1.4,step_id='s2'))",
    );
    let env = ledger.envelope("run1", Some(9.0), None);
    assert!(env.exhausted && env.spent_usd == 9.6, "{env:?}");
    assert_eq!(ledger.state_of("cc").level, "hard");
}
