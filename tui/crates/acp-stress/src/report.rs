//! One JSONL record per prompt-turn plus an end-of-run summary.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ToolRecord {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionRecord {
    pub tool: String,
    pub decision: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct TurnRecord {
    pub scenario: String,
    pub session_id: Option<String>,
    /// `None` for a session-level failure before any prompt ran.
    pub prompt_index: Option<usize>,
    pub prompt: String,
    pub mode: Option<String>,
    pub started_unix_ms: u64,
    pub stop_reason: Option<String>,
    /// First streamed model output (reply or reasoning) after the prompt was sent.
    pub ttft_ms: Option<u64>,
    pub latency_ms: Option<u64>,
    pub tool_calls: Vec<ToolRecord>,
    pub permission_requests: usize,
    pub permissions: Vec<PermissionRecord>,
    pub response_chars: usize,
    pub error: Option<String>,
    /// Best effort: the text a `context_usage` tool call returned, if the model called it.
    pub context_usage: Option<String>,
}

impl TurnRecord {
    /// Healthy means a completed turn that ended normally.
    pub fn is_ok(&self) -> bool {
        self.error.is_none() && self.stop_reason.as_deref() == Some("end_turn")
    }
}

/// Serialises concurrent scenarios' records into one file, a line at a time.
pub struct Sink {
    file: Option<Mutex<BufWriter<File>>>,
}

impl Sink {
    pub fn create(path: Option<&Path>) -> Result<Self> {
        let file = match path {
            Some(path) => Some(Mutex::new(BufWriter::new(File::create(path)?))),
            None => None,
        };
        Ok(Self { file })
    }

    pub fn write(&self, record: &TurnRecord) {
        let Some(file) = &self.file else { return };
        if let (Ok(mut file), Ok(line)) = (file.lock(), serde_json::to_string(record)) {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

/// Nearest-rank percentile of an already-sorted slice.
pub fn percentile(sorted: &[u64], pct: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

pub struct ScenarioOutcome {
    pub name: String,
    pub error: Option<String>,
    pub turns: Vec<TurnRecord>,
}

pub struct Summary {
    pub text: String,
    pub failed: bool,
}

pub fn summarize(outcomes: &[ScenarioOutcome]) -> Summary {
    let turns: Vec<&TurnRecord> = outcomes.iter().flat_map(|o| o.turns.iter()).collect();
    let errored_scenarios = outcomes.iter().filter(|o| o.error.is_some()).count();
    let unhealthy_turns = turns.iter().filter(|t| !t.is_ok()).count();

    let mut stop_reasons: BTreeMap<String, usize> = BTreeMap::new();
    for turn in &turns {
        *stop_reasons
            .entry(turn.stop_reason.clone().unwrap_or_else(|| "(none)".into()))
            .or_default() += 1;
    }

    let mut latencies: Vec<u64> = turns.iter().filter_map(|t| t.latency_ms).collect();
    let mut ttfts: Vec<u64> = turns.iter().filter_map(|t| t.ttft_ms).collect();
    latencies.sort_unstable();
    ttfts.sort_unstable();

    let mut tools: BTreeMap<String, usize> = BTreeMap::new();
    for turn in &turns {
        for call in &turn.tool_calls {
            *tools.entry(call.name.clone()).or_default() += 1;
        }
    }
    let most_common_tool = tools
        .iter()
        .max_by_key(|(name, count)| (**count, std::cmp::Reverse((*name).clone())));
    let permission_requests: usize = turns.iter().map(|t| t.permission_requests).sum();

    let fmt_ms = |value: Option<u64>| value.map_or("-".to_owned(), |ms| format!("{ms}ms"));
    let mut text = String::new();
    text.push_str(&format!(
        "scenarios: {} ({} errored)   turns: {} ({} unhealthy)\n",
        outcomes.len(),
        errored_scenarios,
        turns.len(),
        unhealthy_turns
    ));
    text.push_str(&format!(
        "stop reasons: {}\n",
        stop_reasons
            .iter()
            .map(|(reason, n)| format!("{reason}={n}"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    text.push_str(&format!(
        "latency  p50 {}  p95 {}  max {}\n",
        fmt_ms(percentile(&latencies, 50.0)),
        fmt_ms(percentile(&latencies, 95.0)),
        fmt_ms(latencies.last().copied())
    ));
    text.push_str(&format!(
        "ttft     p50 {}  p95 {}\n",
        fmt_ms(percentile(&ttfts, 50.0)),
        fmt_ms(percentile(&ttfts, 95.0))
    ));
    text.push_str(&format!(
        "tool calls: {}   most common: {}   permission requests: {}\n",
        tools.values().sum::<usize>(),
        most_common_tool.map_or("-".to_owned(), |(name, count)| format!("{name} ({count})")),
        permission_requests
    ));

    // Group identical problems across `#n` repeats so 100 copies of one
    // failure read as one line with a count, not 100 lines.
    let base = |name: &str| name.split('#').next().unwrap_or(name).to_owned();
    let mut problems: BTreeMap<String, usize> = BTreeMap::new();
    for outcome in outcomes {
        if let Some(error) = &outcome.error {
            *problems
                .entry(format!("{}: {error}", base(&outcome.name)))
                .or_default() += 1;
        }
    }
    for outcome in outcomes {
        for turn in &outcome.turns {
            let skipped = turn
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("skipped:"));
            let message = match (&turn.error, turn.stop_reason.as_deref()) {
                // Already reported once as the scenario-level failure.
                _ if skipped || (outcome.error.is_some() && turn.error == outcome.error) => {
                    continue;
                }
                (Some(error), _) => format!(
                    "{} turn {}: {error}",
                    base(&turn.scenario),
                    turn.prompt_index.map_or("-".into(), |i| i.to_string())
                ),
                (None, Some("end_turn")) => continue,
                (None, reason) => format!(
                    "{} turn {}: unexpected stop reason {}",
                    base(&turn.scenario),
                    turn.prompt_index.map_or("-".into(), |i| i.to_string()),
                    reason.unwrap_or("(none)")
                ),
            };
            *problems.entry(message).or_default() += 1;
        }
    }
    if !problems.is_empty() {
        text.push_str("problems:\n");
        for (problem, count) in problems.iter().take(15) {
            let suffix = if *count > 1 {
                format!(" (x{count})")
            } else {
                String::new()
            };
            text.push_str(&format!("  - {problem}{suffix}\n"));
        }
        if problems.len() > 15 {
            text.push_str(&format!(
                "  … and {} more kinds (see the JSONL)\n",
                problems.len() - 15
            ));
        }
    }

    Summary {
        text,
        failed: errored_scenarios > 0 || unhealthy_turns > 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(
        scenario: &str,
        stop: Option<&str>,
        latency: Option<u64>,
        error: Option<&str>,
    ) -> TurnRecord {
        TurnRecord {
            scenario: scenario.into(),
            prompt_index: Some(0),
            stop_reason: stop.map(Into::into),
            latency_ms: latency,
            error: error.map(Into::into),
            tool_calls: vec![ToolRecord {
                name: "shell".into(),
                status: "completed".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let data = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&data, 50.0), Some(50));
        assert_eq!(percentile(&data, 95.0), Some(100));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[7], 95.0), Some(7));
    }

    #[test]
    fn summary_flags_errors_and_unexpected_stop_reasons() {
        let healthy = ScenarioOutcome {
            name: "a".into(),
            error: None,
            turns: vec![turn("a", Some("end_turn"), Some(100), None)],
        };
        assert!(!summarize(&[healthy]).failed);

        let refusal = ScenarioOutcome {
            name: "b".into(),
            error: None,
            turns: vec![turn("b", Some("refusal"), Some(5), None)],
        };
        let summary = summarize(&[refusal]);
        assert!(summary.failed && summary.text.contains("unexpected stop reason refusal"));

        let broken = ScenarioOutcome {
            name: "c".into(),
            error: Some("boom".into()),
            turns: vec![turn("c", None, None, Some("boom"))],
        };
        let summary = summarize(&[broken]);
        assert!(
            summary.failed
                && summary.text.contains("c: boom")
                && summary.text.contains("most common: shell (1)")
        );
    }

    #[test]
    fn turn_errors_in_otherwise_live_sessions_are_listed_and_grouped() {
        let mk = |name: &str| ScenarioOutcome {
            name: name.into(),
            error: None,
            turns: vec![turn(name, None, Some(10), Some("Connection error."))],
        };
        let summary = summarize(&[mk("p#1"), mk("p#2"), mk("p#3")]);
        assert!(summary.failed);
        assert!(
            summary.text.contains("p turn 0: Connection error. (x3)"),
            "{}",
            summary.text
        );
    }
}
