//! `AppEvent`: the one vocabulary both front ends consume, produced by the native engine.

use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl PlanItemStatus {
    pub fn label(self) -> &'static str {
        match self {
            PlanItemStatus::Pending => "pending",
            PlanItemStatus::InProgress => "in_progress",
            PlanItemStatus::Completed => "completed",
            PlanItemStatus::Cancelled => "cancelled",
        }
    }
}

/// One step in a structured plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub step: String,
    pub status: PlanItemStatus,
}

/// A structured plan update from an agent or orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanUpdate {
    Items {
        explanation: Option<String>,
        items: Vec<PlanItem>,
    },
    Markdown(String),
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

/// What one model request cost. `input` is the whole prompt (cached part included): it is how full the context is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    /// The part of `input` read from the provider's prompt cache.
    pub cached: u64,
}

/// One entry of a resumed session, for the front end to redraw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restored {
    User(String),
    Agent(String),
    /// A tool call: its name and a one-line summary of its input.
    Tool { name: String, summary: String },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct ContextBreakdown {
    pub system_tokens: usize,
    pub skills_tokens: usize,
    pub tools_tokens: usize,
    pub pruned_tools_count: usize,
    pub user_tokens: usize,
    pub assistant_tokens: usize,
    pub tool_results_tokens: usize,
    pub total_estimated_tokens: usize,
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
    /// One model request of the turn finished; a turn with tool calls has several.
    Usage(TokenUsage),
    /// The session now answers with another model (`SessionHandle::set_model`); the conversation carries on.
    ModelChanged(String),
    /// Context window compaction happened (`G4`).
    Compacted {
        before: usize,
        after: usize,
        summary: String,
    },
    /// Itemized breakdown of prompt context tokens (`G5`).
    Context(ContextBreakdown),
    /// Auto mode allowed tool call `id` without a card (`auto::judge`), for this reason.
    AutoAllowed {
        id: String,
        reason: String,
    },
    /// A structured plan update from the model or orchestrator (`AcpPlanUpdate`).
    PlanUpdated(PlanUpdate),
    /// Who answers approvals now (`chat.mode`, or `SessionHandle::set_mode`).
    ModeChanged(crate::policy::Mode),
    /// The pinned write-scope prefixes for `auto` mode (`SessionHandle::set_scope`); empty means whole project.
    ScopeChanged(Vec<String>),
    TurnComplete(StopReason),
    /// The turn failed; the session stays usable.
    TurnFailed(String),
    /// Non-fatal problem, or something the person should know (a rule that allowed a call without asking).
    Notice(String),
    /// A saved session was loaded (`/resume`): the conversation so far, and the id it keeps being saved under.
    SessionRestored { session_id: String, entries: Vec<Restored> },
    /// A resume was refused because that conversation is open in another window or thread; the session carries on as it was.
    ResumeRefused { session_id: String, why: String },
    /// The connection died; no more events follow except `Closed`.
    ConnectionError(String),
    Closed,
}
