//! Cross-session micro-skills: one-line repository facts learned from a session ("`cargo test` fails,
//! `cargo test -- --nocapture` works"), injected into the next one. Facts that look like they hold a
//! credential are never stored.

use std::io;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Most facts kept per repository; the weakest are dropped first.
const MAX_PER_REPO: usize = 40;
const MAX_ADVICE_CHARS: usize = 200;
/// About 250 tokens of injected facts.
const INJECT_CHARS: usize = 1_000;
const RETIRE_BELOW: f32 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillCategory {
    Build,
    Test,
    Lint,
    Env,
}

impl SkillCategory {
    /// The category a shell command belongs to.
    pub fn of_command(cmd: &str) -> Self {
        let cmd = cmd.to_ascii_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| cmd.split(|c: char| !c.is_alphanumeric()).any(|t| t == *w));
        if has(&["test", "tests", "pytest", "nextest"]) {
            Self::Test
        } else if has(&["lint", "clippy", "fmt", "format", "eslint", "ruff"]) {
            Self::Lint
        } else if has(&["build", "compile", "check", "make", "install"]) {
            Self::Build
        } else {
            Self::Env
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicroSkill {
    pub id: String,
    pub repo_id: String,
    pub category: SkillCategory,
    /// What situation the advice is for, e.g. "cargo test".
    pub trigger_context: String,
    pub advice: String,
    pub confidence: f32,
    pub hit_count: u32,
    pub updated_at: u64,
}

/// A shell command that ran in a session and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    pub cmd: String,
    pub ok: bool,
}

static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(?:api[_-]?key|token|secret|passw(?:or)?d|authorization|bearer|credential)s?\b",
        r"|\bsk-[A-Za-z0-9_-]{12,}",
        r"|\b(?:ghp|gho|ghs|github_pat|xox[bpas]|AKIA|AIza)[A-Za-z0-9_-]{8,}",
        r"|\b[A-Fa-f0-9]{32,}\b",
        r"|\b[A-Za-z0-9+/_-]{40,}={0,2}",
    ))
    .expect("static regex")
});

/// True when `text` mentions or contains something that looks like a credential.
pub fn looks_secret(text: &str) -> bool {
    SECRET.is_match(text)
}

/// The stable id of a repository path.
pub fn repo_id(repo_path: &Path) -> String {
    crate::short_id(&repo_path.to_string_lossy())
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MicroSkills {
    skills: Vec<MicroSkill>,
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 3).map(str::to_ascii_lowercase).collect()
}

impl MicroSkills {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        crate::write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn for_repo(&self, repo_path: &Path) -> Vec<&MicroSkill> {
        let repo = repo_id(repo_path);
        self.skills.iter().filter(|s| s.repo_id == repo).collect()
    }

    /// Stores a fact. Returns `false` when it was refused (secret-looking, empty, multi-line).
    /// The same fact again raises confidence; a different fact for the same trigger retires the old one.
    pub fn record(&mut self, repo_path: &Path, category: SkillCategory, trigger: &str, advice: &str, now: u64) -> bool {
        let advice = advice.trim();
        let trigger = trigger.trim();
        if advice.is_empty() || trigger.is_empty() || advice.contains('\n') || advice.chars().count() > MAX_ADVICE_CHARS {
            return false;
        }
        if looks_secret(advice) || looks_secret(trigger) {
            return false;
        }
        let repo = repo_id(repo_path);
        let mut found = false;
        for skill in self.skills.iter_mut().filter(|s| s.repo_id == repo && s.category == category && s.trigger_context == trigger) {
            if skill.advice == advice {
                skill.confidence = (skill.confidence + 0.2).min(1.0);
                skill.hit_count += 1;
                skill.updated_at = now;
                found = true;
            } else {
                skill.confidence *= 0.5; // contradicted
            }
        }
        if !found {
            let id = crate::short_id(&format!("{repo}|{category:?}|{trigger}|{advice}"));
            self.skills.push(MicroSkill {
                id,
                repo_id: repo,
                category,
                trigger_context: trigger.to_owned(),
                advice: advice.to_owned(),
                confidence: 0.5,
                hit_count: 1,
                updated_at: now,
            });
        }
        self.skills.retain(|s| s.confidence >= RETIRE_BELOW);
        self.cap(&repo_id(repo_path));
        true
    }

    /// A fact turned out wrong: halve its confidence and drop it once it is weak.
    pub fn contradict(&mut self, id: &str) {
        for skill in self.skills.iter_mut().filter(|s| s.id == id) {
            skill.confidence *= 0.5;
        }
        self.skills.retain(|s| s.confidence >= RETIRE_BELOW);
    }

    fn cap(&mut self, repo: &str) {
        let mut mine: Vec<(usize, f32, u64)> =
            self.skills.iter().enumerate().filter(|(_, s)| s.repo_id == repo).map(|(i, s)| (i, s.confidence, s.updated_at)).collect();
        if mine.len() <= MAX_PER_REPO {
            return;
        }
        mine.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.cmp(&a.2)));
        let drop: Vec<usize> = mine.into_iter().skip(MAX_PER_REPO).map(|(i, ..)| i).collect();
        let mut index = 0;
        self.skills.retain(|_| {
            let keep = !drop.contains(&index);
            index += 1;
            keep
        });
    }

    /// The most relevant facts for `prompt`, as lines that fit in about 250 tokens.
    pub fn top_k(&self, repo_path: &Path, prompt: &str, k: usize) -> Vec<&MicroSkill> {
        let prompt_words = words(prompt);
        let mut scored: Vec<(f32, &MicroSkill)> = self
            .for_repo(repo_path)
            .into_iter()
            .map(|skill| {
                let overlap = words(&skill.trigger_context).iter().filter(|w| prompt_words.contains(w)).count() as f32;
                (overlap * 2.0 + skill.confidence + (skill.hit_count.min(10) as f32) * 0.05, skill)
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.updated_at.cmp(&a.1.updated_at)).then(a.1.id.cmp(&b.1.id)));
        let mut used = 0;
        let mut out = Vec::new();
        for (_, skill) in scored.into_iter().take(k) {
            used += skill.advice.chars().count() + 4;
            if used > INJECT_CHARS {
                break;
            }
            out.push(skill);
        }
        out
    }

    /// The injected lines for [`top_k`](Self::top_k), one per fact.
    pub fn render(&self, repo_path: &Path, prompt: &str, k: usize) -> Vec<String> {
        self.top_k(repo_path, prompt, k).into_iter().map(|s| format!("- [{:?}] {}", s.category, s.advice)).collect()
    }

    /// Distils a finished session: a command that failed and a later command with the same program
    /// that succeeded become one fact. Returns how many facts were stored.
    pub fn fold(&mut self, repo_path: &Path, outcomes: &[CommandOutcome], now: u64) -> usize {
        let mut stored = 0;
        for (i, failed) in outcomes.iter().enumerate().filter(|(_, o)| !o.ok) {
            let program = failed.cmd.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            let fix = outcomes[i + 1..].iter().find(|o| o.ok && o.cmd != failed.cmd && o.cmd.starts_with(&program));
            let Some(fix) = fix else { continue };
            let advice = format!("`{}` fails here; use `{}`", failed.cmd.trim(), fix.cmd.trim());
            if self.record(repo_path, SkillCategory::of_command(&failed.cmd), &program, &advice, now) {
                stored += 1;
            }
        }
        stored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "/work/app";

    fn repo() -> &'static Path {
        Path::new(REPO)
    }

    #[test]
    fn secrets_and_junk_are_refused() {
        let mut skills = MicroSkills::default();
        for bad in [
            "export API_KEY=abc then run",
            "use token ghp_abcdefghijklmnop1234",
            "Authorization: Bearer xyz",
            "key sk-ant-api03-abcdefghijklmnop",
            "hash 0123456789abcdef0123456789abcdef",
            "two\nlines",
            "",
        ] {
            assert!(!skills.record(repo(), SkillCategory::Env, "env", bad, 1), "{bad}");
        }
        assert!(skills.is_empty());
        assert!(skills.record(repo(), SkillCategory::Test, "cargo test", "use `cargo test -- --nocapture`", 1));
    }

    #[test]
    fn repeating_a_fact_raises_confidence_and_a_contradiction_retires_the_old_one() {
        let mut skills = MicroSkills::default();
        skills.record(repo(), SkillCategory::Env, "package manager", "use pnpm, not npm", 1);
        skills.record(repo(), SkillCategory::Env, "package manager", "use pnpm, not npm", 2);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills.for_repo(repo())[0].hit_count, 2);
        assert!(skills.for_repo(repo())[0].confidence > 0.5);

        // A different answer for the same trigger halves the old one and adds the new at 0.5.
        skills.record(repo(), SkillCategory::Env, "package manager", "use yarn", 3);
        assert_eq!(skills.len(), 2);
        skills.record(repo(), SkillCategory::Env, "package manager", "use bun", 4);
        skills.record(repo(), SkillCategory::Env, "package manager", "use bun", 5);
        let advices: Vec<&str> = skills.for_repo(repo()).iter().map(|s| s.advice.as_str()).collect();
        assert!(advices.contains(&"use bun") && !advices.contains(&"use yarn"), "{advices:?}");
    }

    #[test]
    fn contradict_drops_weak_facts() {
        let mut skills = MicroSkills::default();
        skills.record(repo(), SkillCategory::Env, "db", "test db is on port 5433", 1);
        let id = skills.for_repo(repo())[0].id.clone();
        skills.contradict(&id); // 0.5 -> 0.25: weak but kept
        assert_eq!(skills.len(), 1);
        skills.contradict(&id);
        assert_eq!(skills.len(), 0);
    }

    #[test]
    fn facts_are_scoped_per_repo_and_ranked_by_relevance() {
        let mut skills = MicroSkills::default();
        skills.record(repo(), SkillCategory::Test, "auth test failure", "use `make test-auth` with the mock keychain", 1);
        skills.record(repo(), SkillCategory::Build, "release build", "run `make tui-build`", 1);
        skills.record(Path::new("/other"), SkillCategory::Env, "auth test", "other repo fact", 1);
        let lines = skills.render(repo(), "the auth test is failing again", 1);
        assert_eq!(lines, ["- [Test] use `make test-auth` with the mock keychain"]);
        assert!(!skills.render(repo(), "anything", 5).iter().any(|l| l.contains("other repo")));
    }

    #[test]
    fn injection_stays_within_the_budget() {
        let mut skills = MicroSkills::default();
        for i in 0..30 {
            skills.record(repo(), SkillCategory::Env, &format!("topic {i}"), &format!("fact number {i}: {}", "x".repeat(150)), i);
        }
        let chars: usize = skills.render(repo(), "topic", 30).iter().map(|l| l.chars().count()).sum();
        assert!(chars <= INJECT_CHARS + 30 * 12, "{chars}");
    }

    #[test]
    fn fold_turns_fail_then_fix_into_a_fact() {
        let mut skills = MicroSkills::default();
        let outcomes = [
            CommandOutcome { cmd: "cargo test".into(), ok: false },
            CommandOutcome { cmd: "ls".into(), ok: true },
            CommandOutcome { cmd: "cargo test -- --nocapture".into(), ok: true },
            CommandOutcome { cmd: "npm install".into(), ok: false }, // never fixed: no fact
        ];
        assert_eq!(skills.fold(repo(), &outcomes, 9), 1);
        let skill = skills.for_repo(repo())[0];
        assert_eq!(skill.category, SkillCategory::Test);
        assert!(skill.advice.contains("cargo test -- --nocapture"));
    }

    #[test]
    fn store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("economizer/skills.json");
        let mut skills = MicroSkills::default();
        skills.record(repo(), SkillCategory::Lint, "lint", "run `cargo clippy --all-targets`", 1);
        skills.save(&file).unwrap();
        assert_eq!(MicroSkills::load(&file).len(), 1);
        assert_eq!(MicroSkills::load(&dir.path().join("missing.json")).len(), 0);
    }
}
