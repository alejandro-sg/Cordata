//! A run's control file: how a front door that did not start this process pauses or
//! cancels it. One JSON line per request appended to `<run>/control.ndjson`; the machine polls it at every safe point.
//!
//! A request with `"now": true` also asks to stop the worker call in flight; this binary treats it as an ordinary
//! request (honoured at the next safe point).

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Mutex;

use cordata_orchestrator::machine::{ControlRequest, RunControls};
use serde_json::Value;

pub struct FileControls {
    path: Option<PathBuf>,
    state: Mutex<(u64, Option<ControlRequest>)>,
}

impl FileControls {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path, state: Mutex::new((0, None)) }
    }

    fn poll(&self) {
        let Some(path) = &self.path else { return };
        let mut state = self.state.lock().unwrap();
        let Ok(size) = std::fs::metadata(path).map(|m| m.len()) else { return };
        if size <= state.0 {
            return;
        }
        let Ok(mut file) = std::fs::File::open(path) else { return };
        if file.seek(SeekFrom::Start(state.0)).is_err() {
            return;
        }
        let mut data = Vec::new();
        if file.read_to_end(&mut data).is_err() {
            return;
        }
        let complete = data.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        state.0 += complete as u64;
        for line in data[..complete].split(|b| *b == b'\n') {
            let Ok(cmd) = serde_json::from_slice::<Value>(line) else { continue };
            let kind = cmd.get("cmd").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "pause" | "cancel") {
                state.1 = Some(ControlRequest {
                    kind: kind.into(),
                    by: cmd.get("by").and_then(Value::as_str).unwrap_or("").into(),
                    restore: cmd.get("restore").and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
    }
}

impl RunControls for FileControls {
    fn pending(&self) -> Option<ControlRequest> {
        self.poll();
        self.state.lock().unwrap().1.clone()
    }

    fn clear(&self) {
        self.state.lock().unwrap().1 = None;
    }
}

/// Appends one request line to a run's control file (what `cordata pause|cancel` and front doors do).
pub fn request(run_dir: &std::path::Path, kind: &str, now: bool, restore: bool, by: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(run_dir)?;
    let mut out = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(run_dir.join("control.ndjson"))?;
    let at = cordata_checkpoints::now();
    writeln!(out, "{}", serde_json::json!({"cmd": kind, "now": now, "restore": restore, "by": by, "at": at}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_arrive_whole_lines_at_a_time_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let controls = FileControls::new(Some(dir.path().join("control.ndjson")));
        assert!(controls.pending().is_none(), "no file yet");
        request(dir.path(), "pause", false, false, "me").unwrap();
        let r = controls.pending().unwrap();
        assert_eq!((r.kind.as_str(), r.by.as_str(), r.restore), ("pause", "me", false));
        controls.clear();
        assert!(controls.pending().is_none(), "consumed");
        // A half-written line is not read until it is complete.
        std::fs::OpenOptions::new().append(true).open(dir.path().join("control.ndjson")).unwrap().write_all_at_end(b"{\"cmd\":\"cancel\",\"restore\":true");
        assert!(controls.pending().is_none());
        std::fs::OpenOptions::new().append(true).open(dir.path().join("control.ndjson")).unwrap().write_all_at_end(b"}\ngarbage\n");
        let r = controls.pending().unwrap();
        assert_eq!((r.kind.as_str(), r.restore), ("cancel", true));
        assert!(FileControls::new(None).pending().is_none());
    }

    trait AppendExt {
        fn write_all_at_end(self, bytes: &[u8]);
    }
    impl AppendExt for std::fs::File {
        fn write_all_at_end(mut self, bytes: &[u8]) {
            use std::io::Write;
            self.write_all(bytes).unwrap();
        }
    }
}
