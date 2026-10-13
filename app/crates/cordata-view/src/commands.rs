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
    /// Ask the agent to draft CORDATA.md for this project.
    Init,
    Status,
    /// Keep working until a condition holds (`clear` stops; no argument shows it).
    Goal(Option<String>),
    /// A side question, answered without touching the conversation.
    Btw(Option<String>),
    /// Summarise the older turns to free context.
    Compact(Option<String>),
    /// How full the context window is.
    Context,
    /// Tokens this session, and the quota pools.
    Cost,
    /// The working tree's changes.
    Diff,
    /// The last reply, to the clipboard.
    Copy,
    /// The conversation, to a markdown file.
    Export(Option<String>),
    /// List / enable / disable / reload plugins (MCP servers, memory, LSP tools).
    Plugins(Option<String>),
    /// The project's language servers.
    Lsp,
    /// The self-healing loop's episodes and settings.
    Heal,
    /// The turns the harness can undo, newest first.
    Checkpoints,
    /// Undo a turn's file changes (preview, or `apply`).
    Undo(Option<String>),
    /// Take the *conversation* back: the agent forgets the last turn(s). Files are untouched.
    Back(Option<String>),
    /// The specialist sub-agents.
    Agents,
    /// The Experimental Lab: live pods, A2A links, threat gauge, controls (F2).
    Lab,
    /// The command deck: every mode, model, view and command, searchable (Ctrl+K).
    Palette,
    /// The session HUD: context, usage, tasks, agents and activity on one screen (F3).
    Hud,
    /// The Blade Runner 2049 interactive split-pane workspace (F3).
    Panes,
    /// The interactive diff review of the working tree (F4).
    Review,
    /// Another session in this project (Ctrl+N).
    New,
    /// The open sessions, and saved ones for this folder.
    Sessions,
    /// The daemon hub's conversations, every door's.
    Conversations,
    /// Continue a hub conversation in a new slot, by handle.
    Open(Option<String>),
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
    (
        "/laya [on|off|stats]",
        "Laya's routing line, or its calibration stats",
    ),
    (
        "/plugins [enable|disable <name>|reload]",
        "hot-swap MCP / memory / LSP plugins",
    ),
    ("/lsp", "language servers for this project"),
    ("/heal", "self-healing episodes and settings"),
    (
        "/undo [turn] [apply]",
        "preview, or with 'apply' undo, a turn's file changes",
    ),
    ("/checkpoints", "the turns that can be undone"),
    (
        "/back [n | list | to <number>]",
        "take the conversation back n turns — the agent forgets them (files are untouched: /undo)",
    ),
    ("/agents", "the architect and debugger specialists"),
    (
        "/lab",
        "Experimental Lab: live pods, E-Stop, Laya dial, guidance (F2)",
    ),
    ("/palette", "the command deck: every mode, model, view and command, searchable (Ctrl+K)"),
    ("/hud", "session HUD: context, usage, tasks, agents, activity (F3)"),
    ("/diffview", "review the working tree's changes: files, hunks, mouse (F4)"),
    ("/new", "another session in this project (Ctrl+N) — a new shared conversation on the hub"),
    ("/sessions", "open and saved sessions"),
    ("/conversations", "the hub's conversations — shared with Discord, the menu bar and the web app"),
    ("/open <handle>", "continue a hub conversation here (from /conversations, Discord or the menu bar)"),
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
    ("/init", "have the agent draft CORDATA.md"),
    ("/status", "session, mode, model, rules, quota pools"),
    ("/goal [<condition> [--check <command>] [--turns N] | clear]", "keep working until it holds — a command decides it, or a judge reads the turns"),
    ("/btw <question>", "a side question, answered without touching the conversation (even mid-turn)"),
    ("/compact [focus]", "summarise the older turns to free context"),
    ("/context", "how full the model's context window is"),
    ("/cost", "tokens this session, and the quota pools"),
    ("/diff", "what is changed in the working tree"),
    ("/copy", "copy the last reply to the clipboard"),
    ("/export [file]", "save this conversation as markdown"),
    ("/panes", "toggle Blade Runner split-pane workspace (F3)"),
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
        "goal" => Command::Goal(arg),
        "btw" => Command::Btw(arg),
        "compact" => Command::Compact(arg),
        "context" | "ctx" => Command::Context,
        "cost" | "usage" | "tokens" => Command::Cost,
        "diff" | "changes" => Command::Diff,
        "copy" => Command::Copy,
        "export" => Command::Export(arg),
        "plugins" | "plugin" | "mcp" => Command::Plugins(arg),
        "lsp" => Command::Lsp,
        "heal" | "self-heal" => Command::Heal,
        "undo" | "rewind" => Command::Undo(arg),
        "back" | "forget" => Command::Back(arg),
        "checkpoints" | "checkpoint" | "history" => Command::Checkpoints,
        "agents" | "specialists" => Command::Agents,
        "lab" | "pods" => Command::Lab,
        "palette" | "deck" => Command::Palette,
        "hud" | "dashboard" => Command::Hud,
        "panes" | "workspace" => Command::Panes,
        "diffview" | "dv" => Command::Review,
        "new" => Command::New,
        "sessions" => Command::Sessions,
        "conversations" | "convs" | "chats" => Command::Conversations,
        "open" | "join" => Command::Open(arg),
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
/// A text gauge: `pct` of `width` cells filled. `None` (an uncapped pool) is all dots.
pub fn gauge(pct: Option<f64>, width: usize) -> String {
    match pct {
        None => "·".repeat(width),
        Some(p) => {
            let filled = ((p.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
            format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
        }
    }
}

fn compact(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0).replace(".0M", "M")
    } else if n >= 10_000.0 {
        format!("{:.0}k", n / 1000.0)
    } else if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n:.2}").trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

/// The orchestrator's `/status` block: one gauge per quota pool, recent runs, recent handoffs.
pub fn format_orchestrator_status(value: &serde_json::Value) -> String {
    use serde_json::Value;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut out = vec!["quota pools".to_owned()];
    for pool in value.get("pools").and_then(Value::as_array).cloned().unwrap_or_default() {
        let limit = pool.get("limit").and_then(Value::as_f64);
        let used = pool.get("used").and_then(Value::as_f64).unwrap_or(0.0);
        let pct = pool.get("pct").and_then(Value::as_f64).unwrap_or(0.0);
        let provider = pool.get("provider_pct").and_then(Value::as_f64);
        let capped = limit.is_some() || provider.is_some();
        let unit = s(&pool, "unit");
        let amount = match limit {
            Some(l) => format!("{}/{} {unit}", compact(used), compact(l)),
            None => format!("{} {unit}", compact(used)),
        };
        let source = if s(&pool, "source") == "provider" { " · provider" } else { "" };
        out.push(format!(
            "  {:<18} {} {:>4}  {:<9} {amount}{source}",
            s(&pool, "pool_id"),
            gauge(capped.then_some(pct), 12),
            if capped { format!("{:.0}%", pct * 100.0) } else { "—".into() },
            s(&pool, "level"),
        ));
    }
    let runs = value.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
    out.push(if runs.is_empty() { "orchestrator runs  (none)".into() } else { "orchestrator runs".into() });
    for run in runs {
        let handoffs = run.get("handoffs").and_then(Value::as_u64).unwrap_or(0);
        let goal: String = s(&run, "goal").chars().take(70).collect();
        out.push(format!(
            "  {} {}: {goal}{}",
            s(&run, "run_id"),
            s(&run, "state"),
            if handoffs > 0 { format!(" · {handoffs} handoff(s)") } else { String::new() }
        ));
    }
    let handoffs = value.get("handoffs").and_then(Value::as_array).cloned().unwrap_or_default();
    out.push(if handoffs.is_empty() { "handoffs  (none)".into() } else { "handoffs".into() });
    for item in handoffs {
        out.push(format!(
            "  {} · step {} · {} → {}",
            s(&item, "reason"),
            s(&item, "step_id"),
            s(&item, "from_worker"),
            item.get("to_worker").and_then(Value::as_str).unwrap_or("—")
        ));
    }
    out.join("\n")
}

/// One line for a live orchestrator event worth a line in the transcript (state changes that end or pause a run,
/// handoffs, rollbacks); step-level chatter stays in the tool card's progress.
pub fn format_orchestrator_event(event: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    let s = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match s("kind").as_str() {
        "handoff" => Some(format!(
            "⇄ orchestrator handoff ({}): {} → {}",
            s("reason"),
            s("from_worker"),
            event.get("to_worker").and_then(Value::as_str).unwrap_or("none")
        )),
        "rollback" => Some(format!("↺ orchestrator rolled back step {}", s("step_id"))),
        "state" if matches!(s("state").as_str(), "done" | "failed" | "paused" | "budget_exhausted" | "cancelled") => {
            Some(format!("◆ orchestrator run {} {}", s("run_id"), s("state")))
        }
        _ => None,
    }
}

pub fn format_ext(tag: &str, value: &serde_json::Value) -> String {
    use serde_json::Value;
    let text = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    if let Some(text) = crate::extras::format_reply(tag, value) {
        return text;
    }
    match tag {
        "orchestrator" => format_orchestrator_status(value),
        "plugins" => {
            let rows = value
                .get("plugins")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "no plugins (add MCP servers to .mcp.json or ~/.cordata/mcp.json)".into();
            }
            let mut out = vec!["plugins:".to_owned()];
            for row in rows {
                let tools = row
                    .get("tools")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
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
            for warning in value
                .get("warnings")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                out.push(format!("  ⚠ {}", warning.as_str().unwrap_or_default()));
            }
            out.push(
                "  /plugins enable|disable <name> swaps one in or out of the live session".into(),
            );
            out.join("\n")
        }
        "plugins enable" | "plugins disable" => {
            let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let tools = value
                .get("tools")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if ok {
                format!(
                    "plugin {}: {} ({} tools in the live session)",
                    text(value, "name"),
                    text(value, "status"),
                    tools
                )
            } else {
                format!("plugin {}: {}", text(value, "name"), text(value, "error"))
            }
        }
        "lsp" => {
            let rows = value
                .get("servers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "no language detected that a known language server handles".into();
            }
            let mut out = vec![format!(
                "language servers ({}):",
                if value.get("enabled").and_then(Value::as_bool) == Some(false) {
                    "disabled by lsp.enabled"
                } else {
                    "enabled"
                }
            )];
            for row in rows {
                let command = text(&row, "command");
                let name = command.rsplit('/').next().unwrap_or(&command).to_owned();
                let mut line = format!(
                    "  {:<11} {:<14} {}",
                    text(&row, "server"),
                    text(&row, "state"),
                    name
                );
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
                if config.get("enabled").and_then(Value::as_bool) == Some(false) {
                    "off"
                } else {
                    "on"
                },
                config
                    .get("max_attempts")
                    .and_then(Value::as_u64)
                    .unwrap_or(3),
                text(&config, "verify"),
                config
                    .get("auto_apply")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            )];
            let episodes = value
                .get("episodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(memory) = value.get("memory").filter(|m| m.is_object()) {
                let n = |key: &str| memory.get(key).and_then(Value::as_u64).unwrap_or(0);
                out.push(format!(
                    "  memory: {} remembered fix(es) in this project ({} with a patch) · worked {} · failed {} · retired {}",
                    n("fixes"),
                    n("with_patch"),
                    n("uses_ok"),
                    n("uses_failed"),
                    n("retired")
                ));
            }
            if episodes.is_empty() {
                out.push("  no failures handled this session".into());
            }
            for ep in episodes {
                out.push(format!(
                    "  {:<10} `{}` · {} attempt(s){} — {}",
                    text(&ep, "status"),
                    text(&ep, "command"),
                    ep.get("attempts").and_then(Value::as_u64).unwrap_or(0),
                    if ep.get("verified").and_then(Value::as_bool) == Some(true) {
                        " · verified"
                    } else {
                        ""
                    },
                    text(&ep, "root_cause")
                ));
            }
            out.join("\n")
        }
        "checkpoints" => {
            let turns = value
                .get("turns")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if value.get("enabled").and_then(Value::as_bool) == Some(false) {
                return "checkpoints are off (checkpoints.enabled) — nothing is being snapshotted".into();
            }
            if turns.is_empty() {
                return "no turns with checkpoints yet — a snapshot is taken before any call that may change files".into();
            }
            let mut out = vec!["turns that can be undone (newest first):".to_owned()];
            for turn in turns {
                let files = turn.get("file_count").and_then(Value::as_u64);
                out.push(format!(
                    "  {}  {:>2} call(s){}{}  {}",
                    text(&turn, "turn").chars().take(8).collect::<String>(),
                    turn.get("calls").and_then(Value::as_u64).unwrap_or(0),
                    files.map_or(String::new(), |n| format!(" · {n} file(s)")),
                    if turn.get("undone").and_then(Value::as_bool) == Some(true) {
                        " · undone"
                    } else {
                        ""
                    },
                    text(&turn, "last_call")
                ));
            }
            out.push("  /undo shows what undoing the latest would do; /undo apply does it".into());
            out.join("\n")
        }
        "back list" => {
            let rows = value
                .get("checkpoints")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if value.get("enabled").and_then(Value::as_bool) == Some(false) {
                return "this session has no conversation checkpoints (it is not on snapshot storage)".into();
            }
            if rows.is_empty() {
                return "no checkpoints yet — one is taken as each turn starts".into();
            }
            let turns = value.get("turns").and_then(Value::as_u64).unwrap_or(0);
            let mut out = vec![format!(
                "conversation checkpoints (newest first) — {turns} turn(s) so far; /back goes back one, /back to <number> restores one:"
            )];
            for row in rows {
                let kind = text(&row, "kind");
                let label = if kind == "turn" {
                    format!("before turn {}: {}", row.get("turn").and_then(Value::as_u64).unwrap_or(0), text(&row, "prompt"))
                } else {
                    format!("↺ {}", text(&row, "prompt"))
                };
                out.push(format!(
                    "  {:>2}  {:>3} msg  {}",
                    row.get("index").and_then(Value::as_u64).unwrap_or(0),
                    row.get("messages").and_then(Value::as_u64).unwrap_or(0),
                    label
                ));
            }
            out.join("\n")
        }
        "back" => {
            if value.get("ok").and_then(Value::as_bool) == Some(false) {
                return format!("back: {}", text(value, "reason"));
            }
            let forgot: Vec<String> = value
                .get("forgot")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let now = value.get("now").and_then(Value::as_u64).unwrap_or(0);
            let mut out = Vec::new();
            if forgot.is_empty() {
                out.push(format!("↶ restored an earlier checkpoint — {now} turn(s) in the conversation"));
            } else {
                out.push(format!(
                    "↶ went back {} turn(s) — the agent no longer remembers:",
                    forgot.len()
                ));
                for prompt in &forgot {
                    out.push(format!("    · {prompt}"));
                }
            }
            out.push("  files are untouched (/undo for those); /back list shows the state you left, to restore it".into());
            out.join("\n")
        }
        "undo" | "undo applied" => {
            if value.get("ok").and_then(Value::as_bool) == Some(false) {
                return format!("undo: {}", text(value, "reason"));
            }
            let dry = value.get("dry_run").and_then(Value::as_bool) == Some(true);
            let count = value.get("count").and_then(Value::as_u64).unwrap_or(0);
            let turn = text(value, "turn");
            let turn: String = turn.chars().take(8).collect();
            if count == 0 {
                return format!("turn {turn}: nothing to change — the project already matches");
            }
            let mut out = vec![format!(
                "{} turn {turn}: {count} file(s)",
                if dry { "undoing would change" } else { "undid" }
            )];
            for file in value
                .get("files")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(12)
            {
                out.push(format!("  {:<9} {}", text(file, "action"), text(file, "path")));
            }
            if count > 12 {
                out.push(format!("  … and {} more", count - 12));
            }
            let edited = value
                .get("edited_since")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !edited.is_empty() {
                let names: Vec<&str> = edited.iter().filter_map(Value::as_str).take(5).collect();
                out.push(format!(
                    "  ⚠ edited since the turn ended (their changes would be overwritten): {}",
                    names.join(", ")
                ));
            }
            if dry {
                out.push("  /undo apply to do it".into());
            } else if let Some(id) = value.get("undo_with").and_then(Value::as_str) {
                out.push(format!(
                    "  to put it back: restore checkpoint {} (the ledger has it)",
                    id.chars().take(12).collect::<String>()
                ));
            }
            out.join("\n")
        }
        "laya stats" => {
            let mut out =
                vec!["laya calibration (prequential proper scores — lower is better):".to_owned()];
            out.extend(text(value, "summary").lines().map(|l| format!("  {l}")));
            if let Some(policy) = value.get("policy") {
                out.push(format!(
                    "  policy: gate {} · min_confidence {} · dispatch {} · on abstain {} → {}",
                    policy.get("gate").and_then(Value::as_f64).unwrap_or(0.0),
                    policy
                        .get("min_confidence")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                    text(policy, "dispatch"),
                    text(policy, "on_abstain"),
                    policy
                        .get("frontier_model")
                        .and_then(Value::as_str)
                        .unwrap_or("(no tiers.strong)")
                ));
            }
            out.join("\n")
        }
        "sessions" => {
            let rows = value
                .get("sessions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                return "no saved sessions for this folder".into();
            }
            let mut out = vec![
                "saved sessions here (cordata --resume <id>, or --continue for the newest):"
                    .to_owned(),
            ];
            for row in rows.iter().take(10) {
                let id = text(row, "sessionId");
                out.push(format!(
                    "  {}  {}  {}",
                    id.chars().take(8).collect::<String>(),
                    text(row, "updatedAt")
                        .replace('T', " ")
                        .trim_end_matches('Z'),
                    text(row, "title")
                ));
            }
            out.join("\n")
        }
        _ => format!("{tag}: {value}"),
    }
}

/// The one-line notice for a `_cordata/mode` event: why the session's mode changed under the person.
pub fn format_mode_event(event: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
    let (mode, from, reason) = (text("mode"), text("from"), text("reason"));
    if mode.is_empty() {
        return None;
    }
    match text("kind") {
        "adaptive" => Some(format!("⇣ {from} → {mode} · {reason} — Tab or /mode {from} to override")),
        "plan_approved" => Some(format!("✓ plan approved · {from} → {mode}")),
        _ => Some(format!("↹ {from} → {mode}{}", if reason.is_empty() { String::new() } else { format!(" · {reason}") })),
    }
}

/// The one-line notice for a `_cordata/checkpoint` event, or `None` for a phase that needs no line.
pub fn format_checkpoint_event(event: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
    match text("phase") {
        "unprotected" => Some(format!(
            "⚠ not checkpointed ({}): changes from {} cannot be undone",
            text("reason"),
            text("tool")
        )),
        "outside" => {
            let paths: Vec<&str> = event
                .get("outside")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            Some(format!(
                "⚠ {} writes outside the project ({}) — a checkpoint cannot undo that",
                text("tool"),
                paths.join(", ")
            ))
        }
        "restored" => Some(format!(
            "↶ restored {} file(s){}",
            event.get("files").and_then(Value::as_u64).unwrap_or(0),
            if event.get("ok").and_then(Value::as_bool) == Some(false) {
                " — some could not be restored"
            } else {
                ""
            }
        )),
        _ => None,
    }
}

/// "12400" → "12.4k", "1500000" → "1.5M": a token count a person can read at a glance.
fn human_count(n: u64) -> String {
    match n {
        1_000_000.. => format!("{:.1}M", n as f64 / 1_000_000.0),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => n.to_string(),
    }
}

/// The receipt line for a `_cordata/turn` event: `✓ 41s · 3 tool calls · 12.4k in · 1.1k out`. What a turn
/// cost, shown every time — a person watching their credits should not have to ask. `None` for a turn that did
/// nothing worth a line (no time, no tokens). Mirrors `daemon/discord_cards.turn_footer`.
pub fn format_turn_receipt(event: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    let number = |key: &str| event.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let seconds = number("seconds").round() as u64;
    let took = if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    };
    let stop = event
        .get("stop")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    if stop == "cancelled" {
        return Some(format!("■ stopped after {took}"));
    }
    let (calls, tokens_in, tokens_out) = (
        number("calls") as u64,
        number("tokensIn") as u64,
        number("tokensOut") as u64,
    );
    if seconds == 0 && calls == 0 && tokens_in == 0 && tokens_out == 0 {
        return None;
    }
    let mut parts = vec![format!("✓ {took}")];
    if calls > 0 {
        parts.push(format!(
            "{calls} tool call{}",
            if calls == 1 { "" } else { "s" }
        ));
    }
    if tokens_in > 0 || tokens_out > 0 {
        parts.push(format!(
            "{} in · {} out",
            human_count(tokens_in),
            human_count(tokens_out)
        ));
    }
    if stop != "end_turn" && !stop.is_empty() {
        parts.push(stop.replace('_', " "));
    }
    Some(parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orchestrator_status_shows_a_gauge_per_pool_and_the_handoffs() {
        let value = serde_json::json!({
            "pools": [
                {"pool_id": "claude_code", "unit": "usd", "used": 11.6, "limit": 20.0, "pct": 0.58, "level": "ok", "source": "meter"},
                {"pool_id": "antigravity", "unit": "tokens", "used": 7_600_000.0, "limit": 8_000_000.0, "pct": 0.95, "level": "hard", "source": "meter"},
                {"pool_id": "claude_code_p", "unit": "usd", "used": 1.0, "limit": 20.0, "pct": 0.91, "level": "soft", "provider_pct": 0.91, "source": "provider"},
                {"pool_id": "ollama_local", "unit": "requests", "used": 4.0, "limit": null, "pct": 0.0, "level": "ok", "source": "meter"}
            ],
            "runs": [{"run_id": "run-1", "state": "done", "goal": "add retries", "handoffs": 1}],
            "handoffs": [{"reason": "provider_rate_limit", "step_id": "s2", "from_worker": "claude_code:sonnet", "to_worker": "antigravity:gemini"}]
        });
        let text = format_ext("orchestrator", &value);
        assert!(text.contains("claude_code        ███████░░░░░  58%  ok        11.6/20 usd"), "{text}");
        assert!(text.contains("antigravity") && text.contains("7.6M/8M tokens") && text.contains("hard"), "{text}");
        assert!(text.contains("· provider"), "the provider's own reading is marked: {text}");
        assert!(text.contains("ollama_local       ············    —"), "an uncapped pool has no gauge: {text}");
        assert!(text.contains("run-1 done: add retries · 1 handoff(s)"), "{text}");
        assert!(text.contains("provider_rate_limit · step s2 · claude_code:sonnet → antigravity:gemini"), "{text}");
    }

    #[test]
    fn orchestrator_events_worth_a_line() {
        let handoff = serde_json::json!({"kind": "handoff", "reason": "quota_soft_limit_reached", "from_worker": "a", "to_worker": "b"});
        assert_eq!(format_orchestrator_event(&handoff).as_deref(), Some("⇄ orchestrator handoff (quota_soft_limit_reached): a → b"));
        let done = serde_json::json!({"kind": "state", "state": "done", "run_id": "run-9"});
        assert_eq!(format_orchestrator_event(&done).as_deref(), Some("◆ orchestrator run run-9 done"));
        let step = serde_json::json!({"kind": "step_dispatch", "step_id": "s1"});
        assert_eq!(format_orchestrator_event(&step), None, "step chatter stays in the tool card");
        let executing = serde_json::json!({"kind": "state", "state": "execute"});
        assert_eq!(format_orchestrator_event(&executing), None);
    }

    #[test]
    fn a_turn_receipt_says_what_it_cost() {
        let line = format_turn_receipt(
            &serde_json::json!({"stop": "end_turn", "seconds": 41.3, "calls": 3, "tokensIn": 12400, "tokensOut": 1100}),
        );
        assert_eq!(
            line.as_deref(),
            Some("✓ 41s · 3 tool calls · 12.4k in · 1.1k out")
        );
        let one = format_turn_receipt(
            &serde_json::json!({"seconds": 75, "calls": 1, "tokensIn": 950, "tokensOut": 40}),
        );
        assert_eq!(
            one.as_deref(),
            Some("✓ 1m 15s · 1 tool call · 950 in · 40 out")
        );
        let stopped = format_turn_receipt(&serde_json::json!({"stop": "cancelled", "seconds": 12}));
        assert_eq!(stopped.as_deref(), Some("■ stopped after 12s"));
        let long = format_turn_receipt(
            &serde_json::json!({"stop": "max_tokens", "seconds": 3, "tokensIn": 2_500_000, "tokensOut": 8000}),
        );
        assert_eq!(
            long.as_deref(),
            Some("✓ 3s · 2.5M in · 8.0k out · max tokens")
        );
        assert!(
            format_turn_receipt(&serde_json::json!({})).is_none(),
            "a turn that did nothing needs no line"
        );
    }

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
        assert_eq!(
            parse("/plugins disable echo"),
            Some(Command::Plugins(Some("disable echo".into())))
        );
        assert_eq!(parse("/mcp"), Some(Command::Plugins(None)));
        assert_eq!(parse("/switch 2"), Some(Command::Switch(Some("2".into()))));
        assert_eq!(parse("/resume-turn"), Some(Command::ResumeTurn));
        assert_eq!(parse("/conversations"), Some(Command::Conversations));
        assert_eq!(parse("/open c3f9a1e"), Some(Command::Open(Some("c3f9a1e".into()))));
        assert_eq!(
            parse("/laya stats"),
            Some(Command::Laya(Some("stats".into())))
        );
    }

    #[test]
    fn heal_status_reports_what_is_remembered() {
        let text = format_ext(
            "heal",
            &serde_json::json!({"config": {"enabled": true, "max_attempts": 3, "verify": "sandbox", "auto_apply": false},
                "episodes": [], "memory": {"fixes": 4, "with_patch": 3, "uses_ok": 5, "uses_failed": 1, "retired": 1}}),
        );
        assert!(text.contains("memory: 4 remembered fix(es) in this project (3 with a patch) · worked 5 · failed 1 · retired 1"), "{text}");
        let none = format_ext("heal", &serde_json::json!({"config": {}, "episodes": [], "memory": null}));
        assert!(!none.contains("memory:"), "{none}");
    }

    #[test]
    fn back_parses_and_formats_the_checkpoints_and_the_result() {
        assert_eq!(parse("/back"), Some(Command::Back(None)));
        assert_eq!(parse("/back 2"), Some(Command::Back(Some("2".into()))));
        assert_eq!(parse("/forget"), Some(Command::Back(None)));
        let listing = format_ext(
            "back list",
            &serde_json::json!({"enabled": true, "turns": 3, "checkpoints": [
                {"index": 1, "id": "a", "turn": 3, "prompt": "explain the biggest one", "kind": "turn", "messages": 4},
                {"index": 2, "id": "b", "turn": 2, "prompt": "as it was before going back to turn 2", "kind": "before-back", "messages": 6}
            ]}),
        );
        assert!(listing.contains("3 turn(s) so far") && listing.contains("before turn 3: explain the biggest one"), "{listing}");
        assert!(listing.contains("↺ as it was before going back"), "{listing}");
        let off = format_ext("back list", &serde_json::json!({"enabled": false, "turns": 0, "checkpoints": []}));
        assert!(off.contains("not on snapshot storage"), "{off}");
        let done = format_ext("back", &serde_json::json!({"ok": true, "now": 1, "forgot": ["now list the files"]}));
        assert!(done.contains("went back 1 turn(s)") && done.contains("· now list the files") && done.contains("/undo"), "{done}");
        let restored = format_ext("back", &serde_json::json!({"ok": true, "now": 3, "forgot": []}));
        assert!(restored.contains("restored an earlier checkpoint — 3 turn(s)"), "{restored}");
    }

    #[test]
    fn undo_and_checkpoint_commands_parse_and_format() {
        assert_eq!(parse("/undo"), Some(Command::Undo(None)));
        assert_eq!(
            parse("/undo apply"),
            Some(Command::Undo(Some("apply".into())))
        );
        assert_eq!(parse("/checkpoints"), Some(Command::Checkpoints));
        let preview = serde_json::json!({
            "ok": true, "dry_run": true, "turn": "34e119c76abc4050", "count": 2,
            "files": [{"path": "a.txt", "action": "revert"}, {"path": "b.txt", "action": "delete"}],
            "edited_since": ["a.txt"]
        });
        let text = format_ext("undo", &preview);
        assert!(text.contains("undoing would change turn 34e119c7: 2 file(s)"), "{text}");
        assert!(text.contains("revert    a.txt") && text.contains("delete    b.txt"), "{text}");
        assert!(text.contains("edited since the turn ended") && text.contains("/undo apply"), "{text}");
        let refused = format_ext("undo applied", &serde_json::json!({"ok": false, "reason": "a turn is running"}));
        assert_eq!(refused, "undo: a turn is running");
        let listing = format_ext(
            "checkpoints",
            &serde_json::json!({"enabled": true, "turns": [
                {"turn": "34e119c76abc4050", "calls": 3, "file_count": 3, "undone": false, "last_call": "rm old.txt"}
            ]}),
        );
        assert!(listing.contains("34e119c7") && listing.contains("3 call(s) · 3 file(s)"), "{listing}");
        let off = format_ext("checkpoints", &serde_json::json!({"enabled": false, "turns": []}));
        assert!(off.contains("checkpoints are off"), "{off}");
    }

    #[test]
    fn checkpoint_notices_say_what_cannot_be_undone() {
        let unprotected = serde_json::json!({"phase": "unprotected", "tool": "shell", "reason": "disk full"});
        assert_eq!(
            format_checkpoint_event(&unprotected).as_deref(),
            Some("⚠ not checkpointed (disk full): changes from shell cannot be undone")
        );
        let outside = serde_json::json!({"phase": "outside", "tool": "file_write", "outside": ["/etc/hosts"]});
        assert!(format_checkpoint_event(&outside).unwrap().contains("/etc/hosts"));
        assert!(format_checkpoint_event(&serde_json::json!({"phase": "taken"})).is_none());
    }

    #[test]
    fn extension_replies_format_as_tables() {
        let plugins = serde_json::json!({"plugins": [
            {"name": "echo", "kind": "tools", "status": "running", "source": "project", "tools": ["a", "b"], "approval": "ask"},
            {"name": "mem", "kind": "memory", "status": "blocked", "source": "project", "tools": [], "approval": "ask", "error": "folder not trusted"}
        ], "warnings": []});
        let text = format_ext("plugins", &plugins);
        assert!(
            text.contains("echo")
                && text.contains("2 tools")
                && text.contains("folder not trusted"),
            "{text}"
        );
        let lsp = serde_json::json!({"enabled": true, "servers": [{"server": "rust", "state": "running", "command": "/x/rust-analyzer-2026"}]});
        assert!(format_ext("lsp", &lsp).contains("rust-analyzer-2026"));
        let stats = serde_json::json!({"summary": "route: n=4", "policy": {"gate": 0.85, "min_confidence": 0.5, "dispatch": "direct", "on_abstain": "escalate", "frontier_model": null}});
        let stats = format_ext("laya stats", &stats);
        assert!(
            stats.contains("route: n=4") && stats.contains("(no tiers.strong)"),
            "{stats}"
        );
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
