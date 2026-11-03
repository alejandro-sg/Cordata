//! Dynamic tool pruning: the core tools always go out; the rest only when the prompt or the
//! repository's history asks for them.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use serde_json::Value;

use crate::profile::Profile;

/// Tools every request carries.
pub const CORE_TOOLS: [&str; 4] = ["read_file", "edit_file", "run_command", "code_search"];

/// Anything with a tool name, so callers prune their own spec type without converting it.
pub trait Named {
    fn tool_name(&self) -> &str;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

impl Named for ToolDefinition {
    fn tool_name(&self) -> &str {
        &self.name
    }
}

impl ToolDefinition {
    /// Tokens this definition costs on every request.
    pub fn tokens(&self) -> usize {
        crate::estimate_tokens(&self.name) + crate::estimate_tokens(&self.description) + crate::estimate_tokens(&self.schema.to_string())
    }
}

/// Words in a prompt that call for a tool beyond the core, matched by prefix on whole words.
const INTENTS: &[(&str, &[&str])] = &[
    ("code_outline", &["outline", "structure", "overview"]),
    ("code_map", &["where", "find", "locate", "which", "how", "architecture", "map", "explain", "understand"]),
];

/// Name fragments too common to signal intent on their own.
const GENERIC: [&str; 4] = ["code", "file", "tool", "run"];

fn words(prompt: &str) -> Vec<String> {
    prompt.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_ascii_lowercase).collect()
}

/// Prompt words that point at `tool`: its family's keywords count once, its own name fragments count extra,
/// so `git_diff` outranks `git_log` when the prompt says "diff".
fn intent_hits(tool: &str, words: &[String]) -> usize {
    let family = INTENTS.iter().filter(|(prefix, _)| tool.starts_with(prefix)).flat_map(|(_, k)| k.iter().copied());
    let own = tool.split('_').filter(|t| t.len() >= 3 && !GENERIC.contains(t)).collect::<Vec<_>>();
    let family_hits = words.iter().filter(|w| family.clone().any(|k| w.starts_with(k))).count();
    let own_hits = words.iter().filter(|w| own.iter().any(|k| w.starts_with(k))).count();
    family_hits + own_hits * 3
}

#[derive(Debug, Default, Clone)]
pub struct ToolPruner {
    /// repository path → tool name → invocations ever recorded.
    usage_matrix: HashMap<String, HashMap<String, u32>>,
}

impl ToolPruner {
    /// The tools to send: core tools, then the best-scoring others up to `profile.max_tools`.
    /// The result keeps the order of `all_tools`, so the serialized list is stable for prompt caching.
    pub fn select_tools<'a, T: Named>(&self, repo_path: &Path, prompt: &str, all_tools: &'a [T], profile: &Profile) -> Vec<&'a T> {
        let words = words(prompt);
        let usage = self.usage_matrix.get(&repo_path.to_string_lossy().into_owned());
        let mut extras: Vec<(usize, usize)> = Vec::new(); // (score, index)
        let mut chosen = vec![false; all_tools.len()];
        let mut taken = 0;
        for (index, tool) in all_tools.iter().enumerate() {
            let name = tool.tool_name();
            if CORE_TOOLS.contains(&name) {
                chosen[index] = true;
                taken += 1;
                continue;
            }
            let used = usage.and_then(|u| u.get(name)).copied().unwrap_or(0);
            let score = intent_hits(name, &words) * 10 + used.min(9) as usize;
            // History alone never pulls a tool in on the most constrained models.
            if intent_hits(name, &words) > 0 || (used > 0 && profile.max_tools > CORE_TOOLS.len()) {
                extras.push((score, index));
            }
        }
        extras.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for (_, index) in extras.into_iter().take(profile.max_tools.saturating_sub(taken)) {
            chosen[index] = true;
        }
        all_tools.iter().zip(chosen).filter_map(|(tool, keep)| keep.then_some(tool)).collect()
    }

    pub fn record_tool_invoked(&mut self, repo_path: &Path, tool_name: &str) {
        let repo = self.usage_matrix.entry(repo_path.to_string_lossy().into_owned()).or_default();
        let count = repo.entry(tool_name.to_owned()).or_default();
        *count = count.saturating_add(1);
    }

    pub fn usage(&self, repo_path: &Path, tool_name: &str) -> u32 {
        self.usage_matrix.get(&repo_path.to_string_lossy().into_owned()).and_then(|u| u.get(tool_name)).copied().unwrap_or(0)
    }

    /// A missing or unreadable file is an empty matrix: pruning is an optimisation, never a failure.
    pub fn load(path: &Path) -> Self {
        let usage_matrix = std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        Self { usage_matrix }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        crate::write_atomic(path, &serde_json::to_vec_pretty(&self.usage_matrix)?)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::profile::Profile;

    /// The engine's five tools: the core four and the outline.
    fn catalog() -> Vec<ToolDefinition> {
        let mut names: Vec<String> = CORE_TOOLS.iter().map(|s| (*s).to_owned()).collect();
        names.push("code_outline".into());
        names
            .into_iter()
            .map(|name| ToolDefinition {
                description: format!("{name}: a tool with a deliberately wordy description so each schema costs real tokens, {}", "lorem ipsum ".repeat(12)),
                schema: json!({"type":"object","properties":{"path":{"type":"string","description":"x".repeat(150)}},"required":["path"]}),
                name,
            })
            .collect()
    }

    fn names(selected: &[&ToolDefinition]) -> Vec<String> {
        selected.iter().map(|t| t.name.clone()).collect()
    }

    #[test]
    fn a_small_model_gets_only_the_core_and_pays_for_fewer_tools() {
        let all = catalog();
        let selected = ToolPruner::default().select_tools(Path::new("/r"), "explain this function", &all, &Profile::for_model("llama3"));
        assert_eq!(names(&selected), CORE_TOOLS);
        let full: usize = all.iter().map(ToolDefinition::tokens).sum();
        let pruned: usize = selected.iter().map(|t| t.tokens()).sum();
        assert!(pruned < full, "pruned {pruned} of {full}");
    }

    #[test]
    fn asking_for_a_structure_pulls_in_the_outline_in_registry_order() {
        let all = catalog();
        let claude = Profile::for_model("claude-sonnet-5-5");
        let got = names(&ToolPruner::default().select_tools(Path::new("/r"), "give me an overview of the structure", &all, &claude));
        assert_eq!(got, ["read_file", "edit_file", "run_command", "code_search", "code_outline"]);
        let quiet = names(&ToolPruner::default().select_tools(Path::new("/r"), "fix the bug", &all, &claude));
        assert_eq!(quiet, CORE_TOOLS);
    }

    #[test]
    fn usage_history_adds_the_outline_for_roomier_profiles_only() {
        let all = catalog();
        let repo = Path::new("/r");
        let mut pruner = ToolPruner::default();
        pruner.record_tool_invoked(repo, "code_outline");
        assert!(names(&pruner.select_tools(repo, "hello", &all, &Profile::for_model("claude-x"))).contains(&"code_outline".into()));
        assert!(!names(&pruner.select_tools(repo, "hello", &all, &Profile::for_model("llama3"))).contains(&"code_outline".into()));
        assert!(!names(&pruner.select_tools(Path::new("/other"), "hello", &all, &Profile::for_model("claude-x"))).contains(&"code_outline".into()));
    }

    #[test]
    fn words_match_by_prefix_not_substring() {
        let all = catalog();
        let selected = ToolPruner::default().select_tools(Path::new("/r"), "outlook widget", &all, &Profile::for_model("claude-x"));
        assert!(!names(&selected).contains(&"code_outline".into()));
    }

    #[test]
    fn matrix_round_trips_and_a_corrupt_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/tools.json");
        let mut pruner = ToolPruner::default();
        pruner.record_tool_invoked(Path::new("/r"), "code_outline");
        pruner.record_tool_invoked(Path::new("/r"), "code_outline");
        pruner.save(&file).unwrap();
        assert_eq!(ToolPruner::load(&file).usage(Path::new("/r"), "code_outline"), 2);
        std::fs::write(&file, "{not json").unwrap();
        assert_eq!(ToolPruner::load(&file).usage(Path::new("/r"), "code_outline"), 0);
    }
}
