//! The Python smoke test's ledger scenario, step for step.

use std::sync::{Arc, Mutex};

use cordata_ledger::{Allowance, Headroom, Ledger, Outcome, Pool, UsageRow, Unit, pools_from_settings};
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
    let first_use = l.state_of("cc").resets_at.expect("metered use frees up as it leaves the window");
    assert!(first_use > at() && first_use <= at() + 5.0 * 3600.0, "{first_use}");
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
    assert_eq!(s.resets_at, Some(at() + 3600.0), "the provider's soonest reset");
    let Allowance::Usd(budget) = l.step_allowance("claude_code", Some("sonnet")) else { panic!("usd allowance") };
    assert!((budget - (0.95 - 0.91) * 10.0).abs() < 1e-6, "{budget}");
    advance(3601.0);
    assert_eq!(l.state_of("cc").provider_pct, Some(0.30), "a reading past its reset time is stale");

    let reset = at() + 600.0;
    let rejected = Outcome { rate_limited: true, provider_quota: json!({"status":"rejected","resets_at":reset,"windows":{}}), ..Default::default() };
    l.record_result("s5", Some("f"), "claude_code", Some("sonnet"), &rejected, None, None);
    assert_eq!(l.state_of("cc").limited_until, Some(reset));
    assert_eq!(l.state_of("cc").resets_at, Some(reset), "a limit's end comes first");

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

#[test]
fn headroom_compares_what_is_left_with_what_a_step_usually_takes() {
    let dir = tempfile::tempdir().unwrap();
    let (now, read) = clock();
    let l = Ledger::open(&dir.path().join("state.db"), vec![Pool::new("cc", "claude_code").unit(Unit::Usd).limit(10.0)]).unwrap().with_clock(read);
    let pool = l.pool_for("claude_code", None);
    // No history: no guess, so nothing is held back.
    assert_eq!(l.typical_step(&pool), None);
    assert_eq!(l.headroom("claude_code", None), Headroom::Fits);
    // Old steps (long out of the window) still teach what a step costs; two attempts at one step are one step.
    for (step, usd) in [("a", 0.5), ("b", 1.0), ("c", 1.5), ("d", 2.0)] {
        l.record_result("old-run", Some(step), "claude_code", None, &spend(usd), Some("execute"), None);
    }
    l.record_result("old-run", Some("a"), "claude_code", None, &spend(0.25), Some("execute"), None);
    l.record_result("old-run", None, "claude_code", None, &spend(9.0), Some("plan"), None);
    assert_eq!(l.typical_step(&pool), Some(1.5), "p75 of 0.75, 1.0, 1.5, 2.0");
    *now.lock().unwrap() += 10.0 * 3600.0;
    l.record_result("run", Some("x"), "claude_code", None, &spend(8.2), Some("execute"), None);
    // 9.5 hard line - 8.2 = 1.3 left; with this step a sample too, a step takes ~2.0 (p75): stop before the next.
    match l.headroom("claude_code", None) {
        Headroom::Short { pool_id, left, typical, resets_at, .. } => {
            assert_eq!(pool_id, "cc");
            assert!((left - 1.3).abs() < 1e-9 && (typical - 2.0).abs() < 1e-9, "{left} {typical}");
            assert!(resets_at.is_some());
        }
        Headroom::Fits => panic!("expected short"),
    }
}

#[test]
fn a_pool_list_that_leaves_out_claude_code_keeps_its_default_cap() {
    let dir = tempfile::tempdir().unwrap();
    let l = Ledger::open(&dir.path().join("state.db"), vec![Pool::new("agy", "antigravity").limit(1_000.0)]).unwrap();
    let pool = l.pool_for("claude_code", Some("opus"));
    assert_eq!((pool.pool_id.as_str(), pool.unit, pool.window_limit, pool.weekly_limit), ("claude_code", Unit::Usd, Some(20.0), Some(150.0)));
    assert_eq!(l.pool_for("antigravity", None).window_limit, Some(1_000.0), "the configured pool still wins");
    let bad = pools_from_settings(Some(&json!([{"pool_id": "typo", "family": "claude_code", "model_regex": "(", "unit": "usd", "window_limit": 1.0}])));
    let l = Ledger::open(&dir.path().join("c.db"), bad).unwrap();
    assert_eq!(l.pool_for("claude_code", Some("opus")).window_limit, Some(20.0), "a pool whose regex does not compile is dropped");
    let raw = json!([{"pool_id": "typo", "family": "claude_code", "model_regex": "(", "unit": "usd"}]);
    assert_eq!(cordata_ledger::bad_pool_regexes(Some(&raw)), ["typo (model_regex `(`)"]);
    let l = Ledger::open(&dir.path().join("b.db"), vec![Pool::new("mine", "ollama").model(r".*:local").unit(Unit::Requests)]).unwrap();
    assert_eq!((l.pool_for("ollama", Some("kimi-k3")).pool_id.as_str(), l.pool_for("ollama", Some("x:local")).pool_id.as_str()), ("ollama_cloud", "mine"));
}

#[test]
fn hold_back_keeps_room_for_the_costliest_request_seen() {
    let dir = tempfile::tempdir().unwrap();
    let l = Ledger::open(&dir.path().join("state.db"), Vec::new()).unwrap();
    assert!((l.hold_back("r", Some(1.0)).unwrap() - 0.9).abs() < 1e-9, "nothing seen yet: a tenth");
    let mut row = spend(0.3);
    row.requests = Some(3);
    l.record_result("r", Some("a"), "claude_code", None, &row, Some("execute"), None);
    assert!((l.hold_back("r", Some(1.0)).unwrap() - 0.8).abs() < 1e-9, "twice the $0.10 request");
    assert!((l.hold_back("a-new-run", Some(1.0)).unwrap() - 0.8).abs() < 1e-9, "a run's first request uses the ledger's history");
    assert!((l.hold_back("r", Some(0.1)).unwrap() - 0.05).abs() < 1e-9, "never more than half");
    assert_eq!(l.hold_back("r", None), None);
}

#[test]
fn spend_the_database_refuses_still_counts() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let l = Ledger::open(&db, vec![Pool::new("cc", "claude_code").unit(Unit::Usd).limit(10.0)]).unwrap();
    let other = rusqlite::Connection::open(&db).unwrap();
    other.execute_batch("CREATE TRIGGER refuse BEFORE INSERT ON quota_usage BEGIN SELECT RAISE(ABORT, 'disk says no'); END;").unwrap();
    l.record_result("run-x", Some("a"), "claude_code", None, &spend(4.0), Some("execute"), None);
    let env = l.envelope("run-x", Some(5.0), None);
    assert!((env.spent_usd - 4.0).abs() < 1e-9 && !env.exhausted, "{env:?}");
    assert!((l.state_of("cc").used - 4.0).abs() < 1e-9);
}
