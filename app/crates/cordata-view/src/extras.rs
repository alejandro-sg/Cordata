//! Session conveniences: `/copy`, `/export`, `/diff`, the project's git branch and the suggested next prompt.

use serde_json::Value;

use crate::app::{App, Effect, Message};

impl App {
    pub fn run_copy(&mut self) -> Option<Effect> {
        let last = self.messages.iter().rev().find_map(|m| match m {
            Message::Agent(text) if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        });
        match last {
            Some(text) => {
                self.info(format!("copied the last reply ({} characters)", text.chars().count()));
                Some(Effect::Copy(text))
            }
            None => {
                self.info("nothing to copy yet");
                None
            }
        }
    }

    pub fn run_export(&mut self, arg: Option<String>) {
        let id: String = self.session.as_ref().map(|s| s.session_id.chars().take(8).collect()).unwrap_or_else(|| "session".into());
        let name = arg.filter(|a| !a.trim().is_empty()).unwrap_or_else(|| format!("cordata-{id}.md"));
        let path = if std::path::Path::new(&name).is_absolute() { std::path::PathBuf::from(&name) } else { self.project_dir().join(&name) };
        match std::fs::write(&path, self.transcript_markdown()) {
            Ok(()) => self.info(format!("saved the conversation to {}", path.display())),
            Err(error) => self.messages.push(Message::Error(format!("could not write {}: {error}", path.display()))),
        }
    }

    /// The conversation as markdown: what was asked, what was answered, which tools ran. Reasoning and the harness's
    /// own notices are left out — this is what you would hand a colleague.
    pub fn transcript_markdown(&self) -> String {
        let mut out = String::from("# Cordata session\n");
        if let Some(session) = &self.session {
            out.push_str(&format!("\n`{}` · {}\n", session.session_id, session.cwd));
        }
        for message in &self.messages {
            match message {
                Message::User(text) => out.push_str(&format!("\n## ❯ {}\n", text.trim().replace('\n', "\n> "))),
                Message::Agent(text) => out.push_str(&format!("\n{}\n", text.trim())),
                Message::Tool(card) => {
                    let detail = card
                        .raw_input
                        .as_ref()
                        .and_then(|input| input.get("cmd").or_else(|| input.get("path")).or_else(|| input.get("pattern")))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    out.push_str(&format!("\n- `{}` {}\n", card.name, if detail.is_empty() { String::new() } else { format!("`{}`", detail.lines().next().unwrap_or_default()) }));
                }
                _ => {}
            }
        }
        out
    }

    /// `/diff`: what is changed in the working tree, as the person's own command (not shared with the agent).
    pub fn run_diff(&mut self) -> Option<Effect> {
        Some(Effect::Shell {
            command: "git --no-pager diff --stat HEAD 2>&1 | tail -n 40; echo; git --no-pager diff HEAD --unified=2 2>&1 | head -n 400".into(),
            share: false,
            as_diff: true,
        })
    }

    /// `/context`: itemized breakdown of prompt tokens (`G5`).
    pub fn run_context(&mut self) {
        if let Some(ctx) = &self.telemetry.last_context {
            let mut lines = Vec::new();
            lines.push(format!("Context composition (~{} tokens total):", ctx.total_estimated_tokens));
            lines.push(format!("  - System instructions: ~{} tokens", ctx.system_tokens));
            lines.push(format!("  - Tool schemas:        ~{} tokens ({} pruned)", ctx.tools_tokens, ctx.pruned_tools_count));
            lines.push(format!("  - User prompts:        ~{} tokens", ctx.user_tokens));
            lines.push(format!("  - Assistant replies:   ~{} tokens", ctx.assistant_tokens));
            lines.push(format!("  - Tool results:        ~{} tokens", ctx.tool_results_tokens));
            self.messages.push(Message::Info(lines.join("\n")));
        } else {
            let used = self.telemetry.context_used;
            self.messages.push(Message::Info(format!("Context used: ~{used} tokens. Run a prompt to see full itemized breakdown.")));
        }
    }

    /// `/run [goal]`: start an orchestrated multi-agent run from this thread (`G6`).
    pub fn run_multiagent_run(&mut self, goal: Option<String>) -> Option<Effect> {
        let goal_text = goal.unwrap_or_else(|| {
            self.messages.iter().rev().find_map(|m| if let Message::User(t) = m { Some(t.clone()) } else { None }).unwrap_or_else(|| "build plan".into())
        });
        self.messages.push(Message::Info(format!("Starting orchestrated multi-agent run for goal: \"{goal_text}\"...")));
        let home = std::env::var("HOME").unwrap_or_default();
        let local_bin = format!("{home}/.local/bin/cordata");
        let bin = if std::path::Path::new(&local_bin).exists() { local_bin } else { "cordata".to_string() };
        let cwd = self.project_dir();
        Some(Effect::Shell {
            command: format!("\"{bin}\" run \"{}\" --cwd \"{}\"", goal_text.replace('"', "\\\""), cwd.display()),
            share: false,
            as_diff: false,
        })
    }

    /// `/floorplan`: visual architectural map & active-work visualizer.
    pub fn run_floorplan(&mut self, arg: Option<String>) -> Option<Effect> {
        let flag = match arg.as_deref() {
            Some("term" | "cli" | "text") => "",
            _ => " --html",
        };
        let cwd = self.project_dir();
        let home = std::env::var("HOME").unwrap_or_default();
        let local_bin = format!("{home}/.local/bin/cordata");
        let bin = if std::path::Path::new(&local_bin).exists() {
            local_bin
        } else {
            "cordata".to_string()
        };
        Some(Effect::Shell {
            command: format!("\"{bin}\" floorplan --cwd \"{}\"{flag}", cwd.display()),
            share: false,
            as_diff: false,
        })
    }

}

/// The branch the project is on (`None` outside a repository or on a detached HEAD).
pub fn git_branch(dir: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// A prompt worth offering after a turn, from what the turn did — no model call, so it costs nothing and is there
/// the moment the turn ends. `None` when nothing obvious follows.
pub fn suggest_next(messages: &[Message]) -> Option<String> {
    use acp_core::{ToolKind, ToolStatus};
    let start = messages.iter().rposition(|m| matches!(m, Message::User(_)))?;
    let cards: Vec<&crate::app::ToolCard> = messages[start..]
        .iter()
        .filter_map(|m| match m {
            Message::Tool(card) => Some(card),
            _ => None,
        })
        .collect();
    let is_edit = |c: &&crate::app::ToolCard| matches!(c.kind, ToolKind::Edit);
    let last_edit = cards.iter().rposition(is_edit)?;
    let command = |c: &crate::app::ToolCard| c.raw_input.as_ref().and_then(|i| i.get("cmd")).and_then(Value::as_str).unwrap_or_default().to_lowercase();
    let looks_like_a_check = |c: &&crate::app::ToolCard| {
        let command = command(c);
        ["test", "pytest", "cargo check", "cargo build", "clippy", "lint", "tsc", "mypy", "ruff", "make check"].iter().any(|k| command.contains(k))
    };
    let after = &cards[last_edit + 1..];
    match after.iter().rev().find(|c| looks_like_a_check(c)) {
        Some(check) if matches!(check.status, ToolStatus::Failed) => Some("fix the failures above".to_owned()),
        Some(_) => Some("commit these changes with a clear message".to_owned()),
        None if after.iter().any(|c| matches!(c.status, ToolStatus::Failed)) => Some("fix the error above".to_owned()),
        None => Some("run the tests and show me the result".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn card(name: &str, kind: acp_core::ToolKind, status: acp_core::ToolStatus, command: Option<&str>) -> Message {
        Message::Tool(crate::app::ToolCard {
            id: name.into(),
            name: name.into(),
            kind,
            status,
            raw_input: command.map(|c| json!({"cmd": c})),
            content: String::new(),
            diffs: vec![],
        })
    }

    #[test]
    fn what_follows_a_turn_depends_on_what_the_turn_did() {
        use acp_core::{ToolKind::*, ToolStatus::*};
        let user = || Message::User("fix it".into());
        assert_eq!(suggest_next(&[user(), Message::Agent("hi".into())]), None, "no edits: nothing obvious follows");
        assert_eq!(
            suggest_next(&[user(), card("file_write", Edit, Completed, None)]).as_deref(),
            Some("run the tests and show me the result"),
        );
        assert_eq!(
            suggest_next(&[user(), card("file_write", Edit, Completed, None), card("shell", Execute, Failed, Some("cargo test -q"))]).as_deref(),
            Some("fix the failures above"),
        );
        assert_eq!(
            suggest_next(&[user(), card("file_write", Edit, Completed, None), card("shell", Execute, Completed, Some("pytest"))]).as_deref(),
            Some("commit these changes with a clear message"),
        );
        assert_eq!(
            suggest_next(&[user(), card("file_write", Edit, Completed, None), card("shell", Execute, Failed, Some("ls nope"))]).as_deref(),
            Some("fix the error above"),
        );
        // A check that ran *before* the last edit says nothing about it.
        assert_eq!(
            suggest_next(&[user(), card("shell", Execute, Completed, Some("pytest")), card("file_write", Edit, Completed, None)]).as_deref(),
            Some("run the tests and show me the result"),
        );
        // Only this turn counts.
        assert_eq!(suggest_next(&[user(), card("file_write", Edit, Completed, None), Message::User("thanks".into()), Message::Agent("np".into())]), None);
    }

    #[test]
    fn the_transcript_exports_as_markdown_without_the_noise() {
        let mut app = App::new();
        app.messages.push(Message::User("fix it".into()));
        app.messages.push(Message::Thought("hmm".into()));
        app.messages.push(Message::Info("a notice".into()));
        app.messages.push(Message::Agent("Done — see `a.rs`.".into()));
        let markdown = app.transcript_markdown();
        assert!(markdown.contains("## ❯ fix it") && markdown.contains("Done — see `a.rs`."), "{markdown}");
        assert!(!markdown.contains("hmm") && !markdown.contains("a notice"));
    }
}
