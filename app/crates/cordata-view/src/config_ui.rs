//! `/memory`, `/init`, `/status`, `/permissions`, the saved-session commands (`/history`, `/resume`, `/forget`) and
//! the stored display preferences (see `settings.rs`).
//!
//! Everything here is a method on `App` so it stays unit-testable without a
//! terminal; files are only touched through `settings::Store`.

use std::path::Path;

use crate::app::{App, Effect, Message, sanitize};
use crate::settings::{Prefs, Store, instruction_files};

const INIT_PROMPT: &str = "Look through this project (its README, manifests and layout) and write a CORDATA.md in the project root: concise guidance for a coding agent working here — how to build, test and run it, how the code is laid out, the conventions to follow, and any gotchas. Keep it under 60 lines and state only what you verified in the files; ask me about anything you can't tell.";

/// `42s`, `5m`, `3h`, `2d` ago.
fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

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

impl App {
    pub fn attach_store(&mut self, store: Store) {
        self.settings = store.load();
        self.custom = crate::custom::load(&store.user_dir, &store.cwd);
        self.store = Some(store);
        let prefs = self.settings.prefs();
        self.apply_prefs(prefs);
    }

    pub fn reload_settings(&mut self) {
        if let Some(store) = &self.store {
            self.settings = store.load();
        }
    }

    fn apply_prefs(&mut self, prefs: Prefs) {
        self.thoughts_expanded = prefs.thinking_expanded;
        self.expand_tools = prefs.tools_expanded;
    }

    /// The settings problems worth telling the user about once, at connect.
    pub fn settings_notices(&self) -> Vec<String> {
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

    fn info_msg(&mut self, text: impl Into<String>) {
        self.messages.push(Message::Info(text.into()));
    }

    // ----------------------------------------------------------- /permissions

    pub fn run_permissions(&mut self) {
        let Some(store) = self.store() else { return };
        let policy = acp_core::policy::Policy::load_in(Some(&store.user_dir), &store.cwd);
        let mut lines = vec!["the agent runs under (from chat.* in your settings; a new session picks up changes):".to_owned()];
        lines.extend(policy.describe().into_iter().map(|line| format!("  {line}")));
        lines.push("every other edit and command asks first; reads and searches do not.".to_owned());
        self.info_msg(sanitize(&lines.join("\n")));
    }

    // ------------------------------------------------------ saved sessions

    fn saved_sessions(&mut self) -> Vec<acp_core::history::Summary> {
        self.store().map(|store| acp_core::history::list_in(&store.user_dir, &store.cwd)).unwrap_or_default()
    }

    pub fn run_history(&mut self) {
        let saved = self.saved_sessions();
        if saved.is_empty() {
            self.info_msg("no saved sessions in this project (with chat.history on, each session is saved after every turn)");
            return;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let current = self.session.as_ref().map(|s| s.session_id.clone()).unwrap_or_default();
        let mut lines = vec!["saved sessions, newest first (/resume <id> continues one, /forget deletes them):".to_owned()];
        for s in saved.iter().take(20) {
            let marker = if s.id == current { "  ← this one" } else { "" };
            let prompts = if s.prompts == 1 { "1 prompt".to_owned() } else { format!("{} prompts", s.prompts) };
            lines.push(format!("  {:<16} {:>8}  {prompts:<11} {}{marker}", s.id, ago(now.saturating_sub(s.updated)), s.title));
        }
        if saved.len() > 20 {
            lines.push(format!("  … and {} older", saved.len() - 20));
        }
        self.info_msg(sanitize(&lines.join("\n")));
    }

    pub fn run_resume(&mut self, arg: Option<String>) -> Option<Effect> {
        if !self.input_enabled_for_prompt() {
            self.info_msg("wait for the turn to end (or cancel it) before resuming a session");
            return None;
        }
        Some(Effect::Resume(arg.unwrap_or_else(|| "last".into())))
    }

    pub fn run_forget(&mut self, arg: Option<String>) -> Option<Effect> {
        if arg.as_deref() == Some("confirm") {
            return Some(Effect::Forget);
        }
        let count = self.saved_sessions().len();
        self.info_msg(match count {
            0 => "no saved sessions in this project; /forget confirm still stops this one from being saved".to_owned(),
            n => format!("this deletes {n} saved session{} of this project (this one included) and stops saving the open sessions here (in Cordata.app, other windows keep saving until closed). Type /forget confirm to do it.", if n == 1 { "" } else { "s" }),
        });
        None
    }

    // ------------------------------------------------------- trust / memory

    pub fn run_memory(&mut self) {
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

    pub fn run_init(&mut self) -> Option<Effect> {
        if !self.input_enabled_for_prompt() {
            return None;
        }
        self.messages.push(Message::User("/init".into()));
        Some(self.begin_turn(INIT_PROMPT.to_owned()))
    }

    pub fn run_status(&mut self) {
        let mut lines = Vec::new();
        if let Some(session) = &self.session {
            lines.push(format!("session  {}", session.session_id));
            lines.push(format!("folder   {}", tidy(Path::new(&session.cwd))));
            lines.push(format!("engine   {}", session.transport));
        }
        let found = self.store().map(|store| instruction_files(&store)).unwrap_or_default();
        lines.push(if found.is_empty() {
            "instructions  none (add CORDATA.md, AGENTS.md or CLAUDE.md)".to_owned()
        } else {
            let names: Vec<String> = found.iter().map(|(_, path)| tidy(path)).collect();
            format!("instructions  {}", names.join(", "))
        });
        self.info_msg(sanitize(&lines.join("\n")));
    }
}

#[cfg(test)]
mod tests {
    use acp_core::{AppEvent, SessionInfo};
    use crate::key::{KeyCode, KeyEvent, KeyModifiers};
    use crate::settings::Scope;

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
        app.on_event(AppEvent::Connected(SessionInfo { session_id: "s1".into(), cwd: store.cwd.display().to_string(), model: "m1".into(), transport: "native · m1".into() }));
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
            r#"{"tui":{"thinking":"expanded","toolOutput":"expanded"}}"#,
        )
        .unwrap();
        rig.app.attach_store(rig.store.clone());
        assert!(rig.app.thoughts_expanded && rig.app.expand_tools);
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
    fn status_describes_the_session() {
        let mut rig = rig();
        run(&mut rig.app, "/status");
        let status = last_info(&rig.app);
        for needle in ["session  s1", "folder", "engine", "instructions"] {
            assert!(status.contains(needle), "missing {needle:?}:\n{status}");
        }
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
    fn permissions_show_the_rules_and_ignore_a_repository_granting_itself_more() {
        let mut rig = rig();
        std::fs::write(rig.store.path(Scope::User), r#"{"chat": {"allow": ["run_command(cargo test*)"], "network": "off"}}"#).unwrap();
        std::fs::create_dir_all(rig.store.cwd.join(".cordata")).unwrap();
        std::fs::write(rig.store.path(Scope::Project), r#"{"chat": {"allow": ["run_command"], "deny": ["run_command(git push*)"]}}"#).unwrap();
        run(&mut rig.app, "/permissions");
        let text = last_info(&rig.app);
        assert!(text.contains("allow without asking: run_command(cargo test*)\n"), "{text}");
        assert!(text.contains("refuse without asking: run_command(git push*)"), "{text}");
        assert!(text.contains("requests for the network are refused"), "{text}");
        assert!(text.contains("'chat.allow' ignored"), "{text}");
    }

    #[test]
    fn forget_asks_for_confirmation_and_resume_defaults_to_the_last_session() {
        let mut rig = rig();
        assert_eq!(run(&mut rig.app, "/forget"), None);
        assert!(last_info(&rig.app).contains("/forget confirm"));
        assert_eq!(run(&mut rig.app, "/forget confirm"), Some(Effect::Forget));
        assert_eq!(run(&mut rig.app, "/resume"), Some(Effect::Resume("last".into())));
        assert_eq!(run(&mut rig.app, "/resume 19ab"), Some(Effect::Resume("19ab".into())));
        run(&mut rig.app, "/history");
        assert!(last_info(&rig.app).starts_with("no saved sessions"));
    }

    #[test]
    fn a_restored_session_is_redrawn_and_takes_over_the_id() {
        use acp_core::Restored;
        let mut rig = rig();
        rig.app.on_event(AppEvent::SessionRestored {
            session_id: "abc-1".into(),
            entries: vec![Restored::User("hi".into()), Restored::Agent("hello".into()), Restored::Tool { name: "run_command".into(), summary: "ls".into() }],
        });
        assert!(matches!(&rig.app.messages[1], Message::User(t) if t == "hi"));
        assert!(matches!(&rig.app.messages[2], Message::Agent(t) if t == "hello"));
        assert!(matches!(&rig.app.messages[3], Message::Info(t) if t == "↳ run_command ls"));
        assert_eq!(rig.app.session.as_ref().unwrap().session_id, "abc-1");
    }
}
