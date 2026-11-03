//! What the engine asks the person, and what the person answers.

use serde_json::Value;

use crate::events::ToolKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    /// Allow, and stop asking for this tool for the rest of the session.
    AllowAlways,
    Reject,
    /// Refuse, in the person's own words: the text goes back to the agent as the reason.
    Say(String),
}

#[derive(Debug, Clone)]
pub struct PermissionInfo {
    pub tool_call_id: String,
    pub tool_name: String,
    pub title: String,
    pub kind: ToolKind,
    pub raw_input: Option<Value>,
    /// "Always allow <tool> this session", when the person may offer it.
    pub always_label: Option<String>,
    /// Why auto mode left this call to the person (`auto::judge`); `None` in ask mode.
    pub reason: Option<String>,
}
