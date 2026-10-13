//! The session conveniences that arrive with this client: the context gauge, the agent's checklist, `/goal`, `/btw`,
//! `/compact`, `/context`, `/cost`, `/copy`, `/export` and `/diff`.
//!
//! Most of them are thin: the backend does the work (`session_extras.py`, `goal.py`, `task_tool.py`) and this file
//! turns its answers into a line, a gauge or a panel. Nothing here sends anything to a model on its own.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::app::{App, Effect, Message, sanitize};

/// How full the conversation is, as last reported.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextGauge {
    pub used: u64,
    pub window: Option<u64>,
    pub pct: Option<f64>,
    pub estimated: bool,
}

impl ContextGauge {
    pub fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            used: value.get("used")?.as_u64()?,
            window: value.get("window").and_then(Value::as_u64),
            pct: value.get("pct").and_then(Value::as_f64),
            estimated: value.get("estimated").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// A goal being pursued, for the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalView {
    pub condition: String,
    pub turn: u64,
    pub max: u64,
    /// `checking` while the harness decides whether it is met; otherwise `working`.
    pub checking: bool,
    pub how: String,
}

/// How long a finished checklist stays on screen before it folds into one line of scrollback.
pub const CHECKLIST_LINGER: Duration = Duration::from_secs(4);

/// `▰▰▰▱▱▱` — `cells` wide, `pct` filled.
pub fn meter(pct: f64, cells: usize) -> String {
    let filled = ((pct.clamp(0.0, 1.0) * cells as f64).round() as usize).min(cells);
    format!("{}{}", "▰".repeat(filled), "▱".repeat(cells - filled))
}

/// "12400" → "12.4k".
pub fn short_count(n: u64) -> String {
    match n {
        1_000_000.. => format!("{:.1}M", n as f64 / 1_000_000.0),
        10_000..=999_999 => format!("{}k", n / 1000),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1000.0),
        _ => n.to_string(),
    }
}

pub fn describe_context(gauge: &ContextGauge) -> String {
    let approx = if gauge.estimated { "~" } else { "" };
    match (gauge.window, gauge.pct) {
        (Some(window), Some(pct)) => format!(
            "context  {} {approx}{:.0}%  ({approx}{} of {} tokens)",
            meter(pct, 20),
            pct * 100.0,
            short_count(gauge.used),
            short_count(window)
        ),
        _ => format!("context  {approx}{} tokens (this model's window is unknown)", short_count(gauge.used)),
    }
}

impl App {
    // ------------------------------------------------------------ events

    pub fn on_context(&mut self, value: &Value) {
        self.context = ContextGauge::from_json(value);
        // Once, when it gets tight — not on every turn after.
        let high = self.context.as_ref().and_then(|g| g.pct).is_some_and(|p| p >= 0.8);
        if high && !self.warned_context {
            self.warned_context = true;
            self.info("context is over 80% full — /compact summarises the older turns (the agent also does it automatically near 70%)");
        } else if !high {
            self.warned_context = false;
        }
    }

    pub fn on_checklist(&mut self, value: &Value) {
        let items: Vec<TodoItem> = value
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let content = sanitize(item.get("content")?.as_str()?);
                let status = match item.get("status")?.as_str()? {
                    "completed" => TodoStatus::Completed,
                    "in_progress" => TodoStatus::InProgress,
                    _ => TodoStatus::Pending,
                };
                Some(TodoItem { content, status })
            })
            .collect();
        self.checklist_done_at = (!items.is_empty() && items.iter().all(|i| i.status == TodoStatus::Completed)).then(Instant::now);
        self.checklist = items;
    }

    /// Fold a finished checklist into one line once it has been on screen long enough to be read.
    pub fn settle_checklist(&mut self) {
        if self.checklist_done_at.is_some_and(|at| at.elapsed() >= CHECKLIST_LINGER) {
            let done = self.checklist.len();
            self.checklist.clear();
            self.checklist_done_at = None;
            self.info(format!("☑ plan complete · {done} step{}", if done == 1 { "" } else { "s" }));
        }
    }

    pub fn on_goal(&mut self, value: &Value) {
        let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let number = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
        let (turn, max) = (number("turn"), number("max"));
        let how = text("how");
        let reason = sanitize(&text("reason"));
        let condition = sanitize(&text("condition"));
        let view = |checking: bool| GoalView { condition: condition.clone(), turn, max, checking, how: how.clone() };
        match text("event").as_str() {
            "set" => {
                self.goal = Some(view(false));
                let decided = if how == "check" {
                    format!("decided by `{}`", sanitize(&text("check")))
                } else {
                    "judged from what each turn did".to_owned()
                };
                self.info(format!("◎ goal: {condition}\n  {decided} · up to {max} follow-up turns · /goal clear stops it"));
            }
            "checking" => self.goal = Some(view(true)),
            "continue" => {
                self.goal = Some(view(false));
                let first = reason.lines().next().unwrap_or_default();
                self.info(format!("◎ goal {turn}/{max} — not yet: {first}"));
            }
            "met" => {
                self.goal = None;
                self.info(format!("◎ goal met after {} turn{} — {}", turn + 1, if turn == 0 { "" } else { "s" }, reason.lines().next().unwrap_or_default()));
            }
            "exhausted" => {
                self.goal = None;
                self.info(format!("◎ goal not met after {max} follow-up turns — {}\n  /goal {condition} --turns {} tries again with a bigger budget", reason.lines().next().unwrap_or_default(), max * 2));
            }
            _ => {
                self.goal = None;
                self.info(format!("◎ goal stopped — {}", if reason.is_empty() { "cleared" } else { reason.lines().next().unwrap_or_default() }));
            }
        }
    }

    // ---------------------------------------------------------- commands

    /// `/goal`: with text, a prompt the backend turns into a goal (so it works the same in every front door);
    /// `clear` and no argument ask the backend directly, because they must work while a turn is running.
    pub fn run_goal(&mut self, arg: Option<String>) -> Option<Effect> {
        match arg.as_deref().map(str::trim) {
            None | Some("") => Some(ext_effect("_cordata/goal/status", "goal")),
            Some("clear" | "stop" | "off" | "cancel") => Some(ext_effect("_cordata/goal/clear", "goal clear")),
            Some(text) => self.run_line_as_prompt(&format!("/goal {text}")),
        }
    }

    pub fn run_btw(&mut self, arg: Option<String>) -> Option<Effect> {
        let Some(question) = arg.filter(|q| !q.trim().is_empty()) else {
            self.info("/btw <question> — a side question, answered from the conversation without touching it (works while the agent is busy)");
            return None;
        };
        self.info(format!("btw ▸ {}", sanitize(&question)));
        Some(Effect::Ext { method: "_cordata/btw".into(), params: serde_json::json!({ "question": question }), tag: "btw".into() })
    }

    pub fn run_compact(&mut self, arg: Option<String>) -> Option<Effect> {
        if self.turn_active {
            self.info("the agent is working — /compact once it has finished (or Esc to stop it)");
            return None;
        }
        self.info("compacting…");
        Some(Effect::Ext { method: "_cordata/context/compact".into(), params: serde_json::json!({ "focus": arg.unwrap_or_default() }), tag: "compact".into() })
    }

    pub fn run_cost(&mut self) -> Option<Effect> {
        let (tin, tout) = self.session_tokens;
        let mut lines = vec![format!(
            "this session  {} turn{} · {} in · {} out",
            self.turns_done,
            if self.turns_done == 1 { "" } else { "s" },
            short_count(tin),
            short_count(tout)
        )];
        if let Some(gauge) = &self.context {
            lines.push(describe_context(gauge));
        }
        self.info(lines.join("\n"));
        Some(Effect::Ext { method: "_cordata/orchestrator/status".into(), params: Value::Null, tag: "orchestrator".into() })
    }

    pub fn run_copy(&mut self) -> Option<Effect> {
        let last = self.messages.iter().rev().find_map(|m| match m {
            Message::Agent(text) if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        });
        match last {
            Some(text) => {
                self.info(format!("copied the last reply ({} characters)", text.chars().count()));
                Some(Effect::Copy(text))
            }
            None => {
                self.info("nothing to copy yet");
                None
            }
        }
    }

    pub fn run_export(&mut self, arg: Option<String>) {
        let id: String = self.session.as_ref().map(|s| s.session_id.chars().take(8).collect()).unwrap_or_else(|| "session".into());
        let name = arg.filter(|a| !a.trim().is_empty()).unwrap_or_else(|| format!("cordata-{id}.md"));
        let path = if std::path::Path::new(&name).is_absolute() { std::path::PathBuf::from(&name) } else { self.project_dir().join(&name) };
        match std::fs::write(&path, self.transcript_markdown()) {
            Ok(()) => self.info(format!("saved the conversation to {}", path.display())),
            Err(error) => self.messages.push(Message::Error(format!("could not write {}: {error}", path.display()))),
        }
    }

    /// The conversation as markdown: what was asked, what was answered, which tools ran. Reasoning and the harness's
    /// own notices are left out — this is what you would hand a colleague.
    pub fn transcript_markdown(&self) -> String {
        let mut out = String::from("# Cordata session\n");
        if let Some(session) = &self.session {
            out.push_str(&format!("\n`{}` · {}\n", session.session_id, session.cwd));
        }
        for message in &self.messages {
            match message {
                Message::User(text) => out.push_str(&format!("\n## ❯ {}\n", text.trim().replace('\n', "\n> "))),
                Message::Agent(text) => out.push_str(&format!("\n{}\n", text.trim())),
                Message::Tool(card) if card.name != "todo_write" => {
                    let detail = card
                        .raw_input
                        .as_ref()
                        .and_then(|input| input.get("command").or_else(|| input.get("path")).or_else(|| input.get("query")))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    out.push_str(&format!("\n- `{}` {}\n", card.name, if detail.is_empty() { String::new() } else { format!("`{}`", detail.lines().next().unwrap_or_default()) }));
                }
                _ => {}
            }
        }
        out
    }

    /// `/diff`: what is changed in the working tree, as the person's own command (not shared with the agent).
    pub fn run_diff(&mut self) -> Option<Effect> {
        Some(Effect::Shell {
            command: "git --no-pager diff --stat HEAD 2>&1 | tail -n 40; echo; git --no-pager diff HEAD --unified=2 2>&1 | head -n 400".into(),
            share: false,
            as_diff: true,
        })
    }

    pub fn run_line_as_prompt(&mut self, line: &str) -> Option<Effect> {
        if self.turn_active {
            self.queue.push_back(line.to_owned());
            return None;
        }
        self.messages.push(Message::User(sanitize(line)));
        Some(self.begin_turn(line.to_owned()))
    }
}

/// The branch the project is on (`None` outside a repository or on a detached HEAD).
pub fn git_branch(dir: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// A prompt worth offering after a turn, from what the turn did — no model call, so it costs nothing and is there
/// the moment the turn ends. `None` when nothing obvious follows.
pub fn suggest_next(messages: &[Message]) -> Option<String> {
    use acp_core::{ToolKind, ToolStatus};
    let start = messages.iter().rposition(|m| matches!(m, Message::User(_)))?;
    let cards: Vec<&crate::app::ToolCard> = messages[start..]
        .iter()
        .filter_map(|m| match m {
            Message::Tool(card) if card.name != "todo_write" => Some(card),
            _ => None,
        })
        .collect();
    let is_edit = |c: &&crate::app::ToolCard| matches!(c.kind, ToolKind::Edit) || matches!(c.name.as_str(), "file_write" | "editor" | "apply_patch");
    let last_edit = cards.iter().rposition(is_edit)?;
    let command = |c: &crate::app::ToolCard| c.raw_input.as_ref().and_then(|i| i.get("command")).and_then(Value::as_str).unwrap_or_default().to_lowercase();
    let looks_like_a_check = |c: &&crate::app::ToolCard| {
        let command = command(c);
        ["test", "pytest", "cargo check", "cargo build", "clippy", "lint", "tsc", "mypy", "ruff", "make check"].iter().any(|k| command.contains(k))
    };
    let after = &cards[last_edit + 1..];
    match after.iter().rev().find(|c| looks_like_a_check(c)) {
        Some(check) if matches!(check.status, ToolStatus::Failed) => Some("fix the failures above".to_owned()),
        Some(_) => Some("commit these changes with a clear message".to_owned()),
        None if after.iter().any(|c| matches!(c.status, ToolStatus::Failed)) => Some("fix the error above".to_owned()),
        None => Some("run the tests and show me the result".to_owned()),
    }
}

pub fn ext_effect(method: &str, tag: &str) -> Effect {
    Effect::Ext { method: method.into(), params: Value::Null, tag: tag.into() }
}

/// Text for the replies of the extension methods behind these commands.
pub fn format_reply(tag: &str, value: &Value) -> Option<String> {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
    match tag {
        "btw" => Some(if ok { format!("btw ◂ {}", text(value, "answer")) } else { format!("btw: {}", text(value, "reason")) }),
        "compact" => {
            if !ok {
                return Some(format!("compact: {}", text(value, "reason")));
            }
            let (before, after) = (ContextGauge::from_json(value.get("before")?)?, ContextGauge::from_json(value.get("after")?)?);
            let messages = |v: &Value| v.get("messages").and_then(Value::as_u64).unwrap_or(0);
            let pct = |g: &ContextGauge| g.pct.map_or_else(|| short_count(g.used), |p| format!("{:.0}%", p * 100.0));
            Some(format!(
                "compacted · {} → {} messages · context {} → ~{}",
                messages(value.get("before")?),
                messages(value.get("after")?),
                pct(&before),
                pct(&after)
            ))
        }
        "goal" => {
            let goal = value.get("goal")?;
            if goal.is_null() {
                return Some("no active goal — /goal <condition> [--check <command>] starts one".into());
            }
            Some(format!(
                "goal ({}): {}\n  {} · follow-up turn {}/{}{}",
                text(goal, "state"),
                text(goal, "condition"),
                if goal.get("how").and_then(Value::as_str) == Some("check") { format!("checked by `{}`", text(goal, "check")) } else { "judged from what each turn did".into() },
                goal.get("turn").and_then(Value::as_u64).unwrap_or(0),
                goal.get("max").and_then(Value::as_u64).unwrap_or(0),
                { let r = text(goal, "reason"); if r.is_empty() { String::new() } else { format!("\n  last: {}", r.lines().next().unwrap_or_default()) } }
            ))
        }
        "goal clear" => Some(if value.get("cleared").and_then(Value::as_bool).unwrap_or(false) { "goal cleared".into() } else { "there was no active goal".into() }),
        "context" => ContextGauge::from_json(value).map(|g| describe_context(&g)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn card(name: &str, kind: acp_core::ToolKind, status: acp_core::ToolStatus, command: Option<&str>) -> Message {
        Message::Tool(crate::app::ToolCard {
            id: name.into(),
            name: name.into(),
            kind,
            status,
            raw_input: command.map(|c| json!({"command": c})),
            content: String::new(),
            progress: String::new(),
            untrusted: false,
            untrusted_source: false,
            diffs: vec![],
            p_ok: None,
            outcome: None,
        })
    }

    #[test]
    fn what_follows_a_turn_depends_on_what_the_turn_did() {
        use acp_core::{ToolKind::*, ToolStatus::*};
        let user = || Message::User("fix it".into());
        assert_eq!(suggest_next(&[user(), Message::Agent("hi".into())]), None, "no edits: nothing obvious follows");
        assert_eq!(
            suggest_next(&[user(), card("file_write", Edit, Completed, None)]).as_deref(),
            Some("run the tests and show me the result"),
        );
        assert_eq!(
            suggest_next(&[user(), card("file_write", Edit, Completed, None), card("shell", Execute, Failed, Some("cargo test -q"))]).as_deref(),
            Some("fix the failures above"),
        );
        assert_eq!(
            suggest_next(&[user(), card("file_write", Edit, Completed, None), card("shell", Execute, Completed, Some("pytest"))]).as_deref(),
            Some("commit these changes with a clear message"),
        );
        assert_eq!(
            suggest_next(&[user(), card("file_write", Edit, Completed, None), card("shell", Execute, Failed, Some("ls nope"))]).as_deref(),
            Some("fix the error above"),
        );
        // A check that ran *before* the last edit says nothing about it.
        assert_eq!(
            suggest_next(&[user(), card("shell", Execute, Completed, Some("pytest")), card("file_write", Edit, Completed, None)]).as_deref(),
            Some("run the tests and show me the result"),
        );
        // Only this turn counts.
        assert_eq!(suggest_next(&[user(), card("file_write", Edit, Completed, None), Message::User("thanks".into()), Message::Agent("np".into())]), None);
    }

    #[test]
    fn the_meter_fills_proportionally() {
        assert_eq!(meter(0.0, 6), "▱▱▱▱▱▱");
        assert_eq!(meter(0.5, 6), "▰▰▰▱▱▱");
        assert_eq!(meter(2.0, 4), "▰▰▰▰");
        assert_eq!(short_count(999), "999");
        assert_eq!(short_count(1500), "1.5k");
        assert_eq!(short_count(128_000), "128k");
        assert_eq!(short_count(2_500_000), "2.5M");
    }

    #[test]
    fn the_context_reads_as_a_gauge_with_real_numbers() {
        let gauge = ContextGauge::from_json(&json!({"used": 45_000, "window": 128_000, "pct": 0.352, "estimated": false})).unwrap();
        let text = describe_context(&gauge);
        assert!(text.contains("35%") && text.contains("45k of 128k tokens") && text.contains("▰"), "{text}");
        let unknown = ContextGauge::from_json(&json!({"used": 900, "window": null, "pct": null, "estimated": true})).unwrap();
        assert!(describe_context(&unknown).contains("window is unknown"));
        assert!(ContextGauge::from_json(&json!({"window": 5})).is_none());
    }

    #[test]
    fn a_checklist_replaces_itself_and_folds_into_a_line_when_done() {
        let mut app = App::new();
        app.on_checklist(&json!({"items": [
            {"content": "read", "status": "completed"},
            {"content": "write", "status": "in_progress"},
            {"content": "test", "status": "pending"},
        ]}));
        assert_eq!(app.checklist.len(), 3);
        assert_eq!(app.checklist[1].status, TodoStatus::InProgress);
        assert!(app.checklist_done_at.is_none());
        app.on_checklist(&json!({"items": [{"content": "read", "status": "completed"}, {"content": "write", "status": "completed"}]}));
        assert!(app.checklist_done_at.is_some(), "all done starts the countdown");
        app.settle_checklist();
        assert_eq!(app.checklist.len(), 2, "not before it has been readable for a moment");
        app.checklist_done_at = Some(Instant::now() - CHECKLIST_LINGER - Duration::from_millis(1));
        app.settle_checklist();
        assert!(app.checklist.is_empty());
        assert!(matches!(app.messages.last(), Some(Message::Info(t)) if t.contains("plan complete · 2 steps")));
    }

    #[test]
    fn a_goal_is_followed_step_by_step_in_the_transcript_and_the_footer() {
        let mut app = App::new();
        app.on_goal(&json!({"event": "set", "condition": "tests pass", "check": "make test", "how": "check", "turn": 0, "max": 8}));
        assert!(matches!(app.messages.last(), Some(Message::Info(t)) if t.contains("goal: tests pass") && t.contains("`make test`") && t.contains("up to 8")));
        app.on_goal(&json!({"event": "checking", "condition": "tests pass", "how": "check", "turn": 0, "max": 8}));
        assert!(app.goal.as_ref().is_some_and(|g| g.checking));
        app.on_goal(&json!({"event": "continue", "condition": "tests pass", "how": "check", "turn": 1, "max": 8, "reason": "`make test` exited 2:\n3 failed"}));
        assert!(matches!(app.messages.last(), Some(Message::Info(t)) if t == "◎ goal 1/8 — not yet: `make test` exited 2:"), "only the first line: {:?}", app.messages.last());
        assert!(app.goal.as_ref().is_some_and(|g| !g.checking && g.turn == 1));
        app.on_goal(&json!({"event": "met", "condition": "tests pass", "how": "check", "turn": 1, "max": 8, "reason": "`make test` exited 0"}));
        assert!(app.goal.is_none());
        assert!(matches!(app.messages.last(), Some(Message::Info(t)) if t.contains("met after 2 turns")));
        app.on_goal(&json!({"event": "exhausted", "condition": "tests pass", "turn": 8, "max": 8, "reason": "still failing"}));
        assert!(matches!(app.messages.last(), Some(Message::Info(t)) if t.contains("not met after 8 follow-up turns") && t.contains("--turns 16")));
    }

    #[test]
    fn the_context_warning_is_said_once() {
        let mut app = App::new();
        app.on_context(&json!({"used": 90, "window": 100, "pct": 0.9}));
        app.on_context(&json!({"used": 95, "window": 100, "pct": 0.95}));
        let warnings = app.messages.iter().filter(|m| matches!(m, Message::Info(t) if t.contains("over 80%"))).count();
        assert_eq!(warnings, 1);
        app.on_context(&json!({"used": 10, "window": 100, "pct": 0.1}));
        app.on_context(&json!({"used": 90, "window": 100, "pct": 0.9}));
        assert_eq!(app.messages.iter().filter(|m| matches!(m, Message::Info(t) if t.contains("over 80%"))).count(), 2, "again after it fell");
    }

    #[test]
    fn replies_read_as_sentences() {
        assert_eq!(format_reply("btw", &json!({"ok": true, "answer": "We chose X."})).unwrap(), "btw ◂ We chose X.");
        assert!(format_reply("btw", &json!({"ok": false, "reason": "no model"})).unwrap().contains("no model"));
        let compacted = format_reply("compact", &json!({"ok": true,
            "before": {"used": 90_000, "window": 100_000, "pct": 0.9, "messages": 40, "estimated": false},
            "after": {"used": 9_000, "window": 100_000, "pct": 0.09, "messages": 6, "estimated": true}})).unwrap();
        assert_eq!(compacted, "compacted · 40 → 6 messages · context 90% → ~9%");
        assert!(format_reply("compact", &json!({"ok": false, "reason": "a turn is running"})).unwrap().contains("a turn is running"));
        assert!(format_reply("goal", &json!({"goal": null})).unwrap().contains("no active goal"));
        let status = format_reply("goal", &json!({"goal": {"state": "active", "condition": "x", "check": "make", "how": "check", "turn": 2, "max": 8, "reason": "failing"}})).unwrap();
        assert!(status.contains("goal (active): x") && status.contains("`make`") && status.contains("2/8") && status.contains("last: failing"), "{status}");
        assert_eq!(format_reply("goal clear", &json!({"cleared": true})).unwrap(), "goal cleared");
    }

    #[test]
    fn the_transcript_exports_as_markdown_without_the_noise() {
        let mut app = App::new();
        app.messages.push(Message::User("fix it".into()));
        app.messages.push(Message::Thought("hmm".into()));
        app.messages.push(Message::Info("a notice".into()));
        app.messages.push(Message::Agent("Done — see `a.rs`.".into()));
        let markdown = app.transcript_markdown();
        assert!(markdown.contains("## ❯ fix it") && markdown.contains("Done — see `a.rs`."), "{markdown}");
        assert!(!markdown.contains("hmm") && !markdown.contains("a notice"));
    }
}
