//! Cache-aligned prompt layout. Prompt caching needs a byte-identical prefix, so everything that
//! changes goes last:
//!
//! 1. static prefix (identity, guardrails; the tool list is sent by the provider right after it)
//! 2. session-stable micro-skills
//! 3. conversation history (append only; owned by the caller)
//! 4. ephemeral tail (clock, uncommitted changes, running tasks)

use std::sync::LazyLock;

use ahash::RandomState;
use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Slots 1 and 2, as the system prompt.
    pub system: String,
    /// Slot 4, to append to the final message of a request and never keep in history. Empty when there is none.
    pub tail: String,
}

static VOLATILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}|\b\d{1,2}:\d{2}:\d{2}\b|\b[0-9a-f]{40}\b|\b[0-9a-f]{7,12}\b.*\bHEAD\b|\bpid[ =:]+\d+")
        .expect("static regex")
});

/// Parts of `text` that would change between requests and so break the cached prefix.
pub fn volatile_findings(text: &str) -> Vec<String> {
    VOLATILE.find_iter(text).map(|m| m.as_str().to_owned()).collect()
}

/// Builds the layout. `volatile` lines (clock, git status, running tasks) go to the tail.
pub fn layout(static_prefix: &str, skills: &[String], volatile: &[String]) -> Layout {
    let mut system = static_prefix.trim_end().to_owned();
    if !skills.is_empty() {
        system.push_str("\n\nKnown facts about this repository (learned in earlier sessions):\n");
        system.push_str(&skills.join("\n"));
    }
    let tail = if volatile.is_empty() { String::new() } else { format!("<context>\n{}\n</context>", volatile.join("\n")) };
    Layout { system, tail }
}

/// Watches the prefix across requests and counts how often it changed.
#[derive(Debug)]
pub struct PrefixTracker {
    hasher: RandomState,
    last: Option<u64>,
    pub requests: usize,
    pub breaks: usize,
}

impl Default for PrefixTracker {
    fn default() -> Self {
        Self { hasher: RandomState::with_seeds(1, 2, 3, 4), last: None, requests: 0, breaks: 0 }
    }
}

impl PrefixTracker {
    /// Records the prefix of one request (system prompt plus serialized tools). Returns whether it matched the previous one.
    pub fn observe(&mut self, system: &str, tools: &str) -> bool {
        let digest = self.hasher.hash_one((system, tools));
        self.requests += 1;
        let stable = self.last.is_none_or(|last| last == digest);
        if !stable {
            self.breaks += 1;
        }
        self.last = Some(digest);
        stable
    }

    /// Share of requests after the first that reused the prefix.
    pub fn hit_rate(&self) -> f64 {
        if self.requests <= 1 { 1.0 } else { 1.0 - self.breaks as f64 / (self.requests - 1) as f64 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volatile_data_goes_to_the_tail_and_the_system_prompt_is_stable() {
        let skills = vec!["- [Test] use make test".to_owned()];
        let a = layout("You are Cordata.", &skills, &["time: 10:00:01".into(), "git: 2 files changed".into()]);
        let b = layout("You are Cordata.", &skills, &["time: 10:05:44".into(), "git: 3 files changed".into()]);
        assert_eq!(a.system, b.system);
        assert_ne!(a.tail, b.tail);
        assert!(a.system.contains("use make test") && volatile_findings(&a.system).is_empty());
        assert!(a.tail.starts_with("<context>"));
        assert_eq!(layout("p", &[], &[]).tail, "");
    }

    #[test]
    fn findings_flag_prefix_jitter() {
        assert!(!volatile_findings("now it is 2026-11-01 10:00 sharp").is_empty());
        assert!(!volatile_findings("at 10:00:01 we started").is_empty());
        assert!(!volatile_findings("commit 0123456789abcdef0123456789abcdef01234567").is_empty());
        assert!(volatile_findings("You are Cordata, a coding agent.").is_empty());
    }

    #[test]
    fn a_stable_prefix_scores_over_90_percent_and_a_jittery_one_does_not() {
        let mut stable = PrefixTracker::default();
        let mut jittery = PrefixTracker::default();
        for round in 0..20 {
            stable.observe("sys", "tools");
            jittery.observe(&format!("sys {round}"), "tools");
        }
        assert!(stable.hit_rate() > 0.9 && stable.breaks == 0);
        assert!(jittery.hit_rate() < 0.1);
    }

    #[test]
    fn a_tool_list_change_counts_as_a_break() {
        let mut tracker = PrefixTracker::default();
        assert!(tracker.observe("s", "t1"));
        assert!(!tracker.observe("s", "t2"));
        assert_eq!(tracker.breaks, 1);
    }
}
