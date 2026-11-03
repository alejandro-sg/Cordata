//! `AgyWorker`: Google Antigravity's CLI (`agy`) as an orchestrator worker — a second
//! quota pool, including Claude models served by Antigravity (`claude-sonnet-5-5-high`).
//!
//! The headless surface, as observed (print mode, `stream-json`): input is `-p=` plus `--input-format stream-json`
//! reading one NDJSON message from stdin; output is `init`, `step_update` events and a final `result`. Headless mode
//! cannot prompt, so anything needing a grant is auto-denied and listed in `denied_actions`: plan/audit run in the
//! default mode (no edits, no commands), execute/harden in `accept-edits` (edits, still no commands). No dollar
//! figure — tokens only; the ledger meters this pool in tokens and requests.

use std::sync::Arc;

use serde_json::{Value, json};

use super::cli::{CliWorker, Dialect, LineParser, ParserState};
use super::vault::Egress;
use crate::worker::{TaskPacket, WorkerEvent};

pub const BINARY_ENV: &str = "CORDATA_AGY_BIN";

pub const AGY_MODELS: [&str; 18] = [
    "gemini-3.8-flash-low", "gemini-3.8-flash-medium", "gemini-3.8-flash-high", "gemini-3.7-flash-low", "gemini-3.7-flash-medium", "gemini-3.7-flash-high",
    "gemini-3.6-flash-low", "gemini-3.6-flash-medium", "gemini-3.6-flash-high", "gemini-3.1-pro-low", "gemini-3.1-pro-high", "claude-opus-5-5-low",
    "claude-opus-5-5-medium", "claude-opus-5-5-high", "claude-sonnet-5-5-low", "claude-sonnet-5-5-medium", "claude-sonnet-5-5-high", "gpt-oss-120b-medium",
];

/// The vendor behind an agy model id — what the harden role's "different family" rule compares.
pub fn model_family(model: Option<&str>) -> &'static str {
    let name = model.unwrap_or("").to_lowercase();
    if name.starts_with("claude") {
        "anthropic"
    } else if name.starts_with("gpt") {
        "openai"
    } else {
        "google"
    }
}

fn describe(tool_info: Option<&Value>) -> String {
    let Some(params) = tool_info.and_then(|t| t.get("parameters")).and_then(Value::as_object) else { return String::new() };
    for key in ["CommandLine", "TargetFile", "AbsolutePath", "SearchPath", "DirectoryPath", "Query"] {
        if let Some(v) = params.get(key).filter(|v| !v.is_null() && v.as_str() != Some("") && v.as_bool() != Some(false)) {
            let text = v.as_str().map_or(v.to_string(), str::to_owned);
            return text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
        }
    }
    String::new()
}

fn int(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))).unwrap_or(0)
}

fn tokens(usage: &Value) -> (i64, i64) {
    let u = |k: &str| int(usage.get(k));
    (u("input_tokens") + u("cache_read_tokens"), u("output_tokens") + u("thinking_tokens"))
}

#[derive(Default)]
pub struct AgyStream {
    st: ParserState,
    step_usage: Vec<(i64, (i64, i64))>,
}

impl LineParser for AgyStream {
    fn st(&mut self) -> &mut ParserState {
        &mut self.st
    }

    fn apply(&mut self, event: &Value) -> Vec<WorkerEvent> {
        let mut out = Vec::new();
        let ev = |t: &str, payload: Value| WorkerEvent { event_type: t.into(), payload };
        match event.get("event").and_then(Value::as_str) {
            Some("init") => {
                if let Some(id) = event.get("conversation_id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    self.st.session_handle = Some(id.to_owned());
                }
                if let Some(model) = event.get("init").and_then(|i| i.get("model")).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    self.st.model = Some(model.to_owned());
                }
            }
            Some("step_update") => {
                let step = event.get("step_update").cloned().unwrap_or(Value::Null);
                let (step_type, state) = (step.get("step_type").and_then(Value::as_str), step.get("state").and_then(Value::as_str));
                if step_type == Some("tool") {
                    let name = step.get("tool_name").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("tool");
                    match state {
                        Some("ACTIVE") => out.push(ev("tool_use", json!({"tool": name, "detail": describe(step.get("tool_info"))}))),
                        Some("DONE") => out.push(ev("tool_result", json!({"tool": name, "ok": true}))),
                        _ => {}
                    }
                } else if step_type == Some("agent_response")
                    && let Some(delta) = step.get("text_delta").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                        out.push(ev("text", json!({"text": delta})));
                        self.st.text.push_str(delta);
                    }
                if let (Some(usage), Some("DONE")) = (step.get("usage").filter(|u| u.is_object()), state) {
                    let index = step.get("step_index").map_or(self.step_usage.len() as i64, |v| int(Some(v)));
                    if !self.step_usage.iter().any(|(i, _)| *i == index) {
                        self.st.requests += 1;
                    }
                    self.step_usage.retain(|(i, _)| *i != index);
                    self.step_usage.push((index, tokens(usage)));
                    self.st.tokens_in = self.step_usage.iter().map(|(_, v)| v.0).sum();
                    self.st.tokens_out = self.step_usage.iter().map(|(_, v)| v.1).sum();
                }
            }
            Some("result") => {
                let result = event.get("result").cloned().unwrap_or(Value::Null);
                if let Some(id) = result.get("conversation_id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    self.st.session_handle = Some(id.to_owned());
                }
                if let Some(resp) = result.get("response").filter(|v| !v.is_null() && v.as_str() != Some("")) {
                    self.st.text = resp.as_str().map_or(resp.to_string(), str::to_owned);
                }
                if let Some(usage) = result.get("usage").filter(|u| u.is_object()) {
                    let (tin, tout) = tokens(usage);
                    self.st.tokens_in = self.st.tokens_in.max(tin);
                    self.st.tokens_out = self.st.tokens_out.max(tout);
                }
                if let Some(obj) = result.get("structured_output").and_then(Value::as_object) {
                    // agy enforces a schema through its `finish` tool and may add that tool's own envelope fields.
                    self.st.structured = Some(Value::Object(obj.iter().filter(|(k, _)| *k != "toolAction" && *k != "toolSummary").map(|(k, v)| (k.clone(), v.clone())).collect()));
                }
                self.st.denied = result
                    .get("denied_actions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|d| match d.as_object() {
                        Some(o) => format!("{}:{}", o.get("action").map_or("None".to_owned(), py_str), o.get("display_name").map_or("None".to_owned(), py_str)),
                        None => py_str(d),
                    })
                    .collect();
                let ok = result.get("status").and_then(Value::as_str).unwrap_or("").to_uppercase() == "SUCCESS";
                self.st.ok = Some(ok);
                if !ok {
                    let message = result.get("error").filter(|v| !v.is_null() && v.as_str() != Some("")).map_or("agy reported an error".to_owned(), py_str);
                    self.st.note_error(&message);
                }
                out.push(ev("progress", json!({"phase": "result", "status": result.get("status").cloned().unwrap_or(Value::Null), "turns": result.get("num_turns").cloned().unwrap_or(Value::Null), "denied": self.st.denied})));
            }
            _ => {}
        }
        out
    }
}

/// Python's `str()` for a JSON scalar.
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        other => other.to_string(),
    }
}

pub struct AgyDialect;

impl Dialect for AgyDialect {
    fn family(&self) -> &'static str {
        "antigravity"
    }
    fn models(&self) -> &'static [&'static str] {
        &AGY_MODELS
    }
    fn model_family(&self) -> &'static str {
        "google"
    }
    fn binary_env(&self) -> &'static str {
        BINARY_ENV
    }
    fn binary_name(&self) -> &'static str {
        "agy"
    }
    fn home_rules(&self, home: &str) -> Vec<String> {
        crate::scope::agy_home_rules(home)
    }
    fn can_run_commands(&self) -> bool {
        false
    }

    fn build_argv(&self, binary: &str, task: &TaskPacket, resume: Option<&str>, model: Option<&str>) -> Vec<String> {
        let mut argv: Vec<String> = [binary, "--output-format", "stream-json", "--input-format", "stream-json", "--disable-slash-commands", "--print-timeout"].iter().map(|s| (*s).to_owned()).collect();
        argv.push(format!("{}s", (task.timeout_s as i64).max(1)));
        if !task.writable_scope().is_empty() {
            argv.extend(["--mode".into(), "accept-edits".into()]);
        }
        if let Some(model) = model {
            argv.extend(["--model".into(), model.into()]);
        }
        let effort_in_model = model.is_some_and(|m| matches!(m.rsplit('-').next(), Some("low" | "medium" | "high")));
        if let Some(effort) = task.effort.as_ref().filter(|_| !effort_in_model) {
            argv.extend(["--effort".into(), effort.clone()]);
        }
        if let Some(schema) = &task.json_schema {
            argv.extend(["--json-schema".into(), super::prompting::py_json_compact(schema)]);
        }
        if let Some(conversation) = resume {
            argv.extend(["--conversation".into(), conversation.into()]);
        }
        argv.push("-p=".into());
        argv
    }

    fn build_stdin(&self, _task: &TaskPacket, prompt: &str) -> Option<String> {
        Some(format!("{}\n", super::prompting::py_json_dumps(&json!({"event": "user", "message": {"content": prompt}}))))
    }

    fn new_parser(&self) -> Box<dyn LineParser> {
        Box::<AgyStream>::default()
    }
}

pub fn worker(egress: Arc<dyn Egress>) -> CliWorker {
    CliWorker::new(Arc::new(AgyDialect), egress)
}
