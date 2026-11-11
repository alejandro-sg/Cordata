//! Ollama Cloud models as orchestrator workers, through Cordata's own agent engine (`acp-core`). The engine runs the
//! tool loop (read, edit, run, search) with its guard and Seatbelt sandbox; this worker answers its approval prompts for
//! the step: edits only inside the step's files, offline commands only for roles that change files, nothing else.
//! Model names are Ollama's bare API names (`kimi-k3`), sent as `ollama_cloud::<name>`.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use acp_core::{AppEvent, ApprovalDecision, BackendTransport, PermissionInfo, SessionHandle, SessionOptions, StopReason};
use cordata_orchestrator::scope;
use cordata_orchestrator::worker::{EventSink, TaskPacket, Worker, WorkerEvent, WorkerResult};
use cordata_orchestrator::workers::{cli::schema_problem, prompting, tree};
use serde_json::{Value, json};
use tokio::sync::Notify;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct NativeWorker {
    id: String,
    /// `ollama_cloud` for Ollama's cloud; the engine's provider prefix.
    provider: String,
    running: Mutex<HashMap<String, Arc<Notify>>>,
}

impl NativeWorker {
    pub fn ollama_cloud() -> Self {
        Self { id: "ollama".into(), provider: "ollama_cloud".into(), running: Mutex::new(HashMap::new()) }
    }
}

/// The answer to one approval prompt: `None` allows it, `Some(why)` refuses it with the reason the model reads.
pub fn decide(info: &PermissionInfo, root: &Path, scope: &[String]) -> Option<String> {
    let args = info.raw_input.clone().unwrap_or(Value::Null);
    match info.tool_name.as_str() {
        "edit_file" => {
            let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
            let path = Path::new(raw);
            let rel = if path.is_absolute() { path.strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default() } else { raw.to_owned() };
            if !rel.is_empty() && !rel.split('/').any(|part| part == "..") && scope::matches(&rel, scope, &root.to_string_lossy()) {
                None
            } else if scope.is_empty() {
                Some("this step changes no files: report instead".into())
            } else {
                Some(format!("{raw} is outside the files this step may change ({})", scope.join(", ")))
            }
        }
        "run_command" if scope.is_empty() => Some("this step changes nothing, so it runs no commands: read the files instead".into()),
        "run_command" if args.get("network").and_then(Value::as_bool) == Some(true) => Some("workers run commands offline only".into()),
        "run_command" => None,
        other => Some(format!("{other} is not available to orchestrator workers")),
    }
}

/// A provider error that means the account's limit, not the task.
fn is_limit(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("429") || e.contains("rate limit") || e.contains("usage limit") || e.contains("quota") || e.contains("too many requests")
}

/// The follow-up sent when a turn ended without an answer the schema accepts.
fn answer_nudge(problem: &str) -> String {
    format!("Your turn ended without a usable answer ({problem}). Do not call any more tools. Reply now with only the JSON object the task asked for, built from what you have already read.")
}

impl Worker for NativeWorker {
    fn worker_id(&self) -> &str {
        &self.id
    }

    fn run<'a>(&'a self, task: TaskPacket, events: EventSink) -> BoxFuture<'a, WorkerResult> {
        Box::pin(async move {
            let started = Instant::now();
            let root = std::fs::canonicalize(&task.cwd).unwrap_or_else(|_| PathBuf::from(&task.cwd));
            let root_str = root.to_string_lossy().into_owned();
            let scope = task.writable_scope();
            let model = task.model.clone().unwrap_or_else(|| "kimi-k3".into());
            let stop = Arc::new(Notify::new());
            self.running.lock().unwrap().insert(task.task_id.clone(), stop.clone());
            let before = tree::snapshot(&root);
            let (session, mut rx) = SessionHandle::spawn(SessionOptions {
                cwd: root.clone(),
                transport: BackendTransport::Native,
                debug_log: None,
                model: Some(format!("{}::{model}", self.provider)),
                history: false,
            });
            events(WorkerEvent { event_type: "start".into(), payload: json!({"worker_id": self.id, "role": task.role, "model": model, "scope": scope}) });
            let prompt = prompting::compose(&task, false, !scope.is_empty());
            let mut r = WorkerResult::new(&self.id, "failed");
            r.model = Some(model.clone());
            let (mut text, mut requests, mut denied) = (String::new(), 0i64, Vec::<String>::new());
            let outcome: Result<(), String> = if let Err(e) = session.send_prompt(prompt) {
                Err(e.to_string())
            } else {
                let mut nudged = false;
                let deadline = tokio::time::sleep(Duration::from_secs_f64(task.timeout_s.max(1.0)));
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        () = &mut deadline => {
                            let _ = session.cancel();
                            break Err(format!("timed out after {}s", task.timeout_s as i64));
                        }
                        () = stop.notified() => {
                            let _ = session.cancel();
                            r.status = "cancelled".into();
                            break Err("cancelled".into());
                        }
                        event = rx.recv() => match event {
                            None | Some(AppEvent::Closed) => break Err("the agent engine stopped".into()),
                            Some(AppEvent::ConnectionError(e)) => break Err(e),
                            Some(AppEvent::TurnFailed(e)) => break Err(e),
                            Some(AppEvent::TurnComplete(StopReason::Cancelled)) => break Err("cancelled".into()),
                            Some(AppEvent::TurnComplete(_)) => {
                                // A model that spent its turn exploring, or answered off-schema, gets one chance to
                                // answer properly on the same session: its context is paid for, a fresh worker's is not.
                                let problem = task.json_schema.as_ref().and_then(|schema| schema_problem(prompting::parse_json_answer(&text).as_ref(), schema));
                                match problem {
                                    Some(problem) if !nudged => {
                                        nudged = true;
                                        text.clear();
                                        events(WorkerEvent { event_type: "nudge".into(), payload: json!({"problem": problem}) });
                                        if let Err(e) = session.send_prompt(answer_nudge(&problem)) {
                                            break Err(e.to_string());
                                        }
                                    }
                                    _ => break Ok(()),
                                }
                            }
                            Some(AppEvent::AgentText(t)) => text.push_str(&t),
                            Some(AppEvent::Usage(u)) => {
                                requests += 1;
                                r.tokens_in += u.input as i64;
                                r.tokens_out += u.output as i64;
                            }
                            Some(AppEvent::ToolCallStarted(call)) => {
                                let detail = call.raw_input.as_ref().and_then(|i| i.get("path").or_else(|| i.get("cmd"))).and_then(Value::as_str).unwrap_or("").to_owned();
                                events(WorkerEvent { event_type: "tool_use".into(), payload: json!({"tool": call.name, "detail": detail}) });
                            }
                            Some(AppEvent::PermissionRequested { info, decision }) => {
                                let answer = match decide(&info, &root, &scope) {
                                    None => ApprovalDecision::Allow,
                                    Some(why) => {
                                        denied.push(info.tool_name.clone());
                                        ApprovalDecision::Say(why)
                                    }
                                };
                                let _ = decision.send(answer);
                            }
                            Some(_) => {}
                        }
                    }
                }
            };
            session.shutdown().await;
            self.running.lock().unwrap().remove(&task.task_id);
            let changed = tree::changed(before.as_ref(), tree::snapshot(&root).as_ref()).unwrap_or_default();
            match outcome {
                Ok(()) => r.status = "completed".into(),
                Err(e) if r.status == "cancelled" || e == "cancelled" => {
                    r.status = "cancelled".into();
                    r.error_message = Some(e);
                }
                Err(e) => {
                    r.rate_limited = is_limit(&e);
                    r.status = if r.rate_limited { "quota_limit" } else { "failed" }.into();
                    r.error_message = Some(e);
                }
            }
            if r.status == "completed"
                && let Some(schema) = &task.json_schema
            {
                r.structured = prompting::parse_json_answer(&text);
                if let Some(problem) = schema_problem(r.structured.as_ref(), schema) {
                    (r.status, r.error_message) = ("failed".into(), Some(format!("the answer does not match the schema: {problem}")));
                }
            }
            if r.status == "completed" && !scope.is_empty() && changed.is_empty() && !denied.is_empty() {
                r.status = "failed".into();
                r.error_message = Some(format!("{} {}: it stopped instead of finishing the step", cordata_orchestrator::worker::GAVE_UP, denied.join(", ")));
            }
            r.out_of_scope = scope::out_of_scope(&changed, &scope, &root_str);
            r.diff_stat = tree::diff_stat(&root, &changed);
            r.files_changed = changed;
            r.raw_output = text.chars().take(20_000).collect();
            r.denied = denied;
            r.requests = requests.max(1);
            r.exit_code = i32::from(r.status != "completed");
            r.duration_s = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
            if r.status != "completed" {
                events(WorkerEvent { event_type: "error".into(), payload: json!({"message": r.error_message.clone().unwrap_or_else(|| r.status.clone())}) });
            }
            events(WorkerEvent { event_type: "done".into(), payload: serde_json::to_value(&r).unwrap_or(Value::Null) });
            r
        })
    }

    fn cancel<'a>(&'a self, task_id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            match self.running.lock().unwrap().get(task_id) {
                Some(stop) => {
                    stop.notify_one();
                    true
                }
                None => false,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_core::ToolKind;

    fn ask(tool: &str, args: Value) -> PermissionInfo {
        PermissionInfo { tool_call_id: "c".into(), tool_name: tool.into(), title: String::new(), kind: ToolKind::Other, raw_input: Some(args), always_label: None, reason: None }
    }

    #[test]
    fn approvals_follow_the_step_scope_and_stay_offline() {
        let root = Path::new("/p");
        let scope = vec!["src/a.rs".to_owned(), "docs/".to_owned()];
        assert_eq!(decide(&ask("edit_file", json!({"path": "src/a.rs"})), root, &scope), None);
        assert_eq!(decide(&ask("edit_file", json!({"path": "/p/src/a.rs"})), root, &scope), None);
        assert!(decide(&ask("edit_file", json!({"path": "src/b.rs"})), root, &scope).unwrap().contains("outside the files"));
        assert!(decide(&ask("edit_file", json!({"path": "src/../../etc/x"})), root, &scope).is_some());
        assert!(decide(&ask("edit_file", json!({"path": "/elsewhere/a.rs"})), root, &scope).is_some());
        assert_eq!(decide(&ask("run_command", json!({"cmd": "cargo test"})), root, &scope), None);
        assert!(decide(&ask("run_command", json!({"cmd": "curl x", "network": true})), root, &scope).unwrap().contains("offline"));
        assert!(decide(&ask("run_command", json!({"cmd": "ls"})), root, &[]).unwrap().contains("runs no commands"));
        assert!(decide(&ask("edit_file", json!({"path": "a"})), root, &[]).unwrap().contains("changes no files"));
        assert!(decide(&ask("mcp__x__y", json!({})), root, &scope).is_some());
    }

    #[test]
    fn provider_limits_are_told_apart_from_failures() {
        assert!(is_limit("HTTP 429 Too Many Requests"));
        assert!(is_limit("weekly usage limit reached"));
        assert!(!is_limit("tests failed"));
    }
}
