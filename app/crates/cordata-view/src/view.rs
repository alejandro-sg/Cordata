//! Small pieces of presentation *logic* every front end needs and none should re-derive: which kind of block a message
//! is, whether a finished task is still worth showing, which self-heal phases get a block, which options of an
//! `ask_user` card are its numbered choices. No drawing here.

use std::time::{Duration, Instant};

use acp_core::TaskInfo;
use serde_json::Value;

use crate::app::Message;

/// A transcript block's kind (spacing, colour and when it may be committed all key on it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    User,
    Laya,
    LayaOutcome,
    Agent,
    Thought,
    Tool,
    Divider,
    Info,
    Diff,
    Error,
    Help,
    Welcome,
    Heal,
}

impl Message {
    pub fn kind(&self) -> Kind {
        match self {
            Message::User(_) => Kind::User,
            Message::Laya(_) => Kind::Laya,
            Message::LayaOutcome(_) => Kind::LayaOutcome,
            Message::Agent(_) => Kind::Agent,
            Message::Thought(_) => Kind::Thought,
            Message::Tool(_) => Kind::Tool,
            Message::Divider(_) => Kind::Divider,
            Message::Info(_) => Kind::Info,
            Message::Diff(_) => Kind::Diff,
            Message::Error(_) => Kind::Error,
            Message::Help => Kind::Help,
            Message::Heal(_) => Kind::Heal,
            Message::Welcome(_) => Kind::Welcome,
        }
    }
}

/// How long a finished background task stays on screen, so a fast one is still seen.
pub const LINGER: Duration = Duration::from_millis(2500);

/// A background task (plugin start-up, an indexing language server, a self-heal step, a model loading) and when it
/// started and finished.
#[derive(Debug, Clone)]
pub struct TaskView {
    pub info: TaskInfo,
    pub started: Instant,
    pub finished: Option<Instant>,
}

impl TaskView {
    pub fn visible(&self, now: Instant) -> bool {
        self.finished.is_none_or(|at| now.duration_since(at) < LINGER)
    }
}

/// Self-heal phases that are worth a transcript block (the others are progress bars).
pub fn is_shown(phase: &str) -> bool {
    !matches!(phase, "diagnosing" | "verifying")
}

/// The choices of an `ask_user` question, in the order the agent gave them. The backend marks them `reject_always` so that a
/// front door which only knows ACP's four kinds (the Discord bot, the GUI's modal) can never turn a click into an answer;
/// "decide for me" and "skip" carry the other kinds and are not numbered.
pub fn ask_choices(info: &acp_core::PermissionInfo) -> Vec<acp_core::PermissionChoice> {
    info.options.iter().filter(|option| option.kind == acp_core::ChoiceKind::RejectAlways).cloned().collect()
}

/// One line saying what a tool call does: `$ command` for a shell, the code for a REPL, the task for a sub-agent team,
/// the raw JSON otherwise. While a call streams in, its input is the JSON text so far. Sanitized (no escapes).
pub fn summarize_input(value: &Value, is_team: bool) -> Option<String> {
    let text = match value {
        Value::Null => return None,
        Value::String(raw) if raw.is_empty() => return None,
        // While a call streams in, its input is the JSON *text* so far. Once it
        // parses, summarise the parsed object; until then a team call is still
        // being written (its input is long), anything else shows the raw text.
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(parsed @ Value::Object(_)) => return summarize_input(&parsed, is_team),
            _ if is_team => return Some("(preparing the team…)".to_owned()),
            _ => raw.clone(),
        },
        // A team tool's input is a task plus every specialist's full system
        // prompt; the task is what a reader wants (the agents get their own rows).
        // `subagent`: `{task, agent_type, …}` — the task, and which role it went to.
        Value::Object(map) if is_team && map.get("agents").is_none() && map.get("task").is_some() => {
            let task = map.get("task").and_then(Value::as_str).unwrap_or_default();
            match map.get("agent_type").and_then(Value::as_str) {
                Some(role) => format!("{role}: {task}"),
                None => format!("task: {task}"),
            }
        }
        Value::Object(map) if map.get("agents").is_some() && map.get("task").is_some() => {
            format!("task: {}", map.get("task").and_then(Value::as_str).unwrap_or_default())
        }
        Value::Object(map) => {
            let command = map.get("command").and_then(Value::as_str);
            let code = map.get("code").and_then(Value::as_str);
            match (command, code) {
                (Some(command), _) => format!("$ {command}"),
                (_, Some(code)) => code.to_owned(),
                _ => value.to_string(),
            }
        }
        other => other.to_string(),
    };
    Some(crate::app::sanitize(&text))
}
