//! The desk: a thread read as a piece of work rather than a chat log. Everything here is derived from the transcript
//! (no new state, nothing stored), so the native app and the terminal can draw the same facts:
//!
//! - the **brief** — what you asked for, in your words (the thread's first prompt);
//! - the **tape** — one stop per prompt you sent, so a long thread can be walked by turn;
//! - the **places** — the files this thread touched, grouped by where they live, each with what happened to it;
//! - the **receipt** — what the last turn changed and whether the project's check ran and passed;
//! - the **check** — the command that says the work is done (`scripts/verify.sh`, a Makefile `test`, `cargo test`…).
//!
//! Pure functions over [`Message`]s and the project folder; no drawing.

use std::collections::BTreeMap;
use std::path::Path;

use acp_core::{FileDiff, ToolStatus};

use crate::app::{Message, ToolCard};
use crate::fold::verb;

/// The thread's brief: the first thing you asked, without `[attached: …]` tags. `None` before you have asked.
pub fn brief(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|m| match m {
        Message::User(text) => {
            let body: Vec<&str> = text.lines().filter(|l| !l.trim_start().starts_with("[attached:")).collect();
            let body = body.join(" ");
            let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
            (!body.is_empty()).then_some(body)
        }
        _ => None,
    })
}

/// One stop on the tape: a prompt you sent, by its index in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub message: usize,
    pub prompt: String,
}

pub fn turns(messages: &[Message]) -> Vec<Turn> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(message, m)| match m {
            Message::User(text) => Some(Turn { message, prompt: text.split_whitespace().collect::<Vec<_>>().join(" ") }),
            _ => None,
        })
        .collect()
}

/// Whether an `Info` line is the session talking about itself (settings read, instructions loaded, MCP servers
/// starting) rather than part of the work. These arrive before and just after the first prompt.
pub fn is_session_note(text: &str) -> bool {
    ["settings:", "instructions:", "mcp:"].iter().any(|p| text.starts_with(p))
}

/// The indexes of the session's own notes. They are said once, in the brief's meta line (hover lists them), not as
/// the transcript's first screen.
pub fn session_notes(messages: &[Message]) -> Vec<usize> {
    messages.iter().enumerate().filter_map(|(i, m)| matches!(m, Message::Info(t) if is_session_note(t)).then_some(i)).collect()
}

/// The index of the prompt that is the brief: the first `User` message. The brief stands for it above the
/// transcript, so the transcript does not say it twice.
pub fn brief_message(messages: &[Message]) -> Option<usize> {
    messages.iter().position(|m| matches!(m, Message::User(t) if !t.trim().is_empty()))
}

/// What happened to a file in this thread, most pressing first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Touch {
    /// An edit waiting on your approval.
    Waiting,
    Failed,
    /// A call on it is running now.
    Working,
    Edited,
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// Relative to the project when it is inside it.
    pub path: String,
    pub touch: Touch,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Area {
    /// The component a file belongs to: a crate under `crates/`, else the first folder, else the project root.
    pub name: String,
    pub places: Vec<Place>,
}

impl Area {
    pub fn touch(&self) -> Touch {
        self.places.iter().map(|p| p.touch).min().unwrap_or(Touch::Read)
    }
}

/// The files this thread touched, grouped by component; areas and files with the most pressing state first, then by
/// name. `waiting` is the id of the call an approval is open for.
pub fn places(messages: &[Message], cwd: &Path, waiting: Option<&str>) -> Vec<Area> {
    let mut files: BTreeMap<String, Place> = BTreeMap::new();
    for m in messages {
        let Message::Tool(card) = m else { continue };
        let touch = touch_of(card, waiting);
        let mut paths: Vec<(String, usize, usize)> = card.diffs.iter().map(|d| {
            let (a, r) = diff_counts(std::slice::from_ref(d));
            (d.path.clone(), a, r)
        }).collect();
        if paths.is_empty()
            && let Some(path) = input_path(card)
        {
            paths.push((path, 0, 0));
        }
        for (path, added, removed) in paths {
            let path = relative(&path, cwd);
            if path.is_empty() {
                continue;
            }
            let entry = files.entry(path.clone()).or_insert(Place { path, touch, added: 0, removed: 0 });
            // A file read and later edited reads as edited; an edit still waiting outranks both.
            entry.touch = entry.touch.min(touch);
            if touch != Touch::Waiting {
                entry.added += added;
                entry.removed += removed;
            }
        }
    }
    let mut areas: BTreeMap<String, Vec<Place>> = BTreeMap::new();
    for place in files.into_values() {
        areas.entry(area_of(&place.path)).or_default().push(place);
    }
    let mut out: Vec<Area> = areas
        .into_iter()
        .map(|(name, mut places)| {
            places.sort_by(|a, b| a.touch.cmp(&b.touch).then_with(|| a.path.cmp(&b.path)));
            Area { name, places }
        })
        .collect();
    out.sort_by(|a, b| a.touch().cmp(&b.touch()).then_with(|| a.name.cmp(&b.name)));
    out
}

fn touch_of(card: &ToolCard, waiting: Option<&str>) -> Touch {
    if waiting == Some(card.id.as_str()) {
        return Touch::Waiting;
    }
    match card.status {
        ToolStatus::Failed => Touch::Failed,
        ToolStatus::Pending | ToolStatus::InProgress => Touch::Working,
        ToolStatus::Completed if verb(&card.name) == "edit" || !card.diffs.is_empty() => Touch::Edited,
        ToolStatus::Completed => Touch::Read,
    }
}

/// The file a read or edit names in its input (`path`), when it names one.
fn input_path(card: &ToolCard) -> Option<String> {
    if !matches!(verb(&card.name), "read" | "edit") {
        return None;
    }
    let input = card.raw_input.as_ref()?;
    ["path", "file_path", "file"].iter().find_map(|k| input.get(*k).and_then(|v| v.as_str())).map(str::to_owned)
}

fn relative(path: &str, cwd: &Path) -> String {
    let p = Path::new(path);
    let rel = p.strip_prefix(cwd).unwrap_or(p);
    rel.to_string_lossy().trim_start_matches("./").to_owned()
}

/// `app/crates/cordata-view/src/desk.rs` is in `cordata-view`; `docs/app.md` in `docs`; `Makefile` in the project root.
pub fn area_of(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    if parts.len() <= 1 {
        return "project root".into();
    }
    if let Some(i) = parts.iter().position(|p| matches!(*p, "crates" | "packages" | "apps" | "libs"))
        && i + 2 < parts.len()
    {
        return parts[i + 1].to_owned();
    }
    parts[0].to_owned()
}

/// Lines added and removed across `diffs` (common prefix and suffix trimmed, as the diff preview shows them).
pub fn diff_counts(diffs: &[FileDiff]) -> (usize, usize) {
    let (mut add, mut del) = (0usize, 0usize);
    for diff in diffs {
        let old: Vec<&str> = diff.old_text.as_deref().unwrap_or("").lines().collect();
        let new: Vec<&str> = diff.new_text.lines().collect();
        let mut prefix = 0;
        while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
            prefix += 1;
        }
        let (mut old_end, mut new_end) = (old.len(), new.len());
        while old_end > prefix && new_end > prefix && old[old_end - 1] == new[new_end - 1] {
            old_end -= 1;
            new_end -= 1;
        }
        del += old_end - prefix;
        add += new_end - prefix;
    }
    (add, del)
}

/// How the last run of the check went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Passed,
    Failed,
    Running,
}

/// What the last turn did, said once it is over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Receipt {
    pub files: usize,
    pub added: usize,
    pub removed: usize,
    pub commands: usize,
    pub failed: usize,
    /// The check, if this turn ran it (or something that is plainly a test run).
    pub check: Option<(String, CheckState)>,
}

/// The receipt for everything since your last prompt. `check` is the project's check command, if one is known.
pub fn receipt(messages: &[Message], check: Option<&str>) -> Receipt {
    let start = messages.iter().rposition(|m| matches!(m, Message::User(_))).map_or(0, |i| i + 1);
    let mut r = Receipt::default();
    let mut files = std::collections::BTreeSet::new();
    for m in &messages[start..] {
        let Message::Tool(card) = m else { continue };
        if card.status == ToolStatus::Failed {
            r.failed += 1;
        }
        for d in &card.diffs {
            files.insert(d.path.clone());
        }
        let (a, d) = diff_counts(&card.diffs);
        r.added += a;
        r.removed += d;
        if verb(&card.name) == "run" {
            r.commands += 1;
            let cmd = card.raw_input.as_ref().and_then(|i| i.get("cmd").or_else(|| i.get("command"))).and_then(|v| v.as_str()).unwrap_or("");
            if is_check_run(cmd, check) {
                let state = match card.status {
                    ToolStatus::Completed => CheckState::Passed,
                    ToolStatus::Failed => CheckState::Failed,
                    _ => CheckState::Running,
                };
                // A command that "completed" but printed a failing test summary did not pass.
                let state = if state == CheckState::Passed && says_failed(&card.content) { CheckState::Failed } else { state };
                r.check = Some((cmd.trim().to_owned(), state));
            }
        }
    }
    r.files = files.len();
    r
}

fn is_check_run(cmd: &str, check: Option<&str>) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return false;
    }
    if check.is_some_and(|c| cmd.contains(c.trim())) {
        return true;
    }
    let words: Vec<&str> = cmd.split(|c: char| c.is_whitespace() || c == '&' || c == ';' || c == '|').filter(|w| !w.is_empty()).collect();
    words.windows(2).any(|w| matches!(w, ["cargo", "test" | "nextest"] | ["npm" | "pnpm" | "yarn" | "bun", "test"] | ["go", "test"] | ["make", "test" | "check"]))
        || words.iter().any(|w| matches!(*w, "pytest" | "verify.sh" | "scripts/verify.sh") || w.ends_with("-test") && words.first() == Some(&"make"))
}

fn says_failed(output: &str) -> bool {
    output.lines().rev().take(40).any(|l| {
        let l = l.to_ascii_lowercase();
        (l.contains("test result: failed") || l.contains(" failed;") && !l.contains(" 0 failed")) || l.starts_with("error: test failed") || l.contains("tests failed")
    })
}

/// The command that says this project's work is done, by the project's own convention: `scripts/verify.sh`, then a
/// Makefile `verify`/`test`/`check` target, then the toolchain's test runner. `None` when nothing says.
pub fn check_command(root: &Path) -> Option<String> {
    if root.join("scripts/verify.sh").is_file() {
        return Some("scripts/verify.sh".into());
    }
    if let Ok(makefile) = std::fs::read_to_string(root.join("Makefile")) {
        for target in ["verify", "test", "check"] {
            if makefile.lines().any(|l| l.split(':').next().is_some_and(|t| t.trim() == target) && l.contains(':') && !l.starts_with('\t')) {
                return Some(format!("make {target}"));
            }
        }
    }
    if root.join("Cargo.toml").is_file() {
        return Some("cargo test".into());
    }
    if let Ok(pkg) = std::fs::read_to_string(root.join("package.json"))
        && pkg.contains("\"test\"")
    {
        return Some("npm test".into());
    }
    if root.join("go.mod").is_file() {
        return Some("go test ./...".into());
    }
    if root.join("pyproject.toml").is_file() || root.join("pytest.ini").is_file() {
        return Some("pytest".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_core::ToolKind;
    use serde_json::json;

    fn card(id: &str, name: &str, input: serde_json::Value, status: ToolStatus) -> ToolCard {
        ToolCard { id: id.into(), name: name.into(), kind: ToolKind::Other, status, raw_input: Some(input), content: String::new(), diffs: Vec::new() }
    }

    #[test]
    fn the_brief_is_your_first_prompt_without_attachment_tags() {
        let msgs = vec![
            Message::Info("settings: …".into()),
            Message::User("Make the map\n  real\n[attached: a.png]".into()),
            Message::User("and faster".into()),
        ];
        assert_eq!(brief(&msgs).as_deref(), Some("Make the map real"));
        assert_eq!(turns(&msgs).iter().map(|t| t.message).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(session_notes(&msgs), vec![0]);
        assert_eq!(brief_message(&msgs), Some(1));
        assert_eq!(brief(&[]), None);
    }

    #[test]
    fn places_group_by_component_and_keep_the_most_pressing_touch() {
        let cwd = Path::new("/repo");
        let mut edit = card("2", "edit_file", json!({"path": "/repo/app/crates/cordata-view/src/desk.rs"}), ToolStatus::Completed);
        edit.diffs.push(FileDiff { path: "/repo/app/crates/cordata-view/src/desk.rs".into(), old_text: Some("a\nb\n".into()), new_text: "a\nc\nd\n".into() });
        let msgs = vec![
            Message::Tool(card("1", "read_file", json!({"path": "app/crates/cordata-view/src/desk.rs"}), ToolStatus::Completed)),
            Message::Tool(edit),
            Message::Tool(card("3", "read_file", json!({"path": "docs/app.md"}), ToolStatus::Completed)),
            Message::Tool(card("4", "edit_file", json!({"path": "Makefile"}), ToolStatus::Pending)),
            Message::Tool(card("5", "run_command", json!({"cmd": "ls"}), ToolStatus::Completed)),
        ];
        let areas = places(&msgs, cwd, Some("4"));
        assert_eq!(areas.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["project root", "cordata-view", "docs"]);
        assert_eq!(areas[0].places[0].touch, Touch::Waiting);
        let desk = &areas[1].places[0];
        assert_eq!((desk.path.as_str(), desk.touch, desk.added, desk.removed), ("app/crates/cordata-view/src/desk.rs", Touch::Edited, 2, 1));
        assert_eq!(areas[2].places[0].touch, Touch::Read);
    }

    #[test]
    fn the_receipt_covers_the_last_turn_and_reads_the_check() {
        let mut edit = card("1", "edit_file", json!({"path": "a.rs"}), ToolStatus::Completed);
        edit.diffs.push(FileDiff { path: "a.rs".into(), old_text: None, new_text: "x\ny\n".into() });
        let mut test = card("2", "run_command", json!({"cmd": "cd app && cargo test -p x"}), ToolStatus::Completed);
        test.content = "test result: FAILED. 3 passed; 1 failed;".into();
        let msgs = vec![
            Message::User("old".into()),
            Message::Tool(card("0", "run_command", json!({"cmd": "cargo test"}), ToolStatus::Completed)),
            Message::User("new".into()),
            Message::Tool(edit),
            Message::Tool(test),
            Message::Agent("done".into()),
        ];
        let r = receipt(&msgs, Some("make app-test"));
        assert_eq!((r.files, r.added, r.removed, r.commands), (1, 2, 0, 1));
        assert_eq!(r.check, Some(("cd app && cargo test -p x".into(), CheckState::Failed)));
        let quiet = receipt(&[Message::User("q".into()), Message::Tool(card("9", "run_command", json!({"cmd": "ls"}), ToolStatus::Completed))], None);
        assert_eq!(quiet.check, None);
    }

    #[test]
    fn components_are_crates_or_top_folders() {
        assert_eq!(area_of("tui/crates/acp-core/src/lib.rs"), "acp-core");
        assert_eq!(area_of("docs/app.md"), "docs");
        assert_eq!(area_of("Makefile"), "project root");
        assert_eq!(area_of("crates/x/Cargo.toml"), "x");
    }

    #[test]
    fn the_check_follows_the_project_convention() {
        let dir = std::env::temp_dir().join(format!("cordata-desk-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(check_command(&dir), None);
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(check_command(&dir).as_deref(), Some("cargo test"));
        std::fs::write(dir.join("Makefile"), "build:\n\tcargo build\ntest: build\n\tcargo test\n").unwrap();
        assert_eq!(check_command(&dir).as_deref(), Some("make test"));
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/verify.sh"), "#!/bin/sh").unwrap();
        assert_eq!(check_command(&dir).as_deref(), Some("scripts/verify.sh"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
