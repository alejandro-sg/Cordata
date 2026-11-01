//! The permission gate: turns a tool call into a `PermissionInfo` card and a decision into a verdict.

use serde_json::Value;

use super::tools;
use crate::events::ToolKind;
use crate::permission::{ApprovalDecision, ChoiceKind, PermissionChoice, PermissionInfo};

pub(crate) const ALLOW: &str = "allow";
pub(crate) const ALLOW_ALWAYS: &str = "allow_always";
pub(crate) const REJECT: &str = "reject";

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
        untrusted_source: false,
        always_label: Some(format!("Always allow {name} this session")),
        options: vec![
            PermissionChoice { id: ALLOW.into(), name: "Allow".into(), kind: ChoiceKind::AllowOnce },
            PermissionChoice { id: ALLOW_ALWAYS.into(), name: format!("Always allow {name}"), kind: ChoiceKind::AllowAlways },
            PermissionChoice { id: REJECT.into(), name: "Reject".into(), kind: ChoiceKind::RejectOnce },
        ],
    }
}

pub(crate) fn interpret(decision: ApprovalDecision) -> Verdict {
    match decision {
        ApprovalDecision::Allow => Verdict::Allow { always: false },
        ApprovalDecision::AllowAlways => Verdict::Allow { always: true },
        ApprovalDecision::Reject => Verdict::Deny("the user rejected this tool call".into()),
        ApprovalDecision::Choose(id) => match id.as_str() {
            ALLOW => Verdict::Allow { always: false },
            ALLOW_ALWAYS => Verdict::Allow { always: true },
            _ => Verdict::Deny("the user rejected this tool call".into()),
        },
        ApprovalDecision::Say(reason) => Verdict::Deny(format!("the user rejected this tool call: {reason}")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn card_offers_three_options_and_names_the_tool() {
        let info = request("c1", "run_command", &json!({"cmd":"ls"}));
        assert_eq!(info.tool_name, "run_command");
        assert_eq!(info.kind, ToolKind::Execute);
        assert_eq!(info.options.len(), 3);
        assert!(info.option(ALLOW_ALWAYS).is_some());
    }

    #[test]
    fn decisions_map_to_verdicts() {
        assert_eq!(interpret(ApprovalDecision::Allow), Verdict::Allow { always: false });
        assert_eq!(interpret(ApprovalDecision::Choose(ALLOW_ALWAYS.into())), Verdict::Allow { always: true });
        assert!(matches!(interpret(ApprovalDecision::Reject), Verdict::Deny(_)));
        assert!(matches!(interpret(ApprovalDecision::Say("no".into())), Verdict::Deny(m) if m.ends_with(": no")));
    }
}
