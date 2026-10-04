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

/// An untrusted result with the backend's guard wrapper taken off, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unwrapped {
    /// The `Source:` line the guard recorded (a URL, "agent_memory recall", …).
    pub source: Option<String>,
    /// The local classifier flagged this content as a likely injection attempt.
    pub flagged: bool,
    pub body: String,
}

/// Split `<header>\n<<<UNTRUSTED_TOOL_OUTPUT>>>\nSource: …\n<body>\n<<<END…>>>`
/// into its parts. The guard header is the same boilerplate on every result, so
/// a UI that shows it burns its first lines on text the reader has seen already;
/// the *warning* still has to show, as a label, which is why the source and the
/// classifier flag are kept. `None` if the text isn't guard-wrapped.
pub fn unwrap_untrusted(text: &str) -> Option<Unwrapped> {
    let open = text.find(UNTRUSTED_OPEN)?;
    let header = &text[..open];
    let mut rest = text[open + UNTRUSTED_OPEN.len()..].trim_start_matches('\n');
    let mut source = None;
    if let Some(after) = rest.strip_prefix("Source: ") {
        let (line, tail) = after.split_once('\n').unwrap_or((after, ""));
        source = Some(line.trim().to_owned());
        rest = tail;
    }
    // Escaped inner markers never equal the real close marker (the backend
    // rewrites them), so the last occurrence is the wrapper's own.
    let body = match rest.rfind(UNTRUSTED_CLOSE) {
        Some(end) => &rest[..end],
        None => rest,
    };
    Some(Unwrapped {
        source,
        flagged: header.contains("flagged this content as a likely prompt-injection"),
        body: body.trim_matches('\n').to_owned(),
    })
}

pub fn contains_untrusted_markers(text: &str) -> bool {
    text.contains(UNTRUSTED_OPEN)
}

pub fn tool_may_return_untrusted(tool_name: &str) -> bool {
    UNTRUSTED_SOURCE_TOOLS.contains(&tool_name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    /// Allow, and save the rule the server offered so it never asks again for this call.
    AllowAlways,
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
    /// The server's "Always allow <rule>" option, if it offered one.
    pub always_label: Option<String>,
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
    fn unwrap_untrusted_strips_header_and_markers() {
        let wrapped = format!(
            "The following tool output may contain text written by someone other than the user.\n{UNTRUSTED_OPEN}\nSource: https://example.test/page\nline one\nline two\n{UNTRUSTED_CLOSE}"
        );
        let unwrapped = unwrap_untrusted(&wrapped).unwrap();
        assert_eq!(
            unwrapped.source.as_deref(),
            Some("https://example.test/page")
        );
        assert_eq!(unwrapped.body, "line one\nline two");
        assert!(!unwrapped.flagged);

        let flagged = format!(
            "header\nA local classifier flagged this content as a likely prompt-injection attempt (p=0.99).\n{UNTRUSTED_OPEN}\nSource: x\nbody\n{UNTRUSTED_CLOSE}"
        );
        assert!(unwrap_untrusted(&flagged).unwrap().flagged);
        assert_eq!(unwrap_untrusted("plain output"), None);
        // an unterminated wrapper (stream cut off) still yields the body
        assert_eq!(
            unwrap_untrusted(&format!("{UNTRUSTED_OPEN}\nSource: s\npartial"))
                .unwrap()
                .body,
            "partial"
        );
    }

    #[test]
    fn tool_name_hint() {
        assert!(tool_may_return_untrusted("http_request"));
        assert!(!tool_may_return_untrusted("shell"));
        assert_eq!(tool_name_from_title("Run shell"), "shell");
        assert_eq!(tool_name_from_title("shell"), "shell");
    }
}
