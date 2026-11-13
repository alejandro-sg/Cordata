//! What the person configured for the chat agent (`chat.*` in the settings files): rules that allow or refuse a tool
//! call without asking, whether commands may reach the network, which extra folders commands may write, and whether
//! sessions are kept on disk.
//!
//! A repository's own files (`.cordata/settings.json`, `.cordata/settings.local.json`) can only tighten: they may add
//! `chat.deny` rules, set `chat.network` to `"off"` and `chat.history` to `"off"`. `chat.allow` and `chat.writable`
//! loosen what the agent may do without the person, so only `~/.cordata/settings.json` may set them; elsewhere they are
//! ignored and reported.
//!
//! Rules are `tool` or `tool(pattern)`. For `run_command` the pattern is a glob over the whole command (`*` matches
//! anything); for the file tools it is a glob over the project-relative path (`*` stays inside one folder, `**` crosses
//! folders). Deny wins over allow, and allow over asking. An allow rule never matches a command that chains, pipes,
//! redirects or substitutes (`; & | $ \` < > ( )`, a backslash or a newline), and never a command that asks for the
//! network: those always ask. A deny rule is checked against every segment of a chained command as well as the whole.

use std::path::{Component, Path, PathBuf};

use regex::Regex;
use serde_json::Value;

/// Whether `run_command` may ask for network access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// Commands run offline; one that sets `network: true` asks, and its card says so (the default).
    Ask,
    /// Commands run offline and a request for the network is refused without asking.
    Off,
}

/// Who answers the approval card when no rule decides (`chat.mode`, read only from the user file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Every call no rule decides asks the person (the default).
    #[default]
    Ask,
    /// `auto::judge` runs the calls it can show are safe and asks for the rest.
    Auto,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Auto => "auto",
        }
    }
}

/// What the rules say about one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Refused without asking; the rule that matched.
    Deny(String),
    /// Runs without asking; the rule that matched.
    Allow(String),
    Ask,
}

#[derive(Debug, Clone)]
struct Rule {
    text: String,
    tool: String,
    /// `None`: every call of the tool.
    pattern: Option<Regex>,
}

#[derive(Debug, Clone)]
pub struct Policy {
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    pub network: Network,
    /// Absolute folders `run_command` may write besides the project, temporary folders and package caches.
    pub writable: Vec<PathBuf>,
    /// Keep sessions in `~/.cordata/sessions` so they can be resumed (`chat.history`, default on).
    pub history: bool,
    /// `chat.mode`: only the user file may set it; a repository's file is ignored with a line in `ignored`.
    pub mode: Mode,
    /// Entries that could not be used (not a rule, not a list, not an absolute path), worded for the person.
    pub warnings: Vec<String>,
    /// Loosening keys a repository's file tried to set; the settings panel reports these too.
    pub ignored: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self { allow: Vec::new(), deny: Vec::new(), network: Network::Ask, writable: Vec::new(), history: true, mode: Mode::Ask, warnings: Vec::new(), ignored: Vec::new() }
    }
}

/// `$CORDATA_HOME`, or `~/.cordata`.
/// Drops a `CORDATA_HOME` the current repository could have chosen (`cordata_guard::home`). First thing in `main`.
pub use cordata_guard::home::enforce as enforce_cordata_home;

pub fn cordata_home() -> Option<PathBuf> {
    std::env::var_os("CORDATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cordata")))
}

const TOOLS: [&str; 5] = ["read_file", "edit_file", "run_command", "code_search", "code_outline"];
/// Characters that let one command run another, or feed it from somewhere an allow rule did not name.
const SHELL_CONTROL: [char; 12] = [';', '&', '|', '`', '$', '<', '>', '(', ')', '\\', '\n', '\r'];

fn lookup<'a>(doc: &'a Value, dotted: &str) -> Option<&'a Value> {
    doc.get(dotted).or_else(|| dotted.split('.').try_fold(doc, |v, part| v.get(part)))
}

fn strings(value: Option<&Value>, key: &str, file: &str, warnings: &mut Vec<String>) -> Vec<String> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item.as_str() {
                Some(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
                _ => {
                    warnings.push(format!("{file}: an entry of '{key}' is not a string, ignored"));
                    None
                }
            })
            .collect(),
        Some(_) => {
            warnings.push(format!("{file}: '{key}' must be a list of strings, ignored"));
            Vec::new()
        }
    }
}

/// A glob as an anchored regex. For paths `*` stops at `/` and `**` does not, and case is ignored (APFS ignores it, so
/// `Migrations/x` is the same folder as `migrations/x`); for commands `*` matches anything.
fn glob(pattern: &str, path_like: bool) -> Regex {
    let mut out = String::from(if path_like { "(?i)^" } else { "^" });
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if path_like && chars.peek() == Some(&'*') => {
                chars.next();
                // `src/**/x` also matches `src/x`.
                if chars.peek() == Some(&'/') {
                    chars.next();
                    out.push_str("(?:.*/)?");
                } else {
                    out.push_str(".*");
                }
            }
            '*' if path_like => out.push_str("[^/]*"),
            '*' => out.push_str(".*"),
            '?' => out.push_str(if path_like { "[^/]" } else { "." }),
            other => out.push_str(&regex::escape(&other.to_string())),
        }
    }
    out.push('$');
    Regex::new(&out).expect("an escaped glob is a valid regex")
}

fn parse_rule(text: &str) -> Result<Rule, String> {
    let (tool, pattern) = match text.split_once('(') {
        Some((tool, rest)) => {
            let inner = rest.strip_suffix(')').ok_or_else(|| format!("'{text}' has no closing parenthesis"))?;
            (tool.trim(), Some(inner.trim()))
        }
        None => (text.trim(), None),
    };
    if !TOOLS.contains(&tool) {
        return Err(format!("'{text}' names no tool (one of {})", TOOLS.join(", ")));
    }
    let pattern = pattern.filter(|p| !p.is_empty() && *p != "*").map(|p| glob(p, tool != "run_command"));
    Ok(Rule { text: text.to_owned(), tool: tool.to_owned(), pattern })
}

fn normalize(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    clean
}

/// The project-relative path a file tool names, or `None` when it points outside the project.
pub(crate) fn relative(raw: &str, cwd: &Path) -> Option<String> {
    let joined = normalize(&if Path::new(raw).is_absolute() { PathBuf::from(raw) } else { cwd.join(raw) });
    let root = normalize(cwd);
    let rel = joined.strip_prefix(&root).ok()?;
    Some(rel.to_string_lossy().into_owned())
}

/// Words that start a shell clause or wrap a command without changing what it runs.
const PREFIXES: [&str; 16] = ["then", "do", "else", "elif", "if", "while", "until", "!", "time", "exec", "command", "builtin", "env", "nohup", "sudo", "xargs"];

/// The simple commands inside `cmd`, as a deny rule should see them: split at every operator, subshell, substitution
/// and quote; leading keywords, wrappers and `VAR=value` dropped; `/usr/bin/git` read as `git`; spaces collapsed.
/// Best effort: a shell can always spell a command in a way no pattern foresees, which is why deny rules sit in front
/// of the approval card and the sandbox, not instead of them.
pub(crate) fn simple_commands(cmd: &str) -> Vec<String> {
    let separators = [';', '&', '|', '\n', '\r', '(', ')', '`', '{', '}', '\'', '"'];
    cmd.replace("$(", "(")
        .split(separators)
        .filter_map(|segment| {
            let mut words: Vec<&str> = segment.split_whitespace().collect();
            while let Some(first) = words.first() {
                let assignment = first.contains('=') && !first.starts_with('=') && first.split('=').next().is_some_and(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
                if PREFIXES.contains(first) || assignment || (first.starts_with('-') && words.len() > 1) {
                    words.remove(0);
                } else {
                    break;
                }
            }
            let (first, rest) = words.split_first()?;
            let name = first.rsplit('/').next().unwrap_or(first);
            Some(std::iter::once(name).chain(rest.iter().copied()).collect::<Vec<_>>().join(" "))
        })
        .collect()
}

/// What a rule's pattern is matched against, per segment for a deny rule.
fn subjects(tool: &str, args: &Value, cwd: &Path) -> Vec<String> {
    let text = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default();
    match tool {
        "run_command" => {
            let cmd = text("cmd").trim();
            let mut out = vec![cmd.to_owned()];
            out.extend(simple_commands(cmd).into_iter().filter(|s| s != cmd));
            out
        }
        "code_search" => relative(if text("path").is_empty() { "." } else { text("path") }, cwd).into_iter().collect(),
        _ => relative(text("path"), cwd).into_iter().collect(),
    }
}

pub fn wants_network(args: &Value) -> bool {
    args.get("network").and_then(Value::as_bool).unwrap_or(false)
}

impl Policy {
    /// The policy for `cwd`, from the user file, then the project's, then this machine's.
    pub fn load(cwd: &Path) -> Self {
        Self::load_in(cordata_home().as_deref(), cwd)
    }

    /// [`Policy::load`] with the user file under `home` (`None`: no user file).
    pub fn load_in(home: Option<&Path>, cwd: &Path) -> Self {
        let read = |path: PathBuf| -> (String, Option<Value>) {
            let name = path.display().to_string();
            let value = std::fs::read_to_string(&path).ok().filter(|t| !t.trim().is_empty()).and_then(|t| serde_json::from_str(&t).ok());
            (name, value)
        };
        let user_path = home.map(|h| h.join("settings.json"));
        let user = user_path.clone().map(read);
        // A user file that exists but does not parse may have held the person's restrictions: fail closed.
        let broken = user_path.as_ref().filter(|p| std::fs::read_to_string(p).is_ok_and(|t| !t.trim().is_empty() && serde_json::from_str::<Value>(&t).is_err()));
        let project = read(cwd.join(".cordata/settings.json"));
        let local = read(cwd.join(".cordata/settings.local.json"));
        let mut files = Vec::new();
        if let Some((name, Some(v))) = user {
            files.push((name, v, true));
        }
        for (name, value) in [project, local] {
            if let Some(v) = value {
                files.push((name, v, false));
            }
        }
        let mut policy = Self::from_files(&files);
        if let Some(path) = broken {
            policy.allow.clear();
            policy.writable.clear();
            policy.network = Network::Off;
            policy.history = false;
            policy.mode = Mode::Ask;
            policy.warnings.push(format!(
                "{}: not valid JSON, so no allow rules, no network, no history and no auto mode until it is fixed (`cordata doctor`)",
                path.display()
            ));
        }
        policy
    }

    /// `(file name, document, is the user file)`, lowest precedence first. Invalid files are `cordata doctor`'s to report.
    pub fn from_files(files: &[(String, Value, bool)]) -> Self {
        let mut policy = Policy::default();
        let warnings = &mut policy.warnings;
        for (file, doc, user) in files {
            let rules = |key: &str, warnings: &mut Vec<String>| -> Vec<Rule> {
                strings(lookup(doc, key), key, file, warnings)
                    .into_iter()
                    .filter_map(|text| parse_rule(&text).map_err(|e| warnings.push(format!("{file}: {key}: {e}, ignored"))).ok())
                    .collect()
            };
            policy.deny.extend(rules("chat.deny", warnings));
            if *user {
                policy.allow.extend(rules("chat.allow", warnings));
                for raw in strings(lookup(doc, "chat.writable"), "chat.writable", file, warnings) {
                    let expanded = match raw.strip_prefix("~/") {
                        Some(rest) => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)),
                        None => Some(PathBuf::from(&raw)),
                    };
                    match expanded.filter(|p| p.is_absolute()) {
                        Some(p) => policy.writable.push(p.canonicalize().unwrap_or(p)),
                        None => warnings.push(format!("{file}: chat.writable: '{raw}' is not an absolute path, ignored")),
                    }
                }
                match lookup(doc, "chat.mode").map(|v| v.as_str()) {
                    None => {}
                    Some(Some("ask")) => policy.mode = Mode::Ask,
                    Some(Some("auto")) => policy.mode = Mode::Auto,
                    Some(_) => warnings.push(format!("{file}: chat.mode must be \"ask\" or \"auto\", ignored")),
                }
            } else {
                for key in ["chat.allow", "chat.writable", "chat.mode"] {
                    if lookup(doc, key).is_some() {
                        policy.ignored.push(format!("{file}: '{key}' ignored, only ~/.cordata/settings.json may set it"));
                    }
                }
            }
            match lookup(doc, "chat.network").map(|v| v.as_str()) {
                None => {}
                Some(Some("off")) => policy.network = Network::Off,
                Some(Some("ask")) => {
                    if *user {
                        policy.network = Network::Ask;
                    }
                }
                Some(_) => warnings.push(format!("{file}: chat.network must be \"ask\" or \"off\", ignored")),
            }
            match lookup(doc, "chat.history").map(|v| v.as_str()) {
                None => {}
                Some(Some("off")) => policy.history = false,
                Some(Some("on")) => {
                    if *user {
                        policy.history = true;
                    }
                }
                Some(_) => warnings.push(format!("{file}: chat.history must be \"on\" or \"off\", ignored")),
            }
        }
        policy
    }

    /// Deny rules first, then allow rules; `Ask` when none decides.
    pub fn decide(&self, tool: &str, args: &Value, cwd: &Path) -> Verdict {
        let subjects = subjects(tool, args, cwd);
        let matches = |rule: &Rule, subject: &str| rule.pattern.as_ref().is_none_or(|re| re.is_match(subject));
        for rule in self.deny.iter().filter(|r| r.tool == tool) {
            // A file tool pointing outside the project has no subject; a deny rule without a pattern still covers it.
            if rule.pattern.is_none() || subjects.iter().any(|s| matches(rule, s)) {
                return Verdict::Deny(rule.text.clone());
            }
        }
        if tool == "run_command" && (wants_network(args) || subjects[0].contains(SHELL_CONTROL)) {
            return Verdict::Ask;
        }
        let whole = subjects.first();
        match self.allow.iter().find(|r| r.tool == tool && whole.is_some_and(|s| matches(r, s))) {
            Some(rule) => Verdict::Allow(rule.text.clone()),
            None => Verdict::Ask,
        }
    }

    /// One line per rule and setting, for `/permissions`.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let list = |rules: &[Rule]| if rules.is_empty() { "none".to_owned() } else { rules.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join(", ") };
        lines.push(format!("allow without asking: {}", list(&self.allow)));
        lines.push(format!("refuse without asking: {}", list(&self.deny)));
        lines.push(match self.network {
            Network::Ask => "network: commands run offline; a command that asks for the network asks you first".to_owned(),
            Network::Off => "network: commands run offline; requests for the network are refused".to_owned(),
        });
        let extra = if self.writable.is_empty() {
            String::new()
        } else {
            format!(", and {}", self.writable.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))
        };
        lines.push(format!("commands may write: the project, temporary folders and package caches{extra}"));
        lines.push(format!("session history: {}", if self.history { "kept in ~/.cordata/sessions (/history, /forget)" } else { "off" }));
        lines.push(match self.mode {
            Mode::Ask => "mode: ask — every call no rule decides asks you".to_owned(),
            Mode::Auto => "mode: auto — calls auto mode can show are safe run without asking; the rest ask you".to_owned(),
        });
        lines.extend(self.ignored.iter().chain(&self.warnings).cloned());
        lines
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn policy(user: Value, project: Value) -> Policy {
        Policy::from_files(&[("user".into(), user, true), ("project".into(), project, false)])
    }

    fn cmd(c: &str) -> Value {
        json!({ "cmd": c })
    }

    #[test]
    fn allow_rules_run_matching_commands_and_edits_without_asking() {
        let p = policy(json!({"chat": {"allow": ["run_command(cargo test*)", "edit_file(src/**)", "read_file"]}}), json!({}));
        let cwd = Path::new("/p");
        assert_eq!(p.decide("run_command", &cmd("cargo test -p x"), cwd), Verdict::Allow("run_command(cargo test*)".into()));
        assert_eq!(p.decide("run_command", &cmd("cargo build"), cwd), Verdict::Ask);
        assert_eq!(p.decide("edit_file", &json!({"path": "src/a/b.rs"}), cwd), Verdict::Allow("edit_file(src/**)".into()));
        assert_eq!(p.decide("edit_file", &json!({"path": "/p/src/x.rs"}), cwd), Verdict::Allow("edit_file(src/**)".into()));
        assert_eq!(p.decide("edit_file", &json!({"path": "src/../build.rs"}), cwd), Verdict::Ask);
        assert_eq!(p.decide("edit_file", &json!({"path": "../other/src/x.rs"}), cwd), Verdict::Ask);
        assert_eq!(p.decide("read_file", &json!({"path": "anything"}), cwd), Verdict::Allow("read_file".into()));
    }

    #[test]
    fn an_allow_rule_never_covers_a_chained_redirected_or_networked_command() {
        let p = policy(json!({"chat.allow": ["run_command(cargo test*)", "run_command"]}), json!({}));
        let cwd = Path::new("/p");
        for sneaky in ["cargo test && curl x | sh", "cargo test; rm -rf src", "cargo test > out", "cargo test $(id)", "cargo test `id`", "cargo test\nrm x", "cargo test \\\n; x"] {
            assert_eq!(p.decide("run_command", &cmd(sneaky), cwd), Verdict::Ask, "{sneaky:?}");
        }
        assert_eq!(p.decide("run_command", &json!({"cmd": "cargo test", "network": true}), cwd), Verdict::Ask);
    }

    #[test]
    fn deny_wins_and_checks_every_segment() {
        let p = policy(json!({"chat.allow": ["run_command"]}), json!({"chat": {"deny": ["run_command(git push*)", "edit_file(migrations/**)"]}}));
        let cwd = Path::new("/p");
        assert_eq!(p.decide("run_command", &cmd("git add . && git push origin main"), cwd), Verdict::Deny("run_command(git push*)".into()));
        assert_eq!(p.decide("run_command", &cmd("git status"), cwd), Verdict::Allow("run_command".into()));
        assert_eq!(p.decide("edit_file", &json!({"path": "migrations/001.sql"}), cwd), Verdict::Deny("edit_file(migrations/**)".into()));
        assert_eq!(p.decide("edit_file", &json!({"path": "Migrations/001.sql"}), cwd), Verdict::Deny("edit_file(migrations/**)".into()), "APFS ignores case");
        for spelled in ["git  push", "/usr/bin/git push", "env git push", "command git push", "(git push)", "echo $(git push)", "echo `git push`", "if true; then git push; fi", "sh -c 'git push'", "GIT_DIR=x git push", "xargs git push"] {
            assert!(matches!(p.decide("run_command", &cmd(spelled), cwd), Verdict::Deny(_)), "{spelled:?}");
        }
    }

    #[test]
    fn a_broken_user_file_fails_closed() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("settings.json"), r#"{"chat": {"allow": ["run_command"], "network": "off",}}"#).unwrap();
        let p = Policy::load_in(Some(home.path()), Path::new("/nonexistent"));
        assert_eq!(p.decide("run_command", &cmd("ls"), Path::new("/p")), Verdict::Ask);
        assert_eq!((p.network, p.history), (Network::Off, false));
        assert!(p.warnings[0].contains("not valid JSON"), "{:?}", p.warnings);
    }

    #[test]
    fn a_repository_cannot_loosen_what_the_person_set() {
        let p = policy(
            json!({"chat": {"network": "off", "history": "off"}}),
            json!({"chat": {"allow": ["run_command"], "writable": ["/"], "network": "ask", "history": "on"}}),
        );
        assert_eq!(p.decide("run_command", &cmd("ls"), Path::new("/p")), Verdict::Ask);
        assert!(p.writable.is_empty());
        assert_eq!(p.network, Network::Off);
        assert!(!p.history);
        assert_eq!(p.ignored.len(), 2, "{:?}", p.ignored);
        // …but it may tighten.
        let p = policy(json!({}), json!({"chat": {"network": "off", "history": "off"}}));
        assert_eq!((p.network, p.history), (Network::Off, false));
    }

    #[test]
    fn chat_mode_is_read_only_from_the_user_file() {
        assert_eq!(policy(json!({}), json!({})).mode, Mode::Ask);
        assert_eq!(policy(json!({"chat": {"mode": "auto"}}), json!({})).mode, Mode::Auto);
        assert_eq!(policy(json!({"chat.mode": "auto"}), json!({})).mode, Mode::Auto);
        // A repository cannot turn auto mode on…
        let p = policy(json!({}), json!({"chat": {"mode": "auto"}}));
        assert_eq!(p.mode, Mode::Ask);
        assert!(p.ignored.iter().any(|w| w.contains("'chat.mode' ignored")), "{:?}", p.ignored);
        assert!(p.describe().iter().any(|l| l.contains("'chat.mode' ignored")));
        // …nor off, and the local file is no different.
        let p = Policy::from_files(&[
            ("user".into(), json!({"chat.mode": "auto"}), true),
            ("project".into(), json!({"chat.mode": "ask"}), false),
            ("local".into(), json!({"chat": {"mode": "ask"}}), false),
        ]);
        assert_eq!(p.mode, Mode::Auto);
        assert_eq!(p.ignored.len(), 2, "{:?}", p.ignored);
        let p = policy(json!({"chat": {"mode": "yolo"}}), json!({}));
        assert_eq!(p.mode, Mode::Ask);
        assert!(p.warnings.iter().any(|w| w.contains("chat.mode must be")), "{:?}", p.warnings);
    }

    #[test]
    fn a_broken_user_file_never_leaves_auto_mode_on() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("settings.json"), r#"{"chat": {"mode": "auto",}}"#).unwrap();
        assert_eq!(Policy::load_in(Some(home.path()), Path::new("/nonexistent")).mode, Mode::Ask);
    }

    #[test]
    fn bad_entries_are_reported_and_skipped() {
        let p = policy(json!({"chat": {"allow": ["rm_rf", "run_command(ls", 3], "writable": ["relative/dir"], "network": "yes"}}), json!({}));
        assert!(p.allow.is_empty() && p.writable.is_empty());
        assert_eq!(p.warnings.len(), 5, "{:?}", p.warnings);
    }

    #[test]
    fn globs_respect_folders_for_paths_only() {
        assert!(glob("src/*.rs", true).is_match("src/a.rs"));
        assert!(!glob("src/*.rs", true).is_match("src/a/b.rs"));
        assert!(glob("src/**/*.rs", true).is_match("src/a.rs") && glob("src/**/*.rs", true).is_match("src/a/b/c.rs"));
        assert!(glob("npm run *", false).is_match("npm run lint/fix"));
        assert!(!glob("a.b", true).is_match("axb"), "dots are literal");
    }
}
