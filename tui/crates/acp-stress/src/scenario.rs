//! Scenario files: what to run, how to answer permission requests.
//!
//! ```json
//! { "scenarios": [
//!   { "name": "pong", "prompts": ["Reply with exactly the word: pong"], "mode": "auto", "repeat": 5 },
//!   { "name": "shell-gated", "prompts": ["Call the shell tool to run exactly: echo hi"],
//!     "mode": "build", "approval_policy": "auto_allow" },
//!   { "name": "scripted", "prompts": ["..."],
//!     "approval_policy": { "shell": "allow", "python_repl": "reject", "*": "reject" } }
//! ] }
//! ```
//! A bare top-level array of scenarios is accepted too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use acp_core::ApprovalDecision;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Reject,
}

impl From<Decision> for ApprovalDecision {
    fn from(decision: Decision) -> Self {
        match decision {
            Decision::Allow => ApprovalDecision::Allow,
            Decision::Reject => ApprovalDecision::Reject,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SimplePolicy {
    AutoAllow,
    AutoReject,
}

/// How a scenario answers `session/request_permission`. Only consulted in
/// `build` mode — `auto` never asks and `plan` denies without asking.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ApprovalPolicy {
    Simple(SimplePolicy),
    /// Per tool name; `"*"` is the fallback, and an unlisted tool with no
    /// fallback is rejected (never approve something the script didn't name).
    PerTool(HashMap<String, Decision>),
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        ApprovalPolicy::Simple(SimplePolicy::AutoReject)
    }
}

impl ApprovalPolicy {
    pub fn decide(&self, tool_name: &str) -> Decision {
        match self {
            ApprovalPolicy::Simple(SimplePolicy::AutoAllow) => Decision::Allow,
            ApprovalPolicy::Simple(SimplePolicy::AutoReject) => Decision::Reject,
            ApprovalPolicy::PerTool(map) => map
                .get(tool_name)
                .or_else(|| map.get("*"))
                .copied()
                .unwrap_or(Decision::Reject),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    /// Project directory for the session (default: the runner's cwd).
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    pub prompts: Vec<String>,
    /// `plan` | `build` | `edit` | `auto` (default: whatever the backend starts in: build).
    #[serde(default)]
    pub mode: Option<String>,
    /// Model id; must be one the backend advertises.
    #[serde(default)]
    pub model: Option<String>,
    /// Preset id: `focused` | `balanced` | `creative`.
    #[serde(default)]
    pub temperature: Option<String>,
    #[serde(default)]
    pub approval_policy: ApprovalPolicy,
    /// Run this many independent copies (each its own backend process).
    #[serde(default = "one")]
    pub repeat: usize,
}

fn one() -> usize {
    1
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScenarioFile {
    List(Vec<Scenario>),
    Wrapped { scenarios: Vec<Scenario> },
}

pub fn load(path: &Path) -> Result<Vec<Scenario>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn parse(text: &str) -> Result<Vec<Scenario>> {
    let scenarios = match serde_json::from_str::<ScenarioFile>(text)? {
        ScenarioFile::List(list) | ScenarioFile::Wrapped { scenarios: list } => list,
    };
    if scenarios.is_empty() {
        bail!("no scenarios");
    }

    let mut expanded = Vec::new();
    for scenario in scenarios {
        if scenario.prompts.is_empty() {
            bail!("scenario {:?} has no prompts", scenario.name);
        }
        if scenario.repeat == 0 {
            bail!("scenario {:?}: repeat must be >= 1", scenario.name);
        }
        if let Some(mode) = &scenario.mode
            && !["plan", "build", "edit", "auto"].contains(&mode.as_str())
        {
            bail!(
                "scenario {:?}: unknown mode {mode:?} (plan|build|edit|auto)",
                scenario.name
            );
        }
        for copy in 0..scenario.repeat {
            let mut one = scenario.clone();
            if scenario.repeat > 1 {
                one.name = format!("{}#{}", scenario.name, copy + 1);
            }
            one.repeat = 1;
            expanded.push(one);
        }
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wrapped_and_bare_forms_and_expands_repeat() {
        let wrapped = parse(r#"{"scenarios":[{"name":"a","prompts":["x"],"repeat":3}]}"#).unwrap();
        assert_eq!(
            wrapped.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["a#1", "a#2", "a#3"]
        );
        let bare = parse(r#"[{"name":"b","prompts":["x"]}]"#).unwrap();
        assert_eq!(bare[0].name, "b");
    }

    #[test]
    fn rejects_bad_scenarios() {
        assert!(parse(r#"[{"name":"a","prompts":[]}]"#).is_err());
        assert!(parse(r#"[{"name":"a","prompts":["x"],"mode":"yolo"}]"#).is_err());
        assert!(parse(r#"[{"name":"a","prompts":["x"],"bogus":1}]"#).is_err());
        assert!(parse("[]").is_err());
    }

    #[test]
    fn policies_decide_per_tool_and_default_to_reject() {
        let all: ApprovalPolicy = serde_json::from_str(r#""auto_allow""#).unwrap();
        assert_eq!(all.decide("shell"), Decision::Allow);
        let none = ApprovalPolicy::default();
        assert_eq!(none.decide("shell"), Decision::Reject);
        let scripted: ApprovalPolicy =
            serde_json::from_str(r#"{"shell":"allow","*":"reject"}"#).unwrap();
        assert_eq!(scripted.decide("shell"), Decision::Allow);
        assert_eq!(scripted.decide("python_repl"), Decision::Reject);
        let no_fallback: ApprovalPolicy = serde_json::from_str(r#"{"shell":"allow"}"#).unwrap();
        assert_eq!(no_fallback.decide("editor"), Decision::Reject);
    }
}
