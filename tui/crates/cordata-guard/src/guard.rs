//! Deterministic PreToolUse hook. Reads `{"tool": "...", "input": {...}}`; a returned message means "block" (the binary
//! exits 2 and the message goes back to the model).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

static ENV_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|/)(\.env|\.env\..+)$").expect("env path"));
static GIT_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|/)\.git(/|$)").expect("git path"));
static RM_RF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\brm\s+(-[a-zA-Z]*r[a-zA-Z]*f*|-f[a-zA-Z]*r)\s+(/|~|\.\.|/\w+)(\s|$)").expect("rm")
});
static ENV_REDIRECT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r">+\s*(\./)?(\w+/)*\.env\b").expect("redirect"));

/// `Some(reason)` when the call must be blocked. Unparseable input is allowed: a broken hook must not wedge the agent.
pub fn check(raw: &str) -> Option<String> {
    let payload: Value = serde_json::from_str(raw).ok()?;
    let tool = payload.get("tool").and_then(Value::as_str).unwrap_or("");
    let input = payload.get("input").cloned().unwrap_or(Value::Null);
    let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("").to_owned();

    if matches!(tool, "editor" | "apply_patch" | "write_file" | "file_write") {
        let path = Some(text("path")).filter(|p| !p.is_empty()).unwrap_or_else(|| text("file"));
        if ENV_PATH.is_match(&path) {
            return Some("GUARD REFUSAL (exit 2): Direct edits to .env files are blocked.\nSecrets must be stored in the macOS Keychain using `make key` or `scripts/store-key.sh`.\n".into());
        }
        if GIT_PATH.is_match(&path) {
            return Some("GUARD REFUSAL (exit 2): Direct modifications inside .git/ are blocked.\nUse git CLI commands instead.\n".into());
        }
    }
    if matches!(tool, "shell" | "background_shell") {
        let command = text("command");
        if RM_RF.is_match(&command) {
            return Some("GUARD REFUSAL (exit 2): Dangerous recursive delete detected outside workspace boundaries.\n".into());
        }
        if ENV_REDIRECT.is_match(&command) {
            return Some("GUARD REFUSAL (exit 2): Shell redirection into .env is blocked. Store credentials in macOS Keychain.\n".into());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tool: &str, input: &str) -> Option<String> {
        check(&format!(r#"{{"tool":"{tool}","input":{input}}}"#))
    }

    #[test]
    fn blocks_env_git_rm_and_redirects_but_not_ordinary_calls() {
        assert!(call("write_file", r#"{"path":"app/.env"}"#).is_some());
        assert!(call("editor", r#"{"file":".git/config"}"#).is_some());
        assert!(call("shell", r#"{"command":"rm -rf /"}"#).is_some());
        assert!(call("shell", r#"{"command":"echo K=1 >> ./cfg/.env"}"#).is_some());
        assert!(call("write_file", r#"{"path":"src/main.rs"}"#).is_none());
        assert!(call("shell", r#"{"command":"rm -rf target"}"#).is_none());
        assert!(check("not json").is_none() && check("").is_none());
    }
}
