//! Where the orchestrator stands: read from `state.db`, no model call, no tokens. Each
//! pool's headroom, the recent runs and the recent handoffs.

use cordata_ledger::Ledger;
use cordata_orchestrator::handoff::HandoffPacket;
use cordata_orchestrator::machine;
use serde_json::{Value, json};

use crate::local_time;

pub const GAUGE_WIDTH: usize = 12;

/// `{"calls", "tokens", "cost_usd"}` summed over a run's (role, worker) rows.
pub fn usage_totals(rows: &[Value]) -> Value {
    let int = |k: &str| rows.iter().map(|r| r.get(k).and_then(Value::as_i64).unwrap_or(0)).sum::<i64>();
    let cost: f64 = rows.iter().map(|r| r.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0)).sum::<f64>() + 0.0;
    json!({"calls": int("calls"), "tokens": int("tokens_in") + int("tokens_out"), "cost_usd": (cost * 1e6).round() / 1e6})
}

pub fn status_data(ledger: &Ledger, limit: usize) -> Value {
    let runs: Vec<Value> = machine::recent(ledger, limit)
        .iter()
        .map(|r| {
            let usage = if r.usage.is_empty() { ledger.run_usage(&r.run_id) } else { r.usage.clone() };
            json!({
                "run_id": r.run_id, "state": r.state, "goal": r.goal.chars().take(120).collect::<String>(), "cwd": r.cwd, "updated": r.updated,
                "handoffs": r.handoffs.len(), "drift_retries": r.drift_retries, "replans": r.replans, "reason": r.reason.chars().take(200).collect::<String>(),
                "usage": usage_totals(&usage),
            })
        })
        .collect();
    let handoffs: Vec<Value> = ledger
        .recent_handoffs(limit)
        .iter()
        .filter_map(HandoffPacket::from_json)
        .map(|p| {
            let j = p.to_json();
            let mut out = serde_json::Map::new();
            for k in ["handoff_id", "session_id", "timestamp", "reason", "step_id", "step_status", "from_worker", "to_worker"] {
                out.insert(k.into(), j[k].clone());
            }
            Value::Object(out)
        })
        .collect();
    let pools: Vec<Value> = ledger
        .states()
        .into_iter()
        .map(|state| {
            let mut pool = serde_json::to_value(&state).unwrap_or(Value::Null);
            if let (Some(obj), Some(n)) = (pool.as_object_mut(), ledger.steps_left(&state.pool_id)) {
                obj.insert("steps_left".into(), json!(n));
            }
            pool
        })
        .collect();
    json!({"pools": pools, "runs": runs, "handoffs": handoffs, "ts": cordata_checkpoints::now()})
}

pub fn gauge(pct: Option<f64>, width: usize) -> String {
    match pct {
        None => "·".repeat(width),
        Some(p) => {
            let filled = ((p * width as f64).round().clamp(0.0, width as f64)) as usize;
            format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
        }
    }
}

/// Python's `_compact`.
pub fn compact(n: f64) -> String {
    if n >= 1_000_000.0 {
        return format!("{:.1}M", n / 1_000_000.0).replace(".0M", "M");
    }
    if n >= 10_000.0 {
        return format!("{:.0}k", n / 1000.0);
    }
    if n.fract() != 0.0 { format!("{n:.2}").trim_end_matches('0').trim_end_matches('.').to_owned() } else { format!("{n:.0}") }
}

fn level_mark(level: &str) -> &'static str {
    match level {
        "ok" => "🟢",
        "soft" => "🟡",
        "hard" => "🟠",
        "exhausted" => "🔴",
        _ => "",
    }
}

fn pad(text: &str, width: usize) -> String {
    format!("{text:<width$}")
}

/// `claude_code  ███████░░░░░  58%  soft  11.6/20 usd · provider` — one pool, one line.
pub fn pool_line(pool: &Value, marks: bool) -> String {
    let limit = pool.get("limit").and_then(Value::as_f64).filter(|l| *l != 0.0);
    let pct = pool.get("pct").and_then(Value::as_f64);
    let capped = limit.is_some() || pool.get("provider_pct").is_some_and(|v| !v.is_null());
    let used = pool.get("used").and_then(Value::as_f64).unwrap_or(0.0);
    let unit = pool.get("unit").and_then(Value::as_str).unwrap_or("");
    let used = format!("{}/{} {unit}", compact(used), limit.map_or("∞".to_owned(), compact));
    let level = pool.get("level").and_then(Value::as_str).unwrap_or("ok");
    let mark = if marks { format!("{} ", level_mark(level)) } else { String::new() };
    let source = if pool.get("source").and_then(Value::as_str) == Some("provider") { " · provider" } else { "" };
    let until = pool.get("limited_until").and_then(Value::as_f64).filter(|t| *t != 0.0).map_or(String::new(), |t| format!(" · resets {}", local_time(t, "%H:%M")));
    let steps = pool.get("steps_left").and_then(Value::as_i64).map_or(String::new(), |n| format!(" · ~{n} step{} left", if n == 1 { "" } else { "s" }));
    let pct_text = if capped && pct.is_some() { format!("{:4.0}%", pct.unwrap_or(0.0) * 100.0) } else { "  — ".to_owned() };
    format!("{mark}{} {} {pct_text}  {}{used}{source}{steps}{until}", pad(pool.get("pool_id").and_then(Value::as_str).unwrap_or(""), 18), gauge(if capped { pct } else { None }, GAUGE_WIDTH), pad(level, 9) + " ")
}

pub fn handoff_line(item: &Value) -> String {
    let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let to = item.get("to_worker").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or("—").to_owned();
    format!("{} {} · step {} · {} → {to}", local_time(item.get("timestamp").and_then(Value::as_f64).unwrap_or(0.0), "%H:%M"), s("reason"), s("step_id"), s("from_worker"))
}

pub fn run_line(run: &Value) -> String {
    let n = run.get("handoffs").and_then(Value::as_i64).unwrap_or(0);
    let extra = if n > 0 { format!(" · {n} handoff(s)") } else { String::new() };
    let goal: String = run.get("goal").and_then(Value::as_str).unwrap_or("").chars().take(70).collect();
    format!("{} {}: {goal}{extra}", run.get("run_id").and_then(Value::as_str).unwrap_or(""), run.get("state").and_then(Value::as_str).unwrap_or(""))
}

pub fn render(data: &Value, marks: bool) -> String {
    let width = 72;
    let top = format!("╭── Quota Pools {}╮", "─".repeat(width - 16));
    let bot = format!("╰{}╯", "─".repeat(width));
    let list = |key: &str| data.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut lines = vec![top];
    lines.extend(list("pools").iter().map(|p| format!("│  {}", pool_line(p, marks))));
    lines.push(bot.clone());
    let runs = list("runs");
    lines.push(format!("╭── Orchestrator Runs {}╮", "─".repeat(width - 22)));
    if runs.is_empty() {
        lines.push("│  (none)".into());
    } else {
        lines.extend(runs.iter().map(|r| format!("│  {}", run_line(r))));
    }
    lines.push(bot.clone());
    let handoffs = list("handoffs");
    lines.push(format!("╭── Recent Handoffs {}╮", "─".repeat(width - 20)));
    if handoffs.is_empty() {
        lines.push("│  (none)".into());
    } else {
        lines.extend(handoffs.iter().map(|h| format!("│  {}", handoff_line(h))));
    }
    lines.push(bot);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_and_gauge_match_the_python_helpers() {
        assert_eq!([compact(0.0), compact(3.0), compact(11.6), compact(9999.0), compact(12_345.0), compact(1_500_000.0), compact(2_000_000.0), compact(0.125)], ["0", "3", "11.6", "9999", "12k", "1.5M", "2M", "0.12"]);
        assert_eq!(gauge(Some(0.5), 12), "██████░░░░░░");
        assert_eq!(gauge(None, 4), "····");
        assert_eq!(gauge(Some(1.7), 4), "████");
    }
}
