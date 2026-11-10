//! The run feed: orchestrated runs as the window can see them, read from `cordata-run status --json` and
//! `cordata-run show <id> --json` (the ledger's own record; no model call).
//!
//! A run is a plan of steps, each with the worker that last ran it. When a worker stops (quota line, rate limit,
//! failure, or the person) the step moves on with a handoff packet; those are listed per run. This module is pure:
//! it parses what the CLI printed and writes the one control line a front door may send. It never starts a run.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Worker families `cordata-run` can drive (`--workers`).
pub const FAMILIES: [&str; 3] = ["claude_code", "antigravity", "ollama"];
const TERMINAL: [&str; 4] = ["done", "failed", "budget_exhausted", "cancelled"];
/// Runs shown for a project: this many, newest first.
pub const MAX_RUNS: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    pub name: String,
    /// `pending`, `running`, `completed` or `failed`.
    pub status: String,
    pub worker: Option<String>,
    pub attempts: i64,
    pub scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    pub id: String,
    pub step_id: String,
    pub reason: String,
    pub from: String,
    pub to: Option<String>,
    /// `pending` (nothing done yet), `partial` (work kept) or `failed` (rolled back).
    pub step_status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub run_id: String,
    pub goal: String,
    pub state: String,
    pub reason: String,
    pub steps: Vec<Step>,
    pub handoffs: Vec<Handoff>,
    /// `(unix seconds, state)` for every transition, oldest first.
    pub history: Vec<(f64, String)>,
}

impl Run {
    pub fn finished(&self) -> bool {
        TERMINAL.contains(&self.state.as_str())
    }

    /// A pause is honoured at the run's next safe point, so it makes sense only while the run is going.
    pub fn can_hold(&self) -> bool {
        !self.finished() && self.state != "paused"
    }

    pub fn can_resume(&self) -> bool {
        self.state == "paused"
    }
}

/// Plain words for a handoff's reason.
pub fn reason_words(reason: &str) -> &str {
    match reason {
        "quota_soft_limit_reached" => "quota soft line",
        "quota_hard_limit_reached" => "quota hard line",
        "usage_low" => "usage low",
        "provider_rate_limit" => "rate limit",
        "worker_failed" => "worker failed",
        "worker_unavailable" => "worker unavailable",
        "user_pause" => "paused by you",
        other => other,
    }
}

/// Ids of the runs for this project to look at: its newest, with ones still going first.
pub fn pick(status: &Value, project: &Path) -> Vec<String> {
    let mut runs: Vec<(&str, bool)> = status
        .get("runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("cwd").and_then(Value::as_str).is_some_and(|c| Path::new(c) == project))
        .filter_map(|r| Some((r.get("run_id")?.as_str()?, !TERMINAL.contains(&r.get("state")?.as_str()?))))
        .collect();
    runs.sort_by_key(|(_, live)| !live); // stable: newest first within each group
    runs.into_iter().take(MAX_RUNS).map(|(id, _)| id.to_owned()).collect()
}

/// One run from its `show --json` record, with its handoffs picked out of `status --json`.
pub fn parse(show: &Value, status: &Value) -> Option<Run> {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let mine: Vec<&str> = show.get("handoffs").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let steps = show
        .get("plan")
        .and_then(|p| p.get("steps"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| Step {
            id: text(s, "step_id"),
            name: text(s, "name"),
            status: text(s, "status"),
            worker: s.get("worker").and_then(Value::as_str).filter(|w| !w.is_empty()).map(str::to_owned),
            attempts: s.get("attempts").and_then(Value::as_i64).unwrap_or(0),
            scope: s.get("file_scope").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
        })
        .collect();
    let handoffs = status
        .get("handoffs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|h| h.get("handoff_id").and_then(Value::as_str).is_some_and(|id| mine.contains(&id)))
        .map(|h| Handoff {
            id: text(h, "handoff_id"),
            step_id: text(h, "step_id"),
            reason: text(h, "reason"),
            from: text(h, "from_worker"),
            to: h.get("to_worker").and_then(Value::as_str).filter(|w| !w.is_empty()).map(str::to_owned),
            step_status: text(h, "step_status"),
        })
        .collect();
    let history = show
        .get("history")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e.get("ts")?.as_f64()?, e.get("state")?.as_str()?.to_owned())))
        .collect();
    Some(Run { run_id: show.get("run_id")?.as_str()?.to_owned(), goal: text(show, "goal"), state: text(show, "state"), reason: text(show, "reason"), steps, handoffs, history })
}

/// `<cordata home>/orchestrator/<run>/control.ndjson`, where the run polls for a pause.
pub fn control_file(home: &Path, run_id: &str) -> PathBuf {
    home.join("orchestrator").join(run_id).join("control.ndjson")
}

/// Ask a run to pause at its next safe point. The one line `cordata-run`'s `FileControls` reads; an id that is not
/// a plain run id is refused so nothing here can name a path outside the run folder.
pub fn request_hold(home: &Path, run_id: &str, by: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    if run_id.is_empty() || !run_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a run id"));
    }
    let path = control_file(home, run_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut out = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path)?;
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    writeln!(out, "{}", serde_json::json!({"cmd": "pause", "now": false, "restore": false, "by": by, "at": at}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status() -> Value {
        json!({
            "runs": [
                {"run_id": "run-new", "state": "done", "cwd": "/p/app"},
                {"run_id": "run-live", "state": "execute", "cwd": "/p/app"},
                {"run_id": "run-other", "state": "execute", "cwd": "/p/other"},
                {"run_id": "run-old", "state": "failed", "cwd": "/p/app"},
            ],
            "handoffs": [
                {"handoff_id": "h1", "step_id": "s1", "reason": "quota_soft_limit_reached", "from_worker": "claude_code:fast", "to_worker": "antigravity:x", "step_status": "partial"},
                {"handoff_id": "h9", "step_id": "s1", "reason": "worker_failed", "from_worker": "a", "to_worker": null, "step_status": "failed"},
            ],
        })
    }

    #[test]
    fn this_projects_runs_come_live_first_then_newest_and_only_two() {
        assert_eq!(pick(&status(), Path::new("/p/app")), ["run-live", "run-new"]);
        assert!(pick(&status(), Path::new("/nowhere")).is_empty());
        assert!(pick(&json!({}), Path::new("/p/app")).is_empty());
    }

    #[test]
    fn a_run_reads_its_steps_and_only_its_own_handoffs() {
        let show = json!({
            "run_id": "run-live", "goal": "refuse redirects", "state": "paused", "reason": "paused by the person",
            "handoffs": ["h1"],
            "history": [{"ts": 10.0, "state": "plan"}, {"ts": 20.5, "state": "execute", "reason": "x"}, {"state": "bad"}],
            "plan": {"steps": [
                {"step_id": "s1", "name": "guard", "status": "running", "worker": "claude_code:fast", "attempts": 2, "file_scope": ["src/**"]},
                {"step_id": "s2", "name": "tests", "status": "pending"},
            ]},
        });
        let run = parse(&show, &status()).unwrap();
        assert_eq!((run.state.as_str(), run.steps.len(), run.handoffs.len()), ("paused", 2, 1));
        assert_eq!(run.steps[0], Step { id: "s1".into(), name: "guard".into(), status: "running".into(), worker: Some("claude_code:fast".into()), attempts: 2, scope: vec!["src/**".into()] });
        assert_eq!(run.steps[1].worker, None);
        assert_eq!((run.handoffs[0].id.as_str(), run.handoffs[0].to.as_deref()), ("h1", Some("antigravity:x")));
        assert_eq!(run.history, vec![(10.0, "plan".to_owned()), (20.5, "execute".to_owned())]);
        assert!(run.can_resume() && !run.can_hold() && !run.finished());
    }

    #[test]
    fn a_record_without_an_id_is_not_a_run_and_a_bare_one_is_empty() {
        assert!(parse(&json!({"goal": "x"}), &json!({})).is_none());
        let run = parse(&json!({"run_id": "r", "state": "done"}), &json!({})).unwrap();
        assert!(run.steps.is_empty() && run.finished() && !run.can_hold() && !run.can_resume());
    }

    #[test]
    fn holding_appends_one_line_the_run_understands_and_refuses_odd_ids() {
        let dir = tempfile::tempdir().unwrap();
        request_hold(dir.path(), "run-abc_1", "window").unwrap();
        request_hold(dir.path(), "run-abc_1", "window").unwrap();
        let text = std::fs::read_to_string(control_file(dir.path(), "run-abc_1")).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0]["cmd"].as_str(), lines[0]["by"].as_str(), lines[0]["restore"].as_bool()), (Some("pause"), Some("window"), Some(false)));
        for bad in ["", "../x", "a/b", "run id"] {
            assert!(request_hold(dir.path(), bad, "window").is_err(), "{bad}");
        }
    }

    #[test]
    fn reasons_have_words() {
        assert_eq!(reason_words("user_pause"), "paused by you");
        assert_eq!(reason_words("something_new"), "something_new");
    }
}
