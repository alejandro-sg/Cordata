//! Is everything this tool leans on actually there, and what is the
//! one command that fixes each gap? Every check is local; a key is only ever asked about ("is there one?").

pub mod docs;

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
    // `localhost` resolves to ::1 first on macOS while Ollama listens on 127.0.0.1: try every address, not the first.
    let Ok(addrs) = host_port.to_socket_addrs() else { return false };
    let timeout = Duration::from_millis(1500);
    let Some(mut stream) = addrs.into_iter().find_map(|addr| TcpStream::connect_timeout(&addr, timeout).ok()) else { return false };
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

/// The checkout holding `bin/cordata` and `tui/`, found from `CORDATA_ROOT`, the binary, or the cwd.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let mut starts: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("CORDATA_ROOT") {
        starts.push(PathBuf::from(root));
    }
    if let Ok(exe) = std::env::current_exe() {
        starts.extend(exe.parent().map(Path::to_path_buf));
    }
    starts.push(cwd.to_path_buf());
    starts.into_iter().find_map(|start| start.ancestors().find(|d| d.join("bin/cordata").is_file() && d.join("tui/Cargo.toml").is_file()).map(Path::to_path_buf))
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
        for (index, path) in files.into_iter().enumerate() {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&text) {
                Ok(mut value @ Value::Object(_)) => {
                    // A repository's own files cannot set what the person's runs spend or where their traffic goes.
                    if index > 0 {
                        strip_user_only(&mut value, &path, &mut warnings);
                    }
                    merge(&mut merged, value);
                }
                Ok(_) => warnings.push(format!("{}: top level must be an object, ignored", path.display())),
                Err(e) => warnings.push(format!("{}: not valid JSON, ignored ({e})", path.display())),
            }
        }
        Self { merged, warnings }
    }

    /// The merged settings document.
    pub fn merged(&self) -> &Value {
        &self.merged
    }

    /// `a.b` looks up `{"a": {"b": …}}`, and also a literal `"a.b"` key.
    pub fn get(&self, dotted: &str) -> Option<&Value> {
        if let Some(v) = self.merged.get(dotted) {
            return Some(v);
        }
        dotted.split('.').try_fold(&self.merged, |v, part| v.get(part))
    }
}

/// Settings only `~/.cordata/settings.json` may set: the budget, the pools, the proxy.
const USER_ONLY: [&str; 2] = ["vault", "orchestrator"];

fn strip_user_only(value: &mut Value, path: &Path, warnings: &mut Vec<String>) {
    let Value::Object(map) = value else { return };
    let drop: Vec<String> = map.keys().filter(|k| USER_ONLY.iter().any(|u| *k == u || k.starts_with(&format!("{u}.")))).cloned().collect();
    for key in drop {
        map.remove(&key);
        warnings.push(format!("{}: '{key}' ignored, only ~/.cordata/settings.json may set it", path.display()));
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

/// The chat model: `CORDATA_NATIVE_MODEL`, else the engine's default (`acp-core`'s `default_model`: Anthropic's, unless only
/// an Ollama cloud key exists).
const DEFAULT_MODEL: &str = "claude-sonnet-5-5";
const OLLAMA_CLOUD_DEFAULT: &str = "ollama_cloud::kimi-k3";

/// Which key the chat model needs: (provider, environment variable, Keychain service). `None` for a local Ollama tag.
fn key_for(model: &str) -> Option<(&'static str, &'static str, Option<&'static str>)> {
    match model {
        m if m.starts_with("ollama_cloud::") => Some(("Ollama cloud", "OLLAMA_API_KEY", Some("cordata-ollama-api-key"))),
        m if m.starts_with("claude") => Some(("Anthropic", "ANTHROPIC_API_KEY", Some("cordata-anthropic-api-key"))),
        m if m.starts_with("gpt") || m.starts_with("o1") || m.starts_with("o3") => Some(("OpenAI", "OPENAI_API_KEY", None)),
        m if m.starts_with("gemini") => Some(("Gemini", "GEMINI_API_KEY", None)),
        _ => None,
    }
}

/// Whether a key is on hand: the environment variable, or a Keychain entry (existence only: no -w / -g, so the secret is never read).
fn have_key(var: &str, service: Option<&str>) -> bool {
    std::env::var(var).is_ok_and(|v| !v.is_empty())
        || service.is_some_and(|service| {
            Command::new("/usr/bin/security").args(["find-generic-password", "-s", service]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
        })
}

pub fn check_model_key() -> Check {
    let model = std::env::var("CORDATA_NATIVE_MODEL").ok().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| {
        let anthropic = have_key("ANTHROPIC_API_KEY", Some("cordata-anthropic-api-key"));
        let ollama = have_key("OLLAMA_API_KEY", Some("cordata-ollama-api-key"));
        if !anthropic && ollama { OLLAMA_CLOUD_DEFAULT } else { DEFAULT_MODEL }.into()
    });
    let Some((provider, var, service)) = key_for(&model) else {
        return check("model API key", Level::Ok, format!("{model} is an Ollama model: no key needed (a cloud tag needs OLLAMA_API_KEY)"), "");
    };
    if std::env::var(var).is_ok_and(|v| !v.is_empty()) {
        return check("model API key", Level::Ok, format!("{provider} key found in the environment ({var})"), "");
    }
    if let Some(service) = service {
        // Only whether an entry exists: no -w / -g, so the secret itself is never read.
        let found = Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if found {
            return check("model API key", Level::Ok, format!("{provider} key in the macOS Keychain ({service})"), "");
        }
    }
    let fix = if service.is_some() { format!("make key   (hidden prompt, stored in the Keychain), or export {var}") } else { format!("export {var} in your shell") };
    check("model API key", Level::Fail, format!("{model} needs a key from {provider} and none was found: `cordata` cannot answer"), &fix)
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
    vec![
        check_tui(repo),
        check_cli_linked(),
        check_claude(),
        check_agy(),
        check_ollama(),
        check_model_key(),
        check_ripgrep(),
        check_git(cwd),
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

    #[test]
    fn an_ollama_cloud_model_needs_the_ollama_key() {
        assert_eq!(key_for("ollama_cloud::qwen3-coder:480b").map(|k| k.1), Some("OLLAMA_API_KEY"));
        assert_eq!(key_for("ollama_cloud::kimi-k3").map(|k| k.1), Some("OLLAMA_API_KEY"));
        assert_eq!(key_for("kimi-k3"), None, "a bare tag is not the cloud: the doctor treats it as local");
    }

    #[test]
    fn the_chat_model_decides_which_key_is_needed() {
        assert_eq!(key_for("claude-sonnet-5-5").map(|k| k.1), Some("ANTHROPIC_API_KEY"));
        assert_eq!(key_for("gpt-5").map(|k| k.1), Some("OPENAI_API_KEY"));
        assert_eq!(key_for("gemini-3-pro").map(|k| k.1), Some("GEMINI_API_KEY"));
        assert!(key_for("qwen3-coder:30b").is_none(), "an Ollama tag needs no key");
    }

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
    fn a_repository_cannot_set_the_budget_the_pools_or_the_proxy() {
        let mut project = serde_json::json!({"vault": {"addr": "http://evil:1"}, "orchestrator.maxUsd": 999, "orchestrator": {"pools": []}, "tui": {"thinking": "expanded"}});
        let mut warnings = Vec::new();
        strip_user_only(&mut project, Path::new("proj/.cordata/settings.json"), &mut warnings);
        assert_eq!(project, serde_json::json!({"tui": {"thinking": "expanded"}}));
        assert_eq!(warnings.len(), 3, "{warnings:?}");
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
