//! What the two subprocess workers share: spawn, confine, stream, meter, stop.
//!
//! The flow for one task, the same for both CLIs — only the argv and the event dialect differ ([`Dialect`]):
//!
//! 1. **Refuse early, as a result.** An empty or oversized prompt, a bad model name, a scope entry that is absolute,
//!    leaves the project or names a secrets file, a missing CLI, or a vault that is off or down end the run with
//!    `status: failed` and the reason, before anything is spawned.
//! 2. **Egress through the vault, or not at all** ([`Egress`]); the child gets an allowlisted environment, never a
//!    provider key.
//! 3. **Confined writes.** A Seatbelt profile denies every write inside the project except the task's writable scope
//!    (none for plan/audit), and `.env` / credential files everywhere. Fail closed when it cannot be applied.
//! 4. **Metered while it runs.** Every event is folded into tokens and dollars; a task that passes its own budget is
//!    killed there and ends `quota_limit`. A provider limit also ends `quota_limit`, with `rate_limited`.
//! 5. **Stoppable.** [`CliWorker::cancel`] kills the process group; the run ends `cancelled`.
//! 6. **Accounted after.** Snapshot diff → `files_changed`, `git diff --stat`, and `out_of_scope`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use regex::Regex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::vault::Egress;
use super::{prompting, tree};
use crate::scope;
use crate::secrets;
use crate::worker::{EventSink, TaskPacket, Worker, WorkerEvent, WorkerResult};

pub const MAX_PROMPT_CHARS: usize = 60_000;
pub const MAX_OUTPUT_CHARS: usize = 60_000;
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const SEATBELT_ENV: &str = "CORDATA_SANDBOX_SEATBELT";
const ENV_ALLOW: [&str; 13] = ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "TERM", "LANG", "TZ", "LC_ALL", "LC_CTYPE", "COLORTERM", "NO_COLOR"];

/// Provider wording for "you are out of quota", across Anthropic, Google and OpenAI-compatible errors.
pub fn rate_limit(text: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)usage limit|rate[ _-]?limit|too many requests|\b429\b|quota|resource[_ ]exhausted|limit reached|overloaded").expect("static"));
    RE.is_match(text)
}

fn model_name_ok(model: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:\[\]-]{0,80}$").expect("static"));
    RE.is_match(model)
}

/// Line-splitting plus the totals a run adds up to.
#[derive(Debug, Default)]
pub struct ParserState {
    pub session_handle: Option<String>,
    pub model: Option<String>,
    pub text: String,
    pub structured: Option<Value>,
    pub cost_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub requests: i64,
    pub rate_limited: bool,
    pub budget_exhausted: bool,
    pub error: Option<String>,
    /// What the CLI's own final event said.
    pub ok: Option<bool>,
    pub denied: Vec<String>,
    pub quota_info: Value,
    buffer: Vec<u8>,
}

impl ParserState {
    pub fn note_error(&mut self, text: &str) {
        self.error = Some(text.to_owned());
        if rate_limit(text) {
            self.rate_limited = true;
        }
    }

    fn parse_line(line: &[u8]) -> Option<Value> {
        let text = String::from_utf8_lossy(line);
        serde_json::from_str::<Value>(text.trim()).ok().filter(Value::is_object)
    }

    /// Complete JSON-object lines in `chunk` (partial trailing data is kept for the next call).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Value> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            events.extend(Self::parse_line(&line[..line.len() - 1]));
        }
        events
    }

    pub fn flush(&mut self) -> Vec<Value> {
        let line = std::mem::take(&mut self.buffer);
        Self::parse_line(&line).into_iter().collect()
    }
}

/// One CLI's event dialect for a single run.
pub trait LineParser: Send {
    fn st(&mut self) -> &mut ParserState;
    fn apply(&mut self, event: &Value) -> Vec<WorkerEvent>;
}

/// What differs between the CLIs: names, argv, stdin, and how to read their stream.
pub trait Dialect: Send + Sync {
    fn family(&self) -> &'static str;
    fn models(&self) -> &'static [&'static str];
    fn model_family(&self) -> &'static str;
    fn binary_env(&self) -> &'static str;
    fn binary_name(&self) -> &'static str;
    /// The CLI enforces a JSON Schema itself (`--json-schema`); otherwise the prompt asks for JSON.
    fn native_schema(&self) -> bool {
        true
    }
    /// False when the CLI refuses every shell command in headless mode (agy), so the prompt says so.
    fn can_run_commands(&self) -> bool {
        true
    }
    fn build_argv(&self, binary: &str, task: &TaskPacket, resume: Option<&str>, model: Option<&str>) -> Vec<String>;
    fn build_stdin(&self, task: &TaskPacket, prompt: &str) -> Option<String>;
    fn new_parser(&self) -> Box<dyn LineParser>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seatbelt {
    Auto,
    On,
    Off,
}

/// Can `sandbox-exec` apply a profile *here*? It fails (exit 71) when this process is already inside a sandbox.
pub fn seatbelt_works() -> bool {
    static PROBE: OnceLock<bool> = OnceLock::new();
    *PROBE.get_or_init(|| {
        Path::new(SANDBOX_EXEC).exists()
            && std::process::Command::new(SANDBOX_EXEC)
                .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
    })
}

struct Running {
    pid: Option<u32>,
    cancelled: bool,
}

#[derive(Default)]
struct Outcome {
    exit_code: Option<i32>,
    stderr: String,
    timed_out: bool,
    budget_reason: String,
    spawn_error: String,
}

pub struct CliWorker {
    dialect: Arc<dyn Dialect>,
    worker_id: String,
    binary: Option<String>,
    egress: Arc<dyn Egress>,
    environ: HashMap<String, String>,
    extra_env: Vec<(String, String)>,
    seatbelt: Seatbelt,
    default_model: Option<String>,
    running: Mutex<HashMap<String, Running>>,
    last_quota: Mutex<Value>,
}

fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid.filter(|p| *p > 0) {
        // SAFETY: killpg only sends a signal; a stale or foreign pgid fails with ESRCH/EPERM, which is ignored.
        unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
    }
}

impl CliWorker {
    pub fn new(dialect: Arc<dyn Dialect>, egress: Arc<dyn Egress>) -> Self {
        let worker_id = dialect.family().to_owned();
        Self {
            dialect,
            worker_id,
            binary: None,
            egress,
            environ: std::env::vars().collect(),
            extra_env: Vec::new(),
            seatbelt: Seatbelt::Auto,
            default_model: None,
            running: Mutex::new(HashMap::new()),
            last_quota: Mutex::new(Value::Null),
        }
    }

    pub fn binary(mut self, path: &str) -> Self {
        self.binary = Some(path.into());
        self
    }
    pub fn environ(mut self, environ: HashMap<String, String>) -> Self {
        self.environ = environ;
        self
    }
    pub fn extra_env(mut self, env: &[(&str, &str)]) -> Self {
        self.extra_env = env.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
        self
    }
    pub fn seatbelt(mut self, mode: Seatbelt) -> Self {
        self.seatbelt = mode;
        self
    }
    pub fn default_model(mut self, model: &str) -> Self {
        self.default_model = Some(model.into());
        self
    }
    pub fn worker_id_override(mut self, id: &str) -> Self {
        self.worker_id = id.into();
        self
    }

    /// `quota` events seen so far (the provider's own window readings).
    pub fn last_quota(&self) -> Value {
        self.last_quota.lock().unwrap().clone()
    }

    pub fn find_binary(&self) -> Option<String> {
        let from_env = self.environ.get(self.dialect.binary_env()).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        let on_path = || {
            self.environ.get("PATH").and_then(|p| p.split(':').map(|d| Path::new(d).join(self.dialect.binary_name())).find(|c| c.is_file()))
        };
        let fallback = || self.environ.get("HOME").map(|h| Path::new(h).join(".local/bin").join(self.dialect.binary_name())).filter(|p| p.exists());
        let candidate = self.binary.clone().or(from_env).or_else(|| on_path().map(|p| p.to_string_lossy().into_owned())).or_else(|| fallback().map(|p| p.to_string_lossy().into_owned()))?;
        Path::new(&candidate).exists().then_some(candidate)
    }

    fn model(&self, task: &TaskPacket) -> Option<String> {
        task.model.clone().or_else(|| self.default_model.clone())
    }

    pub fn validate(&self, task: &TaskPacket) -> Option<String> {
        if !["plan", "execute", "harden", "audit"].contains(&task.role.as_str()) {
            return Some(format!("unknown role {}", scope::py_repr(&task.role)));
        }
        if task.prompt.trim().is_empty() {
            return Some("the task has no prompt".into());
        }
        let len = task.prompt.chars().count();
        if len > MAX_PROMPT_CHARS {
            return Some(format!("the prompt is {len} characters; the limit is {MAX_PROMPT_CHARS}"));
        }
        if let Some(model) = self.model(task).filter(|m| !model_name_ok(m)) {
            return Some(format!("{} is not a valid model name", scope::py_repr(&model)));
        }
        if let Some(effort) = task.effort.as_deref().filter(|e| !EFFORTS.contains(e)) {
            return Some(format!("{} is not an effort level ({})", scope::py_repr(effort), EFFORTS.join(", ")));
        }
        if !Path::new(&task.cwd).is_dir() {
            return Some(format!("{} is not a directory", scope::py_repr(&task.cwd)));
        }
        task.file_scope.iter().find_map(|entry| scope::valid(entry))
    }

    fn over_budget(task: &TaskPacket, st: &ParserState) -> String {
        if let Some(usd) = task.budget_usd.filter(|b| st.cost_usd > *b) {
            return format!("spent ${:.4}, over the task budget of ${usd:.4}", st.cost_usd);
        }
        let used = st.tokens_in + st.tokens_out;
        if let Some(limit) = task.budget_tokens.filter(|b| used > *b) {
            return format!("used {used} tokens, over the task budget of {limit}");
        }
        String::new()
    }

    fn failed(&self, task: &TaskPacket, started: Instant, status: &str, error: &str) -> WorkerResult {
        let mut r = WorkerResult::new(&self.worker_id, status);
        r.duration_s = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
        r.error_message = Some(error.to_owned());
        r.model = self.model(task);
        r.requests = 0;
        r
    }

    fn confinement_ok(&self) -> Result<bool, String> {
        let mode = match self.environ.get(SEATBELT_ENV).map(|s| s.trim().to_lowercase()).as_deref() {
            Some("on") => Seatbelt::On,
            Some("off") => Seatbelt::Off,
            Some("auto") => Seatbelt::Auto,
            _ => self.seatbelt,
        };
        if mode == Seatbelt::Off {
            return Ok(false);
        }
        if seatbelt_works() {
            return Ok(true);
        }
        Err(format!(
            "the Seatbelt sandbox (sandbox-exec) cannot be applied here, so a child process could read .env files; refusing to run it unconfined. Run outside another sandbox, or set {SEATBELT_ENV}=off to accept that."
        ))
    }

    /// Runs `task` (or, with `resume`, continues a provider session). Never fails: errors are results.
    pub async fn execute(&self, task: TaskPacket, resume: Option<&str>, events: EventSink) -> WorkerResult {
        let started = Instant::now();
        let mut problem = self.validate(&task);
        let binary = if problem.is_none() { self.find_binary() } else { None };
        if problem.is_none() && binary.is_none() {
            problem = Some(format!("the {} CLI was not found (set {} to its path)", self.dialect.binary_name(), self.dialect.binary_env()));
        }
        let mut env = Vec::new();
        if problem.is_none() {
            match self.egress.env() {
                Ok(e) => env = e,
                Err(why) => problem = Some(why),
            }
        }
        if let Some(problem) = problem {
            events(WorkerEvent { event_type: "error".into(), payload: json!({"message": problem}) });
            let result = self.failed(&task, started, "failed", &problem);
            events(WorkerEvent { event_type: "done".into(), payload: serde_json::to_value(&result).unwrap_or(Value::Null) });
            return result;
        }
        let binary = binary.expect("checked above");
        env.extend(self.extra_env.clone());

        let root = std::fs::canonicalize(&task.cwd).unwrap_or_else(|_| PathBuf::from(&task.cwd));
        let root_str = root.to_string_lossy().into_owned();
        let writable = task.writable_scope();
        let prompt = prompting::compose(&task, self.dialect.native_schema(), self.dialect.can_run_commands());
        let model = self.model(&task);
        let argv = self.dialect.build_argv(&binary, &task, resume, model.as_deref());
        let stdin = self.dialect.build_stdin(&task, &prompt);
        let mut parser = self.dialect.new_parser();
        let mut outcome = Outcome::default();
        let before = tree::snapshot(&root);

        let confined = match self.confinement_ok() {
            Ok(c) => Some(c),
            Err(why) => {
                outcome.spawn_error = why;
                None
            }
        };
        events(WorkerEvent {
            event_type: "init".into(),
            payload: json!({"worker_id": self.worker_id, "role": task.role, "model": model, "scope": writable, "resume": resume,
                            "confinement": if confined == Some(true) { "seatbelt" } else { "none" }}),
        });
        if let Some(confined) = confined {
            self.stream(&task, &root, &root_str, &writable, &argv, stdin, env, confined, &mut *parser, &mut outcome, &events).await;
        }

        let cancelled = self.running.lock().unwrap().remove(&task.task_id).is_some_and(|r| r.cancelled);
        let after = tree::snapshot(&root);
        let changes = tree::changed(before.as_ref(), after.as_ref());
        let changed = changes.clone().unwrap_or_default();
        let mut result = self.finish(&task, started, parser.st(), &outcome, cancelled, changed, &root, &root_str);
        if changes.is_none() {
            let note = "the project is too large to list what changed";
            result.error_message = Some(match result.error_message.take() {
                Some(e) => format!("{e}; {note}"),
                None => note.to_owned(),
            });
        }
        if result.status != "completed" {
            events(WorkerEvent { event_type: "error".into(), payload: json!({"message": result.error_message.clone().unwrap_or_else(|| result.status.clone())}) });
        }
        events(WorkerEvent { event_type: "done".into(), payload: serde_json::to_value(&result).unwrap_or(Value::Null) });
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream(
        &self,
        task: &TaskPacket,
        root: &Path,
        root_str: &str,
        writable: &[String],
        argv: &[String],
        stdin: Option<String>,
        env: Vec<(String, String)>,
        confined: bool,
        parser: &mut dyn LineParser,
        outcome: &mut Outcome,
        events: &EventSink,
    ) {
        let wrapped: Vec<String> = if confined {
            let home = self.environ.get("HOME").cloned().unwrap_or_default();
            [SANDBOX_EXEC.to_owned(), "-p".into(), scope::scoped_profile(root_str, writable, &home)].into_iter().chain(argv.iter().cloned()).collect()
        } else {
            argv.to_vec()
        };
        let mut child_env: Vec<(String, String)> = ENV_ALLOW.iter().filter_map(|k| self.environ.get(*k).map(|v| ((*k).to_owned(), v.clone()))).collect();
        if !child_env.iter().any(|(k, _)| k == "PATH") {
            child_env.push(("PATH".into(), "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin".into()));
        }
        for (k, v) in env {
            if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.starts_with(|c: char| c.is_ascii_digit()) {
                child_env.retain(|(existing, _)| *existing != k);
                child_env.push((k, v));
            }
        }
        let mut command = Command::new(&wrapped[0]);
        command
            .args(&wrapped[1..])
            .env_clear()
            .envs(child_env)
            .current_dir(root)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                outcome.exit_code = Some(127);
                outcome.stderr = format!("{}: command not found", argv[0]);
                return;
            }
            Err(e) => {
                outcome.spawn_error = format!("could not start the CLI: {e}");
                return;
            }
        };
        let pid = child.id();
        self.running.lock().unwrap().insert(task.task_id.clone(), Running { pid, cancelled: false });
        if let (Some(mut pipe), Some(text)) = (child.stdin.take(), stdin) {
            tokio::spawn(async move {
                // The child may exit before reading its prompt; its exit code says why.
                let _ = pipe.write_all(text.as_bytes()).await;
                let _ = pipe.shutdown().await;
            });
        }
        let mut stderr_pipe = child.stderr.take().expect("piped");
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = (&mut stderr_pipe).take(2_000_000).read_to_end(&mut buf).await;
            // Keep draining so a chatty child never blocks on a full pipe.
            let _ = tokio::io::copy(&mut stderr_pipe, &mut tokio::io::sink()).await;
            String::from_utf8_lossy(&buf).into_owned()
        });
        let mut stdout = child.stdout.take().expect("piped");
        let deadline = tokio::time::sleep(Duration::from_secs_f64(task.timeout_s.max(1.0)));
        tokio::pin!(deadline);
        let mut chunk = vec![0u8; 65_536];
        // killpg races with a fork in flight (a child being created at that instant can miss the signal), so after
        // the first kill the group is signalled again until the pipe closes, for a bounded grace period.
        let mut killing: Option<Instant> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                read = stdout.read(&mut chunk) => {
                    let n = read.unwrap_or(0);
                    if n == 0 {
                        for raw in parser.st().flush() {
                            for event in parser.apply(&raw) {
                                events(event);
                            }
                        }
                        break;
                    }
                    for raw in parser.st().feed(&chunk[..n]) {
                        for event in parser.apply(&raw) {
                            if event.event_type == "quota" {
                                *self.last_quota.lock().unwrap() = event.payload.clone();
                            }
                            events(event);
                        }
                    }
                    if outcome.budget_reason.is_empty() {
                        outcome.budget_reason = Self::over_budget(task, parser.st());
                        if !outcome.budget_reason.is_empty() {
                            events(WorkerEvent { event_type: "quota".into(), payload: json!({"reason": "task_budget", "detail": outcome.budget_reason}) });
                            kill_group(pid);
                            killing.get_or_insert_with(Instant::now);
                        }
                    }
                }
                () = &mut deadline, if killing.is_none() => {
                    outcome.timed_out = true;
                    kill_group(pid);
                    killing = Some(Instant::now());
                }
                _ = tick.tick(), if killing.is_some() => {
                    kill_group(pid);
                    if killing.is_some_and(|since| since.elapsed() > Duration::from_secs(5)) {
                        break;
                    }
                }
            }
        }
        if outcome.timed_out {
            let _ = child.wait().await;
        } else if let Ok(status) = child.wait().await {
            use std::os::unix::process::ExitStatusExt;
            outcome.exit_code = Some(status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0)));
        }
        outcome.stderr = stderr_task.await.unwrap_or_default();
        kill_group(pid); // grandchildren that outlived the CLI
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(&self, task: &TaskPacket, started: Instant, st: &mut ParserState, outcome: &Outcome, cancelled: bool, changed: Vec<String>, root: &Path, root_str: &str) -> WorkerResult {
        let text = secrets::scrub(st.text.trim()).0;
        let mut structured = st.structured.clone();
        if task.json_schema.is_some() && structured.is_none() {
            structured = prompting::parse_json_answer(&st.text);
        }
        let mut error: Option<String> = None;
        let mut status;
        if cancelled {
            (status, error) = ("cancelled", Some("cancelled — the CLI was stopped".into()));
        } else if !outcome.spawn_error.is_empty() {
            (status, error) = ("failed", Some(outcome.spawn_error.clone()));
        } else if !outcome.budget_reason.is_empty() || st.budget_exhausted {
            let why = if outcome.budget_reason.is_empty() { st.error.clone().unwrap_or_else(|| "the task budget was exhausted".into()) } else { outcome.budget_reason.clone() };
            (status, error) = ("quota_limit", Some(why));
        } else if st.rate_limited {
            (status, error) = ("quota_limit", Some(st.error.clone().unwrap_or_else(|| "the provider's usage limit was reached".into())));
        } else if outcome.timed_out {
            (status, error) = ("failed", Some(format!("timed out after {}s", task.timeout_s)));
        } else if outcome.exit_code == Some(0) && st.ok != Some(false) {
            status = "completed";
        } else {
            status = "failed";
            let stderr_tail: String = {
                let trimmed = outcome.stderr.trim();
                let start = trimmed.char_indices().rev().nth(1499).map_or(0, |(i, _)| i);
                secrets::scrub(&trimmed[start..]).0
            };
            let code = outcome.exit_code.map_or("None".to_owned(), |c| c.to_string());
            error = Some(st.error.clone().unwrap_or_else(|| format!("the CLI exited {code}{}", if stderr_tail.is_empty() { String::new() } else { format!(": {stderr_tail}") })));
            if rate_limit(&stderr_tail) {
                status = "quota_limit";
                st.rate_limited = true;
            }
        }
        if status == "completed"
            && let Some(schema) = &task.json_schema
                && let Some(problem) = schema_problem(structured.as_ref(), schema) {
                    (status, error) = ("failed", Some(format!("the answer does not match the schema: {problem}")));
                }
        if status == "completed" && !task.writable_scope().is_empty() && changed.is_empty() && !st.denied.is_empty() {
            // The CLI said "success", changed nothing, and was refused something it asked for: it gave up, not finished.
            status = "failed";
            error = Some(format!("the worker made no change and was refused {}: it stopped instead of finishing the step", st.denied.iter().take(3).cloned().collect::<Vec<_>>().join(", ")));
        }
        let out_of_scope = scope::out_of_scope(&changed, &task.writable_scope(), root_str);
        let mut r = WorkerResult::new(&self.worker_id, status);
        r.exit_code = outcome.exit_code.unwrap_or(if outcome.timed_out { 124 } else { 1 });
        r.duration_s = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
        r.cost_usd = (st.cost_usd * 1e6).round() / 1e6;
        r.tokens_in = st.tokens_in;
        r.tokens_out = st.tokens_out;
        r.diff_stat = tree::diff_stat(root, &changed);
        r.files_changed = changed;
        r.raw_output = text.chars().take(MAX_OUTPUT_CHARS).collect();
        r.error_message = error;
        r.session_handle = st.session_handle.clone();
        r.model = st.model.clone().or_else(|| self.model(task));
        r.structured = structured;
        r.rate_limited = st.rate_limited;
        r.out_of_scope = out_of_scope;
        r.denied = st.denied.iter().take(50).cloned().collect();
        r.requests = st.requests.max(1);
        r.provider_quota = st.quota_info.clone();
        r
    }
}

/// Why `value` does not satisfy `schema`, or `None`.
pub fn schema_problem(value: Option<&Value>, schema: &Value) -> Option<String> {
    let Some(value) = value else { return Some("no JSON object in the answer".into()) };
    match jsonschema::validate(schema, value) {
        Ok(()) => None,
        Err(e) => {
            let path = e.instance_path.to_string();
            let where_ = if path.is_empty() { "(root)".to_owned() } else { path.trim_start_matches('/').to_owned() };
            Some(format!("{where_}: {}", e.to_string().chars().take(300).collect::<String>()))
        }
    }
}

impl Worker for CliWorker {
    fn worker_id(&self) -> &str {
        &self.worker_id
    }

    fn run<'a>(&'a self, task: TaskPacket, events: EventSink) -> BoxFuture<'a, WorkerResult> {
        Box::pin(async move { self.execute(task, None, events).await })
    }

    fn cancel<'a>(&'a self, task_id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut running = self.running.lock().unwrap();
            let Some(r) = running.get_mut(task_id) else { return false };
            r.cancelled = true;
            kill_group(r.pid);
            true
        })
    }
}
