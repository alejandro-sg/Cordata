//! The run bus: how workers the orchestrator delegated to (Claude Code, Antigravity, …) tell each other what they
//! found, ask and answer questions, say what they are doing, and claim files before editing them — while each stays
//! confined to its own step.
//!
//! Shape (herdr's, made safe for untrusted workers): one unix socket per orchestrator run, owned by the orchestrator
//! process for the life of the run (no daemon), newline-delimited JSON each way. A worker reaches it through
//! `cordata-bus mcp`, a stateless MCP stdio shim its CLI starts, so any MCP-capable assistant can join.
//!
//! Trust lives here, outside the sandbox:
//! - the socket sits under `~/.cordata/bus/`, which a worker's Seatbelt profile cannot read or write: it can connect
//!   to its own run's socket (one `path-literal` allowance) but cannot replace it, bind a fake one, or reach another
//!   run's;
//! - every request carries a per-worker token (256 random bits, handed over in the environment, never argv), and the
//!   caller must run as this user and descend from the process the token was issued to;
//! - the sender of a message is stamped from the token, never taken from the request;
//! - bodies are capped, stripped of control characters and credential-shaped spans; posting is rate-limited;
//! - read results are fenced as data from peers: they cannot grant permission, widen a scope or relay an approval.
//!   There is deliberately no way to type into another worker: only the orchestrator starts or wakes workers.

pub mod mcp;
pub mod peer;

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub const ENV_SOCKET: &str = "CORDATA_BUS_SOCKET";
pub const ENV_TOKEN: &str = "CORDATA_BUS_TOKEN";
/// What a message may be.
pub const KINDS: [&str; 4] = ["note", "question", "answer", "finding"];
pub const MAX_BODY: usize = 4096;
/// A request line longer than this closes the connection.
pub const MAX_LINE: usize = 16 * 1024;
pub const READ_LIMIT: usize = 50;
/// Bodies returned by one read, in characters: under the harness's 6,000-character tool-output offload line.
pub const READ_CHARS: usize = 5500;
pub const MAX_WAIT_MS: u64 = 30_000;
pub const POSTS_PER_MINUTE: f64 = 20.0;
pub const POSTS_PER_WORKER: u32 = 200;
pub const RUN_MESSAGES: usize = 2000;
pub const RUN_BYTES: usize = 1 << 20;
pub const MAX_LEASE_S: f64 = 1800.0;
/// Roles that only read the code may only read the bus and say what they are doing.
pub const READ_ONLY_ROLES: [&str; 2] = ["plan", "audit"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub seq: u64,
    pub ts: f64,
    pub from_step: String,
    pub from_worker: String,
    pub topic: String,
    pub kind: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u64>,
}

/// The process a grant is bound to, filled in once the worker is spawned.
#[derive(Debug, Clone, Default)]
pub struct PidSlot(Arc<AtomicU32>);

impl PidSlot {
    pub fn set(&self, pid: u32) {
        self.0.store(pid, Ordering::SeqCst);
    }
    pub fn get(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Two packets with the same token are the same grant; the slot is shared state, not identity.
impl PartialEq for PidSlot {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// What one worker is handed: where the bus is, its token, and the slot its process id goes into.
#[derive(Debug, Clone, PartialEq)]
pub struct BusGrant {
    pub socket: PathBuf,
    /// `cordata-bus`, for the CLI's MCP config.
    pub command: PathBuf,
    pub token: String,
    pub step_id: String,
    pub pid: PidSlot,
}

impl BusGrant {
    /// The environment the worker's CLI runs with (its MCP server inherits it).
    pub fn env(&self) -> Vec<(String, String)> {
        vec![(ENV_SOCKET.into(), self.socket.display().to_string()), (ENV_TOKEN.into(), self.token.clone())]
    }

    /// The `--mcp-config` document for a CLI: only the bus, by absolute path; nothing secret in it (the token is in
    /// the environment, which the MCP server inherits).
    pub fn mcp_config(&self) -> Value {
        json!({"mcpServers": {"cordata_bus": {"command": self.command.display().to_string(), "args": ["mcp"]}}})
    }
}

struct Grant {
    step_id: String,
    worker: String,
    read_only: bool,
    file_scope: Vec<String>,
    pid: PidSlot,
    bucket: f64,
    refilled: Instant,
    posts: u32,
    revoked: bool,
}

#[derive(Default)]
struct State {
    seq: u64,
    messages: Vec<Message>,
    bytes: usize,
    grants: HashMap<String, Grant>,
    /// step → (what it says it is doing, when)
    statuses: BTreeMap<String, (String, f64)>,
    /// path → (step holding it, until)
    leases: BTreeMap<String, (String, f64)>,
}

type Sink = Box<dyn Fn(&Message) + Send + Sync>;

struct Inner {
    run_id: String,
    socket: PathBuf,
    state: Mutex<State>,
    posted: Notify,
    sink: Mutex<Option<Sink>>,
}

/// A running bus. Dropping it stops the listener and removes the socket's folder.
pub struct Bus {
    inner: Arc<Inner>,
    command: PathBuf,
    dir: PathBuf,
    accept: JoinHandle<()>,
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Random hex from the kernel's generator.
pub fn random_hex(bytes: usize) -> std::io::Result<String> {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// The `cordata-bus` binary: `$CORDATA_BUS_BIN`, beside the running binary (or one folder up, for test builds), or
/// on `PATH`. `None`: the bus is not offered.
pub fn binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CORDATA_BUS_BIN").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(path);
    }
    let exe = std::env::current_exe().ok()?;
    let beside = exe.parent().into_iter().flat_map(|d| [d.join("cordata-bus"), d.join("../cordata-bus")]);
    let on_path = std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).map(|d| d.join("cordata-bus")).collect::<Vec<_>>());
    beside.chain(on_path).find(|p| p.is_file()).and_then(|p| std::fs::canonicalize(p).ok())
}

/// `$CORDATA_HOME/bus`, else `~/.cordata/bus`: inside the folder every worker profile denies.
pub fn default_root() -> PathBuf {
    std::env::var_os("CORDATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cordata"))
        .join("bus")
}

static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9-]{10,}|-----BEGIN [A-Z ]*PRIVATE KEY-----)").unwrap()
});

/// What a body may carry: no control characters but newlines and tabs, no credential-shaped spans, at most `MAX_BODY`
/// bytes (cut on a character boundary).
pub fn clean(body: &str) -> String {
    let text: String = body.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect();
    let text = SECRET.replace_all(&text, "[redacted]").into_owned();
    if text.len() <= MAX_BODY {
        return text;
    }
    let mut cut = MAX_BODY;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

pub fn valid_topic(topic: &str) -> bool {
    (1..=64).contains(&topic.len()) && topic.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._/-".contains(&b))
}

/// A project-relative path inside one of the scope's entries (`src/` covers the folder, `src/a.rs` the file).
pub fn in_scope(path: &str, scope: &[String]) -> bool {
    let path = path.trim_start_matches("./");
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|p| p == "..") {
        return false;
    }
    scope.iter().map(|s| s.trim_start_matches("./")).any(|s| if s.ends_with('/') { path.starts_with(s) } else { path == s || path.starts_with(&format!("{s}/")) })
}

impl Bus {
    /// Starts the run's bus under `root` (`default_root()` outside tests). Needs a tokio runtime.
    pub fn start(root: &Path, run_id: &str, command: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(root)?;
        let dir = root.join(random_hex(8)?);
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        // Seatbelt matches the real path: `/var/folders` is `/private/var/folders`.
        let dir = std::fs::canonicalize(&dir)?;
        let socket = dir.join("bus.sock");
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let inner = Arc::new(Inner { run_id: run_id.into(), socket, state: Mutex::new(State::default()), posted: Notify::new(), sink: Mutex::new(None) });
        let serving = inner.clone();
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let inner = serving.clone();
                tokio::spawn(async move { serve(inner, stream).await });
            }
        });
        Ok(Self { inner, command: command.to_owned(), dir, accept })
    }

    pub fn socket(&self) -> &Path {
        &self.inner.socket
    }

    pub fn run_id(&self) -> &str {
        &self.inner.run_id
    }

    /// Every posted message goes here too (the orchestrator writes them to the ledger).
    pub fn on_message(&self, sink: impl Fn(&Message) + Send + Sync + 'static) {
        *self.inner.sink.lock().unwrap() = Some(Box::new(sink));
    }

    /// A token for one worker on one step.
    pub fn grant(&self, step_id: &str, role: &str, worker: &str, file_scope: &[String]) -> std::io::Result<BusGrant> {
        let token = random_hex(32)?;
        let pid = PidSlot::default();
        self.inner.state.lock().unwrap().grants.insert(
            token.clone(),
            Grant {
                step_id: step_id.into(),
                worker: worker.into(),
                read_only: READ_ONLY_ROLES.contains(&role),
                file_scope: file_scope.to_vec(),
                pid: pid.clone(),
                bucket: POSTS_PER_MINUTE,
                refilled: Instant::now(),
                posts: 0,
                revoked: false,
            },
        );
        Ok(BusGrant { socket: self.inner.socket.clone(), command: self.command.clone(), token, step_id: step_id.into(), pid })
    }

    /// The worker is done: its token stops working and its leases are let go.
    pub fn revoke(&self, token: &str) {
        let mut st = self.inner.state.lock().unwrap();
        let step = st.grants.get_mut(token).map(|g| {
            g.revoked = true;
            g.step_id.clone()
        });
        if let Some(step) = step {
            st.leases.retain(|_, (holder, _)| *holder != step);
        }
    }

    pub fn messages(&self) -> Vec<Message> {
        self.inner.state.lock().unwrap().messages.clone()
    }

    /// The newest messages as a short digest for a prompt (a retried or resumed step), newest last.
    pub fn digest(&self, max_chars: usize) -> Option<String> {
        let st = self.inner.state.lock().unwrap();
        let mut lines: Vec<String> = Vec::new();
        let mut used = 0;
        for m in st.messages.iter().rev() {
            let line = format!("[{} from {} on {}] {}", m.kind, m.from_step, m.topic, m.body.replace('\n', " "));
            if used + line.len() > max_chars {
                break;
            }
            used += line.len();
            lines.push(line);
        }
        lines.reverse();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        self.accept.abort();
        let _ = std::fs::remove_file(&self.inner.socket);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// One line, at most `MAX_LINE` bytes; `None` at the end or past the cap.
async fn read_line(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>, buf: &mut Vec<u8>) -> Option<()> {
    buf.clear();
    loop {
        let available = reader.fill_buf().await.ok()?;
        if available.is_empty() {
            return None;
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let take = newline.map_or(available.len(), |i| i + 1);
        buf.extend_from_slice(&available[..newline.unwrap_or(take)]);
        reader.consume(take);
        if buf.len() > MAX_LINE {
            return None;
        }
        if newline.is_some() {
            return Some(());
        }
    }
}

async fn serve(inner: Arc<Inner>, stream: UnixStream) {
    if !peer::same_user(&stream) {
        return;
    }
    let caller = peer::peer_pid(&stream);
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = Vec::new();
    while read_line(&mut reader, &mut line).await.is_some() {
        let reply = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => handle(&inner, caller, &request).await,
            Err(_) => json!({"ok": false, "error": "not JSON"}),
        };
        let mut out = serde_json::to_vec(&reply).unwrap_or_default();
        out.push(b'\n');
        if write.write_all(&out).await.is_err() {
            break;
        }
    }
}

fn fail(error: impl Into<String>) -> Value {
    json!({"ok": false, "error": error.into()})
}

/// Who is asking: the grant's step and worker, if the token is live and the caller descends from its worker.
fn authorize(inner: &Inner, caller: Option<u32>, token: &str) -> Result<(String, String, bool, Vec<String>), Value> {
    let st = inner.state.lock().unwrap();
    let Some(grant) = st.grants.get(token).filter(|g| !g.revoked) else { return Err(fail("unknown or revoked token")) };
    let bound = grant.pid.get();
    if bound == 0 {
        return Err(fail("this worker has not started yet"));
    }
    match caller {
        Some(pid) if peer::descends_from(pid, bound) => Ok((grant.step_id.clone(), grant.worker.clone(), grant.read_only, grant.file_scope.clone())),
        _ => Err(fail("the caller is not the worker this token was issued to")),
    }
}

async fn handle(inner: &Arc<Inner>, caller: Option<u32>, request: &Value) -> Value {
    let token = request.get("token").and_then(Value::as_str).unwrap_or_default();
    let (step, worker, read_only, scope) = match authorize(inner, caller, token) {
        Ok(who) => who,
        Err(refusal) => return refusal,
    };
    let text = |key: &str| request.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    match request.get("op").and_then(Value::as_str).unwrap_or_default() {
        "post" => {
            if read_only {
                return fail("planning and audit steps read the bus and set their status; they do not post");
            }
            let (topic, kind) = (text("topic"), text("kind"));
            if !valid_topic(&topic) {
                return fail("topic: 1-64 of a-z 0-9 . _ / - (e.g. run, step/s2, api)");
            }
            if !KINDS.contains(&kind.as_str()) {
                return fail("kind: note, question, answer or finding");
            }
            let body = clean(&text("body"));
            if body.trim().is_empty() {
                return fail("empty body");
            }
            let message = {
                let mut st = inner.state.lock().unwrap();
                if st.messages.len() >= RUN_MESSAGES || st.bytes + body.len() > RUN_BYTES {
                    return fail("this run's bus is full");
                }
                let grant = st.grants.get_mut(token).expect("authorized");
                let elapsed = grant.refilled.elapsed().as_secs_f64();
                grant.bucket = (grant.bucket + elapsed * POSTS_PER_MINUTE / 60.0).min(POSTS_PER_MINUTE);
                grant.refilled = Instant::now();
                if grant.posts >= POSTS_PER_WORKER {
                    return fail("this worker has posted its share for the run");
                }
                if grant.bucket < 1.0 {
                    return fail("posting too fast; read, then try again in a few seconds");
                }
                grant.bucket -= 1.0;
                grant.posts += 1;
                st.seq += 1;
                let message = Message {
                    seq: st.seq,
                    ts: now(),
                    from_step: step,
                    from_worker: worker,
                    topic,
                    kind,
                    body,
                    reply_to: request.get("reply_to").and_then(Value::as_u64),
                };
                st.bytes += message.body.len();
                st.messages.push(message.clone());
                message
            };
            if let Some(sink) = inner.sink.lock().unwrap().as_ref() {
                sink(&message);
            }
            inner.posted.notify_waiters();
            json!({"ok": true, "seq": message.seq})
        }
        "read" => {
            let since = request.get("since").and_then(Value::as_u64).unwrap_or(0);
            let topics: Vec<String> = request.get("topics").and_then(Value::as_array).map(|t| t.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            let wait = Duration::from_millis(request.get("wait_ms").and_then(Value::as_u64).unwrap_or(0).min(MAX_WAIT_MS));
            let deadline = Instant::now() + wait;
            loop {
                let posted = inner.posted.notified();
                tokio::pin!(posted);
                posted.as_mut().enable();
                let (batch, last) = collect(inner, &step, since, &topics);
                let left = deadline.saturating_duration_since(Instant::now());
                if !batch.is_empty() || left.is_zero() {
                    return json!({"ok": true, "messages": batch, "next": last});
                }
                let _ = tokio::time::timeout(left, posted).await;
            }
        }
        "status" => {
            let line: String = clean(&text("text")).lines().next().unwrap_or_default().chars().take(200).collect();
            inner.state.lock().unwrap().statuses.insert(step, (line, now()));
            json!({"ok": true})
        }
        "peers" => {
            let st = inner.state.lock().unwrap();
            let t = now();
            let statuses: Vec<Value> = st.statuses.iter().map(|(s, (text, at))| json!({"step": s, "status": text, "age_s": (t - at).round()})).collect();
            let leases: Vec<Value> = st.leases.iter().filter(|(_, (_, until))| *until > t).map(|(p, (s, until))| json!({"path": p, "step": s, "for_s": (until - t).round()})).collect();
            json!({"ok": true, "you": step, "statuses": statuses, "leases": leases})
        }
        "claim" | "release" => {
            if read_only {
                return fail("planning and audit steps change no files");
            }
            let path = text("path").trim_start_matches("./").to_owned();
            if !in_scope(&path, &scope) {
                return fail("only paths inside your own file scope can be claimed; a claim never widens it");
            }
            let mut st = inner.state.lock().unwrap();
            let t = now();
            st.leases.retain(|_, (_, until)| *until > t);
            match st.leases.get(&path) {
                Some((holder, until)) if *holder != step => return fail(format!("held by step {holder} for {:.0}s more", until - t)),
                _ => {}
            }
            if request.get("op").and_then(Value::as_str) == Some("release") {
                st.leases.remove(&path);
                return json!({"ok": true});
            }
            let ttl = request.get("ttl_s").and_then(Value::as_f64).unwrap_or(600.0).clamp(10.0, MAX_LEASE_S);
            st.leases.insert(path, (step, t + ttl));
            json!({"ok": true, "for_s": ttl})
        }
        _ => fail("op: post, read, status, peers, claim or release"),
    }
}

/// Messages after `since` from other steps (on `topics`, when given), within the read caps; and the last seq seen.
fn collect(inner: &Inner, step: &str, since: u64, topics: &[String]) -> (Vec<Message>, u64) {
    let st = inner.state.lock().unwrap();
    let mut out = Vec::new();
    let mut chars = 0;
    let mut last = since;
    for m in st.messages.iter().filter(|m| m.seq > since) {
        last = last.max(m.seq);
        if m.from_step == step || (!topics.is_empty() && !topics.iter().any(|t| m.topic == *t || m.topic.starts_with(&format!("{t}/")))) {
            continue;
        }
        if out.len() >= READ_LIMIT || chars + m.body.len() > READ_CHARS {
            last = m.seq - 1;
            break;
        }
        chars += m.body.len();
        out.push(m.clone());
    }
    (out, last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_lose_control_characters_and_secrets_and_are_capped() {
        assert_eq!(clean("ok\x1b[31m red\x07\nnext"), "ok[31m red\nnext");
        assert_eq!(clean("key sk-ant-REDACTEDREDACTEDREDACTED0123 here"), "key [redacted] here");
        let long = "é".repeat(MAX_BODY);
        assert!(clean(&long).len() <= MAX_BODY + "…".len());
    }

    #[test]
    fn topics_and_scopes() {
        assert!(valid_topic("step/s2") && valid_topic("run") && !valid_topic("Run") && !valid_topic("") && !valid_topic(&"a".repeat(65)));
        let scope = vec!["src/".to_owned(), "Cargo.toml".to_owned()];
        assert!(in_scope("src/a.rs", &scope) && in_scope("./Cargo.toml", &scope));
        assert!(!in_scope("tests/a.rs", &scope) && !in_scope("src/../.env", &scope) && !in_scope("/etc/passwd", &scope));
    }
}
