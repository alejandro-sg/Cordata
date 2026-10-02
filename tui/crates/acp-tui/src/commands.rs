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
    ("/laya [on|off]", "show / hide the live Laya triage line"),
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
