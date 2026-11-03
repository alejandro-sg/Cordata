//! Model Context Protocol (MCP) stdio client: the servers in `~/.cordata/mcp.json`, and those in the project's
//! `.mcp.json` only once the person has approved that exact server — its folder, name, command, arguments and
//! environment, pinned by a SHA-256 digest in `mcp.approved` of `~/.cordata/settings.json`. A repository is code someone
//! else wrote: opening it must not start its programs, and a `git pull` that changes one must not start the new one. Servers start in parallel, get a scrubbed
//! environment (no provider keys), and die with the session. Every call still asks first (`tools::needs_approval`).

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::tools::ToolSpec;

/// One request's answer; a tool that takes longer than this is reported as failed and its server dropped.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// `initialize` plus `tools/list`: a server that cannot start in this long is skipped, not waited on.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// A reply line longer than this ends the connection rather than the session's memory.
const MAX_LINE: usize = 8 << 20;
/// Provider APIs accept `^[A-Za-z0-9_-]{1,64}$` for a tool name; one bad name fails the whole request.
const MAX_TOOL_NAME: usize = 64;
/// `tools/list` pages read from one server.
const MAX_PAGES: usize = 20;
/// What a server inherits from Cordata's environment; anything else it needs comes from its own `env` block.
const ENV_ALLOW: &[&str] = &["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Why a request failed: a broken or out-of-step stream ends the connection; the server's own error does not.
#[derive(Debug)]
enum Failure {
    Stream(String),
    Remote(String),
}

impl Failure {
    fn text(self) -> String {
        match self {
            Failure::Stream(why) | Failure::Remote(why) => why,
        }
    }
}

struct Connection {
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    child: Child,
    next_id: u64,
}

/// `npx`, `uvx` and shell wrappers start the real server as a grandchild: the whole process group goes, not just the
/// wrapper, or it outlives the session holding the pipe.
impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: killpg only sends a signal; the group is the one this child leads (`process_group(0)`).
            unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
        }
    }
}

impl Connection {
    async fn spawn(config: &McpServerConfig, cwd: &Path) -> Result<Self, String> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args).current_dir(cwd).env_clear();
        for key in ENV_ALLOW {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        cmd.envs(&config.env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0).kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| format!("cannot start `{}`: {e}", config.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut conn = Self { stdin, reader: BufReader::new(stdout), child, next_id: 1 };
        let init = json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "cordata", "version": env!("CARGO_PKG_VERSION")}});
        conn.request("initialize", init).await.map_err(Failure::text)?;
        conn.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.map_err(Failure::text)?;
        Ok(conn)
    }

    async fn send(&mut self, message: &Value) -> Result<(), Failure> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.map_err(|e| Failure::Stream(format!("write failed: {e}")))?;
        self.stdin.flush().await.map_err(|e| Failure::Stream(format!("write failed: {e}")))
    }

    /// One line, read at most [`MAX_LINE`] bytes at a time: a server that never sends a newline ends the connection,
    /// not the session's memory.
    async fn read_line(&mut self, line: &mut Vec<u8>) -> Result<(), Failure> {
        line.clear();
        let n = (&mut self.reader).take(MAX_LINE as u64 + 1).read_until(b'\n', line).await.map_err(|e| Failure::Stream(format!("read failed: {e}")))?;
        match n {
            0 => Err(Failure::Stream("the server closed the connection".into())),
            _ if line.len() > MAX_LINE => Err(Failure::Stream("reply too large".into())),
            _ => Ok(()),
        }
    }

    /// Sends a request and reads until the reply with its id: servers interleave log notifications and their own
    /// requests (`roots/list`, `sampling/…`), which are skipped — or answered "not supported" — rather than taken
    /// for the answer.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Failure> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        let mut line = Vec::new();
        loop {
            self.read_line(&mut line).await?;
            let Ok(message) = serde_json::from_slice::<Value>(&line) else { continue };
            if message.get("method").is_some() {
                if let Some(their_id) = message.get("id") {
                    let refusal = json!({"jsonrpc": "2.0", "id": their_id, "error": {"code": -32601, "message": "not supported by cordata"}});
                    self.send(&refusal).await?;
                }
                continue;
            }
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(Failure::Remote(error.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_owned()));
            }
            return message.get("result").cloned().ok_or_else(|| Failure::Stream("reply without a result".into()));
        }
    }

    /// Every page of `tools/list` (`nextCursor`), up to a sane number of pages.
    async fn list_tools(&mut self) -> Result<Vec<Value>, Failure> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = cursor.as_ref().map_or_else(|| json!({}), |c| json!({"cursor": c}));
            let page = self.request("tools/list", params).await?;
            tools.extend(page.get("tools").and_then(Value::as_array).cloned().unwrap_or_default());
            cursor = page.get("nextCursor").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }
}

/// The servers this session runs and the tools they offer, under provider-safe names `mcp__<server>__<tool>`.
#[derive(Default)]
pub(crate) struct McpManager {
    connections: HashMap<String, Arc<Mutex<Option<Connection>>>>,
    /// Exposed name → (server, the tool's own name).
    tool_map: HashMap<String, (String, String)>,
    tools: Vec<ToolSpec>,
}

/// What starting the servers did, for the person to see.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Startup {
    pub started: Vec<(String, usize)>,
    pub failed: Vec<(String, String)>,
    /// Project servers left off until the person approves them.
    pub held: Vec<Held>,
}

impl Startup {
    pub fn notices(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.started.is_empty() {
            let list = self.started.iter().map(|(name, n)| format!("{name} ({n})")).collect::<Vec<_>>().join(", ");
            out.push(format!("mcp: started {list}"));
        }
        for (name, why) in &self.failed {
            out.push(format!("mcp: {name} did not start: {why}"));
        }
        for held in &self.held {
            let why = if held.changed { "changed since you approved it" } else { "not approved yet" };
            out.push(format!("mcp: this project's server `{}` ({}) is {why}, so it did not start. Approve it in Settings → MCP.", held.name, held.command_line()));
        }
        out
    }
}

/// A project server that did not start: what it would run, and whether an earlier approval no longer matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub name: String,
    pub config: McpServerConfig,
    /// The `mcp.approved` entry that would let it start.
    pub approval: String,
    /// An approval exists for this folder and name, for a different command, arguments or environment.
    pub changed: bool,
}

impl Held {
    /// The command as it would run, with the names (never the values) of the variables it sets.
    pub fn command_line(&self) -> String {
        let mut line = std::iter::once(self.config.command.as_str()).chain(self.config.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ");
        if !self.config.env.is_empty() {
            let mut keys: Vec<&str> = self.config.env.keys().map(String::as_str).collect();
            keys.sort_unstable();
            line.push_str(&format!(" (sets {})", keys.join(", ")));
        }
        line
    }
}

/// The `mcp.approved` entry for one project server: `<folder>|<name>|sha256:<digest of command, args and env>`. The
/// folder and name stay readable so the person can see and prune what they approved; the digest pins the rest, env
/// values included, without writing them into the settings file.
pub fn approval(cwd: &Path, name: &str, config: &McpServerConfig) -> String {
    use sha2::{Digest as _, Sha256};
    let env: std::collections::BTreeMap<&String, &String> = config.env.iter().collect();
    let canonical = json!({"command": config.command, "args": config.args, "env": env}).to_string();
    let digest = Sha256::digest(canonical.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}|{name}|sha256:{hex}", folder_of(cwd))
}

fn folder_of(cwd: &Path) -> String {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()).display().to_string()
}

/// The person's approvals, from the user settings file only: a repository cannot approve its own servers.
pub fn approved(home: Option<&Path>) -> Vec<String> {
    let Some(home) = home else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(home.join("settings.json")) else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    value.pointer("/mcp/approved").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// The servers a session in `cwd` would run — the person's own, then each project server whose exact configuration
/// they approved — and the project servers held back.
pub fn configured(home: Option<&Path>, cwd: &Path, approvals: &[String]) -> (Vec<(String, McpServerConfig)>, Vec<Held>) {
    let mut servers: Vec<(String, McpServerConfig)> = Vec::new();
    if let Some(home) = home
        && let Ok(text) = std::fs::read_to_string(home.join("mcp.json"))
    {
        for (name, config) in parse_configs(&text) {
            if !servers.iter().any(|(n, _)| *n == name) {
                servers.push((name, config));
            }
        }
    }
    let mut held = Vec::new();
    if let Ok(text) = std::fs::read_to_string(cwd.join(".mcp.json")) {
        let folder = folder_of(cwd);
        for (name, config) in parse_configs(&text) {
            // First wins: the person's own entry is never replaced by a repository's server of the same name.
            if servers.iter().any(|(n, _)| *n == name) {
                continue;
            }
            let entry = approval(cwd, &name, &config);
            if approvals.contains(&entry) {
                servers.push((name, config));
            } else {
                let prefix = format!("{folder}|{name}|");
                let changed = approvals.iter().any(|a| a.starts_with(&prefix));
                held.push(Held { name, config, approval: entry, changed });
            }
        }
    }
    (servers, held)
}

/// `mcp__<server>__<tool>`, every character a provider rejects replaced, cut to the length they accept.
fn exposed_name(server: &str, tool: &str) -> String {
    let safe = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    let mut name = format!("mcp__{}__{}", safe(server), safe(tool));
    name.truncate(MAX_TOOL_NAME);
    name
}

impl McpManager {
    /// Starts every configured server at once and lists their tools; a server that fails is reported, not fatal.
    pub(crate) async fn start(servers: Vec<(String, McpServerConfig)>, cwd: &Path) -> (Self, Startup) {
        let starts = servers.into_iter().map(|(name, config)| async move {
            let result = timeout(START_TIMEOUT, async {
                let mut conn = Connection::spawn(&config, cwd).await?;
                let listed = conn.list_tools().await.map_err(Failure::text)?;
                Ok::<_, String>((conn, listed))
            })
            .await
            .unwrap_or_else(|_| Err(format!("no answer in {}s", START_TIMEOUT.as_secs())));
            (name, result)
        });
        let mut manager = Self::default();
        let mut report = Startup::default();
        for (server, result) in futures::future::join_all(starts).await {
            let (conn, listed) = match result {
                Ok(ok) => ok,
                Err(why) => {
                    report.failed.push((server, why));
                    continue;
                }
            };
            let mut count = 0;
            for tool in &listed {
                let Some(own) = tool.get("name").and_then(Value::as_str) else { continue };
                let name = exposed_name(&server, own);
                if manager.tool_map.contains_key(&name) {
                    report.failed.push((server.clone(), format!("tool `{own}` skipped: its name `{name}` is already taken")));
                    continue;
                }
                let description = tool.get("description").and_then(Value::as_str).unwrap_or("MCP tool");
                let schema = tool.get("inputSchema").cloned().filter(Value::is_object).unwrap_or_else(|| json!({"type": "object"}));
                manager.tool_map.insert(name.clone(), (server.clone(), own.to_owned()));
                manager.tools.push(ToolSpec { name: Cow::Owned(name), description: Cow::Owned(format!("[{server}] {description}")), schema });
                count += 1;
            }
            manager.connections.insert(server.clone(), Arc::new(Mutex::new(Some(conn))));
            report.started.push((server, count));
        }
        (manager, report)
    }

    pub(crate) fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub(crate) fn has_tool(&self, name: &str) -> bool {
        self.tool_map.contains_key(name)
    }

    pub(crate) async fn call_tool(&self, name: &str, args: &Value) -> Result<String, String> {
        let (server, own) = self.tool_map.get(name).ok_or_else(|| format!("unknown MCP tool `{name}`"))?;
        let slot = self.connections.get(server).ok_or_else(|| format!("MCP server `{server}` is not running"))?;
        let mut guard = slot.lock().await;
        let conn = guard.as_mut().ok_or_else(|| format!("MCP server `{server}` stopped after an earlier failure"))?;
        let result = match timeout(CALL_TIMEOUT, conn.request("tools/call", json!({"name": own, "arguments": args}))).await {
            Ok(Ok(result)) => result,
            // A timed-out or broken stream is out of step with its replies: drop the server rather than misread one.
            Ok(Err(Failure::Stream(why))) => {
                *guard = None;
                return Err(why);
            }
            Ok(Err(Failure::Remote(why))) => return Err(why),
            Err(_) => {
                *guard = None;
                return Err(format!("no answer in {}s; the server was stopped", CALL_TIMEOUT.as_secs()));
            }
        };
        let text: Vec<&str> = result.get("content").and_then(Value::as_array).into_iter().flatten().filter_map(|item| item.get("text").and_then(Value::as_str)).collect();
        let output = if text.is_empty() { result.to_string() } else { text.join("\n") };
        if result.get("isError").and_then(Value::as_bool).unwrap_or(false) { Err(output) } else { Ok(output) }
    }
}

/// `{"mcpServers": {name: {command, args, env}}}`, or the bare map.
fn parse_configs(content: &str) -> Vec<(String, McpServerConfig)> {
    let Ok(value) = serde_json::from_str::<Value>(content) else { return Vec::new() };
    let Some(servers) = value.get("mcpServers").and_then(Value::as_object).or_else(|| value.as_object()) else { return Vec::new() };
    servers
        .iter()
        .filter(|(name, _)| *name != "mcpServers")
        .filter_map(|(name, spec)| serde_json::from_value::<McpServerConfig>(spec.clone()).ok().filter(|c| !c.command.trim().is_empty()).map(|c| (name.clone(), c)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stdio MCP server in shell: logs a notification and asks the client something before each reply.
    const FAKE: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"hi"}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":"srv-1","method":"roots/list"}'
  case "$line" in
    *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo.v2","description":"echo","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"env=%s"}]}}\n' "$id" "${CORDATA_MCP_TEST_SECRET:-none}" ;;
    *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#;

    fn fake(dir: &Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-mcp.sh");
        std::fs::write(&path, FAKE).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    #[test]
    fn parses_both_shapes() {
        let wrapped = r#"{"mcpServers": {"rea": {"command": "rea", "args": ["serve"]}, "gh": {"command": "npx", "env": {"T": "x"}}, "bad": {"command": ""}}}"#;
        let got = parse_configs(wrapped);
        let mut names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["gh", "rea"]);
        assert_eq!(parse_configs(r#"{"rea": {"command": "rea"}}"#).len(), 1);
    }

    #[test]
    fn a_project_server_starts_only_as_approved_and_a_change_holds_it_back() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("mcp.json"), r#"{"mcpServers": {"mine": {"command": "a"}}}"#).unwrap();
        let project = |cmd: &str| std::fs::write(cwd.path().join(".mcp.json"), format!(r#"{{"mcpServers": {{"tool": {{"command": "{cmd}", "env": {{"K": "v"}}}}, "mine": {{"command": "b"}}}}}}"#)).unwrap();
        project("node");
        let (servers, held) = configured(Some(home.path()), cwd.path(), &approved(Some(home.path())));
        assert_eq!(servers.iter().map(|(n, s)| (n.as_str(), s.command.as_str())).collect::<Vec<_>>(), [("mine", "a")], "the person's own entry wins");
        assert_eq!(held.len(), 1);
        assert!(!held[0].changed);
        assert_eq!(held[0].command_line(), "node (sets K)", "env names, never values");
        // A repository's own settings cannot approve it.
        std::fs::create_dir_all(cwd.path().join(".cordata")).unwrap();
        std::fs::write(cwd.path().join(".cordata/settings.json"), json!({"mcp": {"approved": [held[0].approval]}}).to_string()).unwrap();
        assert!(approved(Some(&cwd.path().join(".cordata"))).len() == 1 && configured(Some(home.path()), cwd.path(), &approved(Some(home.path()))).1.len() == 1);
        // The person approves it: it starts.
        std::fs::write(home.path().join("settings.json"), json!({"mcp": {"approved": [held[0].approval]}}).to_string()).unwrap();
        let (servers, held) = configured(Some(home.path()), cwd.path(), &approved(Some(home.path())));
        assert!(held.is_empty() && servers.iter().any(|(n, _)| n == "tool"));
        // A pull changes its command: held back again, marked changed.
        project("curl");
        let (servers, held) = configured(Some(home.path()), cwd.path(), &approved(Some(home.path())));
        assert!(!servers.iter().any(|(n, _)| n == "tool"));
        assert!(held[0].changed, "{held:?}");
        // So does a changed environment value, though only names are shown.
        let a = McpServerConfig { command: "x".into(), args: vec![], env: HashMap::from([("K".into(), "1".into())]) };
        let b = McpServerConfig { env: HashMap::from([("K".into(), "2".into())]), ..a.clone() };
        assert_ne!(approval(cwd.path(), "s", &a), approval(cwd.path(), "s", &b));
        // An approval is for one folder: a nested checkout or worktree is asked again.
        assert_ne!(approval(cwd.path(), "s", &a), approval(&cwd.path().join("sub"), "s", &a));
        assert!(configured(None, cwd.path(), &[]).0.is_empty(), "no home, no servers: tests never start the person's own");
    }

    #[test]
    fn names_are_what_providers_accept() {
        assert_eq!(exposed_name("my.server", "echo.v2"), "mcp__my_server__echo_v2");
        assert_eq!(exposed_name(&"s".repeat(80), "t").len(), MAX_TOOL_NAME);
    }

    #[tokio::test]
    async fn talks_past_notifications_and_server_requests_without_leaking_keys() {
        let dir = tempfile::tempdir().unwrap();
        let config = McpServerConfig { command: fake(dir.path()), args: vec![], env: HashMap::new() };
        let missing = McpServerConfig { command: "/nonexistent/mcp".into(), args: vec![], env: HashMap::new() };
        // SAFETY: a test-only variable no other test reads.
        unsafe { std::env::set_var("CORDATA_MCP_TEST_SECRET", "leak") };
        let (manager, report) = McpManager::start(vec![("fake".into(), config), ("gone".into(), missing)], dir.path()).await;
        assert_eq!(report.started, [("fake".to_owned(), 1)]);
        assert_eq!(report.failed.len(), 1);
        assert!(manager.has_tool("mcp__fake__echo_v2"));
        assert!(!manager.has_tool("echo.v2"), "no unprefixed alias");
        let out = manager.call_tool("mcp__fake__echo_v2", &json!({})).await.unwrap();
        assert_eq!(out, "env=none", "the server inherits only the allowlisted environment");
    }

    #[tokio::test]
    async fn a_wrappers_grandchild_dies_with_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let wrapper = dir.path().join("wrap.sh");
        std::fs::write(&wrapper, format!("#!/bin/sh\nsleep 300 &\necho $! > {}\nexec {}\n", pidfile.display(), fake(dir.path()))).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = McpServerConfig { command: wrapper.display().to_string(), args: vec![], env: HashMap::new() };
        let (manager, report) = McpManager::start(vec![("w".into(), config)], dir.path()).await;
        assert_eq!(report.started.len(), 1, "{report:?}");
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        // SAFETY: signal 0 only probes whether the process exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "the grandchild runs while the session does");
        drop(manager);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "and is gone with it");
    }

    #[tokio::test]
    async fn a_reply_with_no_end_is_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let flood = dir.path().join("flood.sh");
        std::fs::write(&flood, "#!/bin/sh\nread line\nexec head -c 20000000 /dev/zero\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&flood, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = McpServerConfig { command: flood.display().to_string(), args: vec![], env: HashMap::new() };
        let (_, report) = McpManager::start(vec![("f".into(), config)], dir.path()).await;
        assert!(report.failed[0].1.contains("too large"), "{report:?}");
    }

    /// Real servers (`npx`, `uvx`) take a second or more to answer `initialize`; the old start waited on each in turn.
    #[tokio::test]
    #[ignore]
    async fn bench_startup() {
        let dir = tempfile::tempdir().unwrap();
        let slow = dir.path().join("slow.sh");
        std::fs::write(&slow, format!("#!/bin/sh\nsleep 0.5\nexec {}\n", fake(dir.path()))).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let servers = |n: usize| (0..n).map(|i| (format!("s{i}"), McpServerConfig { command: slow.display().to_string(), args: vec![], env: HashMap::new() })).collect::<Vec<_>>();
        let start = std::time::Instant::now();
        let (m, _) = McpManager::start(servers(5), dir.path()).await;
        let parallel = start.elapsed().as_secs_f64() * 1e3;
        let start = std::time::Instant::now();
        for one in servers(5) {
            drop(McpManager::start(vec![one], dir.path()).await);
        }
        let sequential = start.elapsed().as_secs_f64() * 1e3;
        println!("BENCH mcp start 5 servers (0.5s init each): parallel {parallel:.0}ms | one at a time (before) {sequential:.0}ms | tools {}", m.tools().len());
    }
}
