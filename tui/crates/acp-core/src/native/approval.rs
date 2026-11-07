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

/// What "always allow" remembers: the tool, scoped to the concrete binary for a clean offline command (`run_command (cargo)`),
/// and distinct keys for network, destructive, or compound commands so blanket `run_command` is never granted.
pub(crate) fn always_key(name: &str, args: &Value) -> String {
    if name == "run_command" {
        if crate::policy::wants_network(args) {
            "run_command with network access".into()
        } else if crate::auto::command_risk(args).is_some() {
            "run_command (destructive)".into()
        } else if let Some(prog) = crate::auto::command_program(args) {
            format!("run_command ({prog})")
        } else {
            "run_command (unscoped)".into()
        }
    } else {
        name.to_owned()
    }
}

pub(crate) fn request(id: &str, name: &str, args: &Value) -> PermissionInfo {
    let kind: ToolKind = tools::kind_of(name);
    let online = name == "run_command" && crate::policy::wants_network(args);
    let cmd_scoped = name != "run_command"
        || (!online && crate::auto::command_risk(args).is_none() && crate::auto::command_program(args).is_some());
    let what = always_key(name, args);
    let title_what = if online { "run_command with network access" } else { name };
    // Network, locally destructive, compound/piped, and interactive calls are granted call by call: no "always".
    let interactive = name == "ask_question";
    PermissionInfo {
        tool_call_id: id.to_owned(),
        tool_name: name.to_owned(),
        title: if interactive { "Clarification Question".into() } else { format!("Run {title_what}") },
        kind,
        raw_input: Some(args.clone()),
        always_label: (cmd_scoped && !interactive).then(|| format!("Always allow {what} this session")),
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

    #[test]
    fn offline_commands_scope_always_to_the_program_and_refuse_destructive_always() {
        let cargo = request("c1", "run_command", &json!({"cmd": "cargo test --lib"}));
        let python = request("c2", "run_command", &json!({"cmd": "python3 script.py"}));
        assert_eq!(cargo.always_label.as_deref(), Some("Always allow run_command (cargo) this session"));
        assert_ne!(always_key("run_command", &json!({"cmd": "cargo test"})), always_key("run_command", &json!({"cmd": "python3 script.py"})));
        assert!(python.always_label.as_deref().is_some_and(|l| l.contains("(python3)")));

        let destructive = request("c3", "run_command", &json!({"cmd": "git reset --hard HEAD"}));
        assert!(destructive.always_label.is_none(), "destructive commands never offer session-wide always");

        let compound = request("c4", "run_command", &json!({"cmd": "ls | head"}));
        assert!(compound.always_label.is_none(), "compound/piped commands never offer blanket always");
    }
}
