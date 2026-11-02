//! Slash commands typed into the input box (`/help`, `/mode plan`, …). Pure
//! parsing; `App` decides what each one does.

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    /// Expand / collapse the agents' reasoning.
    Thinking,
    /// Expand / collapse tool results.
    Tools,
    /// Show the instruction files in the system prompt.
    Memory,
    /// Ask the agent to draft CORDATA.md for this project.
    Init,
    Status,
    /// The working tree's changes.
    Diff,
    /// The last reply, to the clipboard.
    Copy,
    /// The conversation, to a markdown file.
    Export(Option<String>),
    /// The command deck: every mode, model, view and command, searchable (Ctrl+K).
    Palette,
    /// The interactive diff review of the working tree (F4).
    Review,
    /// Architectural floorplan & active-work visualizer.
    Floorplan(Option<String>),
    /// Another session in this project (Ctrl+N).
    New,
    /// Show another session: a number, or the next one.
    Switch(Option<String>),
    /// End the shown session.
    Close,
    /// This project's saved sessions.
    History,
    /// Continue a saved session: `last` (the default), an id or a prefix of one.
    Resume(Option<String>),
    /// Delete this project's saved sessions; `/forget confirm` does it, plain `/forget` says what it would delete.
    Forget(Option<String>),
    /// The rules, network and write scope the agent runs under.
    Permissions,
    Clear,
    Quit,
    Unknown(String),
}

/// What the help overlay lists, so the docs and the parser can't drift apart.
pub const HELP: &[(&str, &str)] = &[
    ("/help", "show this help (also ? or F1)"),
    ("/thinking", "expand / collapse reasoning"),
    ("/tools", "expand / collapse tool results"),
    ("/palette", "the command deck: every mode, model, view and command, searchable (Ctrl+K)"),
    ("/diffview", "review the working tree's changes: files, hunks, mouse (F4)"),
    ("/floorplan [html]", "interactive architectural floorplan & active-work visualizer"),
    ("/new", "another session in this project (Ctrl+N) — a new shared conversation on the hub"),
    ("/switch [n]", "show session n (Ctrl+←/→, Alt+1…9; Ctrl+G: the next one that needs you)"),
    ("/close", "end the shown session"),
    ("/history", "this project's saved sessions"),
    ("/resume [id]", "continue a saved session (the last one by default)"),
    ("/forget", "delete this project's saved sessions"),
    ("/permissions", "what runs without asking, what is refused, network and write scope"),
    ("/memory", "show the instruction files the agent reads"),
    ("/init", "have the agent draft CORDATA.md"),
    ("/status", "session, mode, model, rules, quota pools"),
    ("/diff", "what is changed in the working tree"),
    ("/copy", "copy the last reply to the clipboard"),
    ("/export [file]", "save this conversation as markdown"),
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
        "thinking" | "thoughts" | "reasoning" => Command::Thinking,
        "tools" => Command::Tools,
        "memory" => Command::Memory,
        "init" => Command::Init,
        "status" => Command::Status,
        "diff" | "changes" => Command::Diff,
        "copy" => Command::Copy,
        "export" => Command::Export(arg),
        "palette" | "deck" => Command::Palette,
        "diffview" | "dv" => Command::Review,
        "floorplan" | "fp" => Command::Floorplan(arg),
        "new" => Command::New,
        "switch" | "session" => Command::Switch(arg),
        "close" => Command::Close,
        "history" => Command::History,
        "resume" | "continue" => Command::Resume(arg),
        "forget" => Command::Forget(arg),
        "permissions" | "rules" => Command::Permissions,
        "clear" | "cls" => Command::Clear,
        "quit" | "exit" | "q" => Command::Quit,
        other => Command::Unknown(other.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_with_and_without_arguments() {
        assert_eq!(parse("/help"), Some(Command::Help));
        assert_eq!(parse("/"), Some(Command::Help));
        assert_eq!(parse("  /Thinking "), Some(Command::Thinking));
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
    fn session_commands_parse() {
        assert_eq!(parse("/switch 2"), Some(Command::Switch(Some("2".into()))));
        assert_eq!(parse("/resume 19a"), Some(Command::Resume(Some("19a".into()))));
        assert_eq!(parse("/continue"), Some(Command::Resume(None)));
        assert_eq!(parse("/forget confirm"), Some(Command::Forget(Some("confirm".into()))));
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
