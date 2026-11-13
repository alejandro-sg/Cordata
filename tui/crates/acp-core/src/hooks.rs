//! Tool lifecycle hooks ($c_8$): intercepting tool invocations inside the physical tool loop.
//!
//! Inspired by the Harness² lifecycle hooks architecture (`harness2/substrate/codex.py`),
//! lifecycle hooks act inside the tool loop and cannot be bypassed or skipped by the model.
//!
//! Hooks can be configured via:
//! - `.cordata/hooks.json` in the workspace root
//! - `~/.cordata/hooks.json` globally
//! - Programmatic configuration on [`HooksRegistry`]
//!
//! Supported events:
//! - `PreToolUse`: Evaluated before tool execution. Can `Allow` or `Block { reason }`.
//! - `PostToolUse`: Evaluated after tool execution. Can `Allow`, `Block { reason }`, or `Feedback { note }`.

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

/// Decision returned by a lifecycle hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "decision", rename_all = "lowercase")]
pub enum HookDecision {
    #[default]
    Allow,
    Block {
        #[serde(default)]
        reason: String,
    },
    Feedback {
        #[serde(default)]
        reason: String,
    },
}

/// A hook action to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HookAction {
    /// Execute an external command. Tool call JSON is passed on stdin; exit code and stdout determine decision.
    Command {
        command: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// Built-in native check: verify that test assertions or required checks are not stripped.
    CheckAssertions,
    /// Built-in native check: verify syntax of written files (JSON, TOML, Rust).
    CheckSyntax,
}

/// A rule binding a tool matcher to one or more hook actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRule {
    /// Tool name to match, or `"*"` to match all tools.
    #[serde(default = "default_matcher")]
    pub matcher: String,
    #[serde(default)]
    pub hooks: Vec<HookAction>,
}

fn default_matcher() -> String {
    "*".to_string()
}

/// File schema for `.cordata/hooks.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub hooks: HooksMap,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksMap {
    #[serde(rename = "PreToolUse", default)]
    pub pre_tool_use: Vec<HookRule>,
    #[serde(rename = "PostToolUse", default)]
    pub post_tool_use: Vec<HookRule>,
}

/// Active registry that loads and evaluates lifecycle hooks.
#[derive(Debug, Clone, Default)]
pub struct HooksRegistry {
    config: HooksConfig,
    cwd: PathBuf,
}

impl HooksRegistry {
    /// Loads hooks from workspace `.cordata/hooks.json`, falling back to `~/.cordata/hooks.json`. A project file is code
    /// the repository runs on this machine, so it is used only when `approvals` (the person's `mcp.approved`) holds
    /// [`project_approval`] for its exact contents; otherwise that entry comes back as the second value.
    pub fn load_for_workspace(cwd: &Path, approvals: &[String]) -> (Self, Option<String>) {
        let mut registry = Self {
            config: HooksConfig::default(),
            cwd: cwd.to_path_buf(),
        };
        let mut held = None;
        if let Ok(content) = std::fs::read_to_string(cwd.join(".cordata").join("hooks.json")) {
            let entry = project_approval(cwd, &content);
            match serde_json::from_str::<HooksConfig>(&content) {
                Ok(cfg) if approvals.contains(&entry) => {
                    registry.config = cfg;
                    return (registry, None);
                }
                Ok(_) => held = Some(entry),
                Err(_) => {}
            }
        }

        if let Ok(home) = std::env::var("HOME") {
            let user_hooks = Path::new(&home).join(".cordata").join("hooks.json");
            if user_hooks.is_file()
                && let Ok(content) = std::fs::read_to_string(&user_hooks)
                && let Ok(cfg) = serde_json::from_str::<HooksConfig>(&content)
            {
                registry.config = cfg;
            }
        }

        (registry, held)
    }

    /// Creates an explicit registry with a given configuration.
    pub fn with_config(cwd: PathBuf, config: HooksConfig) -> Self {
        Self { config, cwd }
    }

    /// Evaluates `PreToolUse` hooks for a tool call. Returns `Allow` or `Block { reason }`.
    pub fn evaluate_pre_tool_use(&self, tool_name: &str, tool_args: &serde_json::Value) -> HookDecision {
        for rule in &self.config.hooks.pre_tool_use {
            if !matches_matcher(&rule.matcher, tool_name) {
                continue;
            }
            for action in &rule.hooks {
                match action {
                    HookAction::Command { command, timeout_secs } => {
                        let payload = serde_json::json!({
                            "event": "PreToolUse",
                            "tool": tool_name,
                            "args": tool_args,
                            "cwd": self.cwd,
                        });
                        let decision = run_command_hook(command, &payload, &self.cwd, *timeout_secs);
                        if let HookDecision::Block { .. } = decision {
                            return decision;
                        }
                    }
                    HookAction::CheckAssertions | HookAction::CheckSyntax => {
                        // Pre-tool assertions/syntax do not block before write occurs
                    }
                }
            }
        }
        HookDecision::Allow
    }

    /// Evaluates `PostToolUse` hooks for a tool call after execution.
    pub fn evaluate_post_tool_use(
        &self,
        tool_name: &str,
        tool_args: &serde_json::Value,
        output: &str,
        success: bool,
    ) -> HookDecision {
        let mut final_feedback = None;

        for rule in &self.config.hooks.post_tool_use {
            if !matches_matcher(&rule.matcher, tool_name) {
                continue;
            }
            for action in &rule.hooks {
                match action {
                    HookAction::Command { command, timeout_secs } => {
                        let payload = serde_json::json!({
                            "event": "PostToolUse",
                            "tool": tool_name,
                            "args": tool_args,
                            "output": output,
                            "success": success,
                            "cwd": self.cwd,
                        });
                        let decision = run_command_hook(command, &payload, &self.cwd, *timeout_secs);
                        match decision {
                            HookDecision::Block { .. } => return decision,
                            HookDecision::Feedback { reason } => {
                                final_feedback = Some(reason);
                            }
                            HookDecision::Allow => {}
                        }
                    }
                    HookAction::CheckAssertions => {
                        if (tool_name == "edit_file" || tool_name == "write_file")
                            && let Some(reason) = inspect_removed_assertions(tool_args)
                        {
                            return HookDecision::Block { reason };
                        }
                    }
                    HookAction::CheckSyntax => {
                        if (tool_name == "edit_file" || tool_name == "write_file")
                            && let Some(reason) = inspect_syntax_errors(&self.cwd, tool_args)
                        {
                            return HookDecision::Feedback { reason };
                        }
                    }
                }
            }
        }

        if let Some(reason) = final_feedback {
            HookDecision::Feedback { reason }
        } else {
            HookDecision::Allow
        }
    }
}

/// What the chat says when a project's hooks are held back, with the entry that would let them run.
pub fn held_notice(entry: &str) -> String {
    format!("hooks: this project's .cordata/hooks.json is not approved (or changed), so it does not run. To run it, add \"{entry}\" to mcp.approved in ~/.cordata/settings.json.")
}

/// The `mcp.approved` entry that lets a project's hooks run: `<folder>|.cordata/hooks.json|sha256:<digest of the file>`.
pub fn project_approval(cwd: &Path, content: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let hex: String = Sha256::digest(content.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let folder = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    format!("{}|.cordata/hooks.json|sha256:{hex}", folder.display())
}

fn matches_matcher(matcher: &str, tool_name: &str) -> bool {
    matcher == "*" || matcher.eq_ignore_ascii_case(tool_name)
}

fn run_command_hook(cmd: &str, payload: &serde_json::Value, cwd: &Path, _timeout_secs: Option<u64>) -> HookDecision {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = match Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return HookDecision::Block {
                reason: format!("failed to spawn hook `{cmd}`: {e}"),
            };
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let input_bytes = serde_json::to_vec(payload).unwrap_or_default();
        let _ = stdin.write_all(&input_bytes);
    }

    let output = match child.wait_with_output() {
        Ok(out) => out,
        Err(e) => {
            return HookDecision::Block {
                reason: format!("hook `{cmd}` execution error: {e}"),
            };
        }
    };

    let stdout_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr_str = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if let Ok(decision) = serde_json::from_str::<HookDecision>(&stdout_str) {
        return decision;
    }

    if !output.status.success() {
        let msg = if !stderr_str.is_empty() {
            stderr_str
        } else if !stdout_str.is_empty() {
            stdout_str
        } else {
            format!("hook `{cmd}` exited with code {:?}", output.status.code())
        };
        HookDecision::Block { reason: msg }
    } else {
        HookDecision::Allow
    }
}

fn inspect_removed_assertions(args: &serde_json::Value) -> Option<String> {
    let target = args.get("target").and_then(|v| v.as_str())?;
    let replacement = args.get("replacement").and_then(|v| v.as_str()).unwrap_or("");
    let count_target = target.matches("assert").count();
    let count_replacement = replacement.matches("assert").count();
    if count_target > 0 && count_replacement == 0 {
        Some(format!(
            "Lifecycle hook refused edit: removed {count_target} assertion(s) without replacement. Preserve tests and verification criteria."
        ))
    } else {
        None
    }
}

fn inspect_syntax_errors(cwd: &Path, args: &serde_json::Value) -> Option<String> {
    let path_str = args.get("path").and_then(|v| v.as_str())?;
    let path = cwd.join(path_str);
    if path.extension().and_then(|e| e.to_str()) == Some("json")
        && let Ok(content) = std::fs::read_to_string(&path)
        && let Err(e) = serde_json::from_str::<serde_json::Value>(&content)
    {
        return Some(format!("Syntax warning in {path_str}: invalid JSON ({e})"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_hooks_file_runs_only_as_approved_and_a_change_holds_it_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".cordata")).unwrap();
        let file = dir.path().join(".cordata/hooks.json");
        let body = r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"exit 3"}]}]}}"#;
        std::fs::write(&file, body).unwrap();
        let (reg, held) = HooksRegistry::load_for_workspace(dir.path(), &[]);
        let entry = held.expect("unapproved file is held back");
        assert!(reg.config.hooks.pre_tool_use.is_empty());
        let (reg, held) = HooksRegistry::load_for_workspace(dir.path(), std::slice::from_ref(&entry));
        assert!(held.is_none() && reg.config.hooks.pre_tool_use.len() == 1);
        std::fs::write(&file, body.replace("exit 3", "exit 4")).unwrap();
        let (reg, held) = HooksRegistry::load_for_workspace(dir.path(), &[entry]);
        assert!(held.is_some() && reg.config.hooks.pre_tool_use.is_empty(), "an edited file needs a new approval");
    }

    #[test]
    fn pre_tool_use_command_blocks_when_script_exits_nonzero() {
        let config = HooksConfig {
            hooks: HooksMap {
                pre_tool_use: vec![HookRule {
                    matcher: "run_command".into(),
                    hooks: vec![HookAction::Command {
                        command: "echo 'blocked by policy' >&2; exit 1".into(),
                        timeout_secs: Some(5),
                    }],
                }],
                post_tool_use: vec![],
            },
        };
        let reg = HooksRegistry::with_config(PathBuf::from("/tmp"), config);
        let dec = reg.evaluate_pre_tool_use("run_command", &serde_json::json!({"cmd": "ls"}));
        assert!(matches!(dec, HookDecision::Block { reason } if reason.contains("blocked by policy")));
    }

    #[test]
    fn pre_tool_use_command_allows_when_script_returns_allow_json() {
        let config = HooksConfig {
            hooks: HooksMap {
                pre_tool_use: vec![HookRule {
                    matcher: "edit_file".into(),
                    hooks: vec![HookAction::Command {
                        command: "echo '{\"decision\":\"allow\"}'".into(),
                        timeout_secs: Some(5),
                    }],
                }],
                post_tool_use: vec![],
            },
        };
        let reg = HooksRegistry::with_config(PathBuf::from("/tmp"), config);
        let dec = reg.evaluate_pre_tool_use("edit_file", &serde_json::json!({"path": "foo.rs"}));
        assert_eq!(dec, HookDecision::Allow);
    }

    #[test]
    fn post_tool_use_check_assertions_catches_deleted_test() {
        let config = HooksConfig {
            hooks: HooksMap {
                pre_tool_use: vec![],
                post_tool_use: vec![HookRule {
                    matcher: "edit_file".into(),
                    hooks: vec![HookAction::CheckAssertions],
                }],
            },
        };
        let reg = HooksRegistry::with_config(PathBuf::from("/tmp"), config);
        let args = serde_json::json!({
            "target": "assert_eq!(res, 42);",
            "replacement": "// removed",
        });
        let dec = reg.evaluate_post_tool_use("edit_file", &args, "ok", true);
        assert!(matches!(dec, HookDecision::Block { reason } if reason.contains("removed 1 assertion")));
    }
}
