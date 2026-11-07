//! Live event stream coalescing and tool update throttling inspired by T3 Code.
//!
//! High-frequency token streaming and progress updates can overwhelm UI renderers and IPC
//! transports with thousands of tiny events per second. The coalescer provides:
//!
//! 1. [`ToolEmissionDecider`]: Decides whether an in-progress tool update has grown meaningfully
//!    or reached a coalesce ceiling before emitting an update event.
//! 2. [`StreamCoalescer`]: Buffers high-speed character/token deltas within a sliding window,
//!    flushing consolidated text on boundaries (tool start, approvals, turn end) or buffer limits.

use std::collections::HashMap;

/// Minimum characters an in-progress tool output must grow before a progress event is emitted.
pub const DEFAULT_TOOL_UPDATE_MIN_GROWTH_CHARS: usize = 128;

/// Maximum consecutive skipped updates before forcing an emission.
pub const DEFAULT_TOOL_UPDATE_COALESCE_LIMIT: usize = 16;

/// Default maximum buffer size for live stream text accumulation (8 MiB).
pub const DEFAULT_LIVE_STREAM_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Default maximum number of pending chunks before forced consolidation.
pub const DEFAULT_LIVE_STREAM_MAX_CHUNKS: usize = 1_000;

#[derive(Debug, Clone)]
struct ToolTrackedState {
    status: String,
    title: Option<String>,
    last_emitted_len: usize,
    last_hash: u64,
    skipped_since_emit: usize,
}

/// Throttles progressive tool execution updates to prevent UI jitter and channel flood.
#[derive(Debug, Clone)]
pub struct ToolEmissionDecider {
    min_growth_chars: usize,
    coalesce_limit: usize,
    tracked: HashMap<String, ToolTrackedState>,
}

impl Default for ToolEmissionDecider {
    fn default() -> Self {
        Self::new(DEFAULT_TOOL_UPDATE_MIN_GROWTH_CHARS, DEFAULT_TOOL_UPDATE_COALESCE_LIMIT)
    }
}

impl ToolEmissionDecider {
    pub fn new(min_growth_chars: usize, coalesce_limit: usize) -> Self {
        Self {
            min_growth_chars,
            coalesce_limit,
            tracked: HashMap::new(),
        }
    }

    /// Evaluates whether an update to `tool_id` should be emitted to subscribers.
    ///
    /// - Terminal statuses (`completed`, `failed`) ALWAYS emit immediately.
    /// - Status changes or title changes ALWAYS emit immediately.
    /// - Unchanged output is suppressed.
    /// - In-progress output emits when growth >= `min_growth_chars` or when `coalesce_limit` is reached.
    pub fn decide(
        &mut self,
        tool_id: &str,
        status: &str,
        title: Option<&str>,
        content: &str,
    ) -> bool {
        let is_terminal = matches!(status, "completed" | "failed");
        let content_len = content.chars().count();
        let content_hash = {
            use std::hash::{Hash, Hasher};
            let mut hasher = ahash::AHasher::default();
            content.hash(&mut hasher);
            hasher.finish()
        };

        if is_terminal {
            self.tracked.remove(tool_id);
            return true;
        }

        let entry = match self.tracked.get_mut(tool_id) {
            Some(existing) => existing,
            None => {
                self.tracked.insert(
                    tool_id.to_owned(),
                    ToolTrackedState {
                        status: status.to_owned(),
                        title: title.map(str::to_owned),
                        last_emitted_len: content_len,
                        last_hash: content_hash,
                        skipped_since_emit: 0,
                    },
                );
                return true;
            }
        };

        // Status or title change always forces emission
        if entry.status != status || entry.title.as_deref() != title {
            entry.status = status.to_owned();
            entry.title = title.map(str::to_owned);
            entry.last_emitted_len = content_len;
            entry.last_hash = content_hash;
            entry.skipped_since_emit = 0;
            return true;
        }

        // Exact content match: skip without counting toward growth
        if entry.last_hash == content_hash {
            return false;
        }

        let growth = content_len.saturating_sub(entry.last_emitted_len);
        let grew_meaningfully = growth >= self.min_growth_chars;
        let reached_limit = entry.skipped_since_emit + 1 >= self.coalesce_limit;

        if grew_meaningfully || reached_limit {
            entry.last_emitted_len = content_len;
            entry.last_hash = content_hash;
            entry.skipped_since_emit = 0;
            true
        } else {
            entry.skipped_since_emit += 1;
            entry.last_hash = content_hash;
            false
        }
    }

    /// Clears tracking for a tool call.
    pub fn forget(&mut self, tool_id: &str) {
        self.tracked.remove(tool_id);
    }
}

/// Accumulates streaming deltas (text or thoughts) to emit batched chunks, bounded by memory.
#[derive(Debug, Clone)]
pub struct StreamCoalescer {
    buffered_text: String,
    buffered_thought: String,
    text_chunks: usize,
    thought_chunks: usize,
    max_bytes: usize,
    max_chunks: usize,
}

impl Default for StreamCoalescer {
    fn default() -> Self {
        Self::new(DEFAULT_LIVE_STREAM_MAX_BYTES, DEFAULT_LIVE_STREAM_MAX_CHUNKS)
    }
}

impl StreamCoalescer {
    pub fn new(max_bytes: usize, max_chunks: usize) -> Self {
        Self {
            buffered_text: String::new(),
            buffered_thought: String::new(),
            text_chunks: 0,
            thought_chunks: 0,
            max_bytes,
            max_chunks,
        }
    }

    /// Pushes an incremental assistant text chunk.
    /// Returns `Some(consolidated_text)` if the buffer reached capacity and required early flush.
    pub fn push_text(&mut self, chunk: &str) -> Option<String> {
        self.buffered_text.push_str(chunk);
        self.text_chunks += 1;

        if self.buffered_text.len() >= self.max_bytes || self.text_chunks >= self.max_chunks {
            self.flush_text()
        } else {
            None
        }
    }

    /// Pushes an incremental agent reasoning/thought chunk.
    /// Returns `Some(consolidated_thought)` if the buffer reached capacity.
    pub fn push_thought(&mut self, chunk: &str) -> Option<String> {
        self.buffered_thought.push_str(chunk);
        self.thought_chunks += 1;

        if self.buffered_thought.len() >= self.max_bytes || self.thought_chunks >= self.max_chunks {
            self.flush_thought()
        } else {
            None
        }
    }

    /// Flushes any pending text.
    pub fn flush_text(&mut self) -> Option<String> {
        if self.buffered_text.is_empty() {
            None
        } else {
            self.text_chunks = 0;
            Some(std::mem::take(&mut self.buffered_text))
        }
    }

    /// Flushes any pending thoughts.
    pub fn flush_thought(&mut self) -> Option<String> {
        if self.buffered_thought.is_empty() {
            None
        } else {
            self.thought_chunks = 0;
            Some(std::mem::take(&mut self.buffered_thought))
        }
    }

    /// Flushes all pending text and thought buffers on boundary events.
    pub fn flush_all(&mut self) -> (Option<String>, Option<String>) {
        (self.flush_text(), self.flush_thought())
    }

    pub fn is_empty(&self) -> bool {
        self.buffered_text.is_empty() && self.buffered_thought.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decider_emits_on_first_seen_and_terminal() {
        let mut decider = ToolEmissionDecider::new(100, 5);

        // First call emits
        assert!(decider.decide("t1", "in_progress", Some("Building"), "compiling..."));

        // Identical call does not emit
        assert!(!decider.decide("t1", "in_progress", Some("Building"), "compiling..."));

        // Status change emits
        assert!(decider.decide("t1", "in_progress", Some("Running tests"), "compiling..."));

        // Terminal status emits immediately
        assert!(decider.decide("t1", "completed", Some("Running tests"), "done!"));
    }

    #[test]
    fn decider_throttles_small_growths_until_limit_or_threshold() {
        let mut decider = ToolEmissionDecider::new(50, 3);

        // Start
        assert!(decider.decide("t2", "in_progress", None, "start"));

        // +10 chars: skip 1
        assert!(!decider.decide("t2", "in_progress", None, "start0123456789"));
        // +10 chars: skip 2
        assert!(!decider.decide("t2", "in_progress", None, "start0123456789abcdefghij"));
        // 3rd skip hits coalesce_limit -> forced emit!
        assert!(decider.decide("t2", "in_progress", None, "start0123456789abcdefghij1234567890"));

        // Meaningful jump (> 50 chars) emits immediately
        let big_chunk = "start0123456789abcdefghij1234567890".to_owned() + &"x".repeat(60);
        assert!(decider.decide("t2", "in_progress", None, &big_chunk));
    }

    #[test]
    fn stream_coalescer_batches_and_flushes() {
        let mut coalescer = StreamCoalescer::new(1024, 3);

        assert_eq!(coalescer.push_text("Hello "), None);
        assert_eq!(coalescer.push_text("world"), None);
        // 3rd chunk hits max_chunks -> flushes consolidated
        assert_eq!(coalescer.push_text("!"), Some("Hello world!".to_owned()));

        assert!(coalescer.is_empty());

        coalescer.push_thought("Thinking step 1... ");
        let (text, thought) = coalescer.flush_all();
        assert_eq!(text, None);
        assert_eq!(thought, Some("Thinking step 1... ".to_owned()));
        assert!(coalescer.is_empty());
    }
}
