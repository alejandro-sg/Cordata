//! `AppEvent`: the one vocabulary both binaries consume. This is the single
//! place that encodes backend-specific conventions (tool-kind mapping,
//! untrusted-marker detection, which updates carry replacement vs. appended
//! text) so neither the TUI nor the stress runner re-derives them.

use agent_client_protocol::schema::v1::{
    ContentBlock, SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
    SessionModeState, SessionUpdate, StopReason as SchemaStopReason, ToolCallContent,
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
}

/// A tool-call update. `content`, when present, *replaces* what was shown
/// (swarm progress is a cumulative log; results are final).
#[derive(Debug, Clone)]
pub struct ToolCallPatch {
    pub id: String,
    pub status: Option<ToolStatus>,
    pub content: Option<String>,
    pub untrusted: bool,
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
    pub resumed: bool,
    pub current_mode: String,
    pub modes: Vec<ModeInfo>,
    pub config_options: Vec<ConfigOptionInfo>,
}

/// The extension notification method the backend sends Laya's live triage on.
/// (ACP requires extension methods to start with `_`; a client that doesn't
/// know it must ignore it, so other clients are unaffected.)
pub const LAYA_METHOD: &str = "_sierralab/laya";

/// Laya's decision for the turn that just started (backend `laya_triage.py`).
#[derive(Debug, Clone, PartialEq)]
pub struct LayaDecision {
    /// `route` | `shadow`
    pub mode: String,
    /// `None` when Laya gave no decision (see `error`).
    pub route: Option<String>,
    pub route_confidence: f64,
    /// The confidence a route needs before a hint is sent.
    pub gate: f64,
    pub needs_team: f64,
    pub complexity: f64,
    pub complexity_confidence: f64,
    pub latency_ms: Option<f64>,
    /// How long the turn waited for Laya before going on.
    pub waited_ms: Option<f64>,
    /// The hint actually added to the orchestrator's prompt, if any.
    pub hint: Option<String>,
    /// Model tier suggested for sub-agents this turn, if Laya was sure enough.
    pub tier: Option<String>,
    /// Arrived after the turn had already moved on (timed out earlier).
    pub late: bool,
    pub error: Option<String>,
}

/// What the LLM then actually did, for comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct LayaOutcome {
    pub actual_route: String,
    /// `None` when there was no Laya decision to compare against.
    pub agreed: Option<bool>,
    pub tools: Vec<String>,
    pub tiers: Vec<(String, Option<String>)>,
    pub stop_reason: Option<String>,
    pub turn_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayaEvent {
    Decision(LayaDecision),
    Outcome(LayaOutcome),
}

/// Parse the notification's params. Tolerant by design (a backend a version
/// ahead or behind must not break the client): unknown/missing fields default,
/// and anything unrecognisable is `None`.
pub fn parse_laya(params: &Value) -> Option<LayaEvent> {
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |key: &str| params.get(key).and_then(Value::as_f64);
    match params.get("event")?.as_str()? {
        "decision" => Some(LayaEvent::Decision(LayaDecision {
            mode: text("mode").unwrap_or_default(),
            route: text("route"),
            route_confidence: number("route_confidence").unwrap_or(0.0),
            gate: number("gate").unwrap_or(0.0),
            needs_team: number("needs_team").unwrap_or(0.0),
            complexity: number("complexity").unwrap_or(0.0),
            complexity_confidence: number("complexity_confidence").unwrap_or(0.0),
            latency_ms: number("latency_ms"),
            waited_ms: number("waited_ms"),
            hint: text("hint"),
            tier: text("tier"),
            late: params.get("late").and_then(Value::as_bool).unwrap_or(false),
            error: text("error"),
        })),
        "outcome" => Some(LayaEvent::Outcome(LayaOutcome {
            actual_route: text("actual_route")?,
            agreed: params.get("agreed").and_then(Value::as_bool),
            tools: params
                .get("tools")
                .and_then(Value::as_array)
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            tiers: params
                .get("tiers")
                .and_then(Value::as_object)
                .map(|tiers| {
                    tiers
                        .iter()
                        .map(|(node, tier)| (node.clone(), tier.as_str().map(str::to_owned)))
                        .collect()
                })
                .unwrap_or_default(),
            stop_reason: text("stop_reason"),
            turn_ms: number("turn_ms"),
        })),
        _ => None,
    }
}

#[derive(Debug)]
pub enum AppEvent {
    Connected(SessionInfo),
    /// History replayed by `session/load` (text only — the backend does not
    /// replay tool calls). Followed by `ReplayComplete`.
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
    /// Laya's live triage for the turn (a backend extension; see `LayaEvent`).
    Laya(LayaEvent),
    TurnComplete(StopReason),
    /// The turn's `session/prompt` request failed; the session stays usable.
    TurnFailed(String),
    /// Non-fatal problem (e.g. a rejected set_mode).
    Notice(String),
    /// The connection died; no more events follow except `Closed`.
    ConnectionError(String),
    Closed,
}

pub(crate) fn mode_infos(state: Option<&SessionModeState>) -> (String, Vec<ModeInfo>) {
    match state {
        Some(state) => (
            state.current_mode_id.to_string(),
            state
                .available_modes
                .iter()
                .map(|mode| ModeInfo {
                    id: mode.id.to_string(),
                    name: mode.name.clone(),
                    description: mode.description.clone(),
                })
                .collect(),
        ),
        None => (String::new(), Vec::new()),
    }
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
                content,
            })]
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let fields = update.fields;
            let content = fields.content.as_deref().map(join_tool_content);
            let untrusted = content
                .as_deref()
                .is_some_and(permission::contains_untrusted_markers);
            vec![AppEvent::ToolCallUpdated(ToolCallPatch {
                id: update.tool_call_id.to_string(),
                status: fields.status.map(Into::into),
                content,
                untrusted,
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
        // Plan / available-commands / usage updates are never emitted by
        // acp_server.py; ignoring them keeps new protocol variants harmless.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod laya_tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_a_decision() {
        let event = parse_laya(&json!({
            "event": "decision", "mode": "route", "route": "files_and_code", "route_confidence": 0.98,
            "gate": 0.85, "needs_team": 0.14, "complexity": 1.29, "complexity_confidence": 0.63,
            "latency_ms": 81.6, "waited_ms": 81.6, "hint": "use the file tools", "tier": null, "late": false,
            "sessionId": "s"
        }))
        .unwrap();
        let LayaEvent::Decision(decision) = event else {
            panic!()
        };
        assert_eq!(decision.route.as_deref(), Some("files_and_code"));
        assert_eq!((decision.route_confidence, decision.gate), (0.98, 0.85));
        assert_eq!(decision.hint.as_deref(), Some("use the file tools"));
        assert_eq!(decision.tier, None);
        assert!(!decision.late && decision.error.is_none());
    }

    #[test]
    fn parses_an_error_decision_and_an_outcome() {
        let LayaEvent::Decision(error) =
            parse_laya(&json!({"event": "decision", "error": "timed out after 2s"})).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (error.route, error.error.as_deref()),
            (None, Some("timed out after 2s"))
        );

        let LayaEvent::Outcome(outcome) = parse_laya(&json!({
            "event": "outcome", "actual_route": "team", "agreed": false, "tools": ["local_graph"],
            "tiers": {"a": "strong", "b": null}, "stop_reason": "end_turn", "turn_ms": 1200.5
        }))
        .unwrap() else {
            panic!()
        };
        assert_eq!(outcome.agreed, Some(false));
        assert_eq!(outcome.tools, ["local_graph"]);
        assert_eq!(
            outcome.tiers,
            [
                ("a".to_owned(), Some("strong".to_owned())),
                ("b".to_owned(), None)
            ]
        );
    }

    #[test]
    fn unknown_or_malformed_payloads_are_ignored_not_fatal() {
        assert_eq!(parse_laya(&json!({"event": "from_the_future"})), None);
        assert_eq!(parse_laya(&json!({"no_event": 1})), None);
        assert_eq!(parse_laya(&json!("nonsense")), None);
        assert_eq!(
            parse_laya(&json!({"event": "outcome"})),
            None,
            "an outcome needs its actual route"
        );
    }
}
