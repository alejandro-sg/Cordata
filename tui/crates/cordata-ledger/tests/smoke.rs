//! The Python smoke test's ledger scenario (`backend/_quota_ledger_smoke_test.py::test_ledger`), step for step.

use std::sync::{Arc, Mutex};

use cordata_ledger::{Allowance, Ledger, Outcome, Pool, UsageRow, Unit, pools_from_settings};
use serde_json::json;

fn clock() -> (Arc<Mutex<f64>>, impl Fn() -> f64 + Send + 'static) {
    let now = Arc::new(Mutex::new(1_800_000_000.0));
    let handle = now.clone();
    (now, move || *handle.lock().unwrap())
}

fn spend(usd: f64) -> Outcome {
    Outcome { cost_usd: usd, ..Default::default() }
}

#[test]
fn the_ledger_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let (now, read) = clock();
    let pools = vec![
        Pool::new("cc", "claude_code").unit(Unit::Usd).limit(10.0).weekly(40.0),
        Pool::new("agy_claude", "antigravity").model("claude-.*").limit(1000.0),
        Pool::new("agy", "antigravity").limit(10_000.0),
        Pool::new("ollama", "ollama").unit(Unit::Requests).limit(5.0).window(3600),
    ];
    let l = Ledger::open(&dir.path().join("state.db"), pools).unwrap().with_clock(read);
    let advance = |s: f64| *now.lock().unwrap() += s;
    let at = || *now.lock().unwrap();

    assert_eq!(l.pool_for("antigravity", Some("claude-sonnet-5-5-high")).pool_id, "agy_claude");
    assert_eq!(l.pool_for("antigravity", Some("gemini-3.8-flash-low")).pool_id, "agy");
    assert_eq!(l.state_of("cc").level, "ok");
    assert_eq!(l.state_of("cc").to_hard, Some(9.5));

    l.record_result("s1", Some("a"), "claude_code", Some("sonnet"), &spend(8.2), None, None);
    let s = l.state_of("cc");
    assert_eq!(s.level, "soft");
    assert!((s.pct - 0.82).abs() < 1e-9, "{}", s.pct);
    assert_eq!(l.step_allowance("claude_code", Some("sonnet")), Allowance::Usd(((9.5f64 - 8.2) * 1e6).round() / 1e6));
    l.record_result("s1", Some("b"), "claude_code", Some("sonnet"), &spend(1.4), None, None);
    assert_eq!(l.state_of("cc").level, "hard");
    advance(5.0 * 3600.0 + 1.0);
    assert_eq!(l.state_of("cc").level, "ok", "the five-hour window rolls past the old usage");
    for _ in 0..3 {
        l.record_result("s2", Some("c"), "claude_code", Some("sonnet"), &spend(9.0), None, None);
        advance(5.0 * 3600.0 + 1.0);
    }
    let s = l.state_of("cc");
    assert_eq!(s.level, "soft");
    assert!((s.weekly_used - 36.6).abs() < 1e-9, "weekly still sees all of it: {}", s.weekly_used);

    let limited = Outcome { tokens_in: 100, tokens_out: 50, rate_limited: true, ..Default::default() };
    l.record_result("s3", Some("d"), "antigravity", Some("gemini-3.8-flash-low"), &limited, None, None);
    assert_eq!(l.state_of("agy").level, "exhausted");
    advance(5.0 * 3600.0 + 1.0);
    assert_eq!(l.state_of("agy").level, "ok");

    advance(7.0 * 24.0 * 3600.0 + 1.0);
    let quota = json!({"status":"allowed","windows":{
        "five_hour":{"utilization":0.91,"resets_at":at()+3600.0},
        "seven_day":{"utilization":0.30,"resets_at":at()+86400.0}}});
    let o = Outcome { cost_usd: 0.1, provider_quota: quota, ..Default::default() };
    l.record_result("s4", Some("e"), "claude_code", Some("sonnet"), &o, None, None);
    let s = l.state_of("cc");
    assert_eq!((s.level, s.source, s.provider_pct), ("soft", "provider", Some(0.91)));
    let Allowance::Usd(budget) = l.step_allowance("claude_code", Some("sonnet")) else { panic!("usd allowance") };
    assert!((budget - (0.95 - 0.91) * 10.0).abs() < 1e-6, "{budget}");
    advance(3601.0);
    assert_eq!(l.state_of("cc").provider_pct, Some(0.30), "a reading past its reset time is stale");

    let reset = at() + 600.0;
    let rejected = Outcome { rate_limited: true, provider_quota: json!({"status":"rejected","resets_at":reset,"windows":{}}), ..Default::default() };
    l.record_result("s5", Some("f"), "claude_code", Some("sonnet"), &rejected, None, None);
    assert_eq!(l.state_of("cc").limited_until, Some(reset));

    for _ in 0..4 {
        l.record_result("s6", Some("g"), "ollama", Some("glm-5.3-flash"), &Outcome::default(), None, None);
    }
    assert_eq!(l.state_of("ollama").level, "soft");
    assert_eq!(l.step_allowance("ollama", Some("glm-5.3-flash")), Allowance::Unbounded);

    let env = l.envelope("s1", Some(9.0), None);
    assert!(env.exhausted && env.spent_usd == 9.6, "{env:?}");
    assert!(!l.envelope("s3", None, Some(1000)).exhausted);
    let mut row = UsageRow::new("s7", "agy", "m", at());
    (row.tokens_in, row.tokens_out) = (600, 500);
    l.record(&row).unwrap();
    assert!(l.envelope("s7", None, Some(1000)).exhausted);
}

#[test]
fn run_usage_groups_by_role_and_worker() {
    let dir = tempfile::tempdir().unwrap();
    let l = Ledger::open(&dir.path().join("s.db"), vec![]).unwrap();
    let mut a = Outcome { tokens_in: 10, tokens_out: 5, cost_usd: 0.5, ..Default::default() };
    l.record_result("r", Some("plan"), "claude_code", Some("m"), &a, None, None);
    a.cost_usd = 0.25;
    l.record_result("r", Some("s1"), "claude_code", Some("m"), &a, Some("execute"), Some("claude:m"));
    l.record_result("r", Some("s2"), "claude_code", Some("m"), &a, Some("execute"), Some("claude:m"));
    let usage = l.run_usage("r");
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0]["role"], "execute");
    assert_eq!(usage[0]["calls"], 2);
    assert_eq!(usage[0]["cost_usd"], 0.5);
    assert_eq!(usage[1]["role"], "plan");
    assert_eq!(usage[1]["worker"], "claude_code:m");
}

#[test]
fn settings_pools_replace_defaults_and_bad_entries_are_skipped() {
    assert_eq!(pools_from_settings(None).len(), 5);
    let raw = json!([{"pool_id":"x","family":"f","unit":"usd","window_limit":3.0}, {"nope":1}, {"pool_id":"y","family":"f","unit":"bogus"}]);
    let pools = pools_from_settings(Some(&raw));
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].window_limit, Some(3.0));
    let as_text = json!(raw.to_string());
    assert_eq!(pools_from_settings(Some(&as_text)).len(), 1);
    assert_eq!(pools_from_settings(Some(&json!("not json"))).len(), 5);
    assert_eq!(pools_from_settings(Some(&json!([]))).len(), 5);
}

#[test]
fn the_database_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.db");
    let _l = Ledger::open(&path, vec![]).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}
