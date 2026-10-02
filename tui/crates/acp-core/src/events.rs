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
