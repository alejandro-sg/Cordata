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
}

pub(crate) fn request(id: &str, name: &str, args: &Value) -> PermissionInfo {
    let kind: ToolKind = tools::kind_of(name);
    PermissionInfo {
        tool_call_id: id.to_owned(),
        tool_name: name.to_owned(),
        title: format!("Run {name}"),
        kind,
        raw_input: Some(args.clone()),
        always_label: Some(format!("Always allow {name} this session")),
    }
}

pub(crate) fn interpret(decision: ApprovalDecision) -> Verdict {
    match decision {
        ApprovalDecision::Allow => Verdict::Allow { always: false },
        ApprovalDecision::AllowAlways => Verdict::Allow { always: true },
        ApprovalDecision::Reject => Verdict::Deny("the user rejected this tool call".into()),
        ApprovalDecision::Say(reason) => Verdict::Deny(format!("the user rejected this tool call: {reason}")),
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
    }

    #[test]
    fn decisions_map_to_verdicts() {
        assert_eq!(interpret(ApprovalDecision::Allow), Verdict::Allow { always: false });
        assert_eq!(interpret(ApprovalDecision::AllowAlways), Verdict::Allow { always: true });
        assert!(matches!(interpret(ApprovalDecision::Reject), Verdict::Deny(_)));
        assert!(matches!(interpret(ApprovalDecision::Say("no".into())), Verdict::Deny(m) if m.ends_with(": no")));
    }
}
