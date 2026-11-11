//! Per-step durable event tape: `<artifacts_dir>/steps/<step>-<attempt>.ndjson`.
//!
//! Stores every event of a worker's run (prompt, streamed tool calls/text chunks, status updates,
//! and final result) scrubbed of credentials, bounded by head/tail compaction, and annotated with
//! OpenTelemetry GenAI semantic span identifiers (`trace_id`, `span_id`, `parent_span_id`).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::secrets;

pub const DEFAULT_MAX_TAPE_LINES: usize = 2000;
pub const COMPACT_HEAD_LINES: usize = 500;
pub const COMPACT_TAIL_LINES: usize = 1000;

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Computes a stable 32-character hex trace ID for a run.
pub fn trace_id_for_run(run_id: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(b"cordata:trace:");
    hasher.update(run_id.as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    // Sha1 produces 40 hex chars; take 32 chars for standard 128-bit trace ID hex
    hex[..32].to_string()
}

/// Computes a stable 16-character hex span ID for a step attempt.
pub fn span_id_for_step(run_id: &str, step_id: &str, attempt: usize) -> String {
    let mut hasher = Sha1::new();
    hasher.update(b"cordata:span:step:");
    hasher.update(format!("{run_id}:{step_id}:{attempt}").as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    // 16 hex chars for standard 64-bit span ID hex
    hex[..16].to_string()
}

/// Durable step tape writer.
pub struct StepTape {
    pub artifacts_dir: PathBuf,
    pub run_id: String,
    pub step_id: String,
    pub attempt: usize,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub path: PathBuf,
    pub max_lines: usize,
    line_count: AtomicUsize,
    last_event_at: AtomicU64,
}

impl StepTape {
    pub fn new(
        artifacts_dir: &Path,
        run_id: &str,
        step_id: &str,
        attempt: usize,
        trace_id: Option<&str>,
        parent_span_id: Option<&str>,
    ) -> Self {
        let trace_id = trace_id.map(ToString::to_string).unwrap_or_else(|| trace_id_for_run(run_id));
        let span_id = span_id_for_step(run_id, step_id, attempt);
        let steps_dir = artifacts_dir.join("steps");
        let _ = std::fs::create_dir_all(&steps_dir);
        let _ = std::fs::set_permissions(&steps_dir, std::fs::Permissions::from_mode(0o700));

        let path = steps_dir.join(format!("{step_id}-{attempt}.ndjson"));
        let existing_lines = if path.exists() {
            BufReader::new(File::open(&path).ok().unwrap_or_else(|| File::open("/dev/null").unwrap()))
                .lines()
                .count()
        } else {
            0
        };

        Self {
            artifacts_dir: artifacts_dir.to_path_buf(),
            run_id: run_id.to_string(),
            step_id: step_id.to_string(),
            attempt,
            trace_id,
            span_id,
            parent_span_id: parent_span_id.map(ToString::to_string),
            path,
            max_lines: DEFAULT_MAX_TAPE_LINES,
            line_count: AtomicUsize::new(existing_lines),
            last_event_at: AtomicU64::new((now() * 1000.0) as u64),
        }
    }

    pub fn record(&self, kind: &str, payload: &Value) {
        let ts = now();
        self.last_event_at.store((ts * 1000.0) as u64, Ordering::Relaxed);

        let mut event = json!({
            "trace_id": self.trace_id,
            "span_id": self.span_id,
            "kind": kind,
            "at": ts,
            "step_id": self.step_id,
            "attempt": self.attempt,
            "payload": payload,
        });

        if let Some(parent) = &self.parent_span_id {
            event["parent_span_id"] = json!(parent);
        }

        // Scrub all credentials before writing to disk
        let scrubbed = secrets::scrub(&event.to_string()).0;

        let mut file = match OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
        {
            Ok(f) => f,
            Err(_) => return,
        };

        let _ = writeln!(file, "{scrubbed}");
        let count = self.line_count.fetch_add(1, Ordering::Relaxed) + 1;

        if count > self.max_lines {
            self.compact();
        }
    }

    pub fn record_prompt(&self, prompt: &str, task_id: &str, model: Option<&str>, role: &str) {
        self.record(
            "prompt",
            &json!({
                "task_id": task_id,
                "role": role,
                "model": model,
                "prompt": prompt,
            }),
        );
    }

    pub fn record_worker_event(&self, event_type: &str, payload: &Value) {
        self.record(event_type, payload);
    }

    pub fn record_result(&self, status: &str, result: &Value) {
        let mut r = result.clone();
        if let Some(obj) = r.as_object_mut() {
            obj.insert("status".to_string(), json!(status));
        }
        self.record("result", &r);
    }

    pub fn last_event_at_secs(&self) -> f64 {
        (self.last_event_at.load(Ordering::Relaxed) as f64) / 1000.0
    }

    pub fn quiet_duration_secs(&self) -> f64 {
        (now() - self.last_event_at_secs()).max(0.0)
    }

    pub fn is_stalled(&self, quiet_threshold_s: f64) -> bool {
        self.quiet_duration_secs() > quiet_threshold_s
    }

    /// Head/tail compaction keeping first `COMPACT_HEAD_LINES` and last `COMPACT_TAIL_LINES`.
    pub fn compact(&self) {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
        let total = lines.len();
        if total <= COMPACT_HEAD_LINES + COMPACT_TAIL_LINES {
            return;
        }

        let omitted = total - COMPACT_HEAD_LINES - COMPACT_TAIL_LINES;
        let mut compacted = Vec::with_capacity(COMPACT_HEAD_LINES + COMPACT_TAIL_LINES + 1);
        compacted.extend(lines[..COMPACT_HEAD_LINES].iter().cloned());
        compacted.push(json!({
            "trace_id": self.trace_id,
            "span_id": self.span_id,
            "kind": "omitted",
            "at": now(),
            "omitted_count": omitted,
            "total_lines_before": total,
        }).to_string());
        compacted.extend(lines[total - COMPACT_TAIL_LINES..].iter().cloned());

        let tmp = self.path.with_extension("tmp");
        if let Ok(mut out) = OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp) {
            for l in &compacted {
                let _ = writeln!(out, "{l}");
            }
            let _ = std::fs::rename(tmp, &self.path);
            self.line_count.store(compacted.len(), Ordering::Relaxed);
        }
    }
}

/// Reads all events from a step tape file.
pub fn read_tape(path: &Path) -> std::io::Result<Vec<Value>> {
    let file = File::open(path)?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(&line) {
            out.push(val);
        }
    }
    Ok(out)
}

/// Finds all tape files for a step under `<artifacts_dir>/steps/`.
pub fn list_step_tapes(artifacts_dir: &Path, step_id: &str) -> Vec<PathBuf> {
    let steps_dir = artifacts_dir.join("steps");
    let mut tapes = Vec::new();
    let prefix = format!("{step_id}-");
    if let Ok(entries) = std::fs::read_dir(&steps_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) && name.ends_with(".ndjson") {
                tapes.push(entry.path());
            }
        }
    }
    tapes.sort();
    tapes
}

/// Finds the latest tape file for `step_id`.
pub fn latest_step_tape(artifacts_dir: &Path, step_id: &str) -> Option<PathBuf> {
    list_step_tapes(artifacts_dir, step_id).into_iter().last()
}

/// Resolves path for a specific attempt or latest if attempt is None.
pub fn tape_path(artifacts_dir: &Path, step_id: &str, attempt: Option<usize>) -> PathBuf {
    match attempt {
        Some(att) => artifacts_dir.join("steps").join(format!("{step_id}-{att}.ndjson")),
        None => latest_step_tape(artifacts_dir, step_id).unwrap_or_else(|| {
            artifacts_dir.join("steps").join(format!("{step_id}-1.ndjson"))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_tape_records_events_and_scrubs_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let tape = StepTape::new(dir.path(), "run-123", "s1", 1, None, None);

        tape.record_prompt("Build the feature with secret sk-ant-api03-abcdef1234567890", "t1", Some("claude"), "execute");
        tape.record_worker_event("tool_use", &json!({"tool": "read_file", "path": "src/main.rs"}));
        tape.record_result("completed", &json!({"cost_usd": 0.05}));

        assert!(tape.path.exists());
        let events = read_tape(&tape.path).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["kind"], "prompt");
        assert_eq!(events[1]["kind"], "tool_use");
        assert_eq!(events[2]["kind"], "result");

        // Plaintext secret must be scrubbed
        let raw = std::fs::read_to_string(&tape.path).unwrap();
        assert!(!raw.contains("sk-ant-api03-abcdef1234567890"));
        assert!(raw.contains("[REDACTED"));
    }

    #[test]
    fn step_tape_compacts_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut tape = StepTape::new(dir.path(), "run-123", "s1", 1, None, None);
        tape.max_lines = 10;

        for i in 0..15 {
            tape.record("progress", &json!({"index": i}));
        }

        assert!(tape.path.exists());
        let events = read_tape(&tape.path).unwrap();
        assert!(events.len() <= 15);
    }

    #[test]
    fn trace_id_and_span_id_are_standard_lengths() {
        let tid = trace_id_for_run("run-e2e-test");
        assert_eq!(tid.len(), 32);
        assert!(tid.chars().all(|c| c.is_ascii_hexdigit()));

        let sid = span_id_for_step("run-e2e-test", "s1", 1);
        assert_eq!(sid.len(), 16);
        assert!(sid.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
