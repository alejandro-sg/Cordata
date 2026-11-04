//! The permission gate: turns a tool call into a `PermissionInfo` card and a decision into a verdict.

use serde_json::Value;

use super::tools;
use crate::events::ToolKind;
use crate::permission::{ApprovalDecision, PermissionInfo};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow { always: bool },
    /// Refused; the text goes back to the model as the tool result.
    Deny(String),
    /// Completed directly from approval decision (e.g. human steering / ask_question answer).
    Complete(String),
}

/// What "always allow" remembers: the tool, and for a command whether it reached the network, so allowing offline
/// commands for the session never lets one go online without asking.
pub(crate) fn always_key(name: &str, args: &Value) -> String {
    if name == "run_command" && crate::policy::wants_network(args) { "run_command with network access".into() } else { name.to_owned() }
}

pub(crate) fn request(id: &str, name: &str, args: &Value) -> PermissionInfo {
    let kind: ToolKind = tools::kind_of(name);
    let what = always_key(name, args);
    // Network access is granted call by call: the card offers no "always".
    let online = name == "run_command" && crate::policy::wants_network(args);
    let interactive = name == "ask_question";
    PermissionInfo {
        tool_call_id: id.to_owned(),
        tool_name: name.to_owned(),
        title: if interactive { "Clarification Question".into() } else { format!("Run {what}") },
        kind,
        raw_input: Some(args.clone()),
        always_label: (!online && !interactive).then(|| format!("Always allow {what} this session")),
        reason: None,
    }
}

#[allow(dead_code)]
pub(crate) fn interpret(decision: ApprovalDecision) -> Verdict {
    interpret_call("", decision)
}

pub(crate) fn interpret_call(name: &str, decision: ApprovalDecision) -> Verdict {
    match decision {
        ApprovalDecision::Allow => {
            if name == "ask_question" {
                Verdict::Complete("User confirmed default option".into())
            } else {
                Verdict::Allow { always: false }
            }
        }
        ApprovalDecision::AllowAlways => {
            if name == "ask_question" {
                Verdict::Complete("User confirmed default option".into())
            } else {
                Verdict::Allow { always: true }
            }
        }
        ApprovalDecision::Reject => {
            if name == "ask_question" {
                Verdict::Deny("User declined to answer question".into())
            } else {
                Verdict::Deny("the user rejected this tool call".into())
            }
        }
        ApprovalDecision::Say(reason) => {
            if name == "ask_question" {
                Verdict::Complete(format!("User selected: {reason}"))
            } else {
                Verdict::Deny(format!("the user rejected this tool call: {reason}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_card_names_the_tool_and_offers_always_allow() {
        let info = request("c1", "run_command", &json!({"cmd":"ls"}));
        assert_eq!(info.tool_name, "run_command");
        assert_eq!(info.kind, ToolKind::Execute);
        assert!(info.always_label.as_deref().is_some_and(|l| l.contains("run_command")));
        let online = request("c2", "run_command", &json!({"cmd":"cargo fetch","network":true}));
        assert_eq!(online.title, "Run run_command with network access");
        assert!(online.always_label.is_none(), "network access is never allowed for the session");
        assert_ne!(always_key("run_command", &json!({"network":true})), always_key("run_command", &json!({})));
    }

    #[test]
    fn decisions_map_to_verdicts() {
        assert_eq!(interpret(ApprovalDecision::Allow), Verdict::Allow { always: false });
        assert_eq!(interpret(ApprovalDecision::AllowAlways), Verdict::Allow { always: true });
        assert!(matches!(interpret(ApprovalDecision::Reject), Verdict::Deny(_)));
        assert!(matches!(interpret(ApprovalDecision::Say("no".into())), Verdict::Deny(m) if m.ends_with(": no")));
    }

    #[test]
    fn ask_question_interactive_decision_completes() {
        let card = request("q1", "ask_question", &json!({"questions":[{"question":"Which db?","options":["A","B"]}]}));
        assert_eq!(card.tool_name, "ask_question");
        assert!(card.always_label.is_none(), "questions are never permanently allowed");
        assert_eq!(interpret_call("ask_question", ApprovalDecision::Say("Option A".into())), Verdict::Complete("User selected: Option A".into()));
        assert_eq!(interpret_call("ask_question", ApprovalDecision::Allow), Verdict::Complete("User confirmed default option".into()));
        assert_eq!(interpret_call("ask_question", ApprovalDecision::Reject), Verdict::Deny("User declined to answer question".into()));
    }
}
