//! The run feed: orchestrated runs as the window can see them, read from `cordata-run status --json` and
//! `cordata-run show <id> --json` (the ledger's own record; no model call).
//!
//! A run is a plan of steps, each with the worker that last ran it. When a worker stops (quota line, rate limit,
//! failure, or the person) the step moves on with a handoff packet; those are listed per run. This module is pure:
//! it parses what the CLI printed and writes the one control line a front door may send. It never starts a run.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    /// Ids of the steps that must finish first (`depends_on`). Empty everywhere: the plan runs top to bottom.
    pub deps: Vec<String>,
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

/// One line under a step's name: what it is doing and who has it, or what it is waiting for.
pub fn step_line(run: &Run, step: &Step) -> String {
    let who = step.worker.as_deref();
    let tries = if step.attempts > 1 { format!(" · try {}", step.attempts) } else { String::new() };
    let with = |word: &str| who.map_or_else(|| word.to_owned(), |w| format!("{word} · {w}"));
    match step.status.as_str() {
        "running" => with("running") + &tries,
        "completed" => with("done") + &tries,
        "failed" => with("failed") + &tries,
        _ => {
            let open: Vec<&str> = step.deps.iter().filter(|d| run.steps.iter().any(|s| &s.id == *d && s.status != "completed")).map(String::as_str).collect();
            if open.is_empty() { "not started".to_owned() } else { format!("waits on {}", open.join(", ")) }
        }
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
            deps: s.get("depends_on").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
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

/// How long after a poll saw a step change the window may still show it moving. Past this the picture is still, so
/// reopening the field does not replay something that happened a while ago.
pub const FRESH: Duration = Duration::from_secs(4);

/// What the polls have seen happen. A run is read every few seconds, not heard, so the only events the window has are
/// the differences between two reads: a step starting, finishing, or changing worker. Each one bumps that step's
/// count; motion is keyed on the count, so nothing moves unless a read differed from the one before.
#[derive(Debug, Default)]
pub struct Pulse {
    steps: HashMap<(String, String), Seen>,
}

/// One step as the last read left it.
#[derive(Debug)]
struct Seen {
    status: String,
    /// Worker and attempt: a handoff or a retry changes these without changing the status.
    hands: String,
    /// How many changes were seen, and when the last one was.
    count: u64,
    at: Option<Instant>,
    /// The last change was the step starting, as opposed to finishing or changing worker.
    started: bool,
}

impl Pulse {
    /// Take one read of the runs. The first sight of a step is not an event: nothing was seen to happen.
    pub fn observe(&mut self, runs: &[Run], now: Instant) {
        self.steps.retain(|(run, _), _| runs.iter().any(|r| &r.run_id == run));
        for run in runs {
            for step in &run.steps {
                let hands = format!("{}|{}", step.worker.as_deref().unwrap_or(""), step.attempts);
                match self.steps.get_mut(&(run.run_id.clone(), step.id.clone())) {
                    Some(seen) if seen.status != step.status || seen.hands != hands => {
                        seen.started = seen.status != step.status && step.status == "running";
                        seen.status.clone_from(&step.status);
                        seen.hands = hands;
                        seen.count += 1;
                        seen.at = Some(now);
                    }
                    Some(_) => {}
                    None => {
                        self.steps.insert((run.run_id.clone(), step.id.clone()), Seen { status: step.status.clone(), hands, count: 0, at: None, started: false });
                    }
                }
            }
        }
    }

    /// How many changes this step has been seen to make.
    pub fn count(&self, run_id: &str, step_id: &str) -> u64 {
        self.steps.get(&(run_id.to_owned(), step_id.to_owned())).map_or(0, |seen| seen.count)
    }

    /// The step's change count, while its last change is fresh enough to show; `None` means draw it still.
    pub fn fresh(&self, run_id: &str, step_id: &str, now: Instant) -> Option<u64> {
        let seen = self.steps.get(&(run_id.to_owned(), step_id.to_owned()))?;
        seen.at.filter(|at| now.duration_since(*at) < FRESH).map(|_| seen.count)
    }

    /// As [`Pulse::fresh`], but only when that last change was the step starting: the moment work reached it.
    pub fn started(&self, run_id: &str, step_id: &str, now: Instant) -> Option<u64> {
        self.fresh(run_id, step_id, now).filter(|_| self.steps.get(&(run_id.to_owned(), step_id.to_owned())).is_some_and(|seen| seen.started))
    }
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
                {"step_id": "s2", "name": "tests", "status": "pending", "depends_on": ["s1"]},
            ]},
        });
        let run = parse(&show, &status()).unwrap();
        assert_eq!((run.state.as_str(), run.steps.len(), run.handoffs.len()), ("paused", 2, 1));
        assert_eq!(run.steps[0], Step { id: "s1".into(), name: "guard".into(), status: "running".into(), worker: Some("claude_code:fast".into()), attempts: 2, scope: vec!["src/**".into()], deps: Vec::new() });
        assert_eq!(run.steps[1].worker, None);
        assert_eq!(run.steps[1].deps, vec!["s1".to_owned()]);
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

    fn one_step(status: &str, worker: Option<&str>) -> Run {
        Run {
            run_id: "r".into(),
            goal: String::new(),
            state: "execute".into(),
            reason: String::new(),
            steps: vec![Step { id: "s1".into(), name: "n".into(), status: status.into(), worker: worker.map(str::to_owned), attempts: 1, scope: Vec::new(), deps: Vec::new() }],
            handoffs: Vec::new(),
            history: Vec::new(),
        }
    }

    #[test]
    fn a_pulse_counts_only_what_two_reads_disagree_on() {
        let (mut pulse, t0) = (Pulse::default(), Instant::now());
        pulse.observe(&[one_step("pending", None)], t0);
        assert_eq!(pulse.fresh("r", "s1", t0), None, "first sight is not an event");
        pulse.observe(&[one_step("pending", None)], t0);
        assert_eq!(pulse.fresh("r", "s1", t0), None, "the same read twice is not an event");
        pulse.observe(&[one_step("running", Some("a"))], t0);
        assert_eq!(pulse.fresh("r", "s1", t0), Some(1), "it started");
        assert_eq!(pulse.started("r", "s1", t0), Some(1), "and that change was a start");
        pulse.observe(&[one_step("running", Some("b"))], t0 + Duration::from_secs(3));
        assert_eq!(pulse.fresh("r", "s1", t0 + Duration::from_secs(3)), Some(2), "its worker changed");
        assert_eq!(pulse.started("r", "s1", t0 + Duration::from_secs(3)), None, "a handoff is not work arriving");
    }

    #[test]
    fn a_pulse_goes_still_after_a_while_and_forgets_runs_that_left() {
        let (mut pulse, t0) = (Pulse::default(), Instant::now());
        pulse.observe(&[one_step("pending", None)], t0);
        pulse.observe(&[one_step("completed", Some("a"))], t0);
        assert_eq!(pulse.fresh("r", "s1", t0 + FRESH), None, "old news is drawn still");
        pulse.observe(&[], t0);
        pulse.observe(&[one_step("running", None)], t0);
        assert_eq!(pulse.fresh("r", "s1", t0), None, "a run that came back is first sight again");
    }

    #[test]
    fn a_step_says_what_it_is_doing_or_waiting_for() {
        let mut run = one_step("completed", Some("claude_code:haiku"));
        run.steps.push(Step { id: "s2".into(), name: "n".into(), status: "pending".into(), worker: None, attempts: 0, scope: Vec::new(), deps: vec!["s1".into()] });
        assert_eq!(step_line(&run, &run.steps[0]), "done · claude_code:haiku");
        assert_eq!(step_line(&run, &run.steps[1]), "not started", "what it waited on is done");
        run.steps[0].status = "running".into();
        run.steps[0].attempts = 3;
        assert_eq!(step_line(&run, &run.steps[0]), "running · claude_code:haiku · try 3");
        assert_eq!(step_line(&run, &run.steps[1]), "waits on s1");
    }

    #[test]
    fn reasons_have_words() {
        assert_eq!(reason_words("user_pause"), "paused by you");
        assert_eq!(reason_words("something_new"), "something_new");
    }
}
