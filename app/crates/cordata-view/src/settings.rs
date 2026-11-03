//! Layered user settings — the client half of `backend/settings_store.py`.
//!
//! Three JSON files, lowest precedence first:
//!
//!   user     `~/.cordata/settings.json`            (`CORDATA_HOME` moves the dir)
//!   project  `<cwd>/.cordata/settings.json`        (committed; shared with the team)
//!   local    `<cwd>/.cordata/settings.local.json`  (this machine only; gitignored)
//!
//! The backend enforces permissions and applies model / mode / tier
//! preferences; this module reads the same files so the TUI can show what is in
//! effect and where it came from, apply its own display preferences, and write
//! changes (`/config`, `/permissions`, `/trust`). `docs/settings.md` is the
//! contract; both implementations are tested against `tui/fixtures/settings`.
//!
//! Credentials never belong here (they live in the environment or the macOS
//! Keychain) — a key named like one, or a value shaped like one, is refused.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Scope {
    User,
    Project,
    Local,
}

impl Scope {
    pub const ALL: [Scope; 3] = [Scope::User, Scope::Project, Scope::Local];

    pub fn name(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Local => "local",
        }
    }

    pub fn parse(text: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|scope| scope.name() == text)
    }
}

pub enum Kind {
    Enum(&'static [&'static str]),
    Bool,
    Str,
    Float(f64, f64),
    /// A list of app bundle ids (`capture.allowlist`): reverse-DNS labels only, so no path, pattern
    /// or shell fragment can be stored where an app identity is matched.
    BundleIds,
    /// A list of strings (`chat.allow` rules, `chat.writable` folders).
    Strings,
}

/// Where a change takes effect.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// The TUI (or the running session) picks it up immediately.
    Now,
    /// The backend reads it when a session starts.
    NextSession,
}

pub struct Entry {
    pub key: &'static str,
    pub kind: Kind,
    pub applies: Applies,
    pub help: &'static str,
}

pub const SCHEMA: &[Entry] = &[
    Entry {
        key: "tui.thinking",
        kind: Kind::Enum(&["collapsed", "expanded"]),
        applies: Applies::Now,
        help: "show reasoning as one line or in full",
    },
    Entry {
        key: "tui.toolOutput",
        kind: Kind::Enum(&["collapsed", "expanded"]),
        applies: Applies::Now,
        help: "show tool results trimmed or in full",
    },
    // The native app's own preferences (user file only, `USER_ONLY_PREFIXES`).
    Entry {
        key: "app.theme",
        kind: Kind::Enum(&["system", "light", "2049", "graphite", "dim", "night", "high-contrast"]),
        applies: Applies::Now,
        help: "Cordata.app's palette: system follows macOS Light/Dark (and Increase Contrast); night = 2049, high-contrast = its AAA variant",
    },
    Entry {
        key: "app.density",
        kind: Kind::Enum(&["comfortable", "compact"]),
        applies: Applies::Now,
        help: "Cordata.app's spacing",
    },
    Entry {
        key: "app.notifications",
        kind: Kind::Enum(&["all", "approvals", "off"]),
        applies: Applies::Now,
        help: "which events Cordata.app notifies about",
    },
    Entry {
        key: "app.restoreWindows",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "reopen Cordata.app's sessions on launch",
    },
    Entry {
        key: "app.showReasoning",
        kind: Kind::Bool,
        applies: Applies::Now,
        help: "Cordata.app shows reasoning in full",
    },
    // The chat agent's rules, read by acp-core (`acp_core::policy`). `chat.allow` and `chat.writable` loosen what the
    // agent may do without asking, so they are user-file only (`USER_ONLY_PREFIXES`).
    Entry {
        key: "chat.allow",
        kind: Kind::Strings,
        applies: Applies::NextSession,
        help: "tool calls that run without asking, e.g. [\"run_command(cargo test*)\", \"edit_file(src/**)\"]",
    },
    Entry {
        key: "chat.deny",
        kind: Kind::Strings,
        applies: Applies::NextSession,
        help: "tool calls refused without asking, e.g. [\"run_command(git push*)\"]",
    },
    Entry {
        key: "chat.network",
        kind: Kind::Enum(&["ask", "off"]),
        applies: Applies::NextSession,
        help: "commands run offline; ask = a command may request the network and you are asked, off = never",
    },
    Entry {
        key: "chat.writable",
        kind: Kind::Strings,
        applies: Applies::NextSession,
        help: "absolute folders commands may write besides the project, temporary folders and package caches",
    },
    Entry {
        key: "chat.history",
        kind: Kind::Enum(&["on", "off"]),
        applies: Applies::NextSession,
        help: "keep sessions in ~/.cordata/sessions so /resume can continue them (/forget deletes them)",
    },
    // `chat.mode` decides who answers an approval card, so it is user-file only (`USER_ONLY_PREFIXES`).
    Entry {
        key: "chat.mode",
        kind: Kind::Enum(&["ask", "auto"]),
        applies: Applies::NextSession,
        help: "ask = every call no rule decides asks you; auto = calls auto mode can show are safe run without asking, the rest ask (user file only)",
    },
    // Mirror of backend/settings_store.py's SCHEMA. Both front doors reject an
    // unknown key with a warning, so a key added on one side and not the other
    // shows up as a spurious warning in whichever door is behind.
    Entry {
        key: "vault.enabled",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "run tool subprocesses through the Agent Vault credential proxy",
    },
    Entry {
        key: "vault.addr",
        kind: Kind::Str,
        applies: Applies::NextSession,
        help: "Agent Vault proxy address, e.g. http://127.0.0.1:14322",
    },
    Entry {
        key: "orchestrator.workers",
        kind: Kind::Str,
        applies: Applies::NextSession,
        help: "which coding workers the orchestrator may use (claude_code,antigravity,ollama)",
    },
    Entry {
        key: "orchestrator.dispatch",
        kind: Kind::Enum(&["off", "shadow", "live", "auto"]),
        applies: Applies::NextSession,
        help: "dispatch policy: off = strongest first, shadow = learn without acting, live = act on it, auto = live + capability and context-window routing",
    },
    Entry {
        key: "orchestrator.maxUsd",
        kind: Kind::Float(0.0, 100.0),
        applies: Applies::NextSession,
        help: "per-plan budget envelope in dollars (default 2)",
    },
    Entry {
        key: "orchestrator.maxTokens",
        kind: Kind::Float(0.0, 100000000.0),
        applies: Applies::NextSession,
        help: "per-plan token envelope (0 = no token cap)",
    },
    Entry {
        key: "orchestrator.pools",
        kind: Kind::Str,
        applies: Applies::NextSession,
        help: "quota pools as a JSON list (caps, windows, soft/hard lines)",
    },
    Entry {
        key: "orchestrator.harden",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "review each change with a different model family before the audit",
    },
    Entry {
        key: "orchestrator.tier2",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "after the deterministic checks pass, ask a reviewer model accept/drift/replan",
    },
    Entry {
        key: "orchestrator.screen",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "a reviewer model screens the audit's commands before they run (the pattern guard always does)",
    },
    Entry {
        key: "orchestrator.lambda",
        kind: Kind::Float(0.0, 2.0),
        applies: Applies::NextSession,
        help: "dispatch cost weight (higher prefers cheaper workers)",
    },
    Entry {
        key: "orchestrator.mu",
        kind: Kind::Float(0.0, 2.0),
        applies: Applies::NextSession,
        help: "dispatch quota-scarcity weight (higher avoids pools near their limit)",
    },
    Entry {
        key: "mcp.approved",
        kind: Kind::Strings,
        applies: Applies::NextSession,
        help: "project MCP servers you approved, pinned to their exact command (user file only)",
    },
];

pub fn entry(key: &str) -> Option<&'static Entry> {
    SCHEMA.iter().find(|entry| entry.key == key)
}

static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(api[_-]?key|token|secret|password|passwd|credential)").unwrap()
});
/// Setting groups read only from the user file — mirrors `USER_ONLY_PREFIXES` in `backend/settings_store.py`.
/// `chat.allow`, `chat.writable` and `chat.mode` are single keys: a repository must not grant its own agent anything.
pub const USER_ONLY_PREFIXES: &[&str] = &["app.", "vault.", "orchestrator.", "chat.allow", "chat.writable", "chat.mode", "mcp."];

static SECRET_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(sk-[A-Za-z0-9]{16,}|[A-Za-z0-9]{32,}\.[A-Za-z0-9_-]{8,}|ghp_[A-Za-z0-9]{20,}|xox[bp]-[A-Za-z0-9-]{10,})",
    )
    .unwrap()
});

#[derive(Default, Debug, Clone)]
pub struct Settings {
    /// Merged, validated values by dotted key.
    pub values: BTreeMap<String, Value>,
    /// Which file set each key.
    pub sources: BTreeMap<String, Scope>,
    pub warnings: Vec<String>,
}

/// The display preferences the TUI itself acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefs {
    pub thinking_expanded: bool,
    pub tools_expanded: bool,
}

impl Settings {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    pub fn prefs(&self) -> Prefs {
        Prefs {
            thinking_expanded: self.text("tui.thinking") == Some("expanded"),
            tools_expanded: self.text("tui.toolOutput") == Some("expanded"),
        }
    }

    /// How a value reads in the config panel.
    pub fn display(&self, key: &str) -> String {
        match self.get(key) {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => "—".to_owned(),
        }
    }
}

/// Where the files live.
#[derive(Clone, Debug)]
pub struct Store {
    pub user_dir: PathBuf,
    pub cwd: PathBuf,
}

pub fn default_user_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CORDATA_HOME").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".cordata"),
        None => PathBuf::from(".cordata"),
    }
}

fn flatten(map: &Map<String, Value>, prefix: &str, out: &mut Vec<(String, Value)>) {
    for (key, value) in map {
        let dotted = format!("{prefix}{key}");
        match value {
            Value::Object(inner) => flatten(inner, &format!("{dotted}."), out),
            other => out.push((dotted, other.clone())),
        }
    }
}

fn check(key: &str, value: &Value, location: &str) -> Result<(), String> {
    let leaf = key.rsplit('.').next().unwrap_or(key);
    if SECRET_KEY.is_match(leaf) {
        return Err(format!(
            "{location}: '{key}' looks like a credential — refused (keep keys in the environment or Keychain)"
        ));
    }
    if let Value::String(text) = value
        && SECRET_VALUE.is_match(text)
    {
        return Err(format!(
            "{location}: the value of '{key}' looks like a secret — refused"
        ));
    }
    let Some(entry) = entry(key) else {
        return Err(format!("{location}: unknown setting '{key}' ignored"));
    };
    match &entry.kind {
        Kind::Enum(choices) => {
            if !value.as_str().is_some_and(|text| choices.contains(&text)) {
                return Err(format!(
                    "{location}: '{key}' must be one of {} (got {value})",
                    choices.join(", ")
                ));
            }
        }
        Kind::Bool => {
            if !value.is_boolean() {
                return Err(format!("{location}: '{key}' must be true or false"));
            }
        }
        Kind::Str => {
            if !value.is_string() {
                return Err(format!("{location}: '{key}' must be a string"));
            }
        }
        Kind::Float(low, high) => {
            if !value
                .as_f64()
                .is_some_and(|number| (*low..=*high).contains(&number))
            {
                return Err(format!(
                    "{location}: '{key}' must be a number between {low} and {high}"
                ));
            }
        }
        Kind::Strings => {
            if !value.as_array().is_some_and(|items| items.iter().all(Value::is_string)) {
                return Err(format!("{location}: '{key}' must be a list of strings"));
            }
        }
        Kind::BundleIds => {
            let ok = value.as_array().is_some_and(|items| {
                items
                    .iter()
                    .all(|item| item.as_str().is_some_and(is_bundle_id))
            });
            if !ok {
                return Err(format!(
                    "{location}: '{key}' must be a list of bundle ids, e.g. [\"com.apple.Safari\"]"
                ));
            }
        }
    }
    Ok(())
}

/// Reverse-DNS, at least two labels, each starting with a letter or digit — the same shape
/// `harness_capture::policy::acceptable` and `backend/settings_store.py` accept.
pub fn is_bundle_id(text: &str) -> bool {
    let labels: Vec<&str> = text.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            label
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphanumeric())
                && label
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        })
}

fn read_json(path: &Path, warnings: &mut Vec<String>) -> Map<String, Value> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Map::new(),
        Err(error) => {
            warnings.push(format!("{}: cannot read ({error})", path.display()));
            return Map::new();
        }
    };
    if text.trim().is_empty() {
        return Map::new();
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => map,
        Ok(_) => {
            warnings.push(format!(
                "{}: top level must be an object, ignored",
                path.display()
            ));
            Map::new()
        }
        Err(error) => {
            warnings.push(format!(
                "{}: not valid JSON, ignored ({error})",
                path.display()
            ));
            Map::new()
        }
    }
}

impl Store {
    pub fn discover(cwd: &Path) -> Self {
        Self {
            user_dir: default_user_dir(),
            cwd: cwd.to_path_buf(),
        }
    }

    pub fn path(&self, scope: Scope) -> PathBuf {
        let base = self.cwd.join(".cordata");
        match scope {
            Scope::User => self.user_dir.join("settings.json"),
            Scope::Project => base.join("settings.json"),
            Scope::Local => base.join("settings.local.json"),
        }
    }

    /// Write one setting (`a.b.c` → nested objects) into a scope's file, keeping everything else in it. The value is
    /// checked against the schema first; the file is replaced atomically and kept private (0600).
    pub fn set(&self, scope: Scope, key: &str, value: Value) -> Result<(), String> {
        check(key, &value, scope.name())?;
        if scope != Scope::User && USER_ONLY_PREFIXES.iter().any(|prefix| key.starts_with(prefix)) {
            return Err(format!("'{key}' is only read from the user file"));
        }
        let path = self.path(scope);
        let mut warnings = Vec::new();
        let mut root = read_json(&path, &mut warnings);
        if let Some(problem) = warnings.into_iter().next() {
            return Err(format!("not overwriting a file that does not parse: {problem}"));
        }
        let mut parts: Vec<&str> = key.split('.').collect();
        let last = parts.pop().ok_or("empty key")?;
        let mut node = &mut root;
        for part in parts {
            let next = node.entry(part.to_owned()).or_insert_with(|| Value::Object(Map::new()));
            if !next.is_object() {
                *next = Value::Object(Map::new());
            }
            node = next.as_object_mut().expect("just made an object");
        }
        node.insert(last.to_owned(), value);
        let dir = path.parent().ok_or("no folder")?;
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".settings.{}.tmp", std::process::id()));
        let text = serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())?;
        fs::write(&tmp, text + "\n").map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
        fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }

    /// Merge the three files. Never fails: a bad file or value is skipped and
    /// reported in `warnings`.
    pub fn load(&self) -> Settings {
        let mut result = Settings::default();
        for scope in Scope::ALL {
            let location = self.path(scope).display().to_string();
            let mut flat = Vec::new();
            flatten(&read_json(&self.path(scope), &mut result.warnings), "", &mut flat);
            for (key, value) in flat {
                if scope != Scope::User && USER_ONLY_PREFIXES.iter().any(|prefix| key.starts_with(prefix)) {
                    result.warnings.push(format!("{location}: '{key}' is only read from the user file, ignored"));
                    continue;
                }
                if let Err(warning) = check(&key, &value, &location) {
                    result.warnings.push(warning);
                    continue;
                }
                result.sources.insert(key.clone(), scope);
                result.values.insert(key, value);
            }
        }
        result
    }
}

/// The guidance files the backend puts in the system prompt, found the same way
/// as `settings_store.instruction_files`.
pub fn instruction_files(store: &Store) -> Vec<(Scope, PathBuf)> {
    let project = [
        store.cwd.join("CORDATA.md"),
        store.cwd.join(".cordata").join("CORDATA.md"),
        store.cwd.join("AGENTS.md"),
        store.cwd.join("CLAUDE.md"),
    ];
    let candidates: [(Scope, Vec<PathBuf>); 3] = [
        (Scope::User, vec![store.user_dir.join("CORDATA.md")]),
        (Scope::Project, project.to_vec()),
        (Scope::Local, vec![store.cwd.join("CORDATA.local.md")]),
    ];
    candidates
        .into_iter()
        .filter_map(|(scope, options)| {
            options
                .into_iter()
                .find(|path| path.is_file())
                .map(|path| (scope, path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_store() -> Store {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tui/fixtures/settings");
        Store {
            user_dir: root.join("home"),
            cwd: root.join("project"),
        }
    }

    fn summary(settings: &Settings) -> Value {
        serde_json::json!({
            "values": settings.values,
            "sources": settings.sources.iter().map(|(k, v)| (k.clone(), Value::from(v.name()))).collect::<Map<_, _>>(),
            "warning_count": settings.warnings.len(),
        })
    }

    struct Sandbox {
        _dir: tempfile::TempDir,
        store: Store,
    }

    fn sandbox() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let store = Store {
            user_dir: root.join("home"),
            cwd: root.join("project"),
        };
        fs::create_dir_all(&store.user_dir).unwrap();
        fs::create_dir_all(&store.cwd).unwrap();
        Sandbox { _dir: dir, store }
    }

    fn put(store: &Store, scope: Scope, text: &str) {
        let path = store.path(scope);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn the_shared_fixture_merges_to_the_documented_result() {
        let expected: Value = serde_json::from_str(
            &fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../tui/fixtures/settings/expected.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(summary(&fixture_store().load()), expected);
    }

    #[test]
    fn a_project_file_cannot_set_the_budget_or_the_proxy() {
        let sb = sandbox();
        put(&sb.store, Scope::User, r#"{"orchestrator":{"maxUsd":5}}"#);
        put(&sb.store, Scope::Project, r#"{"orchestrator":{"maxUsd":999},"vault":{"addr":"http://evil:1"},"tui":{"thinking":"expanded"}}"#);
        let loaded = sb.store.load();
        assert_eq!(loaded.get("orchestrator.maxUsd"), Some(&Value::from(5)));
        assert!(loaded.get("vault.addr").is_none());
        assert_eq!(loaded.text("tui.thinking"), Some("expanded"));
        assert_eq!(loaded.warnings.iter().filter(|w| w.contains("only read from the user file")).count(), 2);
    }

    #[test]
    fn chat_mode_is_read_only_from_the_user_file() {
        let sb = sandbox();
        put(&sb.store, Scope::Project, r#"{"chat":{"mode":"auto"}}"#);
        put(&sb.store, Scope::Local, r#"{"chat.mode":"auto"}"#);
        let loaded = sb.store.load();
        assert!(loaded.get("chat.mode").is_none());
        let ignored: Vec<_> = loaded.warnings.iter().filter(|w| w.contains("'chat.mode' is only read from the user file")).collect();
        assert_eq!(ignored.len(), 2, "{:?}", loaded.warnings);
        put(&sb.store, Scope::User, r#"{"chat":{"mode":"auto"}}"#);
        assert_eq!(sb.store.load().text("chat.mode"), Some("auto"));
    }

    #[test]
    fn bad_values_files_and_secrets_are_reported_not_fatal() {
        let sb = sandbox();
        put(
            &sb.store,
            Scope::User,
            r#"{"tui":{"thinking":"loud","toolOutput":"expanded"},"mystery":1,"api_key":"x"}"#,
        );
        put(&sb.store, Scope::Project, "{not json");
        put(
            &sb.store,
            Scope::Local,
            r#"{"tui":{"thinking":"sk-abcdefghijklmnopqrstuvwxyz0123456789"}}"#,
        );
        let loaded = sb.store.load();
        assert_eq!(loaded.text("tui.toolOutput"), Some("expanded"));
        assert!(loaded.get("tui.thinking").is_none());
        let text = loaded.warnings.join("\n");
        for needle in [
            "loud",
            "unknown setting 'mystery'",
            "looks like a credential",
            "not valid JSON",
            "looks like a secret",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
    }

    #[test]
    fn preferences_default_sensibly_and_follow_the_files() {
        let sb = sandbox();
        assert_eq!(
            sb.store.load().prefs(),
            Prefs {
                thinking_expanded: false,
                tools_expanded: false
            }
        );
        put(
            &sb.store,
            Scope::User,
            r#"{"tui":{"thinking":"expanded","toolOutput":"expanded"}}"#,
        );
        assert_eq!(
            sb.store.load().prefs(),
            Prefs {
                thinking_expanded: true,
                tools_expanded: true
            }
        );
    }

    #[test]
    fn instruction_files_prefer_cordata_md_over_other_tools_files() {
        let sb = sandbox();
        assert!(instruction_files(&sb.store).is_empty());
        fs::write(sb.store.cwd.join("CLAUDE.md"), "x").unwrap();
        fs::write(sb.store.cwd.join("AGENTS.md"), "x").unwrap();
        assert!(instruction_files(&sb.store)[0].1.ends_with("AGENTS.md"));
        fs::write(sb.store.cwd.join("CORDATA.md"), "x").unwrap();
        fs::write(sb.store.user_dir.join("CORDATA.md"), "x").unwrap();
        let found = instruction_files(&sb.store);
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].0, found[1].0), (Scope::User, Scope::Project));
        assert!(found[1].1.ends_with("CORDATA.md"));
    }
}
