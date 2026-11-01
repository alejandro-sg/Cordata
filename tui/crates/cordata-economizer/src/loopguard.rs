//! Failure-loop interception: the same call with the same outcome, over and over, or an edit that
//! undoes the previous one.

use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;

use ahash::RandomState;
use regex::Regex;
use serde_json::Value;

/// Repeats (same call, same outcome) at which the model is warned.
pub const NUDGE_AT: usize = 3;
/// Repeats at which the call is refused. The call that would be the fifth is blocked before it runs.
pub const REFUSE_AT: usize = 5;
/// Distinct calls remembered.
const WINDOW: usize = 64;
/// Volatile-substring scrubbing looks only at this much of each end of an output.
const SCAN_CHARS: usize = 2_048;

/// A tool call as the guard sees it.
#[derive(Debug, Clone, Copy)]
pub struct ToolCall<'a> {
    pub name: &'a str,
    pub args: &'a Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    Allow,
    /// Run it, but append this to the output.
    Nudge(String),
    /// Do not run it; this is the result the model gets.
    Refuse(String),
}

static VOLATILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?", // timestamps
        r"|\b\d{1,2}:\d{2}:\d{2}(?:\.\d+)?\b",                                       // clock times
        r"|\b\d+(?:\.\d+)?\s?(?:ms|µs|us|ns|s|secs?|seconds?)\b",                     // durations
        r"|0x[0-9a-fA-F]+",                                                           // addresses
        r"|(?i:pid|tid)[ =:]+\d+",                                                    // process ids
        r"|\.tmp[A-Za-z0-9]{4,}",                                                     // temp dir names
    ))
    .expect("static regex")
});

fn state() -> RandomState {
    // Fixed seeds: digests only live in memory but must agree across the two sides of a comparison.
    RandomState::with_seeds(0x9e37_79b9, 0x7f4a_7c15, 0xf39c_c060, 0x5ced_c834)
}

/// Replaces timestamps, durations, addresses and pids with `~`, looking at the head and tail only.
pub fn normalize(text: &str) -> String {
    let count = text.chars().count();
    let scanned: String = if count <= SCAN_CHARS * 2 {
        text.to_owned()
    } else {
        let head: String = text.chars().take(SCAN_CHARS).collect();
        let tail: String = text.chars().skip(count - SCAN_CHARS).collect();
        format!("{head}\u{0}{count}\u{0}{tail}")
    };
    VOLATILE.replace_all(&scanned, "~").into_owned()
}

#[derive(Debug, Clone, Copy)]
struct Streak {
    outcome: u64,
    count: usize,
}

#[derive(Debug)]
pub struct LoopGuard {
    hasher: RandomState,
    /// Insertion order of call keys, to bound the window.
    history: VecDeque<u64>,
    /// call key → outcome digest and how many times in a row it came out the same.
    repeat_counts: HashMap<u64, Streak>,
    /// path → the last (target, replacement) edit applied to it.
    last_edit: HashMap<String, (String, String)>,
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self { hasher: state(), history: VecDeque::new(), repeat_counts: HashMap::new(), last_edit: HashMap::new() }
    }
}

impl LoopGuard {
    fn call_key(&self, call: &ToolCall) -> u64 {
        self.hasher.hash_one((call.name, normalize(&call.args.to_string())))
    }

    /// Before running: refuses a call whose last four runs all ended the same way.
    pub fn precheck(&self, call: &ToolCall) -> GuardDecision {
        match self.repeat_counts.get(&self.call_key(call)) {
            Some(streak) if streak.count + 1 >= REFUSE_AT => GuardDecision::Refuse(format!(
                "Refused: `{}` has run {} times with the same arguments and the same outcome. Stop repeating it; \
                 change the arguments or the approach, or tell the user what is blocking you.",
                call.name, streak.count
            )),
            _ => GuardDecision::Allow,
        }
    }

    /// After running: records the outcome and decides whether the model needs a warning.
    pub fn inspect(&mut self, call: &ToolCall, output: &str) -> GuardDecision {
        let key = self.call_key(call);
        let outcome = self.hasher.hash_one((key, normalize(output)));
        let count = match self.repeat_counts.get_mut(&key) {
            Some(streak) if streak.outcome == outcome => {
                streak.count += 1;
                streak.count
            }
            Some(streak) => {
                *streak = Streak { outcome, count: 1 };
                1
            }
            None => {
                self.repeat_counts.insert(key, Streak { outcome, count: 1 });
                self.history.push_back(key);
                if self.history.len() > WINDOW
                    && let Some(old) = self.history.pop_front()
                {
                    self.repeat_counts.remove(&old);
                }
                1
            }
        };

        if count >= REFUSE_AT {
            return GuardDecision::Refuse(format!("Refused: `{}` has now run {count} times with the same outcome. Change strategy.", call.name));
        }
        if count >= NUDGE_AT {
            return GuardDecision::Nudge(format!(
                "Notice: You have executed this tool {count} times consecutively with the same outcome. You must change strategy."
            ));
        }
        if let Some(note) = self.reversion(call) {
            return GuardDecision::Nudge(note);
        }
        GuardDecision::Allow
    }

    /// An edit that swaps back what the previous edit to the same file swapped in.
    fn reversion(&mut self, call: &ToolCall) -> Option<String> {
        if call.name != "edit_file" {
            return None;
        }
        let field = |k: &str| call.args.get(k).and_then(Value::as_str).map(str::to_owned);
        let (path, target, replacement) = (field("path")?, field("target")?, field("replacement")?);
        let previous = self.last_edit.insert(path.clone(), (target.clone(), replacement.clone()));
        let (old_target, old_replacement) = previous?;
        (!target.is_empty() && target == old_replacement && replacement == old_target).then(|| {
            format!("Notice: this edit to {path} reverts your previous edit to it. Edits that undo each other go nowhere; decide on one version.")
        })
    }

    /// A new prompt: forget everything.
    pub fn reset_turn(&mut self) {
        self.history.clear();
        self.repeat_counts.clear();
        self.last_edit.clear();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn run(args: &Value) -> ToolCall<'_> {
        ToolCall { name: "run_command", args }
    }

    #[test]
    fn nudges_at_three_and_refuses_before_the_fifth_run() {
        let mut guard = LoopGuard::default();
        let args = json!({"cmd":"cargo test"});
        let call = run(&args);
        let mut decisions = Vec::new();
        for _ in 0..4 {
            assert_eq!(guard.precheck(&call), GuardDecision::Allow);
            decisions.push(guard.inspect(&call, "exit 101\nerror[E0432]"));
        }
        assert_eq!(decisions[0], GuardDecision::Allow);
        assert_eq!(decisions[1], GuardDecision::Allow);
        assert!(matches!(&decisions[2], GuardDecision::Nudge(m) if m.contains("3 times consecutively")));
        assert!(matches!(decisions[3], GuardDecision::Nudge(_)));
        assert!(matches!(guard.precheck(&call), GuardDecision::Refuse(m) if m.contains("4 times")));
    }

    #[test]
    fn volatile_substrings_do_not_hide_a_loop() {
        let mut guard = LoopGuard::default();
        let args = json!({"cmd":"make"});
        let call = run(&args);
        let outputs = [
            "2026-11-01T10:00:01Z failed in 12ms at 0x7ffd1000 pid 411",
            "2026-11-01T10:00:09Z failed in 340ms at 0x7ffd2abc pid 977",
            "2026-11-01 10:01:30 failed in 1.5s at 0x1 pid=5",
        ];
        let last = outputs.iter().map(|o| guard.inspect(&call, o)).last().unwrap();
        assert!(matches!(last, GuardDecision::Nudge(_)));
    }

    #[test]
    fn a_different_outcome_restarts_the_count() {
        let mut guard = LoopGuard::default();
        let args = json!({"cmd":"cargo test"});
        let call = run(&args);
        guard.inspect(&call, "fail a");
        guard.inspect(&call, "fail a");
        assert_eq!(guard.inspect(&call, "fail b"), GuardDecision::Allow);
        assert_eq!(guard.inspect(&call, "fail b"), GuardDecision::Allow);
        assert_eq!(guard.precheck(&call), GuardDecision::Allow);
    }

    #[test]
    fn different_arguments_are_different_calls() {
        let mut guard = LoopGuard::default();
        for i in 0..6 {
            let args = json!({"path": format!("f{i}.rs")});
            let call = ToolCall { name: "read_file", args: &args };
            assert_eq!(guard.inspect(&call, "same text"), GuardDecision::Allow);
        }
    }

    #[test]
    fn reverting_an_edit_is_flagged() {
        let mut guard = LoopGuard::default();
        let forward = json!({"path":"a.rs","target":"let x = 1;","replacement":"let x = 2;"});
        let back = json!({"path":"a.rs","target":"let x = 2;","replacement":"let x = 1;"});
        assert_eq!(guard.inspect(&ToolCall { name: "edit_file", args: &forward }, "edited a.rs"), GuardDecision::Allow);
        let decision = guard.inspect(&ToolCall { name: "edit_file", args: &back }, "edited a.rs");
        assert!(matches!(decision, GuardDecision::Nudge(m) if m.contains("reverts")));
    }

    #[test]
    fn reset_turn_forgets() {
        let mut guard = LoopGuard::default();
        let args = json!({"cmd":"x"});
        let call = run(&args);
        for _ in 0..4 {
            guard.inspect(&call, "boom");
        }
        guard.reset_turn();
        assert_eq!(guard.precheck(&call), GuardDecision::Allow);
        assert_eq!(guard.inspect(&call, "boom"), GuardDecision::Allow);
    }

    #[test]
    fn window_is_bounded() {
        let mut guard = LoopGuard::default();
        for i in 0..(WINDOW * 2) {
            let args = json!({ "n": i });
            guard.inspect(&ToolCall { name: "t", args: &args }, "o");
        }
        assert!(guard.repeat_counts.len() <= WINDOW && guard.history.len() <= WINDOW);
    }

    #[test]
    fn normalize_scrubs_and_bounds_huge_outputs() {
        assert_eq!(normalize("took 12ms at 0xdeadbeef"), "took ~ at ~");
        let huge = "line 2026-11-01 10:00:00\n".repeat(100_000);
        let started = std::time::Instant::now();
        let out = normalize(&huge);
        assert!(out.len() < 10_000);
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
    }
}
