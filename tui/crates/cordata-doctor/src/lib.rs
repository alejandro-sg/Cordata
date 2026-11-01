//! Ported from `backend/orchestrator/doctor.py`: is everything this tool leans on actually there, and what is the
//! one command that fixes each gap? Every check is local; a key is only ever asked about ("is there one?").

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
    pub fix: String,
}

fn check(name: &str, level: Level, detail: impl Into<String>, fix: &str) -> Check {
    Check { name: name.into(), level, detail: detail.into(), fix: fix.into() }
}

// ---------------------------------------------------------------- helpers

/// stdout (or stderr) of a successful command, or `None`; never waits longer than the OS does for a local binary.
fn run(argv: &[&str]) -> Option<String> {
    let out = Command::new(argv[0]).args(&argv[1..]).stdin(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    Some(String::from_utf8_lossy(&text).trim().to_owned())
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| Path::new(d).join(name)).find(|p| p.is_file())
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn cordata_home() -> PathBuf {
    std::env::var_os("CORDATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".cordata"))
}

fn which(name: &str, env_var: &str, fallback: &str) -> Option<PathBuf> {
    if let Some(over) = std::env::var_os(env_var).map(PathBuf::from).filter(|p| p.exists()) {
        return Some(over);
    }
    on_path(name).or_else(|| Some(home().join(fallback)).filter(|p| p.exists()))
}

/// Does anything answer HTTP at `url`? Any reply below 500 counts (a 401 from a daemon wanting its token is a
/// daemon). Plain `http://host:port/path` only — these are loopback probes.
fn ping(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let (authority, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(a, p)| (a, format!("/{p}")));
    let host_port = if authority.contains(':') { authority.to_owned() } else { format!("{authority}:80") };
    let Some(addr) = host_port.to_socket_addrs().ok().and_then(|mut a| a.next()) else { return false };
    let timeout = Duration::from_millis(1500);
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, timeout) else { return false };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    if write!(stream, "GET {path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut head = [0u8; 16];
    let mut got = 0;
    while got < head.len() {
        match stream.read(&mut head[got..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    std::str::from_utf8(&head[..got])
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u16>().ok())
        .is_some_and(|status| (200..500).contains(&status))
}

/// The checkout holding `backend/orchestrator`, found from `ACP_TUI_BACKEND_ROOT`, the binary, or the cwd.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let mut starts: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("ACP_TUI_BACKEND_ROOT") {
        starts.push(PathBuf::from(root));
    }
    if let Ok(exe) = std::env::current_exe() {
        starts.extend(exe.parent().map(Path::to_path_buf));
    }
    starts.push(cwd.to_path_buf());
    starts.into_iter().find_map(|start| start.ancestors().find(|d| d.join("backend/orchestrator").is_dir()).map(Path::to_path_buf))
}

// ---------------------------------------------------------------- settings (the keys doctor reads)

/// `user` < `project` < `local`, objects merged key by key (docs/settings.md).
pub struct Settings {
    merged: Value,
    pub warnings: Vec<String>,
}

impl Settings {
    pub fn load(cwd: &Path) -> Self {
        let files = [cordata_home().join("settings.json"), cwd.join(".cordata/settings.json"), cwd.join(".cordata/settings.local.json")];
        let mut merged = Value::Object(Default::default());
        let mut warnings = Vec::new();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&text) {
                Ok(value @ Value::Object(_)) => merge(&mut merged, value),
                Ok(_) => warnings.push(format!("{}: top level must be an object, ignored", path.display())),
                Err(e) => warnings.push(format!("{}: not valid JSON, ignored ({e})", path.display())),
            }
        }
        Self { merged, warnings }
    }

    /// `a.b` looks up `{"a": {"b": …}}`, and also a literal `"a.b"` key.
    pub fn get(&self, dotted: &str) -> Option<&Value> {
        if let Some(v) = self.merged.get(dotted) {
            return Some(v);
        }
        dotted.split('.').try_fold(&self.merged, |v, part| v.get(part))
    }
}

fn merge(into: &mut Value, from: Value) {
    match (into, from) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                merge(a.entry(k).or_insert(Value::Null), v);
            }
        }
        (slot, v) => *slot = v,
    }
}

// ---------------------------------------------------------------- the checks

pub fn check_python(repo: Option<&Path>) -> Check {
    let Some(repo) = repo else {
        return check("python backend", Level::Ok, "no checkout found; native sessions need none", "");
    };
    let python = repo.join("backend/.venv/bin/python");
    if !python.exists() {
        // The native agent runs without it; the daemon, hub and `cordata run` still use it.
        return check("python backend", Level::Warn, format!("no virtualenv at {} — native sessions work; the daemon and `cordata run` need it", python.display()), "make setup");
    }
    let version = run(&[&python.to_string_lossy(), "--version"]).unwrap_or_else(|| "?".into());
    check("python backend", Level::Ok, version, "")
}

pub fn check_tui(repo: Option<&Path>) -> Check {
    let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("acp-tui"))).filter(|p| p.exists());
    if let Some(p) = beside {
        return check("terminal client", Level::Ok, format!("acp-tui ({})", p.display()), "");
    }
    for kind in ["release", "debug"] {
        if repo.is_some_and(|r| r.join("tui/target").join(kind).join("acp-tui").exists()) {
            return check("terminal client", Level::Ok, format!("acp-tui ({kind} build)"), "");
        }
    }
    check("terminal client", Level::Fail, "acp-tui is not built", "make tui-build")
}

pub fn check_cli_linked() -> Check {
    match on_path("cordata") {
        Some(p) => check("cordata on PATH", Level::Ok, p.display().to_string(), ""),
        None => check("cordata on PATH", Level::Warn, "`cordata` is not on PATH — only runnable from the repo's bin/", "make cli   (links cordata into ~/.local/bin)"),
    }
}

pub fn check_claude() -> Check {
    match which("claude", "CORDATA_CLAUDE_BIN", ".local/bin/claude") {
        None => check("Claude Code CLI", Level::Warn, "not installed — its pool stays out of the roster", "install Claude Code, then sign in once with `claude`"),
        Some(bin) => {
            let v = run(&[&bin.to_string_lossy(), "--version"]).unwrap_or_else(|| "unknown version".into());
            check("Claude Code CLI", Level::Ok, format!("{v} ({})", bin.display()), "")
        }
    }
}

pub fn check_agy() -> Check {
    match which("agy", "CORDATA_AGY_BIN", ".local/bin/agy") {
        None => check("Antigravity CLI (agy)", Level::Warn, "not installed — its pool stays out of the roster", "install the Antigravity CLI, then sign in once with `agy`"),
        Some(bin) => {
            let v = run(&[&bin.to_string_lossy(), "--version"]).unwrap_or_else(|| "unknown version".into());
            check("Antigravity CLI (agy)", Level::Ok, format!("{} ({})", v.lines().next().unwrap_or(""), bin.display()), "")
        }
    }
}

pub fn check_ollama() -> Check {
    let base = std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let base = base.trim_end_matches('/');
    if ping(&format!("{base}/api/version")) {
        return check("Ollama", Level::Ok, format!("answering at {base}"), "");
    }
    if on_path("ollama").is_some() {
        return check("Ollama", Level::Warn, "installed but not answering — the Ollama workers are out of the roster", "ollama serve   (or open the Ollama app)");
    }
    check("Ollama", Level::Warn, "not installed — the Ollama workers are out of the roster", "https://ollama.com/download")
}

/// `MODEL_PROVIDER` from the environment, else from `backend/.env` (that one key only).
fn configured_provider(repo: Option<&Path>) -> String {
    let mut provider = std::env::var("MODEL_PROVIDER").unwrap_or_default().trim().to_owned();
    if provider.is_empty() {
        if let Some(text) = repo.and_then(|r| std::fs::read_to_string(r.join("backend/.env")).ok()) {
            for line in text.lines() {
                if let Some((name, value)) = line.split_once('=') {
                    if name.trim() == "MODEL_PROVIDER" {
                        provider = value.trim().trim_matches(['"', '\'']).to_owned();
                    }
                }
            }
        }
    }
    if provider.is_empty() { "local".into() } else { provider }
}

pub fn check_model_key(repo: Option<&Path>) -> Check {
    const FIX: &str = "make key   (hidden prompt, stored in the Keychain, never in a file)";
    let provider = configured_provider(repo);
    let needs_key = matches!(provider.as_str(), "openai_compatible" | "anthropic");
    if ["OLLAMA_API_KEY", "CLOUD_OPENAI_API_KEY", "ANTHROPIC_API_KEY"].iter().any(|n| std::env::var(n).is_ok_and(|v| !v.is_empty())) {
        return check("model API key", Level::Ok, "found in the environment", "");
    }
    if on_path("security").is_some() || Path::new("/usr/bin/security").exists() {
        for service in ["cordata-ollama-api-key", "cordata-anthropic-api-key", "sierralab-ollama-api-key", "sierralab-anthropic-api-key"] {
            // Only whether an entry exists: no -w / -g, so the secret itself is never read.
            let found = Command::new("/usr/bin/security")
                .args(["find-generic-password", "-s", service])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if found {
                return check("model API key", Level::Ok, format!("in the macOS Keychain ({service})"), "");
            }
        }
    }
    if needs_key {
        return check("model API key", Level::Fail, format!("MODEL_PROVIDER={provider} needs a key and none was found — `cordata` cannot open a conversation"), FIX);
    }
    check("model API key", Level::Warn, format!("no key for a cloud model provider (fine: MODEL_PROVIDER={provider} does not need one)"), FIX)
}

pub fn check_daemon() -> Check {
    let path = cordata_home().join("daemon.json");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let port = serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("port")?.as_u64());
        if let Some(port) = port.filter(|p| ping(&format!("http://127.0.0.1:{p}/health"))) {
            return check("daemon", Level::Ok, format!("running on port {port} — the terminal, Discord and the menu bar share its conversations"), "");
        }
        return check("daemon", Level::Warn, "configured but not answering — the terminal runs standalone", "make daemon-status   (make daemon-install keeps it up)");
    }
    check("daemon", Level::Warn, "not set up — the terminal runs standalone (fine for one terminal)", "make daemon-install")
}

pub fn check_ripgrep() -> Check {
    match on_path("rg") {
        Some(p) => check("ripgrep", Level::Ok, p.display().to_string(), ""),
        None => check("ripgrep", Level::Ok, "not installed; the native code_search tool has its own searcher", ""),
    }
}

pub fn check_git(cwd: &Path) -> Check {
    if on_path("git").is_none() {
        return check("git", Level::Fail, "git is not installed — checkpoints and the orchestrator need it", "xcode-select --install");
    }
    let dir = cwd.to_string_lossy();
    if run(&["git", "-C", &dir, "rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true") {
        let branch = run(&["git", "-C", &dir, "rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "?".into());
        return check("git", Level::Ok, format!("{dir} is a repository ({branch})"), "");
    }
    check("git", Level::Warn, format!("{dir} is not a git repository — undo and the orchestrator's rollback have nothing to lean on"), "git init")
}

pub fn check_mcp_servers(path: &Path) -> Check {
    if !path.exists() {
        return check("MCP servers", Level::Ok, "none configured", "");
    }
    let parsed = std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()));
    let value = match parsed {
        Ok(v) => v,
        Err(e) => return check("MCP servers", Level::Fail, format!("mcp_servers.json is not valid JSON ({e})"), &format!("edit {}", path.display())),
    };
    let servers = value.get("mcpServers").and_then(Value::as_object).cloned().unwrap_or_default();
    let missing = |p: &str| Path::new(p).is_absolute() && !Path::new(p).exists();
    let mut broken = Vec::new();
    for (name, server) in &servers {
        if let Some(cmd) = server.get("command").and_then(Value::as_str).filter(|c| missing(c)) {
            broken.push(format!("{name} → {cmd}"));
        }
        for arg in server.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if [".py", ".js", ".mjs", ".ts"].iter().any(|e| arg.ends_with(e)) && missing(arg) {
                broken.push(format!("{name} → {arg}"));
            }
        }
    }
    if broken.is_empty() {
        return check("MCP servers", Level::Ok, format!("{} configured, every path exists", servers.len()), "");
    }
    check("MCP servers", Level::Fail, format!("a path no longer exists (moved or renamed repo?): {}", broken.join("; ")), &format!("fix the paths in {}", path.display()))
}

pub fn check_pools(settings: &Settings) -> Check {
    let n = settings.get("orchestrator.pools").and_then(|v| v.as_array().map(Vec::len).or_else(|| v.as_str().map(|_| 1)));
    match n.filter(|n| *n > 0) {
        Some(n) => check("quota pools", Level::Ok, format!("{n} set in settings"), ""),
        None => check(
            "quota pools",
            Level::Warn,
            "using the conservative built-in caps — set `orchestrator.pools` to your actual plans so handoffs happen at the right moment",
            "see docs/orchestrator.md (\"Using it\")",
        ),
    }
}

pub fn check_vault(settings: &Settings) -> Check {
    if settings.get("vault.enabled").and_then(Value::as_bool).unwrap_or(false) {
        return check("agent vault", Level::Ok, "on — tool subprocesses reach the network through the credential-substituting proxy", "");
    }
    if std::env::var_os("CORDATA_VAULT_DIRECT").is_some_and(|v| !v.is_empty()) {
        return check("agent vault", Level::Warn, "off, and direct egress is allowed for orchestrator runs (CORDATA_VAULT_DIRECT)", "");
    }
    check("agent vault", Level::Warn, "off — `cordata run` refuses to start the coding CLIs without it unless you pass --allow-direct", "set vault.enabled and start the vault, or run with --allow-direct while developing")
}

pub fn check_settings(settings: &Settings) -> Check {
    match settings.warnings.first() {
        Some(first) => check("settings files", Level::Warn, format!("{} problem(s): {first}", settings.warnings.len()), "/config in the terminal client shows which file"),
        None => check("settings files", Level::Ok, "valid", ""),
    }
}

pub fn run_checks(cwd: &Path) -> Vec<Check> {
    let repo = repo_root(cwd);
    let repo = repo.as_deref();
    let settings = Settings::load(cwd);
    let mcp = repo.map_or_else(|| PathBuf::from("backend/mcp_servers.json"), |r| r.join("backend/mcp_servers.json"));
    vec![
        check_python(repo),
        check_tui(repo),
        check_cli_linked(),
        check_claude(),
        check_agy(),
        check_ollama(),
        check_model_key(repo),
        check_daemon(),
        check_ripgrep(),
        check_git(cwd),
        check_mcp_servers(&mcp),
        check_pools(&settings),
        check_vault(&settings),
        check_settings(&settings),
    ]
}

pub fn render(checks: &[Check], color: bool) -> String {
    let width = checks.iter().map(|c| c.name.chars().count()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for c in checks {
        let (mark, code) = match c.level {
            Level::Ok => ("✓", "32"),
            Level::Warn => ("⚠", "33"),
            Level::Fail => ("✗", "31"),
        };
        let mark = if color { format!("\x1b[{code}m{mark}\x1b[0m") } else { mark.to_owned() };
        lines.push(format!("  {mark} {:<width$}  {}", c.name, c.detail));
        if !c.fix.is_empty() && c.level != Level::Ok {
            lines.push(format!("    {:width$}  → {}", "", c.fix));
        }
    }
    let failed = checks.iter().filter(|c| c.level == Level::Fail).count();
    let warned = checks.iter().filter(|c| c.level == Level::Warn).count();
    let summary = if failed + warned == 0 { "everything this tool leans on is in place".to_owned() } else { format!("{failed} problem(s), {warned} to look at") };
    format!("cordata doctor\n{}\n\n{summary}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(user: &str, project: &str) -> (tempfile::TempDir, Settings) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("settings.json"), user).unwrap();
        let proj = dir.path().join("proj/.cordata");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("settings.json"), project).unwrap();
        // `Settings::load` reads CORDATA_HOME; tests in this module set it once, to one value per process.
        let merged = {
            let mut m = Value::Object(Default::default());
            let mut warnings = Vec::new();
            for text in [user, project] {
                match serde_json::from_str::<Value>(text) {
                    Ok(v) => merge(&mut m, v),
                    Err(e) => warnings.push(format!("not valid JSON, ignored ({e})")),
                }
            }
            Settings { merged: m, warnings }
        };
        (dir, merged)
    }

    #[test]
    fn settings_merge_and_dotted_lookup() {
        let (_d, s) = settings_with(r#"{"vault":{"enabled":false},"orchestrator":{"pools":[1]}}"#, r#"{"vault":{"enabled":true}}"#);
        assert_eq!(s.get("vault.enabled"), Some(&Value::Bool(true)));
        assert_eq!(check_vault(&s).level, Level::Ok);
        assert_eq!(check_pools(&s).detail, "1 set in settings");
        let (_d, flat) = settings_with(r#"{"vault.enabled": true}"#, "{}");
        assert_eq!(check_vault(&flat).level, Level::Ok);
        let (_d, none) = settings_with("{}", "{}");
        assert_eq!(check_pools(&none).level, Level::Warn);
    }

    #[test]
    fn invalid_settings_are_reported_not_fatal() {
        let (_d, s) = settings_with("{nope", "{}");
        let c = check_settings(&s);
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.starts_with("1 problem(s)"));
    }

    #[test]
    fn mcp_paths_that_vanished_fail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mcp.json");
        assert_eq!(check_mcp_servers(&p).detail, "none configured");
        std::fs::write(&p, r#"{"mcpServers":{"a":{"command":"/no/such/bin","args":["/no/such/x.py","--flag"]},"b":{"command":"npx"}}}"#).unwrap();
        let c = check_mcp_servers(&p);
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("a → /no/such/bin") && c.detail.contains("a → /no/such/x.py"));
        std::fs::write(&p, r#"{"mcpServers":{"b":{"command":"npx"}}}"#).unwrap();
        assert_eq!(check_mcp_servers(&p).detail, "1 configured, every path exists");
        std::fs::write(&p, "{").unwrap();
        assert_eq!(check_mcp_servers(&p).level, Level::Fail);
    }

    #[test]
    fn git_check_tells_repositories_from_folders() {
        let dir = tempfile::tempdir().unwrap();
        if on_path("git").is_none() {
            return;
        }
        assert_eq!(check_git(dir.path()).level, Level::Warn);
        Command::new("git").args(["init", "-q"]).current_dir(dir.path()).status().unwrap();
        assert_eq!(check_git(dir.path()).level, Level::Ok);
    }

    #[test]
    fn ping_answers_for_a_listening_server_and_not_for_a_dead_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf);
            s.write_all(b"HTTP/1.0 401 Unauthorized\r\n\r\n").unwrap();
        });
        assert!(ping(&format!("http://127.0.0.1:{port}/health")));
        server.join().unwrap();
        assert!(!ping(&format!("http://127.0.0.1:{port}/health")));
        assert!(!ping("https://example.test/"));
    }

    #[test]
    fn render_matches_the_python_layout() {
        let checks = vec![check("git", Level::Ok, "fine", ""), check("Ollama", Level::Warn, "down", "ollama serve")];
        assert_eq!(render(&checks, false), "cordata doctor\n  ✓ git     fine\n  ⚠ Ollama  down\n            → ollama serve\n\n0 problem(s), 1 to look at");
        assert!(render(&checks[..1], false).ends_with("everything this tool leans on is in place"));
    }
}
