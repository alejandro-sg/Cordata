//! `/memory`, `/init`, `/status` and the stored display preferences (see `settings.rs`).
//!
//! Everything here is a method on `App` so it stays unit-testable without a
//! terminal; files are only touched through `settings::Store`.

use std::path::Path;

use crate::app::{App, Effect, Message, sanitize};
use crate::settings::{Prefs, Store, instruction_files};

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
    use acp_core::{AppEvent, ConfigChoice, ConfigOptionInfo, ModeInfo, SessionInfo};
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

}
