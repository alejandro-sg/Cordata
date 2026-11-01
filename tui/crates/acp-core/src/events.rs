//! `AppEvent`: the one vocabulary both binaries consume. This is the single
//! place that encodes backend-specific conventions (tool-kind mapping,
//! untrusted-marker detection, which updates carry replacement vs. appended
//! text) so neither the TUI nor the stress runner re-derives them.

use agent_client_protocol::schema::v1::{
    ContentBlock, SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
    SessionUpdate, StopReason as SchemaStopReason, ToolCallContent,
    ToolCallStatus, ToolKind as SchemaToolKind,
};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::permission::{self, ApprovalDecision, PermissionInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

impl ToolKind {
    pub fn label(self) -> &'static str {
        match self {
            ToolKind::Read => "read",
            ToolKind::Edit => "edit",
            ToolKind::Delete => "delete",
            ToolKind::Move => "move",
            ToolKind::Search => "search",
            ToolKind::Execute => "execute",
            ToolKind::Think => "think",
            ToolKind::Fetch => "fetch",
            ToolKind::SwitchMode => "switch_mode",
            ToolKind::Other => "other",
        }
    }
}

impl From<SchemaToolKind> for ToolKind {
    fn from(kind: SchemaToolKind) -> Self {
        match kind {
            SchemaToolKind::Read => ToolKind::Read,
            SchemaToolKind::Edit => ToolKind::Edit,
            SchemaToolKind::Delete => ToolKind::Delete,
            SchemaToolKind::Move => ToolKind::Move,
            SchemaToolKind::Search => ToolKind::Search,
            SchemaToolKind::Execute => ToolKind::Execute,
            SchemaToolKind::Think => ToolKind::Think,
            SchemaToolKind::Fetch => ToolKind::Fetch,
            SchemaToolKind::SwitchMode => ToolKind::SwitchMode,
            _ => ToolKind::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolStatus {
    pub fn label(self) -> &'static str {
        match self {
            ToolStatus::Pending => "pending",
            ToolStatus::InProgress => "in_progress",
            ToolStatus::Completed => "completed",
            ToolStatus::Failed => "failed",
        }
    }
}

impl From<ToolCallStatus> for ToolStatus {
    fn from(status: ToolCallStatus) -> Self {
        match status {
            ToolCallStatus::Pending => ToolStatus::Pending,
            ToolCallStatus::InProgress => ToolStatus::InProgress,
            ToolCallStatus::Completed => ToolStatus::Completed,
            ToolCallStatus::Failed => ToolStatus::Failed,
            _ => ToolStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    Other(String),
}

impl StopReason {
    pub fn as_str(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::MaxTurnRequests => "max_turn_requests",
            StopReason::Refusal => "refusal",
            StopReason::Cancelled => "cancelled",
            StopReason::Other(other) => other,
        }
    }

    /// `end_turn` is the only reason a scripted run treats as healthy.
    pub fn is_normal(&self) -> bool {
        matches!(self, StopReason::EndTurn)
    }
}

impl From<SchemaStopReason> for StopReason {
    fn from(reason: SchemaStopReason) -> Self {
        match reason {
            SchemaStopReason::EndTurn => StopReason::EndTurn,
            SchemaStopReason::MaxTokens => StopReason::MaxTokens,
            SchemaStopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
            SchemaStopReason::Refusal => StopReason::Refusal,
            SchemaStopReason::Cancelled => StopReason::Cancelled,
            other => StopReason::Other(format!("{other:?}")),
        }
    }
}

/// One file an edit tool call changes: ACP `diff` tool-call content. The
/// backend reads `old_text` from disk before the call runs (None: a new file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub id: String,
    /// The backend titles a tool-call start with the bare tool name.
    pub name: String,
    pub kind: ToolKind,
    pub status: ToolStatus,
    pub raw_input: Option<Value>,
    pub content: String,
    /// The call is to a tool whose output the backend wraps as untrusted.
    pub untrusted_source: bool,
    /// The result text actually contains the untrusted guard markers.
    pub untrusted: bool,
    pub diffs: Vec<FileDiff>,
}

/// A tool-call update. `content`, when present, *replaces* what was shown
/// (swarm progress is a cumulative log; results are final).
#[derive(Debug, Clone, Default)]
pub struct ToolCallPatch {
    pub id: String,
    pub status: Option<ToolStatus>,
    pub content: Option<String>,
    pub untrusted: bool,
    /// The call's complete input (the start event carries only what had streamed).
    pub raw_input: Option<Value>,
    /// What an edit will change, sent before it runs.
    pub diffs: Option<Vec<FileDiff>>,
}

#[derive(Debug, Clone)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConfigChoice {
    pub value: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct ConfigOptionInfo {
    pub id: String,
    pub name: String,
    pub current_value: String,
    pub choices: Vec<ConfigChoice>,
}

impl ConfigOptionInfo {
    pub fn current_name(&self) -> &str {
        self.choices
            .iter()
            .find(|choice| choice.value == self.current_value)
            .map_or(self.current_value.as_str(), |choice| choice.name.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    /// `"stdio"` (this process spawned the backend) or `"ws"` (attached to the
    /// daemon). Shown in the banner: whether a session survives the terminal is
    /// not something a user should have to infer from how they launched.
    pub transport: String,
    pub resumed: bool,
    pub current_mode: String,
    pub modes: Vec<ModeInfo>,
    pub config_options: Vec<ConfigOptionInfo>,
}

/// Harness extension notifications.
pub const TASK_METHOD: &str = "_cordata/task";
pub const HEAL_METHOD: &str = "_cordata/heal";
pub const PLUGINS_METHOD: &str = "_cordata/plugins";
pub const LSP_METHOD: &str = "_cordata/lsp";
/// Project snapshots: said when a change could not be snapshotted, when a
/// call writes outside the project, and when a restore ran. Taking one is not an event.
pub const CHECKPOINT_METHOD: &str = "_cordata/checkpoint";
/// A loaded session has an interrupted turn: `auto` says the client may resume it on its own.
pub const RESUME_METHOD: &str = "_cordata/resume";
/// What a finished turn cost: `seconds`, `calls`, `tokensIn`,
/// `tokensOut`, `stop`. Sent once per turn, so a status line can show it without a model call of its own.
pub const TURN_METHOD: &str = "_cordata/turn";
/// The orchestrator's run events: `kind` is `state` | `step_dispatch` |
/// `step_result` | `handoff` | `rollback` | `paused` | `budget_exhausted`, with `run_id` and the fields of that kind.
pub const ORCHESTRATOR_METHOD: &str = "_cordata/orchestrator";
/// `/goal` progress: `event` is `set` | `checking` | `continue` | `met` | `stopped` | `exhausted`,
/// with `condition`, `turn`, `max`, `reason` and `how` (`check` | `judge`).
pub const GOAL_METHOD: &str = "_cordata/goal";
/// The session's mode changed under the client: `mode`, `from`, `kind`
/// (`adaptive` — the harness tightened it for a plan-worthy request; `plan_approved` — the person answered a plan
/// card) and `reason`. ACP's own `current_mode_update` says *that* it changed; this says why.
pub const MODE_METHOD: &str = "_cordata/mode";
/// How full the conversation is: `used`, `window`, `pct`, `messages`.
pub const CONTEXT_METHOD: &str = "_cordata/context";
/// The agent's own checklist: `items` of `{content, status}`, `done`, `total`.
pub const TASKLIST_METHOD: &str = "_cordata/tasklist";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Completed,
    Failed,
}

/// One unit of background work: plugin start-up, a language server indexing,
/// an MCP tool reporting progress, the self-heal loop diagnosing / verifying.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskInfo {
    pub id: String,
    pub title: String,
    pub done: f64,
    /// `None`: indeterminate (a spinner, not a bar).
    pub total: Option<f64>,
    pub message: String,
    pub state: TaskState,
    pub source: String,
}

impl TaskInfo {
    /// 0.0..=1.0 when the total is known.
    pub fn fraction(&self) -> Option<f64> {
        self.total
            .filter(|total| *total > 0.0)
            .map(|total| (self.done / total).clamp(0.0, 1.0))
    }
}

/// One phase of the self-healing loop.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HealInfo {
    /// detected | diagnosing | proposed | verifying | verified | verify_failed |
    /// no_patch | healed | escalated | continuing | state_mismatch | missing_file | bad_patch | auto_approved
    pub phase: String,
    pub attempt: Option<u64>,
    pub max: Option<u64>,
    pub command: Option<String>,
    pub root_cause: Option<String>,
    pub patch: Option<String>,
    pub verify: Option<String>,
    pub output_tail: Option<String>,
    pub exit_code: Option<i64>,
    pub tool: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HarnessEvent {
    Task(TaskInfo),
    Heal(HealInfo),
    /// A plugin changed state (the backend's plugin row, as JSON).
    Plugin(Value),
    /// A language-server event (started / failed / diagnostics), as JSON.
    Lsp(Value),
    /// A checkpoint notice: `phase` is `unprotected` | `outside` | `restored`.
    Checkpoint(Value),
    /// The loaded session has an interrupted turn (`auto`, `where`, `attempts`, `max`).
    Resume(Value),
    /// A turn ended: what it cost (`seconds`, `calls`, `tokensIn`, `tokensOut`, `stop`).
    Turn(Value),
    /// An orchestrator run event (state change, step, handoff), as JSON.
    Orchestrator(Value),
    /// A `/goal` step, as JSON.
    Goal(Value),
    /// The context gauge, as JSON.
    Context(Value),
    /// The agent's checklist, as JSON.
    TaskList(Value),
    /// Why the mode changed (`mode`, `from`, `kind`, `reason`), as JSON.
    Mode(Value),
}

/// Parse a harness notification; `None` for unknown methods or malformed params.
pub fn parse_harness(method: &str, params: &Value) -> Option<HarnessEvent> {
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |key: &str| params.get(key).and_then(Value::as_f64);
    match method {
        TASK_METHOD => Some(HarnessEvent::Task(TaskInfo {
            id: text("id")?,
            title: text("title").unwrap_or_default(),
            done: number("done").unwrap_or(0.0),
            total: number("total"),
            message: text("message").unwrap_or_default(),
            state: match params.get("state").and_then(Value::as_str) {
                Some("completed") => TaskState::Completed,
                Some("failed") => TaskState::Failed,
                _ => TaskState::Running,
            },
            source: text("source").unwrap_or_default(),
        })),
        HEAL_METHOD => Some(HarnessEvent::Heal(HealInfo {
            phase: text("phase")?,
            attempt: params.get("attempt").and_then(Value::as_u64),
            max: params.get("max").and_then(Value::as_u64),
            command: text("command"),
            root_cause: text("root_cause"),
            patch: text("patch"),
            verify: text("verify"),
            output_tail: text("output_tail"),
            exit_code: params.get("exit_code").and_then(Value::as_i64),
            tool: text("tool"),
        })),
        PLUGINS_METHOD => params.get("plugin").cloned().map(HarnessEvent::Plugin),
        LSP_METHOD => Some(HarnessEvent::Lsp(params.clone())),
        CHECKPOINT_METHOD => text("phase").map(|_| HarnessEvent::Checkpoint(params.clone())),
        RESUME_METHOD => Some(HarnessEvent::Resume(params.clone())),
        TURN_METHOD => Some(HarnessEvent::Turn(params.clone())),
        ORCHESTRATOR_METHOD => text("kind").map(|_| HarnessEvent::Orchestrator(params.clone())),
        GOAL_METHOD => text("event").map(|_| HarnessEvent::Goal(params.clone())),
        CONTEXT_METHOD => params.get("used").map(|_| HarnessEvent::Context(params.clone())),
        TASKLIST_METHOD => params.get("items").filter(|items| items.is_array()).map(|_| HarnessEvent::TaskList(params.clone())),
        MODE_METHOD => text("mode").map(|_| HarnessEvent::Mode(params.clone())),
        _ => None,
    }
}

#[derive(Debug)]
pub enum AppEvent {
    Connected(SessionInfo),
    /// History replayed by `session/load` (the text; its tool calls arrive as
    /// `ToolCallStarted`/`ToolCallUpdated` tagged `_meta.cordata.replay`). Followed by `ReplayComplete`.
    UserText(String),
    AgentText(String),
    AgentThought(String),
    ReplayComplete,
    ToolCallStarted(ToolCallInfo),
    ToolCallUpdated(ToolCallPatch),
    TitleChanged(String),
    ModeChanged(String),
    ConfigOptionsChanged(Vec<ConfigOptionInfo>),
    /// The server is blocked until `decision` is answered. Dropping the
    /// sender counts as a cancellation (the tool is denied).
    PermissionRequested {
        info: PermissionInfo,
        decision: oneshot::Sender<ApprovalDecision>,
    },
    /// Background tasks, self-heal phases, plugin and LSP changes.
    Harness(HarnessEvent),
    /// The answer to an extension request sent with `SessionHandle::ext`.
    ExtResult {
        tag: String,
        result: Result<Value, String>,
    },
    TurnComplete(StopReason),
    /// The turn's `session/prompt` request failed; the session stays usable.
    TurnFailed(String),
    /// Non-fatal problem (e.g. a rejected set_mode).
    Notice(String),
    /// The connection died; no more events follow except `Closed`.
    ConnectionError(String),
    Closed,
}

pub(crate) fn config_infos(options: &[SessionConfigOption]) -> Vec<ConfigOptionInfo> {
    options
        .iter()
        .filter_map(|option| {
            let SessionConfigKind::Select(select) = &option.kind else {
                return None;
            };
            let choices = match &select.options {
                SessionConfigSelectOptions::Ungrouped(list) => list
                    .iter()
                    .map(|choice| ConfigChoice {
                        value: choice.value.to_string(),
                        name: choice.name.clone(),
                    })
                    .collect(),
                SessionConfigSelectOptions::Grouped(groups) => groups
                    .iter()
                    .flat_map(|group| group.options.iter())
                    .map(|choice| ConfigChoice {
                        value: choice.value.to_string(),
                        name: choice.name.clone(),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            Some(ConfigOptionInfo {
                id: option.id.to_string(),
                name: option.name.clone(),
                current_value: select.current_value.to_string(),
                choices,
            })
        })
        .collect()
}

fn text_of(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }
}

fn diffs_of(content: &[ToolCallContent]) -> Vec<FileDiff> {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Diff(diff) => Some(FileDiff {
                path: diff.path.display().to_string(),
                old_text: diff.old_text.clone(),
                new_text: diff.new_text.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn join_tool_content(content: &[ToolCallContent]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Content(content) => text_of(&content.content),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Translate one `session/update` payload into zero or more events.
pub fn translate_update(update: SessionUpdate) -> Vec<AppEvent> {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => text_of(&chunk.content)
            .map(|text| vec![AppEvent::UserText(text.to_owned())])
            .unwrap_or_default(),
        SessionUpdate::AgentMessageChunk(chunk) => text_of(&chunk.content)
            .map(|text| vec![AppEvent::AgentText(text.to_owned())])
            .unwrap_or_default(),
        SessionUpdate::AgentThoughtChunk(chunk) => text_of(&chunk.content)
            .map(|text| vec![AppEvent::AgentThought(text.to_owned())])
            .unwrap_or_default(),
        SessionUpdate::ToolCall(call) => {
            let content = join_tool_content(&call.content);
            let name = call.name.clone().unwrap_or_else(|| call.title.clone());
            vec![AppEvent::ToolCallStarted(ToolCallInfo {
                id: call.tool_call_id.to_string(),
                untrusted_source: permission::tool_may_return_untrusted(&name),
                untrusted: permission::contains_untrusted_markers(&content),
                name,
                kind: call.kind.into(),
                status: call.status.into(),
                raw_input: call.raw_input,
                diffs: diffs_of(&call.content),
                content,
            })]
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let fields = update.fields;
            let diffs = fields
                .content
                .as_deref()
                .map(diffs_of)
                .filter(|diffs| !diffs.is_empty());
            // A diff-only update (the pre-run preview) must not blank the result text.
            let content = fields
                .content
                .as_deref()
                .filter(|content| {
                    content
                        .iter()
                        .any(|c| matches!(c, ToolCallContent::Content(_)))
                })
                .map(join_tool_content);
            let untrusted = content
                .as_deref()
                .is_some_and(permission::contains_untrusted_markers);
            vec![AppEvent::ToolCallUpdated(ToolCallPatch {
                id: update.tool_call_id.to_string(),
                status: fields.status.map(Into::into),
                content,
                untrusted,
                raw_input: fields.raw_input,
                diffs,
            })]
        }
        SessionUpdate::CurrentModeUpdate(update) => {
            vec![AppEvent::ModeChanged(update.current_mode_id.to_string())]
        }
        SessionUpdate::ConfigOptionUpdate(update) => vec![AppEvent::ConfigOptionsChanged(
            config_infos(&update.config_options),
        )],
        SessionUpdate::SessionInfoUpdate(info) => match info.title {
            agent_client_protocol::schema::MaybeUndefined::Value(title) => {
                vec![AppEvent::TitleChanged(title)]
            }
            _ => Vec::new(),
        },
        // ACP's own plan and usage, from any agent. Cordata's backend sends them beside `_cordata/tasklist` and
        // `_cordata/context` (docs/app.md §6.1), so they land as the same events: replacing a checklist or a gauge with
        // an identical one is a no-op, and an agent that only speaks plain ACP (Zed's agents, Claude's ACP adapter)
        // still fills the checklist and the context gauge.
        SessionUpdate::Plan(plan) => {
            let items: Vec<Value> = plan
                .entries
                .iter()
                .map(|entry| serde_json::json!({"content": entry.content, "status": serde_json::to_value(&entry.status).unwrap_or(Value::Null)}))
                .collect();
            vec![AppEvent::Harness(HarnessEvent::TaskList(serde_json::json!({"items": items, "native": true})))]
        }
        SessionUpdate::UsageUpdate(usage) => {
            let pct = (usage.size > 0).then(|| usage.used as f64 / usage.size as f64);
            // Cordata says whether the count is an estimate (no provider usage yet) in `_meta.cordata.estimated`.
            let estimated = usage.meta.as_ref().and_then(|meta| meta.get("cordata")).and_then(|c| c.get("estimated")).and_then(Value::as_bool).unwrap_or(false);
            vec![AppEvent::Harness(HarnessEvent::Context(serde_json::json!({"used": usage.used, "window": usage.size, "pct": pct, "estimated": estimated, "native": true})))]
        }
        // Available-commands updates and new protocol variants: ignored, harmlessly.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod harness_tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn native_plan_and_usage_become_the_checklist_and_the_gauge() {
        let plan: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "plan",
            "entries": [
                {"content": "read the code", "priority": "medium", "status": "completed"},
                {"content": "write the fix", "priority": "high", "status": "in_progress"}
            ]
        }))
        .unwrap();
        match translate_update(plan).as_slice() {
            [AppEvent::Harness(HarnessEvent::TaskList(value))] => {
                assert_eq!(value["items"][1]["content"], "write the fix");
                assert_eq!(value["items"][1]["status"], "in_progress");
                assert_eq!(value["items"][0]["status"], "completed");
            }
            other => panic!("a plan is a checklist: {other:?}"),
        }
        let usage: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "usage_update", "used": 50_000, "size": 200_000, "_meta": {"cordata": {"estimated": true}}
        }))
        .unwrap();
        match translate_update(usage).as_slice() {
            [AppEvent::Harness(HarnessEvent::Context(value))] => {
                assert_eq!(value["used"], 50_000);
                assert_eq!(value["window"], 200_000);
                assert_eq!(value["pct"], 0.25);
                assert_eq!(value["estimated"], true);
            }
            other => panic!("a usage update is the context gauge: {other:?}"),
        }
    }

    #[test]
    fn goal_context_and_checklist_events_parse_only_when_well_formed() {
        assert!(matches!(
            parse_harness(GOAL_METHOD, &serde_json::json!({"event": "continue", "turn": 1, "max": 8})),
            Some(HarnessEvent::Goal(_))
        ));
        assert!(parse_harness(GOAL_METHOD, &serde_json::json!({"turn": 1})).is_none());
        assert!(matches!(
            parse_harness(CONTEXT_METHOD, &serde_json::json!({"used": 1200, "window": 128000, "pct": 0.009})),
            Some(HarnessEvent::Context(_))
        ));
        assert!(parse_harness(CONTEXT_METHOD, &serde_json::json!({"window": 1})).is_none());
        assert!(matches!(
            parse_harness(TASKLIST_METHOD, &serde_json::json!({"items": [{"content": "a", "status": "pending"}]})),
            Some(HarnessEvent::TaskList(_))
        ));
        assert!(parse_harness(TASKLIST_METHOD, &serde_json::json!({"items": "nope"})).is_none());
    }

    #[test]
    fn an_orchestrator_event_parses_when_it_has_a_kind() {
        let event = parse_harness(ORCHESTRATOR_METHOD, &serde_json::json!({"kind": "handoff", "run_id": "r"}));
        assert!(matches!(event, Some(HarnessEvent::Orchestrator(_))));
        assert!(parse_harness(ORCHESTRATOR_METHOD, &serde_json::json!({"run_id": "r"})).is_none());
    }

    #[test]
    fn parses_harness_tasks_and_heal_phases() {
        let Some(HarnessEvent::Task(task)) = parse_harness(
            TASK_METHOD,
            &json!({"id": "task-3", "title": "lsp: indexing", "done": 30, "total": 120, "state": "running", "source": "lsp"}),
        ) else {
            panic!()
        };
        assert_eq!(task.fraction(), Some(0.25));
        assert_eq!(task.state, TaskState::Running);
        let Some(HarnessEvent::Heal(heal)) = parse_harness(
            HEAL_METHOD,
            &json!({"event": "heal", "phase": "verified", "attempt": 1, "max": 3, "command": "cargo test", "patch": "--- a/x\n+++ b/x\n"}),
        ) else {
            panic!()
        };
        assert_eq!(
            (heal.phase.as_str(), heal.attempt, heal.max),
            ("verified", Some(1), Some(3))
        );
        assert_eq!(parse_harness(TASK_METHOD, &json!({"title": "no id"})), None);
        assert!(matches!(
            parse_harness(MODE_METHOD, &json!({"mode": "plan", "from": "build", "kind": "adaptive", "reason": "why"})),
            Some(HarnessEvent::Mode(_))
        ));
        assert!(parse_harness(MODE_METHOD, &json!({"from": "build"})).is_none());
        assert_eq!(parse_harness("_cordata/unknown", &json!({})), None);
        let Some(HarnessEvent::Turn(turn)) = parse_harness(
            TURN_METHOD,
            &json!({"stop": "end_turn", "seconds": 41.3, "calls": 3, "tokensIn": 12400, "tokensOut": 1100}),
        ) else {
            panic!("a turn receipt parses");
        };
        assert_eq!(turn["tokensIn"], 12400);
    }

}
