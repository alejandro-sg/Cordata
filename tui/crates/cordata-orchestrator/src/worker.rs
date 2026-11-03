//! The one contract every coding worker meets: the orchestrator hands a worker
//! one bounded piece of work and reads back a typed result. A worker never raises for an operational failure — a
//! missing CLI, a timeout, a rate limit and a crash are all results, so the state machine can decide what next.
//!
//! Only the surface the executor uses is here; the subprocess wrappers (Claude Code, `agy`, Ollama) implement
//! [`Worker`] when they are ported.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const READ_ONLY_ROLES: [&str; 2] = ["plan", "audit"];
pub const STATUSES: [&str; 4] = ["completed", "failed", "cancelled", "quota_limit"];
/// How a failed result starts when the worker changed nothing after being refused something: it will do the same again.
pub const GAVE_UP: &str = "the worker made no change and was refused";

#[derive(Debug, Clone, PartialEq)]
pub struct TaskPacket {
    pub task_id: String,
    /// `plan` | `execute` | `harden` | `audit`
    pub role: String,
    pub prompt: String,
    pub cwd: String,
    pub file_scope: Vec<String>,
    pub budget_usd: Option<f64>,
    pub budget_tokens: Option<i64>,
    pub timeout_s: f64,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub checkpoint_hash: Option<String>,
    pub handoff_context: Option<Value>,
    pub json_schema: Option<Value>,
    /// This step's pass to the run bus (`cordata_bus`), when the run has one and the worker can speak MCP.
    pub bus: Option<cordata_bus::BusGrant>,
}

impl TaskPacket {
    pub fn new(task_id: &str, role: &str, prompt: &str, cwd: &str) -> Self {
        Self {
            task_id: task_id.into(),
            role: role.into(),
            prompt: prompt.into(),
            cwd: cwd.into(),
            file_scope: Vec::new(),
            budget_usd: None,
            budget_tokens: None,
            timeout_s: 1800.0,
            model: None,
            effort: None,
            checkpoint_hash: None,
            handoff_context: None,
            json_schema: None,
            bus: None,
        }
    }

    /// The paths this task may change: nothing for a read-only role, the declared scope otherwise.
    pub fn writable_scope(&self) -> Vec<String> {
        if READ_ONLY_ROLES.contains(&self.role.as_str()) { Vec::new() } else { self.file_scope.clone() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerResult {
    pub worker_id: String,
    /// One of [`STATUSES`].
    pub status: String,
    pub exit_code: i32,
    pub duration_s: f64,
    pub cost_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub files_changed: Vec<String>,
    pub diff_stat: String,
    pub raw_output: String,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub session_handle: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub structured: Option<Value>,
    /// The stop was the provider's limit (a 429, "usage limit reached"), not the task's own budget.
    #[serde(default)]
    pub rate_limited: bool,
    /// Files changed outside the task's writable scope. Must be empty; the audit fails the step otherwise.
    #[serde(default)]
    pub out_of_scope: Vec<String>,
    #[serde(default)]
    pub denied: Vec<String>,
    #[serde(default = "one")]
    pub requests: i64,
    /// The provider's own reading of its quota windows (see `cordata_ledger::Outcome::provider_quota`).
    #[serde(default)]
    pub provider_quota: Value,
}

fn one() -> i64 {
    1
}

impl WorkerResult {
    pub fn new(worker_id: &str, status: &str) -> Self {
        Self {
            worker_id: worker_id.into(),
            status: status.into(),
            exit_code: i32::from(status != "completed"),
            duration_s: 0.0,
            cost_usd: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            files_changed: Vec::new(),
            diff_stat: String::new(),
            raw_output: String::new(),
            error_message: None,
            session_handle: None,
            model: None,
            structured: None,
            rate_limited: false,
            out_of_scope: Vec::new(),
            denied: Vec::new(),
            requests: 1,
            provider_quota: Value::Null,
        }
    }

    pub fn ok(&self) -> bool {
        self.status == "completed"
    }

    pub fn ledger_outcome(&self) -> cordata_ledger::Outcome {
        cordata_ledger::Outcome {
            model: self.model.clone(),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cost_usd: self.cost_usd,
            requests: Some(self.requests),
            rate_limited: self.rate_limited,
            provider_quota: self.provider_quota.clone(),
        }
    }
}

/// A progress event while a worker runs (`init`, `progress`, `tool_use`, `text`, `error`, `quota`, …).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerEvent {
    pub event_type: String,
    pub payload: Value,
}

pub type EventSink = Arc<dyn Fn(WorkerEvent) + Send + Sync>;

pub trait Worker: Send + Sync {
    /// A stable id for the ledger: `<family>` or `<family>:<variant>`.
    fn worker_id(&self) -> &str;

    /// Runs `task` to completion, reporting progress to `events`. Never fails: errors are results.
    fn run<'a>(&'a self, task: TaskPacket, events: EventSink) -> BoxFuture<'a, WorkerResult>;

    /// Stops a running task (its process group is killed; `run` then returns a `cancelled` result).
    fn cancel<'a>(&'a self, task_id: &'a str) -> BoxFuture<'a, bool>;
}
