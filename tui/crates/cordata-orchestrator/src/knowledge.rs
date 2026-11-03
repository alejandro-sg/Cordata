//! What runs on a project taught, kept for the next one: Antigravity's knowledge items in miniature. An item is a key,
//! a one-line summary, where it came from and how often it was seen. Prompts carry only the summaries.
//!
//! Lessons are read off the run record, with no model call: cheap, deterministic, and never an invented fact. They
//! live in `<home>/knowledge/<sha1(realpath(project))[:16]>.jsonl`, one item per line, at most [`MAX_ITEMS`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::machine::RunRecord;

pub const MAX_ITEMS: usize = 40;
/// How many summaries a prompt carries.
pub const IN_PROMPT: usize = 8;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub key: String,
    pub summary: String,
    /// The run that last taught it.
    pub source: String,
    pub seen: u32,
    pub last: f64,
}

#[derive(Debug, Default)]
pub struct Knowledge {
    path: Option<PathBuf>,
    pub items: Vec<Item>,
}

impl Knowledge {
    pub fn path_for(home: &Path, project: &Path) -> PathBuf {
        let real = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
        let digest = Sha1::digest(real.to_string_lossy().as_bytes());
        let key: String = digest.iter().map(|b| format!("{b:02x}")).collect::<String>().chars().take(16).collect();
        home.join("knowledge").join(format!("{key}.jsonl"))
    }

    /// The items at `path`; none when the file is missing or a line does not parse (that line is skipped).
    pub fn load(path: PathBuf) -> Self {
        let items = std::fs::read_to_string(&path).map(|text| text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()).unwrap_or_default();
        Self { path: Some(path), items }
    }

    /// Adds the lesson, or counts it again and takes its newer wording.
    pub fn learn(&mut self, key: &str, summary: &str, source: &str, now: f64) {
        match self.items.iter_mut().find(|i| i.key == key) {
            Some(item) => {
                item.seen += 1;
                item.last = now;
                item.summary = summary.to_owned();
                item.source = source.to_owned();
            }
            None => self.items.push(Item { key: key.to_owned(), summary: summary.to_owned(), source: source.to_owned(), seen: 1, last: now }),
        }
    }

    /// Most seen first, then most recent.
    fn ranked(&self) -> Vec<&Item> {
        let mut items: Vec<&Item> = self.items.iter().collect();
        items.sort_by(|a, b| b.seen.cmp(&a.seen).then(b.last.total_cmp(&a.last)));
        items
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.clone() else { return Ok(()) };
        let mut kept: Vec<Item> = self.ranked().into_iter().take(MAX_ITEMS).cloned().collect();
        kept.sort_by(|a, b| a.last.total_cmp(&b.last));
        self.items = kept;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text: String = self.items.iter().filter_map(|i| serde_json::to_string(i).ok()).map(|l| l + "\n").collect();
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }

    /// The prompt section: the most useful summaries, or nothing when there are none.
    pub fn brief(&self) -> String {
        let lines: Vec<String> = self.ranked().into_iter().take(IN_PROMPT).map(|i| format!("- {}", i.summary)).collect();
        if lines.is_empty() { String::new() } else { format!("What earlier runs on this project learned (use it; do not repeat their mistakes):\n{}", lines.join("\n")) }
    }
}

/// The lessons a finished run teaches, as `(key, summary)`.
pub fn lessons(record: &RunRecord) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for w in &record.warnings {
        if let Some(worker) = w.strip_suffix(" gave up after a refused command; the rest of the run prefers other workers") {
            out.push((format!("gave_up:{worker}"), format!("{worker} stops when a shell command is refused: give it file-only steps, or prefer another worker for steps that need commands")));
        }
        if let Some(models) = w.strip_prefix("agy does not offer ").and_then(|m| m.strip_suffix("; left out of this run")) {
            out.push(("agy_offers".into(), format!("the installed agy does not offer {models}")));
        }
    }
    if record.state == "done"
        && let Some(plan) = &record.plan
    {
        let commands = plan.get("verifier_commands").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned);
        let criteria = plan
            .get("acceptance_criteria")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("command"))
            .filter_map(|c| c.get("target").and_then(Value::as_str))
            .map(str::to_owned);
        for cmd in commands.chain(criteria).filter(|c| !c.trim().is_empty()) {
            out.push((format!("check:{cmd}"), format!("`{cmd}` checks changes here and passed after a finished run")));
        }
    }
    if matches!(record.state.as_str(), "failed" | "paused") && !record.reason.is_empty() {
        // The cause, without the run-specific step ids and counts, is what recurs.
        let cause: String = record.reason.split(": ").last().unwrap_or(&record.reason).chars().take(160).collect();
        let key: String = cause.chars().filter(|c| c.is_ascii_alphabetic()).take(60).collect();
        out.push((format!("{}:{key}", record.state), format!("a run {} here because {cause}", if record.state == "paused" { "stopped" } else { "failed" })));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(state: &str) -> RunRecord {
        let mut r: RunRecord = serde_json::from_value(json!({"run_id": "run-1", "goal": "g", "cwd": "/p", "state": state})).unwrap();
        r.warnings = vec!["antigravity:claude-sonnet-5-5-medium gave up after a refused command; the rest of the run prefers other workers".into()];
        r
    }

    #[test]
    fn a_run_teaches_from_its_warnings_its_checks_and_its_failure() {
        let mut done = record("done");
        done.plan = Some(json!({"verifier_commands": ["make tui-test"], "acceptance_criteria": [{"type": "command", "target": "cargo test -p x"}, {"type": "file_exists", "target": "a"}]}));
        let keys: Vec<String> = lessons(&done).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["gave_up:antigravity:claude-sonnet-5-5-medium", "check:make tui-test", "check:cargo test -p x"]);
        let mut failed = record("failed");
        failed.reason = "execution failed: step S1 failed 3 times: agy:m: the worker made no change".into();
        let (key, summary) = lessons(&failed).pop().unwrap();
        assert!(key.starts_with("failed:theworkermadenochange"), "{key}");
        assert_eq!(summary, "a run failed here because the worker made no change");
    }

    #[test]
    fn items_persist_count_repeats_and_reach_the_prompt_most_seen_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = Knowledge::path_for(dir.path(), dir.path());
        let mut k = Knowledge::load(path.clone());
        assert_eq!(k.brief(), "");
        k.learn("a", "lesson a", "run-1", 1.0);
        k.learn("b", "lesson b", "run-1", 2.0);
        k.learn("a", "lesson a, reworded", "run-2", 3.0);
        k.save().unwrap();
        let back = Knowledge::load(path.clone());
        assert_eq!(back.items.iter().find(|i| i.key == "a").map(|i| (i.seen, i.summary.as_str())), Some((2, "lesson a, reworded")));
        assert!(back.brief().ends_with("- lesson a, reworded\n- lesson b"), "{}", back.brief());
        let mut many = Knowledge::load(path.clone());
        for n in 0..(MAX_ITEMS + 5) {
            many.learn(&format!("k{n}"), "x", "r", 10.0 + n as f64);
        }
        many.save().unwrap();
        assert_eq!(Knowledge::load(path).items.len(), MAX_ITEMS);
    }
}
