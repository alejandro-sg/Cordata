//! `AppEvent`: the one vocabulary both front ends consume, produced by the native engine.

use serde_json::Value;
use tokio::sync::oneshot;

use crate::permission::{ApprovalDecision, PermissionInfo};

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
            ToolKind::Other => "other",
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

/// One file an edit tool call changes. `old_text` is read from disk before the call runs (None: a new file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub kind: ToolKind,
    pub status: ToolStatus,
    pub raw_input: Option<Value>,
    pub content: String,
    pub diffs: Vec<FileDiff>,
}

/// A tool-call update. `content`, when present, *replaces* what was shown
/// (swarm progress is a cumulative log; results are final).
#[derive(Debug, Clone, Default)]
pub struct ToolCallPatch {
    pub id: String,
    pub status: Option<ToolStatus>,
    pub content: Option<String>,
    /// The call's complete input (the start event carries only what had streamed).
    pub raw_input: Option<Value>,
    /// What an edit will change, sent before it runs.
    pub diffs: Option<Vec<FileDiff>>,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    /// The model answering, e.g. `claude-sonnet-5-5`.
    pub model: String,
    /// What the banner shows after the model: `native · <model>`.
    pub transport: String,
}

#[derive(Debug)]
pub enum AppEvent {
    Connected(SessionInfo),
    AgentText(String),
    AgentThought(String),
    ToolCallStarted(ToolCallInfo),
    ToolCallUpdated(ToolCallPatch),
    /// The engine is blocked until `decision` is answered. Dropping the sender counts as a rejection.
    PermissionRequested {
        info: PermissionInfo,
        decision: oneshot::Sender<ApprovalDecision>,
    },
    TurnComplete(StopReason),
    /// The turn failed; the session stays usable.
    TurnFailed(String),
    /// Non-fatal problem.
    Notice(String),
    /// The connection died; no more events follow except `Closed`.
    ConnectionError(String),
    Closed,
}
