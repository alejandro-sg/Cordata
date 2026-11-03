//! The coding assistants on this Mac: which known CLIs are installed, their versions, and which of them the
//! orchestrator may delegate to (`orchestrator.workers`). Detection looks where installers put binaries even when the
//! app was opened from Finder with a bare `PATH`. Only `claude_code`, `antigravity` and `ollama` are worker families
//! today; the rest run as terminal agents in the dock, where `detect.toml` follows their state.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::settings::{Scope, Settings, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    pub id: &'static str,
    pub name: &'static str,
    pub binaries: &'static [&'static str],
    /// The orchestrator's worker family, when it can delegate to this assistant.
    pub family: Option<&'static str>,
    /// `$CORDATA_*_BIN`, the override the orchestrator reads.
    pub env_override: Option<&'static str>,
    pub vendor: &'static str,
}

pub const KNOWN: &[Known] = &[
    Known { id: "claude_code", name: "Claude Code", binaries: &["claude"], family: Some("claude_code"), env_override: Some("CORDATA_CLAUDE_BIN"), vendor: "Anthropic" },
    Known { id: "antigravity", name: "Antigravity", binaries: &["agy", "antigravity"], family: Some("antigravity"), env_override: Some("CORDATA_AGY_BIN"), vendor: "Google" },
    Known { id: "ollama", name: "Ollama", binaries: &["ollama"], family: Some("ollama"), env_override: None, vendor: "Ollama" },
    Known { id: "codex", name: "Codex CLI", binaries: &["codex"], family: None, env_override: None, vendor: "OpenAI" },
    Known { id: "gemini", name: "Gemini CLI", binaries: &["gemini"], family: None, env_override: None, vendor: "Google" },
    Known { id: "cursor", name: "Cursor Agent", binaries: &["cursor-agent"], family: None, env_override: None, vendor: "Cursor" },
    Known { id: "copilot", name: "Copilot CLI", binaries: &["copilot"], family: None, env_override: None, vendor: "GitHub" },
    Known { id: "opencode", name: "OpenCode", binaries: &["opencode"], family: None, env_override: None, vendor: "SST" },
    Known { id: "amp", name: "Amp", binaries: &["amp"], family: None, env_override: None, vendor: "Sourcegraph" },
    Known { id: "droid", name: "Droid", binaries: &["droid"], family: None, env_override: None, vendor: "Factory" },
    Known { id: "kiro", name: "Kiro CLI", binaries: &["kiro-cli"], family: None, env_override: None, vendor: "AWS" },
    Known { id: "qwen", name: "Qwen Code", binaries: &["qwen"], family: None, env_override: None, vendor: "Alibaba" },
    Known { id: "kimi", name: "Kimi CLI", binaries: &["kimi"], family: None, env_override: None, vendor: "Moonshot" },
    Known { id: "goose", name: "Goose", binaries: &["goose"], family: None, env_override: None, vendor: "Block" },
    Known { id: "crush", name: "Crush", binaries: &["crush"], family: None, env_override: None, vendor: "Charm" },
    Known { id: "aider", name: "aider", binaries: &["aider"], family: None, env_override: None, vendor: "aider" },
    Known { id: "cline", name: "Cline CLI", binaries: &["cline"], family: None, env_override: None, vendor: "Cline" },
];

/// What the orchestrator delegates to when `orchestrator.workers` is unset (`cordata-run`'s `DEFAULT_WORKERS`).
pub const DEFAULT_WORKERS: [&str; 3] = ["claude_code", "antigravity", "ollama"];
const VERSION_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub known: Known,
    pub path: PathBuf,
    /// The first line `--version` printed, if it answered in time.
    pub version: Option<String>,
}

/// Folders searched besides `PATH`: where Homebrew, npm, bun, cargo, pipx, volta and the vendors' own installers put
/// binaries.
pub fn search_dirs(path_var: Option<&str>, home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_var.unwrap_or_default().split(':').filter(|d| !d.is_empty()).map(PathBuf::from).collect();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
        dirs.push(PathBuf::from(extra));
    }
    for rel in [".local/bin", ".npm-global/bin", ".bun/bin", ".cargo/bin", ".volta/bin", ".deno/bin", ".claude/local", ".amp/bin", ".factory/bin", ".opencode/bin"] {
        dirs.push(home.join(rel));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

fn locate(known: &Known, dirs: &[PathBuf]) -> Option<PathBuf> {
    if let Some(var) = known.env_override
        && let Some(path) = std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_file())
    {
        return Some(path);
    }
    known.binaries.iter().find_map(|bin| dirs.iter().map(|d| d.join(bin)).find(|p| is_executable(p)))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `<bin> --version`, killed if it has not answered in time (some CLIs start a login flow instead).
fn version(path: &Path) -> Option<String> {
    let mut child = Command::new(path).arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > VERSION_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    let text = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    let line = String::from_utf8_lossy(&text).lines().map(str::trim).find(|l| !l.is_empty())?.to_owned();
    Some(crate::shown(&line).chars().take(60).collect())
}

/// Every known assistant installed here, versions read in parallel. Blocking: run it off the UI thread.
pub fn detect() -> Vec<Found> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let dirs = search_dirs(std::env::var("PATH").ok().as_deref(), &home);
    let located: Vec<(Known, PathBuf)> = KNOWN.iter().filter_map(|k| locate(k, &dirs).map(|p| (*k, p))).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = located.iter().map(|(k, p)| scope.spawn(move || Found { known: *k, path: p.clone(), version: version(p) })).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

/// The worker families `orchestrator.workers` turns on (its default when unset).
pub fn enabled(settings: &Settings) -> Vec<String> {
    match settings.get("orchestrator.workers").and_then(Value::as_str) {
        Some(raw) => raw.split(',').map(str::trim).filter(|f| !f.is_empty()).map(str::to_owned).collect(),
        None => DEFAULT_WORKERS.iter().map(|f| (*f).to_owned()).collect(),
    }
}

/// Turn a worker family on or off for delegation, in the user's settings (the only file `orchestrator.*` is read from).
pub fn set_enabled(store: &Store, family: &str, on: bool) -> Result<Vec<String>, String> {
    let mut families = enabled(&store.load());
    families.retain(|f| f != family);
    if on {
        families.push(family.to_owned());
    }
    // Keep a stable order: the known families first, as listed.
    families.sort_by_key(|f| KNOWN.iter().position(|k| k.family == Some(f.as_str())).unwrap_or(usize::MAX));
    store.set(Scope::User, "orchestrator.workers", Value::String(families.join(",")))?;
    Ok(families)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_an_installed_cli_and_reads_its_version() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("codex");
        std::fs::write(&bin, "#!/bin/sh\necho 'codex-cli 9.9.9'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("amp"), "not executable").unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        let codex = KNOWN.iter().find(|k| k.id == "codex").unwrap();
        let amp = KNOWN.iter().find(|k| k.id == "amp").unwrap();
        assert_eq!(locate(codex, &dirs), Some(bin.clone()));
        assert_eq!(locate(amp, &dirs), None, "a file that cannot run is not an install");
        assert_eq!(version(&bin).as_deref(), Some("codex-cli 9.9.9"));
    }

    #[test]
    fn a_hung_version_check_is_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("slow");
        std::fs::write(&bin, "#!/bin/sh\nexec sleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        assert_eq!(version(&bin), None);
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[test]
    fn delegation_is_written_to_the_user_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store { user_dir: dir.path().join("home"), cwd: dir.path().join("project") };
        std::fs::create_dir_all(&store.user_dir).unwrap();
        std::fs::write(store.path(Scope::User), r#"{"chat":{"history":true}}"#).unwrap();
        assert_eq!(enabled(&store.load()), DEFAULT_WORKERS, "unset: the orchestrator's default");
        assert_eq!(set_enabled(&store, "antigravity", false).unwrap(), ["claude_code", "ollama"]);
        assert_eq!(set_enabled(&store, "antigravity", true).unwrap(), ["claude_code", "antigravity", "ollama"]);
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(store.path(Scope::User)).unwrap()).unwrap();
        assert_eq!(saved["chat"]["history"], true, "the rest of the file is kept");
        assert_eq!(saved["orchestrator"]["workers"], "claude_code,antigravity,ollama");
        assert!(store.set(Scope::Project, "orchestrator.workers", Value::from("x")).is_err(), "a repository cannot pick workers");
        assert!(store.set(Scope::User, "chat.apiKey", Value::from("sk-x")).is_err(), "a secret-shaped key is refused");
    }

    #[test]
    fn the_search_covers_installer_folders_once() {
        let dirs = search_dirs(Some("/usr/bin:/opt/homebrew/bin"), Path::new("/Users/x"));
        assert_eq!(dirs.iter().filter(|d| d.as_path() == Path::new("/opt/homebrew/bin")).count(), 1);
        assert!(dirs.contains(&PathBuf::from("/Users/x/.local/bin")));
    }
}
