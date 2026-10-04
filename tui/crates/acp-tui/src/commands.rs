//! Slash commands typed into the input box (`/help`, `/mode plan`, …). Pure
//! parsing; `App` decides what each one does.

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Mode(Option<String>),
    Model(Option<String>),
    Temp(Option<String>),
    /// Expand / collapse the agents' reasoning.
    Thinking,
    /// Expand / collapse tool results.
    Tools,
    /// Show / hide the live Laya triage line (`on` / `off`, no argument toggles).
    Laya(Option<String>),
    /// The settings panel, or `set` / `unset` / `path` subcommands.
    Config(Option<String>),
    /// List, add (`allow|ask|deny <rule>`) or remove permission rules.
    Permissions(Option<String>),
    /// Mark this folder trusted so its project allow rules apply.
    Trust,
    /// Show the instruction files in the system prompt.
    Memory,
    /// Ask the agent to draft SIERRALAB.md for this project.
    Init,
    Status,
    /// List / enable / disable / reload plugins (MCP servers, memory, LSP tools).
    Plugins(Option<String>),
    /// The project's language servers.
    Lsp,
    /// The self-healing loop's episodes and settings.
    Heal,
    /// The specialist sub-agents.
    Agents,
    /// Another session in this project (Ctrl+N).
    New,
    /// The open sessions, and saved ones for this folder.
    Sessions,
    /// Show another session: a number, or the next one.
    Switch(Option<String>),
    /// End the shown session.
    Close,
    /// Continue a turn a crash interrupted (from its last checkpoint).
    ResumeTurn,
    Clear,
    Quit,
    Unknown(String),
}

/// What the help overlay lists, so the docs and the parser can't drift apart.
pub const HELP: &[(&str, &str)] = &[
    ("/help", "show this help (also ? or F1)"),
    ("/mode [plan|build|auto]", "show or set the approval mode"),
    ("/model [name]", "pick the model (no name opens the picker)"),
    (
        "/temp [focused|balanced|creative]",
        "pick the temperature preset",
    ),
    ("/thinking", "expand / collapse reasoning"),
    ("/tools", "expand / collapse tool results"),
    ("/laya [on|off|stats]", "Laya's routing line, or its calibration stats"),
    (
        "/plugins [enable|disable <name>|reload]",
        "hot-swap MCP / memory / LSP plugins",
    ),
    ("/lsp", "language servers for this project"),
    ("/heal", "self-healing episodes and settings"),
    ("/agents", "the architect and debugger specialists"),
    ("/new", "another session in this project (Ctrl+N)"),
    ("/sessions", "open and saved sessions"),
    ("/switch [n]", "show session n (Ctrl+←/→, Alt+1…9)"),
    ("/close", "end the shown session"),
    ("/resume-turn", "continue a turn interrupted by a crash"),
    (
        "/config [set <key> <value>|unset <key>|path]",
        "settings panel (user / project / local files)",
    ),
    (
        "/permissions [allow|ask|deny <rule>|remove <rule>]",
        "list or edit permission rules",
    ),
    ("/trust", "trust this folder's project settings"),
    ("/memory", "show the instruction files the agent reads"),
    ("/init", "have the agent draft SIERRALAB.md"),
    ("/status", "session, mode, model, rules"),
    ("/clear", "clear the screen (the session continues)"),
    ("/quit", "quit"),
];

/// `None` when `input` isn't a slash command (a normal prompt).
pub fn parse(input: &str) -> Option<Command> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    // "/usr/bin/foo is broken" is a prompt, not a command: a command is a
    // single word with no further slash.
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, Some(arg.trim().to_owned()).filter(|a| !a.is_empty())),
        None => (rest, None),
    };
    if name.contains('/') {
        return None;
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "" | "help" | "h" | "?" => Command::Help,
        "mode" => Command::Mode(arg),
        "model" => Command::Model(arg),
        "temp" | "temperature" => Command::Temp(arg),
        "thinking" | "thoughts" | "reasoning" => Command::Thinking,
        "tools" => Command::Tools,
        "laya" => Command::Laya(arg),
        "config" | "settings" | "preferences" | "prefs" => Command::Config(arg),
        "permissions" | "perms" | "allowed-tools" => Command::Permissions(arg),
        "trust" => Command::Trust,
        "memory" => Command::Memory,
        "init" => Command::Init,
        "status" => Command::Status,
        "plugins" | "plugin" | "mcp" => Command::Plugins(arg),
        "lsp" => Command::Lsp,
        "heal" | "self-heal" => Command::Heal,
        "agents" | "specialists" => Command::Agents,
        "new" => Command::New,
        "sessions" => Command::Sessions,
        "switch" | "session" => Command::Switch(arg),
        "close" => Command::Close,
        "resume-turn" | "resume" => Command::ResumeTurn,
        "clear" | "cls" => Command::Clear,
        "quit" | "exit" | "q" => Command::Quit,
        other => Command::Unknown(other.to_owned()),
    })
}

/// An extension reply as the info text `/plugins`, `/lsp`, `/heal` and
/// `/laya stats` print. Tolerant: unknown shapes fall back to JSON.
pub fn format_ext(tag: &str, value: &serde_json::Value) -> String {
    use serde_json::Value;
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    match tag {
        "plugins" => {
            let rows = value.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default();
            if rows.is_empty() {
                return "no plugins (add MCP servers to .mcp.json or ~/.sierralab/mcp.json)".into();
            }
            let mut out = vec!["plugins:".to_owned()];
            for row in rows {
                let tools = row.get("tools").and_then(Value::as_array).map_or(0, Vec::len);
                let mut line = format!(
                    "  {:<12} {:<8} {:<9} {:<8} {} tools · approval {}",
                    text(&row, "name"),
                    text(&row, "kind"),
                    text(&row, "status"),
                    text(&row, "source"),
                    tools,
                    text(&row, "approval")
                );
                let error = text(&row, "error");
                if !error.is_empty() {
                    line.push_str(&format!(" — {error}"));
                }
                out.push(line);
            }
            for warning in value.get("warnings").and_then(Value::as_array).into_iter().flatten() {
                out.push(format!("  ⚠ {}", warning.as_str().unwrap_or_default()));
            }
            out.push("  /plugins enable|disable <name> swaps one in or out of the live session".into());
            out.join("\n")
        }
        "plugins enable" | "plugins disable" => {
            let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let tools = value.get("tools").and_then(Value::as_array).map_or(0, Vec::len);
            if ok {
                format!("plugin {}: {} ({} tools in the live session)", text(value, "name"), text(value, "status"), tools)
            } else {
                format!("plugin {}: {}", text(value, "name"), text(value, "error"))
            }
        }
        "lsp" => {
            let rows = value.get("servers").and_then(Value::as_array).cloned().unwrap_or_default();
            if rows.is_empty() {
                return "no language detected that a known language server handles".into();
            }
            let mut out = vec![format!(
                "language servers ({}):",
                if value.get("enabled").and_then(Value::as_bool) == Some(false) { "disabled by lsp.enabled" } else { "enabled" }
            )];
            for row in rows {
                let command = text(&row, "command");
                let name = command.rsplit('/').next().unwrap_or(&command).to_owned();
                let mut line = format!("  {:<11} {:<14} {}", text(&row, "server"), text(&row, "state"), name);
                let error = text(&row, "error");
                if !error.is_empty() {
                    line.push_str(&format!(" — {error}"));
                }
                out.push(line);
            }
            out.join("\n")
        }
        "heal" => {
            let config = value.get("config").cloned().unwrap_or(Value::Null);
            let mut out = vec![format!(
                "self-heal: {} · max {} attempts · verify in {} · autoApply {}",
                if config.get("enabled").and_then(Value::as_bool) == Some(false) { "off" } else { "on" },
                config.get("max_attempts").and_then(Value::as_u64).unwrap_or(3),
                text(&config, "verify"),
                config.get("auto_apply").and_then(Value::as_bool).unwrap_or(false)
            )];
            let episodes = value.get("episodes").and_then(Value::as_array).cloned().unwrap_or_default();
            if episodes.is_empty() {
                out.push("  no failures handled this session".into());
            }
            for ep in episodes {
                out.push(format!(
                    "  {:<10} `{}` · {} attempt(s){} — {}",
                    text(&ep, "status"),
                    text(&ep, "command"),
                    ep.get("attempts").and_then(Value::as_u64).unwrap_or(0),
                    if ep.get("verified").and_then(Value::as_bool) == Some(true) { " · verified" } else { "" },
                    text(&ep, "root_cause")
                ));
            }
            out.join("\n")
        }
        "laya stats" => {
            let mut out = vec!["laya calibration (prequential proper scores — lower is better):".to_owned()];
            out.extend(text(value, "summary").lines().map(|l| format!("  {l}")));
            if let Some(policy) = value.get("policy") {
                out.push(format!(
                    "  policy: gate {} · min_confidence {} · dispatch {} · on abstain {} → {}",
                    policy.get("gate").and_then(Value::as_f64).unwrap_or(0.0),
                    policy.get("min_confidence").and_then(Value::as_f64).unwrap_or(0.0),
                    text(policy, "dispatch"),
                    text(policy, "on_abstain"),
                    policy.get("frontier_model").and_then(Value::as_str).unwrap_or("(no tiers.strong)")
                ));
            }
            out.join("\n")
        }
        "sessions" => {
            let rows = value.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default();
            if rows.is_empty() {
                return "no saved sessions for this folder".into();
            }
            let mut out = vec!["saved sessions here (acp-tui --resume <id>, or --continue for the newest):".to_owned()];
            for row in rows.iter().take(10) {
                let id = text(row, "sessionId");
                out.push(format!(
                    "  {}  {}  {}",
                    id.chars().take(8).collect::<String>(),
                    text(row, "updatedAt").replace('T', " ").trim_end_matches('Z'),
                    text(row, "title")
                ));
            }
            out.join("\n")
        }
        _ => format!("{tag}: {value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_with_and_without_arguments() {
        assert_eq!(parse("/help"), Some(Command::Help));
        assert_eq!(parse("/"), Some(Command::Help));
        assert_eq!(
            parse("  /mode plan "),
            Some(Command::Mode(Some("plan".into())))
        );
        assert_eq!(parse("/mode"), Some(Command::Mode(None)));
        assert_eq!(
            parse("/MODEL Gpt-OSS:20b"),
            Some(Command::Model(Some("Gpt-OSS:20b".into())))
        );
        assert_eq!(parse("/thoughts"), Some(Command::Thinking));
        assert_eq!(parse("/exit"), Some(Command::Quit));
        assert_eq!(
            parse("/frobnicate x"),
            Some(Command::Unknown("frobnicate".into()))
        );
    }

    #[test]
    fn ordinary_prompts_and_paths_are_not_commands() {
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("what does /usr/bin/env do"), None);
        assert_eq!(
            parse("/usr/bin/env is missing"),
            None,
            "a path is a prompt, not a command"
        );
    }

    #[test]
    fn harness_commands_parse() {
        assert_eq!(parse("/plugins disable echo"), Some(Command::Plugins(Some("disable echo".into()))));
        assert_eq!(parse("/mcp"), Some(Command::Plugins(None)));
        assert_eq!(parse("/switch 2"), Some(Command::Switch(Some("2".into()))));
        assert_eq!(parse("/resume-turn"), Some(Command::ResumeTurn));
        assert_eq!(parse("/laya stats"), Some(Command::Laya(Some("stats".into()))));
    }

    #[test]
    fn extension_replies_format_as_tables() {
        let plugins = serde_json::json!({"plugins": [
            {"name": "echo", "kind": "tools", "status": "running", "source": "project", "tools": ["a", "b"], "approval": "ask"},
            {"name": "mem", "kind": "memory", "status": "blocked", "source": "project", "tools": [], "approval": "ask", "error": "folder not trusted"}
        ], "warnings": []});
        let text = format_ext("plugins", &plugins);
        assert!(text.contains("echo") && text.contains("2 tools") && text.contains("folder not trusted"), "{text}");
        let lsp = serde_json::json!({"enabled": true, "servers": [{"server": "rust", "state": "running", "command": "/x/rust-analyzer-2026"}]});
        assert!(format_ext("lsp", &lsp).contains("rust-analyzer-2026"));
        let stats = serde_json::json!({"summary": "route: n=4", "policy": {"gate": 0.85, "min_confidence": 0.5, "dispatch": "direct", "on_abstain": "escalate", "frontier_model": null}});
        let stats = format_ext("laya stats", &stats);
        assert!(stats.contains("route: n=4") && stats.contains("(no tiers.strong)"), "{stats}");
        assert!(format_ext("whatever", &serde_json::json!({"a": 1})).contains("\"a\":1"));
    }

    #[test]
    fn every_listed_command_parses() {
        for (usage, _) in HELP {
            let word = usage.split_whitespace().next().unwrap();
            assert!(
                !matches!(parse(word), Some(Command::Unknown(_)) | None),
                "{word} is listed but doesn't parse"
            );
        }
    }
}
