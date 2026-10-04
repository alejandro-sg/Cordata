//! Layered user settings — the client half of `backend/settings_store.py`.
//!
//! Three JSON files, lowest precedence first:
//!
//!   user     `~/.sierralab/settings.json`            (`SIERRALAB_HOME` moves the dir)
//!   project  `<cwd>/.sierralab/settings.json`        (committed; shared with the team)
//!   local    `<cwd>/.sierralab/settings.local.json`  (this machine only; gitignored)
//!
//! The backend enforces permissions and applies model / mode / Laya / tier
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RuleKind {
    Allow,
    Ask,
    Deny,
}

impl RuleKind {
    pub const ALL: [RuleKind; 3] = [RuleKind::Allow, RuleKind::Ask, RuleKind::Deny];

    pub fn name(self) -> &'static str {
        match self {
            RuleKind::Allow => "allow",
            RuleKind::Ask => "ask",
            RuleKind::Deny => "deny",
        }
    }

    pub fn parse(text: &str) -> Option<RuleKind> {
        RuleKind::ALL.into_iter().find(|kind| kind.name() == text)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    Scope(Scope),
    BuiltIn,
}

impl Origin {
    pub fn name(self) -> &'static str {
        match self {
            Origin::Scope(scope) => scope.name(),
            Origin::BuiltIn => "built-in",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub kind: RuleKind,
    pub text: String,
    pub origin: Origin,
    /// False: written down but not applied (an `allow` from an untrusted folder).
    pub active: bool,
}

pub enum Kind {
    Enum(&'static [&'static str]),
    Bool,
    Str,
    Float(f64, f64),
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
        key: "permissions.defaultMode",
        kind: Kind::Enum(&["plan", "build", "auto"]),
        applies: Applies::NextSession,
        help: "mode new sessions start in (auto needs a trusted folder)",
    },
    Entry {
        key: "model",
        kind: Kind::Str,
        applies: Applies::Now,
        help: "model id on the active provider",
    },
    Entry {
        key: "temperature",
        kind: Kind::Enum(&["focused", "balanced", "creative"]),
        applies: Applies::Now,
        help: "sampling preset",
    },
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
    Entry {
        key: "tui.showLaya",
        kind: Kind::Bool,
        applies: Applies::Now,
        help: "show Laya's triage lines",
    },
    Entry {
        key: "laya.mode",
        kind: Kind::Enum(&["off", "shadow", "route"]),
        applies: Applies::NextSession,
        help: "off | log only | log + routing hint",
    },
    Entry {
        key: "laya.threshold",
        kind: Kind::Float(0.0, 1.0),
        applies: Applies::NextSession,
        help: "confidence needed before Laya sends a hint",
    },
    Entry {
        key: "tiers.fast",
        kind: Kind::Str,
        applies: Applies::NextSession,
        help: "model for cheap sub-agents",
    },
    Entry {
        key: "tiers.standard",
        kind: Kind::Str,
        applies: Applies::NextSession,
        help: "model for ordinary sub-agents",
    },
    Entry {
        key: "tiers.strong",
        kind: Kind::Str,
        applies: Applies::NextSession,
        help: "model for hard sub-agents (also the frontier model Laya escalates to)",
    },
    Entry {
        key: "laya.dispatch",
        kind: Kind::Enum(&["direct", "hint"]),
        applies: Applies::NextSession,
        help: "confident specialist routes run the specialist directly, or only hint",
    },
    Entry {
        key: "laya.teamDispatch",
        kind: Kind::Enum(&["direct", "hint"]),
        applies: Applies::NextSession,
        help: "confident team routes run the decompose+graph pipeline directly, or only hint",
    },
    Entry {
        key: "laya.minConfidence",
        kind: Kind::Float(0.0, 1.0),
        applies: Applies::NextSession,
        help: "below this answer_confidence Laya abstains (fail closed)",
    },
    Entry {
        key: "laya.onAbstain",
        kind: Kind::Enum(&["escalate", "llm"]),
        applies: Applies::NextSession,
        help: "when Laya abstains: escalate to tiers.strong, or let the LLM decide",
    },
    Entry {
        key: "selfHeal.enabled",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "diagnose and fix failing commands autonomously",
    },
    Entry {
        key: "selfHeal.maxAttempts",
        kind: Kind::Float(1.0, 10.0),
        applies: Applies::NextSession,
        help: "debugger rounds per failure before escalating to you",
    },
    Entry {
        key: "selfHeal.verify",
        kind: Kind::Enum(&["sandbox", "workspace", "off"]),
        applies: Applies::NextSession,
        help: "verify patches in a throwaway copy first, or only in the workspace",
    },
    Entry {
        key: "selfHeal.autoApply",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "apply sandbox-verified fixes without asking (needs a trusted folder)",
    },
    Entry {
        key: "lsp.enabled",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "start the project's language servers for diagnostics after edits",
    },
    // Mirror of backend/settings_store.py's SCHEMA. Both front doors reject an
    // unknown key with a warning, so a key added on one side and not the other
    // shows up as a spurious warning in whichever door is behind.
    Entry {
        key: "laya.riskSafe",
        kind: Kind::Float(0.0, 1.0),
        applies: Applies::NextSession,
        help: "destructiveness score below which a turn counts as safe (0.2)",
    },
    Entry {
        key: "laya.riskDestroy",
        kind: Kind::Float(0.0, 1.0),
        applies: Applies::NextSession,
        help: "destructiveness score above which a turn counts as destructive (0.8)",
    },
    Entry {
        key: "laya.riskAutoApprove",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "let a confidently safe turn skip the approval prompt (off unless you set it)",
    },
    Entry {
        key: "memory.traces",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "record execution traces to the local LanceDB corpus (needs lancedb)",
    },
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
        key: "vault.vault",
        kind: Kind::Str,
        applies: Applies::NextSession,
        help: "Agent Vault vault name to draw credentials from",
    },
    Entry {
        key: "vault.egressPolicy",
        kind: Kind::Enum(&["deny", "proxy"]),
        applies: Applies::NextSession,
        help: "refuse egress when the vault is unreachable, or fall back to a plain proxy",
    },
    Entry {
        key: "observer.enabled",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "run the read-only workspace observer while no task is active",
    },
    Entry {
        key: "observer.intervalMinutes",
        kind: Kind::Float(1.0, 1440.0),
        applies: Applies::NextSession,
        help: "how often the observer wakes up",
    },
    Entry {
        key: "kg.enabled",
        kind: Kind::Bool,
        applies: Applies::NextSession,
        help: "record graph entities and edges from each turn (needs lancedb for the queryable store)",
    },
    Entry {
        key: "kg.sink",
        kind: Kind::Enum(&["lance", "cognee"]),
        applies: Applies::NextSession,
        help: "which knowledge-graph backend keeps the rows",
    },
];

pub fn entry(key: &str) -> Option<&'static Entry> {
    SCHEMA.iter().find(|entry| entry.key == key)
}

pub const BUILTIN_DENY: [&str; 3] = [
    "shell(security *find-generic-password*)",
    "shell(security *find-internet-password*)",
    "shell(security *dump-keychain*)",
];

static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(api[_-]?key|token|secret|password|passwd|credential)").unwrap()
});
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
    pub rules: Vec<Rule>,
    pub warnings: Vec<String>,
    pub trusted: bool,
}

/// The display preferences the TUI itself acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefs {
    pub thinking_expanded: bool,
    pub tools_expanded: bool,
    pub show_laya: bool,
}

impl Settings {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    pub fn rules_for(&self, kind: RuleKind) -> impl Iterator<Item = &Rule> {
        self.rules
            .iter()
            .filter(move |rule| rule.kind == kind && rule.active)
    }

    pub fn prefs(&self) -> Prefs {
        Prefs {
            thinking_expanded: self.text("tui.thinking") == Some("expanded"),
            tools_expanded: self.text("tui.toolOutput") == Some("expanded"),
            show_laya: self.get("tui.showLaya").and_then(Value::as_bool) != Some(false),
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
    if let Some(dir) = std::env::var_os("SIERRALAB_HOME").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".sierralab"))
        .unwrap_or_else(|| PathBuf::from(".sierralab"))
}

fn expand_home(text: &str) -> PathBuf {
    match text.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(text)),
        None => PathBuf::from(text),
    }
}

fn real(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
    }
    Ok(())
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
        match scope {
            Scope::User => self.user_dir.join("settings.json"),
            Scope::Project => self.cwd.join(".sierralab").join("settings.json"),
            Scope::Local => self.cwd.join(".sierralab").join("settings.local.json"),
        }
    }

    /// Merge the three files. Never fails: a bad file or value is skipped and
    /// reported in `warnings`.
    pub fn load(&self) -> Settings {
        let mut result = Settings::default();
        let raw: Vec<Map<String, Value>> = Scope::ALL
            .iter()
            .map(|scope| read_json(&self.path(*scope), &mut result.warnings))
            .collect();

        let here = real(&self.cwd);
        result.trusted = raw[0]
            .get("trustedProjects")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|item| real(&expand_home(item)) == here)
            });

        for (index, scope) in Scope::ALL.into_iter().enumerate() {
            let location = self.path(scope).display().to_string();
            let mut flat = Vec::new();
            flatten(&raw[index], "", &mut flat);
            // Fixed order (allow, ask, deny; each in file order) so every reader agrees.
            let list_key = |kind: RuleKind| format!("permissions.{}", kind.name());
            let is_list = |key: &str| RuleKind::ALL.iter().any(|kind| list_key(*kind) == key);
            let mut ordered: Vec<(String, Value)> = flat
                .iter()
                .filter(|(key, _)| !is_list(key))
                .cloned()
                .collect();
            for kind in RuleKind::ALL {
                ordered.extend(
                    flat.iter()
                        .filter(|(key, _)| *key == list_key(kind))
                        .cloned(),
                );
            }

            for (key, value) in ordered {
                if key == "trustedProjects" {
                    if scope != Scope::User {
                        result.warnings.push(format!(
                            "{location}: '{key}' is only read from the user file — ignored"
                        ));
                    }
                    continue;
                }
                if let Some(kind) = RuleKind::ALL
                    .into_iter()
                    .find(|kind| list_key(*kind) == key)
                {
                    let texts: Option<Vec<&str>> = value.as_array().and_then(|items| {
                        items
                            .iter()
                            .map(|item| item.as_str().filter(|text| !text.trim().is_empty()))
                            .collect()
                    });
                    let Some(texts) = texts else {
                        result.warnings.push(format!(
                            "{location}: '{key}' must be a list of rule strings"
                        ));
                        continue;
                    };
                    for text in texts {
                        if SECRET_VALUE.is_match(text) {
                            result.warnings.push(format!(
                                "{location}: a rule in '{key}' looks like it holds a secret — refused"
                            ));
                            continue;
                        }
                        let active =
                            kind != RuleKind::Allow || scope == Scope::User || result.trusted;
                        result.rules.push(Rule {
                            kind,
                            text: text.trim().to_owned(),
                            origin: Origin::Scope(scope),
                            active,
                        });
                        if !active {
                            result.warnings.push(format!(
                                "{location}: allow rule '{}' ignored — folder not trusted (run /trust to enable)",
                                text.trim()
                            ));
                        }
                    }
                    continue;
                }
                if let Err(warning) = check(&key, &value, &location) {
                    result.warnings.push(warning);
                    continue;
                }
                if key == "permissions.defaultMode"
                    && value == "auto"
                    && scope != Scope::User
                    && !result.trusted
                {
                    result.warnings.push(format!(
                        "{location}: defaultMode 'auto' ignored — folder not trusted (run /trust to enable)"
                    ));
                    continue;
                }
                if key == "selfHeal.autoApply"
                    && value == Value::Bool(true)
                    && scope != Scope::User
                    && !result.trusted
                {
                    result.warnings.push(format!(
                        "{location}: selfHeal.autoApply ignored — folder not trusted (run /trust to enable)"
                    ));
                    continue;
                }
                result.sources.insert(key.clone(), scope);
                result.values.insert(key, value);
            }
        }
        for text in BUILTIN_DENY {
            result.rules.push(Rule {
                kind: RuleKind::Deny,
                text: text.to_owned(),
                origin: Origin::BuiltIn,
                active: true,
            });
        }
        result
    }

    // ----------------------------------------------------------------- writing

    fn editable(&self, scope: Scope) -> Result<(PathBuf, Map<String, Value>), String> {
        let path = self.path(scope);
        let mut warnings = Vec::new();
        let data = read_json(&path, &mut warnings);
        let has_content = fs::metadata(&path).is_ok_and(|meta| meta.len() > 0);
        if let Some(warning) = warnings.into_iter().next()
            && has_content
        {
            // Never overwrite a file we could not understand.
            return Err(warning);
        }
        Ok((path, data))
    }

    fn write(&self, scope: Scope, path: &Path, data: &Map<String, Value>) -> Result<(), String> {
        let dir = path.parent().ok_or("no parent directory")?;
        fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let mut text = serde_json::to_string_pretty(data).map_err(|error| error.to_string())?;
        text.push('\n');
        let temp = dir.join(format!(".settings-{}.tmp", std::process::id()));
        fs::write(&temp, text).map_err(|error| format!("{}: {error}", temp.display()))?;
        #[cfg(unix)]
        if scope == Scope::User {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&temp, fs::Permissions::from_mode(0o600));
        }
        fs::rename(&temp, path).map_err(|error| {
            let _ = fs::remove_file(&temp);
            format!("{}: {error}", path.display())
        })?;
        if scope == Scope::Local {
            let ignore = dir.join(".gitignore");
            if !ignore.exists() {
                let _ = fs::write(ignore, "settings.local.json\nSIERRALAB.local.md\n");
            }
        }
        Ok(())
    }

    /// Set one scalar setting in one file (`None` clears it).
    pub fn set_value(
        &self,
        scope: Scope,
        key: &str,
        value: Option<Value>,
    ) -> Result<PathBuf, String> {
        if entry(key).is_none() {
            return Err(format!("unknown setting '{key}'"));
        }
        if let Some(value) = &value {
            check(key, value, scope.name())?;
        }
        let (path, mut data) = self.editable(scope)?;
        let parts: Vec<&str> = key.split('.').collect();
        let (leaf, parents) = parts.split_last().ok_or("empty key")?;
        let mut node = &mut data;
        for part in parents {
            let child = node
                .entry((*part).to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            node = child
                .as_object_mut()
                .ok_or_else(|| format!("{}: '{part}' is not an object", path.display()))?;
        }
        match value {
            Some(value) => {
                node.insert((*leaf).to_owned(), value);
            }
            None => {
                node.shift_remove(*leaf);
            }
        }
        self.write(scope, &path, &data)?;
        Ok(path)
    }

    pub fn add_rule(&self, kind: RuleKind, rule: &str, scope: Scope) -> Result<PathBuf, String> {
        let rule = rule.trim();
        if rule.is_empty() || SECRET_VALUE.is_match(rule) {
            return Err("empty rule, or it looks like it holds a secret".into());
        }
        let (path, mut data) = self.editable(scope)?;
        let permissions = data
            .entry("permissions")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or("'permissions' is not an object")?;
        let list = permissions
            .entry(kind.name())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| format!("'permissions.{}' is not a list", kind.name()))?;
        if !list.iter().any(|item| item.as_str() == Some(rule)) {
            list.push(Value::String(rule.to_owned()));
        }
        self.write(scope, &path, &data)?;
        Ok(path)
    }

    /// Remove `rule` (any kind) from every file that holds it.
    pub fn remove_rule(&self, rule: &str) -> Result<Vec<PathBuf>, String> {
        let mut changed = Vec::new();
        for scope in Scope::ALL {
            let (path, mut data) = self.editable(scope)?;
            let Some(permissions) = data.get_mut("permissions").and_then(Value::as_object_mut)
            else {
                continue;
            };
            let mut touched = false;
            for kind in RuleKind::ALL {
                if let Some(list) = permissions
                    .get_mut(kind.name())
                    .and_then(Value::as_array_mut)
                {
                    let before = list.len();
                    list.retain(|item| item.as_str() != Some(rule));
                    touched |= list.len() != before;
                }
            }
            if touched {
                self.write(scope, &path, &data)?;
                changed.push(path);
            }
        }
        Ok(changed)
    }

    /// Mark this folder trusted (user file): project and local `allow` rules and
    /// an `auto` default mode then apply.
    pub fn trust(&self) -> Result<PathBuf, String> {
        let (path, mut data) = self.editable(Scope::User)?;
        let here = real(&self.cwd).display().to_string();
        let list = data
            .entry("trustedProjects")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or("'trustedProjects' is not a list")?;
        if !list.iter().any(|item| item.as_str() == Some(&here)) {
            list.push(Value::String(here));
        }
        self.write(Scope::User, &path, &data)?;
        Ok(path)
    }
}

/// The guidance files the backend puts in the system prompt, found the same way
/// as `settings_store.instruction_files`.
pub fn instruction_files(store: &Store) -> Vec<(Scope, PathBuf)> {
    let project = [
        store.cwd.join("SIERRALAB.md"),
        store.cwd.join(".sierralab").join("SIERRALAB.md"),
        store.cwd.join("AGENTS.md"),
        store.cwd.join("CLAUDE.md"),
    ];
    let candidates: [(Scope, Vec<PathBuf>); 3] = [
        (Scope::User, vec![store.user_dir.join("SIERRALAB.md")]),
        (Scope::Project, project.to_vec()),
        (Scope::Local, vec![store.cwd.join("SIERRALAB.local.md")]),
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
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/settings");
        Store {
            user_dir: root.join("home"),
            cwd: root.join("project"),
        }
    }

    fn summary(settings: &Settings) -> Value {
        serde_json::json!({
            "trusted": settings.trusted,
            "values": settings.values,
            "sources": settings.sources.iter().map(|(k, v)| (k.clone(), Value::from(v.name()))).collect::<Map<_, _>>(),
            "rules": settings.rules.iter().map(|r| serde_json::json!([r.kind.name(), r.text, r.origin.name(), r.active])).collect::<Vec<_>>(),
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
                    .join("../../fixtures/settings/expected.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(summary(&fixture_store().load()), expected);
    }

    #[test]
    fn scopes_override_and_permission_lists_accumulate() {
        let sb = sandbox();
        put(
            &sb.store,
            Scope::User,
            r#"{"model":"u","temperature":"focused","permissions":{"deny":["shell(a)"]}}"#,
        );
        put(
            &sb.store,
            Scope::Project,
            r#"{"model":"p","permissions":{"deny":["shell(b)"]}}"#,
        );
        put(
            &sb.store,
            Scope::Local,
            r#"{"model":"l","permissions":{"deny":["shell(c)"]}}"#,
        );
        let loaded = sb.store.load();
        assert_eq!(loaded.text("model"), Some("l"));
        assert_eq!(loaded.sources["model"], Scope::Local);
        assert_eq!(loaded.sources["temperature"], Scope::User);
        let denies: Vec<&str> = loaded
            .rules_for(RuleKind::Deny)
            .filter(|rule| rule.origin != Origin::BuiltIn)
            .map(|rule| rule.text.as_str())
            .collect();
        assert_eq!(denies, ["shell(a)", "shell(b)", "shell(c)"]);
    }

    #[test]
    fn bad_values_files_and_secrets_are_reported_not_fatal() {
        let sb = sandbox();
        put(
            &sb.store,
            Scope::User,
            r#"{"temperature":"lava","tui":{"showLaya":"yes"},"mystery":1,"api_key":"x","model":"ok"}"#,
        );
        put(&sb.store, Scope::Project, "{not json");
        put(
            &sb.store,
            Scope::Local,
            r#"{"model":"sk-abcdefghijklmnopqrstuvwxyz0123456789"}"#,
        );
        let loaded = sb.store.load();
        assert_eq!(loaded.text("model"), Some("ok"));
        assert!(loaded.get("temperature").is_none() && loaded.get("tui.showLaya").is_none());
        let text = loaded.warnings.join("\n");
        for needle in [
            "lava",
            "showLaya",
            "unknown setting 'mystery'",
            "looks like a credential",
            "not valid JSON",
            "looks like a secret",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
    }

    #[test]
    fn a_stranger_folder_cannot_switch_off_the_prompts_until_trusted() {
        let sb = sandbox();
        put(
            &sb.store,
            Scope::User,
            r#"{"permissions":{"allow":["shell(ls)"]}}"#,
        );
        put(
            &sb.store,
            Scope::Project,
            r#"{"permissions":{"defaultMode":"auto","allow":["shell(*)"],"deny":["shell(rm *)"]}}"#,
        );
        let loaded = sb.store.load();
        let allows: Vec<&str> = loaded
            .rules_for(RuleKind::Allow)
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(allows, ["shell(ls)"]);
        assert!(loaded.get("permissions.defaultMode").is_none());
        assert!(
            loaded
                .rules_for(RuleKind::Deny)
                .any(|r| r.text == "shell(rm *)")
        );

        sb.store.trust().unwrap();
        let trusted = sb.store.load();
        assert!(trusted.trusted);
        assert!(
            trusted
                .rules_for(RuleKind::Allow)
                .any(|r| r.text == "shell(*)")
        );
        assert_eq!(trusted.text("permissions.defaultMode"), Some("auto"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(sb.store.path(Scope::User))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the user file is private");
        }
    }

    #[test]
    fn writes_are_validated_idempotent_and_never_clobber_a_broken_file() {
        let sb = sandbox();
        sb.store
            .add_rule(RuleKind::Allow, "shell(git status)", Scope::Local)
            .unwrap();
        sb.store
            .add_rule(RuleKind::Allow, "shell(git status)", Scope::Local)
            .unwrap();
        let local: Value =
            serde_json::from_str(&fs::read_to_string(sb.store.path(Scope::Local)).unwrap())
                .unwrap();
        assert_eq!(
            local,
            serde_json::json!({"permissions": {"allow": ["shell(git status)"]}})
        );
        let ignore = fs::read_to_string(sb.store.cwd.join(".sierralab/.gitignore")).unwrap();
        assert!(ignore.contains("settings.local.json"));

        sb.store
            .set_value(Scope::Project, "tui.thinking", Some("expanded".into()))
            .unwrap();
        sb.store
            .set_value(Scope::Project, "tui.thinking", None)
            .unwrap();
        let project: Value =
            serde_json::from_str(&fs::read_to_string(sb.store.path(Scope::Project)).unwrap())
                .unwrap();
        assert_eq!(project, serde_json::json!({"tui": {}}));

        for (key, value) in [
            ("temperature", "lava"),
            ("nope", "x"),
            ("model", "sk-abcdefghijklmnopqrstuvwxyz0123456789"),
        ] {
            assert!(
                sb.store
                    .set_value(Scope::User, key, Some(value.into()))
                    .is_err(),
                "{key}={value}"
            );
        }

        fs::write(sb.store.path(Scope::Project), "{broken").unwrap();
        assert!(
            sb.store
                .set_value(Scope::Project, "model", Some("x".into()))
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(sb.store.path(Scope::Project)).unwrap(),
            "{broken"
        );

        fs::write(
            sb.store.path(Scope::Project),
            r#"{"permissions":{"deny":["shell(x)"]}}"#,
        )
        .unwrap();
        assert_eq!(sb.store.remove_rule("shell(x)").unwrap().len(), 1);
        assert!(
            sb.store
                .load()
                .rules_for(RuleKind::Deny)
                .all(|r| r.origin == Origin::BuiltIn)
        );
    }

    #[test]
    fn preferences_default_sensibly_and_follow_the_files() {
        let sb = sandbox();
        assert_eq!(
            sb.store.load().prefs(),
            Prefs {
                thinking_expanded: false,
                tools_expanded: false,
                show_laya: true
            }
        );
        put(
            &sb.store,
            Scope::User,
            r#"{"tui":{"thinking":"expanded","toolOutput":"expanded","showLaya":false}}"#,
        );
        assert_eq!(
            sb.store.load().prefs(),
            Prefs {
                thinking_expanded: true,
                tools_expanded: true,
                show_laya: false
            }
        );
    }

    #[test]
    fn instruction_files_prefer_sierralab_md_over_other_tools_files() {
        let sb = sandbox();
        assert!(instruction_files(&sb.store).is_empty());
        fs::write(sb.store.cwd.join("CLAUDE.md"), "x").unwrap();
        fs::write(sb.store.cwd.join("AGENTS.md"), "x").unwrap();
        assert!(instruction_files(&sb.store)[0].1.ends_with("AGENTS.md"));
        fs::write(sb.store.cwd.join("SIERRALAB.md"), "x").unwrap();
        fs::write(sb.store.user_dir.join("SIERRALAB.md"), "x").unwrap();
        let found = instruction_files(&sb.store);
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].0, found[1].0), (Scope::User, Scope::Project));
        assert!(found[1].1.ends_with("SIERRALAB.md"));
    }
}
