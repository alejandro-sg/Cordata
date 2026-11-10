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
    /// Build, lock, sandbox, policy, guard, or credential-adjacent file.
    pub sensitive: bool,
    /// 1-based slot numbers of other open threads that also edited this file.
    pub shared_threads: Vec<usize>,
}

/// Whether `path` touches build scripts, dependency locks, sandbox/policy guards, CI, or auth boundaries.
pub fn is_sensitive_path(path: &str) -> bool {
    let norm = path.replace('\\', "/");
    let lower = norm.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    matches!(
        name,
        "cargo.toml"
            | "cargo.lock"
            | "makefile"
            | "build.rs"
            | "deny.toml"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
    ) || lower.starts_with(".github/")
        || lower.contains("/.github/")
        || ["sandbox", "policy", "guard", "keychain", "auth", "permission", "secret"]
            .iter()
            .any(|kw| name.contains(kw))
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

/// Relative paths edited (or waiting to be edited) in `messages`.
pub fn edited_paths(messages: &[Message], cwd: &Path) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for m in messages {
        let Message::Tool(card) = m else { continue };
        if card.status == ToolStatus::Failed {
            continue;
        }
        for d in &card.diffs {
            let rel = relative(&d.path, cwd);
            if !rel.is_empty() {
                set.insert(rel);
            }
        }
        if card.diffs.is_empty()
            && verb(&card.name) == "edit"
            && let Some(p) = input_path(card)
        {
            let rel = relative(&p, cwd);
            if !rel.is_empty() {
                set.insert(rel);
            }
        }
    }
    set.into_iter().collect()
}

/// Candidate relative write-scope prefixes inferred from `@path` mentions in prompts and files touched so far.
pub fn candidate_scope(messages: &[Message], cwd: &Path) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for m in messages {
        if let Message::User(text) = m {
            for word in text.split_whitespace() {
                if let Some(mention) = word.strip_prefix('@') {
                    let clean = mention.trim_matches(|c: char| matches!(c, ',' | '.' | ';' | ':' | ')' | '(' | '\'' | '"'));
                    let rel = relative(clean, cwd);
                    if !rel.is_empty() && !rel.starts_with("..") && cwd.join(&rel).exists() {
                        out.insert(rel);
                    }
                }
            }
        }
    }
    for area in places(messages, cwd, None) {
        for place in &area.places {
            if matches!(place.touch, Touch::Edited | Touch::Waiting | Touch::Working) {
                out.insert(scope_prefix_of(&place.path));
            }
        }
    }
    if out.is_empty() {
        for area in places(messages, cwd, None) {
            for place in &area.places {
                out.insert(scope_prefix_of(&place.path));
            }
        }
    }
    out.into_iter().take(6).collect()
}

fn scope_prefix_of(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    if parts.len() <= 1 {
        return path.to_owned();
    }
    if let Some(i) = parts.iter().position(|p| matches!(*p, "crates" | "packages" | "apps" | "libs"))
        && i + 2 <= parts.len()
    {
        return parts[..=i + 1].join("/");
    }
    parts[..parts.len() - 1].join("/")
}

/// The files this thread touched, grouped by component; areas and files with the most pressing state first, then by
/// name. `waiting` is the id of the call an approval is open for.
pub fn places(messages: &[Message], cwd: &Path, waiting: Option<&str>) -> Vec<Area> {
    places_with_peers(messages, cwd, waiting, &[])
}

/// [`places`] annotated with `shared_threads` from other open sessions `(1_based_slot, edited_paths)`.
pub fn places_with_peers(
    messages: &[Message],
    cwd: &Path,
    waiting: Option<&str>,
    peers: &[(usize, Vec<String>)],
) -> Vec<Area> {
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
            let entry = files.entry(path.clone()).or_insert_with(|| {
                let mut shared_threads: Vec<usize> = peers
                    .iter()
                    .filter(|(_, edited)| edited.iter().any(|p| p == &path))
                    .map(|(slot, _)| *slot)
                    .collect();
                shared_threads.sort_unstable();
                shared_threads.dedup();
                Place {
                    sensitive: is_sensitive_path(&path),
                    path,
                    touch,
                    added: 0,
                    removed: 0,
                    shared_threads,
                }
            });
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

/// Falsification audit over a slice of file diffs: net removed test assertions, newly ignored tests, and sensitive files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffAudit {
    pub assertions_removed: usize,
    pub tests_ignored: usize,
    pub sensitive_files: usize,
}

fn count_assertions(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with("//")
                && (trimmed.contains("assert!(")
                    || trimmed.contains("assert_eq!(")
                    || trimmed.contains("assert_ne!(")
                    || trimmed.contains("assert_matches!(")
                    || trimmed.starts_with("#[test]")
                    || trimmed.starts_with("#[tokio::test")
                    || trimmed.starts_with("#[gpui::test"))
        })
        .count()
}

fn count_ignores(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("#[ignore") || trimmed.contains(".skip(") || trimmed.contains("@pytest.mark.skip")
        })
        .count()
}

pub fn audit_diffs(diffs: &[FileDiff]) -> DiffAudit {
    let mut audit = DiffAudit::default();
    let mut sensitive = std::collections::BTreeSet::new();
    for diff in diffs {
        let old = diff.old_text.as_deref().unwrap_or("");
        let new = &diff.new_text;
        audit.assertions_removed += count_assertions(old).saturating_sub(count_assertions(new));
        audit.tests_ignored += count_ignores(new).saturating_sub(count_ignores(old));
        if is_sensitive_path(&diff.path) {
            sensitive.insert(diff.path.clone());
        }
    }
    audit.sensitive_files = sensitive.len();
    audit
}

/// Per-turn summary for the tape scrubber and forensic drawer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    /// 0-based turn index on the tape.
    pub index: usize,
    /// Index of the `Message::User` prompt in `messages`.
    pub message: usize,
    /// Exclusive end index in `messages` (start of next user turn or `messages.len()`).
    pub end_message: usize,
    pub prompt: String,
    pub files: usize,
    pub added: usize,
    pub removed: usize,
    pub tools: usize,
    pub conclusion: Option<String>,
}

pub fn turn_summaries(messages: &[Message]) -> Vec<TurnSummary> {
    let stops = turns(messages);
    stops
        .iter()
        .enumerate()
        .map(|(index, stop)| {
            let end_message = stops.get(index + 1).map_or(messages.len(), |next| next.message);
            let slice = &messages[stop.message + 1..end_message];
            let mut files = std::collections::BTreeSet::new();
            let (mut added, mut removed, mut tools) = (0usize, 0usize, 0usize);
            let mut conclusion = None;
            for m in slice {
                match m {
                    Message::Tool(card) => {
                        tools += 1;
                        for d in &card.diffs {
                            files.insert(d.path.clone());
                        }
                        let (a, r) = diff_counts(&card.diffs);
                        added += a;
                        removed += r;
                    }
                    Message::Agent(text) if !text.trim().is_empty() => {
                        conclusion = Some(text.trim().to_owned());
                    }
                    _ => {}
                }
            }
            TurnSummary {
                index,
                message: stop.message,
                end_message,
                prompt: stop.prompt.clone(),
                files: files.len(),
                added,
                removed,
                tools,
                conclusion,
            }
        })
        .collect()
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
    /// Net test assertions or `#[test]` attributes deleted in this turn's edits.
    pub assertions_removed: usize,
    /// Net `#[ignore]` / skipped test annotations added in this turn's edits.
    pub tests_ignored: usize,
    /// Sensitive build/lock/sandbox/policy files edited in this turn.
    pub sensitive_files: usize,
}

impl Receipt {
    pub fn has_falsification_warning(&self) -> bool {
        self.assertions_removed > 0 || self.tests_ignored > 0
    }
}

/// The receipt for everything since your last prompt. `check` is the project's check command, if one is known.
pub fn receipt(messages: &[Message], check: Option<&str>) -> Receipt {
    let start = messages.iter().rposition(|m| matches!(m, Message::User(_))).map_or(0, |i| i + 1);
    let mut r = Receipt::default();
    let mut files = std::collections::BTreeSet::new();
    let mut all_diffs: Vec<FileDiff> = Vec::new();
    for m in &messages[start..] {
        let Message::Tool(card) = m else { continue };
        if card.status == ToolStatus::Failed {
            r.failed += 1;
        }
        for d in &card.diffs {
            files.insert(d.path.clone());
            all_diffs.push(d.clone());
        }
        // A thread restored from disk keeps no diffs, only that a file was edited and which: it still changed it.
        if card.diffs.is_empty()
            && verb(&card.name) == "edit"
            && card.status == ToolStatus::Completed
            && let Some(path) = card.raw_input.as_ref().and_then(|i| i.get("path")).and_then(|v| v.as_str())
        {
            files.insert(path.to_owned());
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
    let audit = audit_diffs(&all_diffs);
    r.assertions_removed = audit.assertions_removed;
    r.tests_ignored = audit.tests_ignored;
    r.sensitive_files = audit.sensitive_files;
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

/// Memoizable snapshot of every derived desk surface for a single conversation version, eliminating per-frame scans
/// across messages and diffs during 120Hz scrolling, hover, and live-line animations.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub brief_shown: Option<String>,
    pub turns: Vec<Turn>,
    pub turn_summaries: Vec<TurnSummary>,
    pub note_texts: Vec<String>,
    pub failing_notes: usize,
    pub edited_files: usize,
    pub candidate_scope: Vec<String>,
    pub places: Vec<Area>,
    pub receipt: Receipt,
    pub session_audit: DiffAudit,
    pub conclusion_shown: Option<String>,
    pub session_diffs: Vec<FileDiff>,
}

/// Computes a complete [`Snapshot`] once when a conversation's version or peer set changes.
pub fn snapshot(
    messages: &[Message],
    cwd: &Path,
    waiting_id: Option<&str>,
    check: Option<&str>,
    peers: &[(usize, Vec<String>)],
) -> Snapshot {
    let brief_shown = brief(messages).map(|b| crate::shown(&b));
    let turns = turns(messages);
    let turn_summaries = turn_summaries(messages);
    let note_ix = session_notes(messages);
    let note_texts: Vec<String> = note_ix
        .iter()
        .filter_map(|i| {
            if let Some(Message::Info(t)) = messages.get(*i) {
                Some(crate::shown(t))
            } else {
                None
            }
        })
        .collect();
    let failing_notes = note_texts
        .iter()
        .filter(|n| n.contains("did not start") || n.contains("error"))
        .count();
    let places = places_with_peers(messages, cwd, waiting_id, peers);
    let edited_files = places
        .iter()
        .map(|a| a.places.iter().filter(|p| p.touch == Touch::Edited).count())
        .sum();
    let candidate_scope = candidate_scope(messages, cwd);
    let receipt = receipt(messages, check);
    let mut session_diffs = Vec::new();
    for m in messages {
        if let Message::Tool(card) = m
            && card.status == ToolStatus::Completed
        {
            session_diffs.extend(card.diffs.iter().cloned());
        }
    }
    let session_audit = audit_diffs(&session_diffs);
    let conclusion_shown = messages.iter().rev().find_map(|m| {
        if let Message::Agent(t) = m
            && !t.trim().is_empty()
        {
            Some(crate::shown(t))
        } else {
            None
        }
    });
    Snapshot {
        brief_shown,
        turns,
        turn_summaries,
        note_texts,
        failing_notes,
        edited_files,
        candidate_scope,
        places,
        receipt,
        session_audit,
        conclusion_shown,
        session_diffs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restored_edit_counts_as_a_changed_file() {
        let messages = vec![
            Message::User("make a.txt".into()),
            Message::Tool(card("e1", "edit_file", serde_json::json!({"path": "a.txt"}), ToolStatus::Completed)),
            Message::Tool(card("e2", "edit_file", serde_json::json!({"path": "a.txt"}), ToolStatus::Completed)),
            Message::Tool(card("e3", "edit_file", serde_json::json!({"path": "b.txt"}), ToolStatus::Failed)),
        ];
        assert_eq!(receipt(&messages, None).files, 1, "one file edited twice; a failed edit changed nothing");
    }
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

    #[test]
    fn falsification_audit_flags_deleted_assertions_ignored_tests_and_sensitive_files() {
        let mut weaken = card("1", "edit_file", json!({"path": "crates/x/src/lib.rs"}), ToolStatus::Completed);
        weaken.diffs.push(FileDiff {
            path: "crates/x/src/lib.rs".into(),
            old_text: Some("#[test]\nfn check_it() {\n    assert_eq!(1 + 1, 2);\n    assert!(true);\n}\n".into()),
            new_text: "#[test]\n#[ignore]\nfn check_it() {\n}\n".into(),
        });
        let mut lock_edit = card("2", "edit_file", json!({"path": "Cargo.toml"}), ToolStatus::Completed);
        lock_edit.diffs.push(FileDiff {
            path: "Cargo.toml".into(),
            old_text: Some("[dependencies]\n".into()),
            new_text: "[dependencies]\nfoo = \"1\"\n".into(),
        });
        let mut pass_run = card("3", "run_command", json!({"cmd": "cargo test"}), ToolStatus::Completed);
        pass_run.content = "test result: ok. 0 passed; 0 failed; 1 ignored".into();
        let msgs = vec![Message::User("fix tests".into()), Message::Tool(weaken), Message::Tool(lock_edit), Message::Tool(pass_run)];
        let r = receipt(&msgs, Some("cargo test"));
        assert_eq!(r.check, Some(("cargo test".into(), CheckState::Passed)));
        assert!(r.has_falsification_warning());
        assert_eq!(r.assertions_removed, 2);
        assert_eq!(r.tests_ignored, 1);
        assert_eq!(r.sensitive_files, 1);
    }

    #[test]
    fn places_with_peers_marks_sensitive_and_cross_thread_overlaps() {
        let cwd = Path::new("/repo");
        let mut edit = card("1", "edit_file", json!({"path": "/repo/tui/crates/acp-core/src/policy.rs"}), ToolStatus::Completed);
        edit.diffs.push(FileDiff { path: "/repo/tui/crates/acp-core/src/policy.rs".into(), old_text: Some("a\n".into()), new_text: "b\n".into() });
        let msgs = vec![Message::User("tighten policy".into()), Message::Tool(edit)];
        let peers = vec![(2, vec!["tui/crates/acp-core/src/policy.rs".to_owned()]), (4, vec!["README.md".to_owned()])];
        let areas = places_with_peers(&msgs, cwd, None, &peers);
        let p = &areas[0].places[0];
        assert!(p.sensitive, "policy.rs is a sensitive boundary file");
        assert_eq!(p.shared_threads, vec![2]);
        assert_eq!(candidate_scope(&msgs, cwd), vec!["tui/crates/acp-core".to_owned()]);
        let summaries = turn_summaries(&msgs);
        assert_eq!(summaries.len(), 1);
        assert_eq!((summaries[0].files, summaries[0].added, summaries[0].removed), (1, 1, 1));
    }

    #[test]
    #[ignore]
    fn bench_ux_revamp_desk_and_sheet_frame() {
        let cwd = Path::new("/repo");
        let mut msgs = vec![Message::Info("settings: ~/.cordata/settings.json".into())];
        for t in 0..30 {
            msgs.push(Message::User(format!("Turn {t}: update module {t} and verify invariants")));
            let mut edit = card(&format!("edit-{t}"), "edit_file", json!({"path": format!("/repo/app/crates/cordata-app/src/mod_{t}.rs")}), ToolStatus::Completed);
            edit.diffs.push(FileDiff {
                path: format!("/repo/app/crates/cordata-app/src/mod_{t}.rs"),
                old_text: Some((0..15).map(|i| format!("    let old_val_{i} = compute({i});\n")).collect()),
                new_text: (0..22).map(|i| format!("    let new_val_{i} = compute_fast({i});\n")).collect(),
            });
            msgs.push(Message::Tool(edit));
            let mut run = card(&format!("run-{t}"), "run_command", json!({"cmd": "scripts/verify.sh"}), ToolStatus::Completed);
            run.content = "test result: ok. 120 passed; 0 failed; 0 ignored".into();
            msgs.push(Message::Tool(run));
            msgs.push(Message::Agent(format!("Completed module {t} changes and verified `scripts/verify.sh` passes cleanly.")));
        }
        let peers = vec![
            (2, vec!["app/crates/cordata-app/src/mod_3.rs".to_owned(), "app/crates/cordata-app/src/mod_7.rs".to_owned()]),
            (3, vec!["app/crates/cordata-app/src/mod_12.rs".to_owned()]),
        ];

        // 22 changed files, ~140 lines per file = ~3,080 short diff lines (< 256 bytes each).
        let mut patch = String::new();
        for f in 0..22 {
            patch.push_str(&format!(
                "diff --git a/app/crates/cordata-app/src/file_{f}.rs b/app/crates/cordata-app/src/file_{f}.rs\n\
                 index 1111111..2222222 100644\n\
                 --- a/app/crates/cordata-app/src/file_{f}.rs\n\
                 +++ b/app/crates/cordata-app/src/file_{f}.rs\n\
                 @@ -1,50 +1,90 @@\n"
            ));
            for l in 0..30 {
                patch.push_str(&format!(" fn context_line_{f}_{l}(state: &Workspace) -> usize {{ state.len() }}\n"));
            }
            for l in 0..20 {
                patch.push_str(&format!("-    let old_metric_{l} = compute_slow_metric(state, {l});\n"));
            }
            for l in 0..90 {
                patch.push_str(&format!("+    let fast_metric_{l} = compute_cached_metric(state, {l});\n"));
            }
        }
        let parsed_files = crate::review::parse(&patch);
        let total_lines: usize = parsed_files.iter().flat_map(|f| &f.hunks).map(|h| h.lines.len()).sum();

        let triage_inputs: Vec<crate::work::TriageIn> = (0..6)
            .map(|ix| crate::work::TriageIn {
                index: ix,
                slot: ix + 1,
                title: format!("Task {}", ix + 1),
                folder: cwd.to_path_buf(),
                state: if ix < 2 { crate::app::TabState::NeedsApproval } else if ix < 4 { crate::app::TabState::Done } else { crate::app::TabState::Working },
                active: ix == 0,
                files: 4,
                added: 80,
                removed: 18,
                check: Some(("scripts/verify.sh".into(), CheckState::Passed)),
                falsification_warning: false,
                gate_title: (ix < 2).then(|| "run_command · cargo test".to_owned()),
                gate_signature: (ix < 2).then(|| "/repo|run_command|{\"cmd\":\"cargo test\"}|".to_owned()),
                gate_high_risk: false,
                working_secs: Some(95),
                overlaps: if ix == 0 { vec![2] } else { Vec::new() },
                now: "editing src/lib.rs".into(),
                beats: (0..crate::flight::LANE_LEN)
                    .map(|b| [crate::flight::Beat::Look, crate::flight::Beat::Edit, crate::flight::Beat::Run][b % 3])
                    .collect(),
                tips: Vec::new(),
            })
            .collect();

        let median_ms = |f: &dyn Fn()| {
            let mut samples: Vec<f64> = (0..31)
                .map(|_| {
                    let start = std::time::Instant::now();
                    f();
                    start.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            samples.sort_by(f64::total_cmp);
            samples[15]
        };

        let uncached = median_ms(&|| {
            let snap = snapshot(&msgs, cwd, None, Some("scripts/verify.sh"), &peers);
            std::hint::black_box(&snap);
            let mut scrubbed_chars = 0usize;
            for file in &parsed_files {
                for hunk in &file.hunks {
                    for ln in &hunk.lines {
                        let (_, rest) = ln.split_at(ln.chars().next().map_or(0, char::len_utf8));
                        scrubbed_chars += crate::shown(rest).len();
                    }
                }
            }
            std::hint::black_box(scrubbed_chars);
            std::hint::black_box(crate::work::triage_rack(&triage_inputs));
        });

        let prep = median_ms(&|| {
            std::hint::black_box(crate::review::display_files(&parsed_files));
        });

        let cached_snap = std::rc::Rc::new(snapshot(&msgs, cwd, None, Some("scripts/verify.sh"), &peers));
        let prepped_files = crate::review::display_files(&parsed_files);
        let run_count: usize = prepped_files.iter().flat_map(|f| &f.hunks).map(|h| h.runs.len()).sum();

        let memoized = median_ms(&|| {
            let snap = std::rc::Rc::clone(&cached_snap);
            std::hint::black_box(&snap.receipt);
            std::hint::black_box(&snap.places);
            let mut draw_bytes = 0usize;
            for file in prepped_files.iter().take(4) {
                for hunk in &file.hunks {
                    for run in &hunk.runs {
                        draw_bytes += run.text.len();
                    }
                }
            }
            std::hint::black_box(draw_bytes);
            std::hint::black_box(crate::work::triage_rack(&triage_inputs));
        });

        println!(
            "BENCH ux revamp (30 turns, 22 files / {total_lines} lines -> {run_count} runs): uncached {uncached:.3}ms | bg prep {prep:.3}ms | memoized frame {memoized:.3}ms | 120Hz budget 8.333ms"
        );
        assert!(memoized < 1.0, "memoized UX revamp frame ({memoized:.3}ms) must stay well under 1.0ms (8.333ms 120Hz budget)");
    }
}
