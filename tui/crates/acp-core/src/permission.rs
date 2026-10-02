//! Permission-request data, plus the untrusted-content heuristics.
//!
//! The backend has no structured "untrusted" flag: `prompt_security.py` wraps
//! `http_request` / `agent_memory`-read / MCP tool results in literal marker
//! lines inside the result text. The only reliable client-side signal is
//! therefore pattern-matching those markers; the tool-name list is a hint
//! about *which calls are likely to produce* such content (useful on the
//! approval card, before any result exists).

use serde_json::Value;

use crate::events::ToolKind;

pub const UNTRUSTED_OPEN: &str = "<<<UNTRUSTED_TOOL_OUTPUT>>>";
pub const UNTRUSTED_CLOSE: &str = "<<<END_UNTRUSTED_TOOL_OUTPUT>>>";

/// Tools the backend always wraps (agent_memory only on reads — the client
/// can't tell, so this is a hint, not proof). MCP tools also get wrapped but
/// their names are server-defined; the marker check covers those.
const UNTRUSTED_SOURCE_TOOLS: &[&str] = &["http_request", "agent_memory"];

pub fn contains_untrusted_markers(text: &str) -> bool {
    text.contains(UNTRUSTED_OPEN)
}

pub fn tool_may_return_untrusted(tool_name: &str) -> bool {
    UNTRUSTED_SOURCE_TOOLS.contains(&tool_name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    Reject,
}

#[derive(Debug, Clone)]
pub struct PermissionInfo {
    pub tool_call_id: String,
    /// Bare tool name (the backend titles requests "Run <tool>").
    pub tool_name: String,
    pub title: String,
    pub kind: ToolKind,
    pub raw_input: Option<Value>,
    pub untrusted_source: bool,
}

pub(crate) fn tool_name_from_title(title: &str) -> &str {
    title.strip_prefix("Run ").unwrap_or(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_detection() {
        let wrapped = format!("{UNTRUSTED_OPEN}\nhello\n{UNTRUSTED_CLOSE}");
        assert!(contains_untrusted_markers(&wrapped));
        assert!(!contains_untrusted_markers("plain output"));
    }

    #[test]
    fn tool_name_hint() {
        assert!(tool_may_return_untrusted("http_request"));
        assert!(!tool_may_return_untrusted("shell"));
        assert_eq!(tool_name_from_title("Run shell"), "shell");
        assert_eq!(tool_name_from_title("shell"), "shell");
    }
}
