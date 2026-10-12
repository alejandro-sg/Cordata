//! `/config`, `/permissions`, `/trust`, `/memory`, `/init`, `/status`: the
//! commands that read and change the layered settings (see `settings.rs`).
//!
//! Everything here is a method on `App` so it stays unit-testable without a
//! terminal; files are only touched through `settings::Store`.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent};
use serde_json::Value;

use crate::app::{App, Effect, Message, sanitize};
use crate::settings::{
    Applies, Entry, Kind, Origin, Prefs, RuleKind, SCHEMA, Scope, Settings, Store, entry,
    instruction_files,
};

/// The interactive `/config` panel's state; its rows come from `settings::SCHEMA`.
#[derive(Debug)]
pub struct ConfigPanel {
    pub selected: usize,
    /// The file a change is written to.
    pub scope: Scope,
    pub status: String,
}

const FLOAT_STEPS: [f64; 6] = [0.5, 0.6, 0.7, 0.85, 0.9, 0.95];

const INIT_PROMPT: &str = "Look through this project (its README, manifests and layout) and write a CORDATA.md in the project root: concise guidance for a coding agent working here — how to build, test and run it, how the code is laid out, the conventions to follow, and any gotchas. Keep it under 60 lines and state only what you verified in the files; ask me about anything you can't tell.";

/// `$HOME` → `~`, for messages.
pub fn tidy(path: &Path) -> String {
    let text = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && text.starts_with(&home) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text,
    }
}

fn tidy_text(text: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => text.replace(&home, "~"),
        _ => text.to_owned(),
    }
}

/// The value a setting has when no file sets it.
pub(crate) fn default_value(entry: &Entry) -> Value {
    match entry.key {
        "permissions.defaultMode" => "build".into(),
        "temperature" => "balanced".into(),
        "tui.thinking" | "tui.toolOutput" => "collapsed".into(),
        "tui.showLaya" => true.into(),
        "laya.mode" => "route".into(),
        "laya.threshold" => 0.85.into(),
        // Destructiveness scoring: the 0.2 / 0.8 the directive names. Shown
        // here so `/config` reads as "what is in effect", not as blank rows.
        "laya.riskSafe" => 0.2.into(),
        "laya.riskDestroy" => 0.8.into(),
        "laya.riskAutoApprove" => false.into(),
        // Off by default: the trace store needs lancedb, and the proxy and the
        // observer are the two settings that change what runs unattended.
        "memory.traces" => false.into(),
        "vault.enabled" => false.into(),
        "vault.addr" => "http://127.0.0.1:14322".into(),
        "vault.egressPolicy" => "deny".into(),
        "observer.enabled" => false.into(),
        "observer.intervalMinutes" => 30.0.into(),
        _ => Value::Null,
    }
}

fn parse_value(entry: &Entry, text: &str) -> Result<Value, String> {
    match &entry.kind {
        Kind::Bool => match text.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" => Ok(true.into()),
            "false" | "off" | "no" => Ok(false.into()),
            _ => Err(format!("{} takes true or false", entry.key)),
        },
        Kind::Float(..) => text
            .parse::<f64>()
            .map(Value::from)
            .map_err(|_| format!("{} takes a number", entry.key)),
        Kind::Enum(choices) => {
            let lower = text.to_ascii_lowercase();
            choices
                .iter()
                .find(|choice| **choice == lower)
                .map(|choice| Value::from(*choice))
                .ok_or_else(|| format!("{} takes one of: {}", entry.key, choices.join(", ")))
        }
        Kind::Str => Ok(Value::from(text)),
        Kind::BundleIds => {
            let ids: Vec<&str> = text
                .split([',', ' '])
                .filter(|id| !id.is_empty())
                .collect();
            match ids.iter().find(|id| !crate::settings::is_bundle_id(id)) {
                Some(bad) => Err(format!(
                    "{} takes bundle ids like com.apple.Safari ({bad:?} is not one)",
                    entry.key
                )),
                None => Ok(Value::from(ids)),
            }
        }
    }
}

/// `tool` or `tool(pattern)`; the pattern is free text.
fn valid_rule(rule: &str) -> bool {
    let tool = rule.split('(').next().unwrap_or("");
    let shaped = !tool.is_empty()
        && tool
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'));
    shaped && (rule == tool || (rule.ends_with(')') && rule.len() > tool.len() + 2))
}

/// Split a trailing `user|project|local` off `text`.
fn split_scope(text: &str) -> (&str, Option<Scope>) {
    match text.rsplit_once(char::is_whitespace) {
        Some((rest, last)) => match Scope::parse(last) {
            Some(scope) => (rest.trim(), Some(scope)),
            None => (text.trim(), None),
        },
        None => (text.trim(), None),
    }
}

impl App {
    pub fn attach_store(&mut self, store: Store) {
        self.settings = store.load();
        self.custom = crate::custom::load(&store.user_dir, &store.cwd);
        self.store = Some(store);
        let prefs = self.settings.prefs();
        self.apply_prefs(prefs);
    }

    pub(crate) fn reload_settings(&mut self) {
        if let Some(store) = &self.store {
            self.settings = store.load();
        }
    }

    fn apply_prefs(&mut self, prefs: Prefs) {
        self.thoughts_expanded = prefs.thinking_expanded;
        self.expand_tools = prefs.tools_expanded;
        self.show_laya = prefs.show_laya;
    }

    /// The settings problems worth telling the user about once, at connect.
    pub(crate) fn settings_notices(&self) -> Vec<String> {
        const SHOWN: usize = 4;
        let mut notices: Vec<String> = self
            .settings
            .warnings
            .iter()
            .take(SHOWN)
            .map(|warning| format!("settings: {}", tidy_text(warning)))
            .collect();
        if self.settings.warnings.len() > SHOWN {
            notices.push(format!(
                "settings: …and {} more — /config path lists the files",
                self.settings.warnings.len() - SHOWN
            ));
        }
        // Say what is being fed to the agent as instructions: a cloned repo's own
        // AGENTS.md / CLAUDE.md is read too, and that should never be a surprise.
        if let Some(store) = &self.store {
            let found = instruction_files(store);
            if !found.is_empty() {
                let names: Vec<String> = found
                    .iter()
                    .map(|(scope, path)| {
                        let name = path
                            .file_name()
                            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                        format!("{name} ({})", scope.name())
                    })
                    .collect();
                notices.push(format!(
                    "instructions: reading {} — /memory",
                    names.join(", ")
                ));
            }
        }
        notices
    }

    fn store(&mut self) -> Option<Store> {
        let store = self.store.clone();
        if store.is_none() {
            self.messages.push(Message::Info(
                "settings are unavailable in this session".into(),
            ));
        }
        store
    }

    // --------------------------------------------------------------- /config

    pub(crate) fn run_config(&mut self, arg: Option<String>) -> Option<Effect> {
        self.reload_settings();
        let Some(arg) = arg else {
            self.config = Some(ConfigPanel {
                selected: 0,
                scope: Scope::User,
                status: String::new(),
            });
            return None;
        };
        let mut words = arg.split_whitespace();
        match words.next().unwrap_or("") {
            "set" => {
                let rest = arg.trim_start()["set".len()..].trim();
                let (rest, scope) = split_scope(rest);
                let Some((key, value)) = rest.split_once(char::is_whitespace) else {
                    self.info_msg("usage: /config set <key> <value> [user|project|local]");
                    return None;
                };
                self.write_setting(key.trim(), Some(value.trim()), scope.unwrap_or(Scope::User))
            }
            "unset" | "reset" => {
                let rest = arg.trim_start()[arg
                    .trim_start()
                    .find(char::is_whitespace)
                    .unwrap_or(arg.len())..]
                    .trim();
                let (key, scope) = split_scope(rest);
                if key.is_empty() {
                    self.info_msg("usage: /config unset <key> [user|project|local]");
                    return None;
                }
                self.write_setting(key, None, scope.unwrap_or(Scope::User))
            }
            "path" | "paths" | "files" => {
                self.show_paths();
                None
            }
            other => {
                self.info_msg(format!(
                    "unknown /config option '{other}' — use /config, /config set <key> <value>, /config unset <key>, /config path"
                ));
                None
            }
        }
    }

    fn info_msg(&mut self, text: impl Into<String>) {
        self.messages.push(Message::Info(text.into()));
    }

    fn show_paths(&mut self) {
        let Some(store) = self.store() else { return };
        let mut lines =
            vec!["settings files (later ones win; permission lists add up):".to_owned()];
        for scope in Scope::ALL {
            let path = store.path(scope);
            let state = if path.exists() {
                ""
            } else {
                "  (not created yet)"
            };
            lines.push(format!("  {:<8}{}{state}", scope.name(), tidy(&path)));
        }
        let guidance = instruction_files(&store);
        if guidance.is_empty() {
            lines.push("instructions: none — add CORDATA.md (or AGENTS.md / CLAUDE.md) to the project, or run /init".to_owned());
        } else {
            lines.push("instructions read into the system prompt:".to_owned());
            lines.extend(
                guidance
                    .iter()
                    .map(|(scope, path)| format!("  {:<8}{}", scope.name(), tidy(path))),
            );
        }
        let trust = if self.settings.trusted {
            "trusted"
        } else {
            "not trusted (project allow rules and `auto` mode ignored — /trust)"
        };
        lines.push(format!("this folder: {trust}"));
        self.info_msg(lines.join("\n"));
    }

    /// Write (or clear) one setting, then apply what can apply now.
    fn write_setting(&mut self, key: &str, value: Option<&str>, scope: Scope) -> Option<Effect> {
        let store = self.store()?;
        let Some(entry) = entry(key) else {
            let keys: Vec<&str> = SCHEMA.iter().map(|entry| entry.key).collect();
            self.info_msg(format!(
                "unknown setting '{key}' — settings: {}",
                keys.join(", ")
            ));
            return None;
        };
        let parsed = match value {
            Some(text) => match parse_value(entry, text) {
                Ok(value) => Some(value),
                Err(error) => {
                    self.info_msg(error);
                    return None;
                }
            },
            None => None,
        };
        self.commit_setting(&store, entry, scope, parsed)
    }

    fn commit_setting(
        &mut self,
        store: &Store,
        entry: &Entry,
        scope: Scope,
        value: Option<Value>,
    ) -> Option<Effect> {
        let shown = value.as_ref().map_or_else(
            || "cleared".to_owned(),
            |v| {
                format!(
                    "= {}",
                    v.as_str().map_or_else(|| v.to_string(), str::to_owned)
                )
            },
        );
        let result = store.set_value(scope, entry.key, value.clone());
        self.reload_settings();
        let message = match result {
            Ok(path) => format!(
                "{} {shown} in {} ({})",
                entry.key,
                scope.name(),
                tidy(&path)
            ),
            Err(error) => {
                self.set_config_status(format!("✗ {error}"));
                self.info_msg(format!("not saved: {}", tidy_text(&error)));
                return None;
            }
        };
        let prefs = self.settings.prefs();
        self.apply_prefs(prefs);
        let mut note = message;
        let mut effect = None;
        if let Some(value) = value.as_ref().and_then(Value::as_str)
            && matches!(entry.key, "model" | "temperature")
            && self.config(entry.key).is_some()
        {
            effect = Some(Effect::SetConfig {
                id: entry.key.to_owned(),
                value: value.to_owned(),
            });
        }
        if entry.applies == Applies::NextSession {
            note.push_str(" — takes effect for the next session");
        }
        self.set_config_status(format!("✓ {note}"));
        // The panel shows its own status line; the transcript only records
        // changes made with the typed command.
        if self.config.is_none() {
            self.info_msg(note);
        }
        effect
    }

    fn set_config_status(&mut self, text: String) {
        if let Some(panel) = &mut self.config {
            panel.status = text;
        }
    }

    /// The effective value of `entry`, falling back to the session's own value
    /// for model/temperature and then to the built-in default.
    fn effective(&self, entry: &Entry) -> Value {
        if let Some(value) = self.settings.get(entry.key) {
            return value.clone();
        }
        if matches!(entry.key, "model" | "temperature")
            && let Some(option) = self.config(entry.key)
        {
            return Value::from(option.current_value.clone());
        }
        default_value(entry)
    }

    /// The next value when the user presses Enter on a row.
    fn next_value(&self, entry: &Entry) -> Option<Value> {
        let current = self.effective(entry);
        match &entry.kind {
            Kind::Bool => Some(Value::from(current.as_bool() != Some(true))),
            Kind::Enum(choices) => {
                let index = choices
                    .iter()
                    .position(|choice| current.as_str() == Some(*choice));
                Some(Value::from(
                    choices[(index.map_or(0, |i| i + 1)) % choices.len()],
                ))
            }
            Kind::Float(..) => {
                let now = current.as_f64().unwrap_or(0.0);
                let next = FLOAT_STEPS
                    .iter()
                    .copied()
                    .find(|step| *step > now + 1e-9)
                    .unwrap_or(FLOAT_STEPS[0]);
                Some(Value::from(next))
            }
            Kind::Str => {
                let choices: Vec<&str> = self
                    .config("model")?
                    .choices
                    .iter()
                    .map(|c| c.value.as_str())
                    .collect();
                if choices.is_empty() {
                    return None;
                }
                let index = choices
                    .iter()
                    .position(|choice| current.as_str() == Some(*choice));
                Some(Value::from(
                    choices[index.map_or(0, |i| i + 1) % choices.len()],
                ))
            }
            // A list of apps is typed (`/settings set capture.allowlist com.apple.Safari`), not cycled:
            // there is no "next" app to allow.
            Kind::BundleIds => None,
        }
    }

    pub(crate) fn on_key_config(&mut self, key: KeyEvent) -> Option<Effect> {
        let rows = SCHEMA.len();
        let panel = self.config.as_mut()?;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.config = None,
            KeyCode::Up | KeyCode::Char('k') => panel.selected = panel.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                panel.selected = (panel.selected + 1).min(rows - 1)
            }
            KeyCode::Tab | KeyCode::BackTab => {
                panel.scope = match panel.scope {
                    Scope::User => Scope::Project,
                    Scope::Project => Scope::Local,
                    Scope::Local => Scope::User,
                };
                panel.status = format!("changes go to the {} file", panel.scope.name());
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ' | 'l') => {
                let (selected, scope) = (panel.selected, panel.scope);
                let store = self.store()?;
                let entry = &SCHEMA[selected];
                let Some(value) = self.next_value(entry) else {
                    self.set_config_status(format!(
                        "no choices to cycle — /config set {} <value>",
                        entry.key
                    ));
                    return None;
                };
                return self.commit_setting(&store, entry, scope, Some(value));
            }
            KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('d') => {
                let (selected, scope) = (panel.selected, panel.scope);
                let store = self.store()?;
                return self.commit_setting(&store, &SCHEMA[selected], scope, None);
            }
            _ => {}
        }
        None
    }

    // ----------------------------------------------------------- /permissions

    pub(crate) fn run_permissions(&mut self, arg: Option<String>) {
        self.reload_settings();
        let Some(arg) = arg else {
            self.show_permissions();
            return;
        };
        let (action, rest) = arg
            .split_once(char::is_whitespace)
            .unwrap_or((arg.as_str(), ""));
        match action {
            "remove" | "rm" | "delete" => {
                let rule = rest.trim();
                let Some(store) = self.store() else { return };
                match store.remove_rule(rule) {
                    Ok(paths) if paths.is_empty() => self.info_msg(format!("no such rule: {rule}")),
                    Ok(paths) => {
                        let files: Vec<String> = paths.iter().map(|path| tidy(path)).collect();
                        self.info_msg(format!("removed {rule} from {}", files.join(", ")));
                    }
                    Err(error) => self.info_msg(format!("not removed: {}", tidy_text(&error))),
                }
                self.reload_settings();
            }
            other => match RuleKind::parse(other) {
                Some(kind) => {
                    let (rule, scope) = split_scope(rest);
                    if !valid_rule(rule) {
                        self.info_msg(format!(
                            "'{rule}' is not a rule — write a tool name or tool(pattern), e.g. shell(git status) or file_write(src/*)"
                        ));
                        return;
                    }
                    let scope = scope.unwrap_or(Scope::Local);
                    let Some(store) = self.store() else { return };
                    match store.add_rule(kind, rule, scope) {
                        Ok(path) => {
                            self.info_msg(format!("{} {rule} — saved to {}", kind.name(), tidy(&path)));
                            if kind == RuleKind::Allow && scope != Scope::User {
                                self.reload_settings();
                                if !self.settings.trusted {
                                    self.info_msg("this folder isn't trusted yet, so project/local allow rules are ignored — run /trust");
                                }
                            }
                        }
                        Err(error) => self.info_msg(format!("not saved: {}", tidy_text(&error))),
                    }
                    self.reload_settings();
                }
                None => self.info_msg(
                    "usage: /permissions  ·  /permissions allow|ask|deny <rule> [user|project|local]  ·  /permissions remove <rule>",
                ),
            },
        }
    }

    fn show_permissions(&mut self) {
        let mut lines = vec![format!(
            "permissions — mode {}, folder {}",
            if self.mode.is_empty() {
                "?"
            } else {
                &self.mode
            },
            if self.settings.trusted {
                "trusted"
            } else {
                "not trusted"
            }
        )];
        for kind in [RuleKind::Deny, RuleKind::Ask, RuleKind::Allow] {
            for rule in self.settings.rules.iter().filter(|rule| rule.kind == kind) {
                let note = match (rule.active, rule.origin) {
                    (false, _) => "  ← ignored: folder not trusted (/trust)",
                    (true, Origin::BuiltIn) => "",
                    _ => "",
                };
                lines.push(format!(
                    "  {:<6}{:<46}{}{note}",
                    kind.name(),
                    rule.text,
                    rule.origin.name()
                ));
            }
        }
        lines.push("deny beats ask beats allow, in every mode. Add: /permissions allow|ask|deny <rule> [scope] · remove: /permissions remove <rule>".to_owned());
        self.info_msg(lines.join("\n"));
    }

    // ------------------------------------------------------- trust / memory

    pub(crate) fn run_trust(&mut self) {
        let Some(store) = self.store() else { return };
        match store.trust() {
            Ok(path) => {
                self.reload_settings();
                self.info_msg(format!(
                    "trusted {} — its project/local allow rules and default mode now apply (recorded in {})",
                    tidy(&store.cwd),
                    tidy(&path)
                ));
            }
            Err(error) => self.info_msg(format!("not saved: {}", tidy_text(&error))),
        }
    }

    pub(crate) fn run_memory(&mut self) {
        let Some(store) = self.store() else { return };
        let found = instruction_files(&store);
        if found.is_empty() {
            self.info_msg("no instruction files found. Create CORDATA.md in the project (or ~/.cordata/CORDATA.md for everywhere), or let the agent draft one with /init. AGENTS.md and CLAUDE.md are read too.");
            return;
        }
        let mut lines =
            vec!["instruction files in the system prompt (broad to specific):".to_owned()];
        for (scope, path) in found {
            let size = std::fs::metadata(&path).map_or(0, |meta| meta.len());
            lines.push(format!(
                "  {:<8}{} ({size} bytes)",
                scope.name(),
                tidy(&path)
            ));
        }
        lines.push("edit them in your editor; they are re-read when a session starts.".to_owned());
        self.info_msg(lines.join("\n"));
    }

    pub(crate) fn run_init(&mut self) -> Option<Effect> {
        if !self.input_enabled_for_prompt() {
            return None;
        }
        self.messages.push(Message::User("/init".into()));
        Some(self.begin_turn(INIT_PROMPT.to_owned()))
    }

    pub(crate) fn run_status(&mut self) {
        self.reload_settings();
        let settings: &Settings = &self.settings;
        let mut lines = Vec::new();
        if let Some(session) = &self.session {
            lines.push(format!(
                "session  {}{}",
                session.session_id,
                if session.resumed { " (resumed)" } else { "" }
            ));
            lines.push(format!(
                "folder   {}  ({})",
                tidy(Path::new(&session.cwd)),
                if settings.trusted {
                    "trusted"
                } else {
                    "not trusted"
                }
            ));
        }
        let name = |id: &str| {
            self.config(id)
                .map(|o| o.current_name().to_owned())
                .unwrap_or_else(|| "?".into())
        };
        lines.push(format!(
            "mode     {}   model {}   temperature {}",
            self.mode,
            name("model"),
            name("temperature")
        ));
        let laya = settings.text("laya.mode").unwrap_or("route (default)");
        lines.push(format!(
            "laya     {laya}   shown: {}",
            if self.show_laya { "yes" } else { "no" }
        ));
        let allow = settings.rules_for(RuleKind::Allow).count();
        let ask = settings.rules_for(RuleKind::Ask).count();
        let deny = settings.rules_for(RuleKind::Deny).count();
        lines.push(format!(
            "rules    {allow} allow · {ask} ask · {deny} deny   (/permissions)"
        ));
        if !settings.warnings.is_empty() {
            lines.push(format!(
                "{} settings warning(s) — /config path",
                settings.warnings.len()
            ));
        }
        self.info_msg(sanitize(&lines.join("\n")));
    }
}

#[cfg(test)]
mod tests {
    use acp_core::{AppEvent, ConfigChoice, ConfigOptionInfo, ModeInfo, SessionInfo};
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::app::Message;

    struct Rig {
        _dir: tempfile::TempDir,
        app: App,
        store: Store,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let store = Store {
            user_dir: root.join("home"),
            cwd: root.join("project"),
        };
        std::fs::create_dir_all(&store.user_dir).unwrap();
        std::fs::create_dir_all(&store.cwd).unwrap();
        let mut app = App::new();
        app.on_event(AppEvent::Connected(SessionInfo {
            session_id: "s1".into(),
            cwd: store.cwd.display().to_string(),
            transport: "stdio".into(),
            resumed: false,
            current_mode: "build".into(),
            modes: ["plan", "build", "auto"]
                .into_iter()
                .map(|id| ModeInfo {
                    id: id.into(),
                    name: id.into(),
                    description: None,
                })
                .collect(),
            config_options: vec![
                ConfigOptionInfo {
                    id: "model".into(),
                    name: "Model".into(),
                    current_value: "m1".into(),
                    choices: ["m1", "m2"]
                        .map(|v| ConfigChoice {
                            value: v.into(),
                            name: v.into(),
                        })
                        .to_vec(),
                },
                ConfigOptionInfo {
                    id: "temperature".into(),
                    name: "Temperature".into(),
                    current_value: "balanced".into(),
                    choices: ["focused", "balanced", "creative"]
                        .map(|v| ConfigChoice {
                            value: v.into(),
                            name: v.into(),
                        })
                        .to_vec(),
                },
            ],
        }));
        app.messages.clear();
        app.attach_store(store.clone());
        Rig {
            _dir: dir,
            app,
            store,
        }
    }

    fn run(app: &mut App, text: &str) -> Option<Effect> {
        for c in text.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    fn press(app: &mut App, code: KeyCode) -> Option<Effect> {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn last_info(app: &App) -> String {
        match app.messages.last() {
            Some(Message::Info(text)) => text.clone(),
            other => panic!("expected an info message, got {other:?}"),
        }
    }

    #[test]
    fn stored_preferences_are_applied_when_the_store_attaches() {
        let mut rig = rig();
        std::fs::write(
            rig.store.path(Scope::User),
            r#"{"tui":{"thinking":"expanded","toolOutput":"expanded","showLaya":false}}"#,
        )
        .unwrap();
        rig.app.attach_store(rig.store.clone());
        assert!(rig.app.thoughts_expanded && rig.app.expand_tools && !rig.app.show_laya);
    }

    #[test]
    fn config_set_writes_the_file_applies_it_and_says_when_it_takes_effect() {
        let mut rig = rig();
        assert_eq!(run(&mut rig.app, "/config set tui.thinking expanded"), None);
        assert!(
            rig.app.thoughts_expanded,
            "display preferences apply at once"
        );
        assert!(
            std::fs::read_to_string(rig.store.path(Scope::User))
                .unwrap()
                .contains("expanded")
        );

        run(&mut rig.app, "/config set laya.dispatch hint project");
        assert!(
            last_info(&rig.app).contains("next session"),
            "{}",
            last_info(&rig.app)
        );
        assert_eq!(rig.store.load().text("laya.dispatch"), Some("hint"));
        // Laya's own lines are trust-gated: an untrusted project file cannot switch Laya off.
        run(&mut rig.app, "/config set laya.mode off project");
        assert_eq!(
            rig.store.load().text("laya.mode"),
            None,
            "laya.mode from an untrusted project file is ignored"
        );

        let effect = run(&mut rig.app, "/config set temperature creative local");
        assert_eq!(
            effect,
            Some(Effect::SetConfig {
                id: "temperature".into(),
                value: "creative".into()
            }),
            "a model/temperature setting also changes the running session"
        );

        run(&mut rig.app, "/config set temperature lava");
        assert!(
            last_info(&rig.app).contains("takes one of"),
            "{}",
            last_info(&rig.app)
        );
        run(&mut rig.app, "/config set nope 1");
        assert!(last_info(&rig.app).contains("unknown setting"));
        run(&mut rig.app, "/config unset tui.thinking");
        assert!(!rig.store.load().get("tui.thinking").is_some());
        assert!(
            !rig.app.thoughts_expanded,
            "…and clearing it restores the default"
        );
    }

    #[test]
    fn a_credential_cannot_be_saved_as_a_setting() {
        let mut rig = rig();
        run(
            &mut rig.app,
            "/config set model sk-abcdefghijklmnopqrstuvwxyz0123456789",
        );
        assert!(
            last_info(&rig.app).contains("not saved"),
            "{}",
            last_info(&rig.app)
        );
        assert!(
            !rig.store.path(Scope::User).exists()
                || !std::fs::read_to_string(rig.store.path(Scope::User))
                    .unwrap()
                    .contains("sk-")
        );
    }

    #[test]
    fn the_config_panel_cycles_values_resets_them_and_switches_files() {
        let mut rig = rig();
        run(&mut rig.app, "/config");
        assert!(rig.app.config.is_some() && !rig.app.input_enabled());

        // Row 0 is permissions.defaultMode: build → edit → auto → plan → build …
        press(&mut rig.app, KeyCode::Enter);
        press(&mut rig.app, KeyCode::Enter);
        let loaded = rig.store.load();
        assert_eq!(
            loaded.text("permissions.defaultMode"),
            Some("auto"),
            "the user file needs no trust for auto"
        );

        press(&mut rig.app, KeyCode::Down); // model
        let effect = press(&mut rig.app, KeyCode::Enter);
        assert_eq!(
            effect,
            Some(Effect::SetConfig {
                id: "model".into(),
                value: "m2".into()
            }),
            "cycles the provider's models"
        );
        press(&mut rig.app, KeyCode::Down); // temperature
        press(&mut rig.app, KeyCode::Down); // tui.thinking
        press(&mut rig.app, KeyCode::Tab); // → project file
        press(&mut rig.app, KeyCode::Enter);
        assert_eq!(
            rig.store.load().sources.get("tui.thinking"),
            Some(&Scope::Project)
        );
        assert!(rig.app.thoughts_expanded);
        press(&mut rig.app, KeyCode::Backspace);
        assert!(
            rig.store.load().get("tui.thinking").is_none(),
            "Backspace clears the key in the chosen file"
        );

        press(&mut rig.app, KeyCode::Esc);
        assert!(rig.app.config.is_none() && rig.app.input_enabled());
    }

    #[test]
    fn permissions_lists_adds_and_removes_rules_and_warns_when_untrusted() {
        let mut rig = rig();
        run(&mut rig.app, "/permissions allow shell(git status)");
        assert!(
            last_info(&rig.app).contains("isn't trusted yet"),
            "{}",
            last_info(&rig.app)
        );
        run(&mut rig.app, "/permissions deny shell(rm -rf *) project");
        run(&mut rig.app, "/permissions");
        let listing = last_info(&rig.app);
        assert!(
            listing.contains("shell(rm -rf *)") && listing.contains("project"),
            "{listing}"
        );
        assert!(listing.contains("ignored: folder not trusted"), "{listing}");
        assert!(
            listing.contains("built-in"),
            "the keychain rules are listed: {listing}"
        );
        assert!(
            listing.find("deny").unwrap() < listing.find("allow").unwrap(),
            "deny first: {listing}"
        );

        run(&mut rig.app, "/trust");
        run(&mut rig.app, "/permissions");
        assert!(!last_info(&rig.app).contains("ignored"));

        run(&mut rig.app, "/permissions remove shell(git status)");
        assert!(last_info(&rig.app).contains("removed"));
        run(&mut rig.app, "/permissions remove shell(git status)");
        assert!(last_info(&rig.app).contains("no such rule"));

        run(&mut rig.app, "/permissions allow not a rule");
        assert!(last_info(&rig.app).contains("is not a rule"));
        run(&mut rig.app, "/permissions bogus");
        assert!(last_info(&rig.app).contains("usage"));
    }

    #[test]
    fn memory_lists_instruction_files_and_init_asks_the_agent_to_draft_one() {
        let mut rig = rig();
        run(&mut rig.app, "/memory");
        assert!(last_info(&rig.app).contains("no instruction files"));
        std::fs::write(rig.store.cwd.join("AGENTS.md"), "be careful").unwrap();
        run(&mut rig.app, "/memory");
        assert!(last_info(&rig.app).contains("AGENTS.md"));

        let effect = run(&mut rig.app, "/init");
        assert!(matches!(&effect, Some(Effect::SendPrompt(text)) if text.contains("CORDATA.md")));
        assert!(matches!(rig.app.messages.last(), Some(Message::User(text)) if text == "/init"));
        assert!(rig.app.turn_active, "/init starts a turn like any prompt");
    }

    #[test]
    fn status_and_paths_describe_the_session() {
        let mut rig = rig();
        run(&mut rig.app, "/status");
        let status = last_info(&rig.app);
        for needle in ["session  s1", "mode     build", "model m1", "rules"] {
            assert!(status.contains(needle), "missing {needle:?}:\n{status}");
        }
        run(&mut rig.app, "/config path");
        let paths = last_info(&rig.app);
        assert!(
            paths.contains("settings.local.json") && paths.contains("not created yet"),
            "{paths}"
        );
    }

    #[test]
    fn warnings_surface_once_the_banner_does() {
        let mut rig = rig();
        std::fs::write(rig.store.path(Scope::User), "{not json").unwrap();
        rig.app.attach_store(rig.store.clone());
        let notices = rig.app.settings_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("not valid JSON"));
    }

    #[test]
    fn the_banner_says_which_instruction_files_the_agent_reads() {
        let rig = rig();
        assert!(rig.app.settings_notices().is_empty());
        std::fs::write(rig.store.cwd.join("AGENTS.md"), "x").unwrap();
        let notices = rig.app.settings_notices();
        assert_eq!(
            notices,
            ["instructions: reading AGENTS.md (project) — /memory"]
        );
    }

    #[test]
    fn rule_and_scope_parsing() {
        assert!(
            valid_rule("shell")
                && valid_rule("shell(git status)")
                && valid_rule("mcp:server.tool(x)")
        );
        assert!(
            !valid_rule("shell()")
                && !valid_rule("(x)")
                && !valid_rule("two words")
                && !valid_rule("shell(unclosed")
        );
        assert_eq!(
            split_scope("shell(a b) local"),
            ("shell(a b)", Some(Scope::Local))
        );
        assert_eq!(split_scope("shell(a local)"), ("shell(a local)", None));
    }
}
