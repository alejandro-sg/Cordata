//! How the agent is performing, measured where the person sits: time to the first word, time to the whole answer,
//! tokens in and out, how full the context is. Fed by the reducer (`App::on_event`) from acp-core's events; nothing
//! here reads a clock except through the `Instant`s it is handed, so tests drive it with made-up times.

use std::collections::VecDeque;
use std::time::Instant;

use acp_core::TokenUsage;

/// Finished turns kept for the trend line.
pub const KEPT: usize = 48;

/// One finished turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// Seconds from sending the prompt to the first streamed text, thought or tool call.
    pub first: Option<f64>,
    /// Seconds from sending the prompt to the end of the turn (approvals included: that is what the person waited).
    pub total: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cached: u64,
    /// Model requests the turn made (one per tool round).
    pub requests: u32,
    pub tools: usize,
    pub ok: bool,
}

impl Turn {
    /// Output tokens per second once output started; `None` when too short to say.
    pub fn rate(&self) -> Option<f64> {
        let streaming = self.total - self.first.unwrap_or(0.0);
        (streaming > 0.25 && self.tokens_out > 0).then(|| self.tokens_out as f64 / streaming)
    }
}

/// The turn in flight.
#[derive(Debug, Clone)]
struct Live {
    started: Instant,
    first: Option<Instant>,
    usage: TokenUsage,
    requests: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Telemetry {
    pub turns: VecDeque<Turn>,
    live: Option<Live>,
    /// The prompt of the newest request plus its answer: what the next request starts from.
    pub context_used: u64,
    pub session_in: u64,
    pub session_out: u64,
    pub session_cached: u64,
}

impl Telemetry {
    pub fn begin(&mut self, at: Instant) {
        self.live = Some(Live { started: at, first: None, usage: TokenUsage::default(), requests: 0 });
    }

    /// Anything streamed back: text, a thought, a tool call. Only the first one counts.
    pub fn output(&mut self, at: Instant) {
        if let Some(live) = &mut self.live
            && live.first.is_none()
        {
            live.first = Some(at);
        }
    }

    pub fn usage(&mut self, usage: TokenUsage) {
        self.context_used = usage.input + usage.output;
        self.session_in += usage.input;
        self.session_out += usage.output;
        self.session_cached += usage.cached;
        if let Some(live) = &mut self.live {
            live.usage.input += usage.input;
            live.usage.output += usage.output;
            live.usage.cached += usage.cached;
            live.requests += 1;
        }
    }

    pub fn end(&mut self, at: Instant, tools: usize, ok: bool) -> Option<&Turn> {
        let live = self.live.take()?;
        let secs = |i: Instant| i.saturating_duration_since(live.started).as_secs_f64();
        if self.turns.len() == KEPT {
            self.turns.pop_front();
        }
        self.turns.push_back(Turn {
            first: live.first.map(secs),
            total: secs(at),
            tokens_in: live.usage.input,
            tokens_out: live.usage.output,
            cached: live.usage.cached,
            requests: live.requests,
            tools,
            ok,
        });
        self.turns.back()
    }

    /// Seconds the turn in flight has run, and whether anything came back yet.
    pub fn running(&self, now: Instant) -> Option<(f64, bool)> {
        self.live.as_ref().map(|l| (now.saturating_duration_since(l.started).as_secs_f64(), l.first.is_some()))
    }

    /// Seconds to the first word of the turn in flight, once there is one.
    pub fn live_first(&self) -> Option<f64> {
        self.live.as_ref().and_then(|l| l.first.map(|f| f.saturating_duration_since(l.started).as_secs_f64()))
    }

    pub fn last(&self) -> Option<&Turn> {
        self.turns.back()
    }

    /// The median time to first output over the kept turns.
    pub fn median_first(&self) -> Option<f64> {
        median(self.turns.iter().filter_map(|t| t.first).collect())
    }

    pub fn median_total(&self) -> Option<f64> {
        median(self.turns.iter().map(|t| t.total).collect())
    }

    /// The share of input tokens served from the prompt cache this session.
    pub fn cache_share(&self) -> Option<f64> {
        (self.session_in > 0).then(|| self.session_cached as f64 / self.session_in as f64)
    }

    /// How full the context is, for a model whose window is known.
    pub fn context_share(&self, model: &str) -> Option<f64> {
        let window = crate::models::window(model)?;
        Some((self.context_used as f64 / window as f64).min(1.0))
    }
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) { (values[mid - 1] + values[mid]) / 2.0 } else { values[mid] })
}

/// `1.2s`, `48s`, `3m 05s`.
pub fn secs(value: f64) -> String {
    if value < 10.0 {
        format!("{value:.1}s")
    } else if value < 60.0 {
        format!("{value:.0}s")
    } else {
        let whole = value.round() as u64;
        format!("{}m {:02}s", whole / 60, whole % 60)
    }
}

/// `950`, `12.4k`, `1.20M`.
pub fn tokens(value: u64) -> String {
    match value {
        0..1_000 => value.to_string(),
        1_000..1_000_000 => format!("{:.1}k", value as f64 / 1e3),
        _ => format!("{:.2}M", value as f64 / 1e6),
    }
}

/// Composition of the conversation's visible context across four semantic buckets, plus offloaded tool outputs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextBreakdown {
    pub instructions_bytes: usize,
    pub tools_bytes: usize,
    pub diffs_bytes: usize,
    pub dialogue_bytes: usize,
    pub offloaded_count: usize,
}

impl ContextBreakdown {
    pub fn total_bytes(&self) -> usize {
        self.instructions_bytes + self.tools_bytes + self.diffs_bytes + self.dialogue_bytes
    }

    /// Four `(label, share_0_to_1)` entries; sums to `1.0` when `total_bytes() > 0`.
    pub fn shares(&self) -> [(&'static str, f64); 4] {
        let total = self.total_bytes();
        if total == 0 {
            return [("Instructions", 0.0), ("Tool output", 0.0), ("Diffs", 0.0), ("Dialogue", 0.0)];
        }
        let denom = total as f64;
        [
            ("Instructions", self.instructions_bytes as f64 / denom),
            ("Tool output", self.tools_bytes as f64 / denom),
            ("Diffs", self.diffs_bytes as f64 / denom),
            ("Dialogue", self.dialogue_bytes as f64 / denom),
        ]
    }
}

pub fn context_breakdown(messages: &[crate::app::Message]) -> ContextBreakdown {
    use crate::app::Message;
    let mut b = ContextBreakdown::default();
    for msg in messages {
        match msg {
            Message::Info(text) => {
                if crate::desk::is_session_note(text) || text.starts_with("attached ") {
                    b.instructions_bytes += text.len();
                } else {
                    b.tools_bytes += text.len();
                }
            }
            Message::Tool(card) => {
                let raw_len = card.raw_input.as_ref().map_or(0, |v| v.to_string().len());
                b.tools_bytes += card.content.len() + raw_len;
                for d in &card.diffs {
                    b.diffs_bytes += d.new_text.len() + d.old_text.as_ref().map_or(0, String::len);
                }
                if card.content.contains(".cordata/offloaded") || card.content.contains("offloaded to ") {
                    b.offloaded_count += 1;
                }
            }
            Message::Diff(diff) => b.diffs_bytes += diff.len(),
            Message::User(t) | Message::Agent(t) | Message::Thought(t) => b.dialogue_bytes += t.len(),
            Message::Welcome(_) | Message::Divider(_) | Message::Error(_) | Message::Help => {}
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_turn_records_first_word_total_and_tokens() {
        let t0 = Instant::now();
        let mut tm = Telemetry::default();
        tm.begin(t0);
        tm.output(t0 + Duration::from_millis(1500));
        tm.output(t0 + Duration::from_millis(1900));
        tm.usage(TokenUsage { input: 10_000, output: 200, cached: 8_000 });
        tm.usage(TokenUsage { input: 10_400, output: 300, cached: 10_000 });
        let turn = tm.end(t0 + Duration::from_secs(6), 2, true).unwrap().clone();
        assert_eq!(turn.first, Some(1.5), "the first output counts, not the last");
        assert_eq!((turn.total, turn.tokens_in, turn.tokens_out, turn.requests), (6.0, 20_400, 500, 2));
        assert_eq!(tm.context_used, 10_700, "the newest request is the context");
        assert_eq!(turn.rate().map(|r| r.round()), Some(111.0));
        assert!((tm.cache_share().unwrap() - 18_000.0 / 20_400.0).abs() < 1e-9);
    }

    #[test]
    fn medians_and_the_kept_window() {
        let t0 = Instant::now();
        let mut tm = Telemetry::default();
        for n in 1..=(KEPT as u64 + 2) {
            tm.begin(t0);
            tm.output(t0 + Duration::from_secs(1));
            tm.end(t0 + Duration::from_secs(n), 0, true);
        }
        assert_eq!(tm.turns.len(), KEPT);
        assert_eq!(tm.median_first(), Some(1.0));
        assert_eq!(tm.median_total(), Some(26.5));
        assert!(tm.end(t0, 0, true).is_none(), "no turn in flight, nothing recorded");
    }

    #[test]
    fn numbers_read_short() {
        assert_eq!((secs(1.24), secs(48.2), secs(185.0)), ("1.2s".into(), "48s".into(), "3m 05s".into()));
        assert_eq!((tokens(950), tokens(12_400), tokens(1_200_000)), ("950".into(), "12.4k".into(), "1.20M".into()));
    }

    #[test]
    fn context_breakdown_separates_instructions_tools_diffs_and_offloads() {
        use crate::app::{Message, ToolCard};
        use acp_core::{FileDiff, ToolKind, ToolStatus};
        let msgs = vec![
            Message::Info("instructions: AGENTS.md loaded".into()),
            Message::User("refactor parser".into()),
            Message::Tool(ToolCard {
                id: "1".into(),
                name: "run_command".into(),
                kind: ToolKind::Execute,
                status: ToolStatus::Completed,
                raw_input: None,
                content: "output offloaded to ~/.cordata/offloaded/1.txt".into(),
                diffs: vec![FileDiff { path: "src/lib.rs".into(), old_text: Some("old".into()), new_text: "new".into() }],
            }),
            Message::Agent("done".into()),
        ];
        let b = context_breakdown(&msgs);
        assert!(b.instructions_bytes > 0 && b.tools_bytes > 0 && b.diffs_bytes == 6 && b.dialogue_bytes > 0);
        assert_eq!(b.offloaded_count, 1);
        let sum: f64 = b.shares().iter().map(|(_, s)| *s).sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }
}
