//! The completion menu above the prompt box: type `/` for commands, `@` for files.
//!
//! It is derived from the text and the cursor after every edit — there is no menu "mode" to get stuck in. Up/Down move,
//! Tab completes, Enter completes (and runs a command that needs no argument), Esc closes it until the text changes.

use std::ops::Range;

use crate::app::{App, Effect};
use crate::{commands, mentions};

/// Rows shown at once.
pub const MAX_ROWS: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Command,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// `/mode`, `src/app.rs`.
    pub label: String,
    /// Arguments the command takes, as written in its usage: `[plan|build|auto]`.
    pub hint: String,
    pub detail: String,
    /// `project` / `user` for a custom command, else empty.
    pub scope: &'static str,
    /// The command cannot run without one (`/open <handle>`): Enter completes instead of running it.
    pub needs_args: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub kind: Kind,
    pub items: Vec<Item>,
    pub selected: usize,
    /// The bytes of the input the choice replaces (`/he`, `@src/a`).
    pub range: Range<usize>,
    pub query: String,
}

impl Menu {
    pub fn current(&self) -> Option<&Item> {
        self.items.get(self.selected)
    }
}

/// Usage text marks a required argument `<like this>` and an optional one `[like this]`; `[a | to <n>]` is optional
/// even though it holds an angle bracket.
pub fn requires_argument(hint: &str) -> bool {
    let mut optional_depth = 0;
    for c in hint.chars() {
        match c {
            '[' => optional_depth += 1,
            ']' => optional_depth -= 1,
            '<' if optional_depth <= 0 => return true,
            _ => {}
        }
    }
    false
}

/// The built-in commands as menu rows, in the order `/help` lists them.
fn builtin_items() -> Vec<Item> {
    commands::HELP
        .iter()
        .map(|(usage, detail)| {
            let (label, hint) = usage.split_once(' ').map_or((*usage, ""), |(l, h)| (l, h));
            Item {
                label: label.to_owned(),
                hint: hint.to_owned(),
                detail: (*detail).to_owned(),
                scope: "",
                needs_args: requires_argument(hint),
            }
        })
        .collect()
}

impl App {
    /// The project's files, listed the first time something asks (a `git ls-files` is cheap; doing it for every
    /// session that never types an `@` would not be).
    fn project_files(&mut self) -> &[String] {
        if self.files.is_none() {
            let cwd = self.project_dir();
            self.files = Some(mentions::index(&cwd));
        }
        self.files.as_deref().unwrap_or_default()
    }

    pub fn project_dir(&self) -> std::path::PathBuf {
        self.session
            .as_ref()
            .map(|s| std::path::PathBuf::from(&s.cwd))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    /// Rebuild the menu for the text as it is now. Call after anything that changes the input.
    pub fn refresh_menu(&mut self) {
        let text = self.input.text().to_owned();
        let cursor = self.input.cursor();
        if self.menu_dismissed.as_deref() == Some(text.as_str()) || !self.can_type() {
            self.menu = None;
            return;
        }
        self.menu_dismissed = None;
        let before = &text[..cursor];

        let built = if text.starts_with('/') && !before.contains(char::is_whitespace) {
            let query = before[1..].to_owned();
            let items = self.command_items(&query);
            (!items.is_empty()).then_some((Kind::Command, items, 0..cursor, query))
        } else {
            let start = before.rfind(char::is_whitespace).map_or(0, |i| i + 1);
            let word = &before[start..];
            match word.strip_prefix('@') {
                Some(query) => {
                    let query = query.to_owned();
                    let items: Vec<Item> = {
                        let files = self.project_files();
                        mentions::rank(&query, files, MAX_ROWS)
                            .into_iter()
                            .map(|path| Item { label: path.to_owned(), hint: String::new(), detail: String::new(), scope: "", needs_args: false })
                            .collect()
                    };
                    (!items.is_empty()).then_some((Kind::File, items, start..cursor, query))
                }
                None => None,
            }
        };

        self.menu = built.map(|(kind, items, range, query)| {
            // Typing narrows the list; the highlight stays on the same choice when it can, else goes back to the top.
            let kept = self
                .menu
                .as_ref()
                .filter(|old| old.kind == kind)
                .and_then(|old| old.current())
                .and_then(|old| items.iter().position(|i| i.label == old.label));
            // …except that a command typed in full is that command: `/diff` must not stay on `/diffview` because that
            // was the highlight while only `/d` was typed.
            let exact = (kind == Kind::Command).then(|| items.iter().position(|i| i.label.trim_start_matches('/').eq_ignore_ascii_case(&query))).flatten();
            Menu { kind, selected: exact.or(kept).unwrap_or(0), items, range, query }
        });
    }

    fn command_items(&self, query: &str) -> Vec<Item> {
        let query = query.to_ascii_lowercase();
        let mut all = builtin_items();
        for custom in &self.custom {
            if all.iter().any(|i| i.label.trim_start_matches('/').eq_ignore_ascii_case(&custom.name)) {
                continue; // a built-in keeps its name
            }
            all.push(Item {
                label: format!("/{}", custom.name),
                hint: custom.hint.clone(),
                detail: custom.description.clone(),
                scope: custom.scope,
                needs_args: !custom.hint.is_empty(),
            });
        }
        let name = |item: &Item| item.label.trim_start_matches('/').to_ascii_lowercase();
        let (mut prefix, mut inside) = (Vec::new(), Vec::new());
        for item in all {
            let n = name(&item);
            if n.starts_with(&query) {
                prefix.push(item);
            } else if (query.len() >= 2 && n.contains(&query)) || (query.len() >= 4 && item.detail.to_ascii_lowercase().contains(&query)) {
                inside.push(item);
            }
        }
        // A name typed in full is that command — `/diff` must not run `/diffview` because it is listed first.
        if let Some(at) = prefix.iter().position(|item| name(item) == query) {
            let exact = prefix.remove(at);
            prefix.insert(0, exact);
        }
        prefix.extend(inside);
        prefix
    }

    pub fn menu_move(&mut self, delta: isize) {
        if let Some(menu) = &mut self.menu {
            let len = menu.items.len() as isize;
            menu.selected = (menu.selected as isize + delta).rem_euclid(len.max(1)) as usize;
        }
    }

    pub fn menu_dismiss(&mut self) {
        self.menu = None;
        self.menu_dismissed = Some(self.input.text().to_owned());
    }

    /// Complete the highlighted choice. With `run`, a command that needs no argument is run straight away (Enter);
    /// otherwise the choice only lands in the box (Tab). A file that is already fully typed is *not* completed
    /// again — Enter then sends the prompt, which is what was meant.
    pub fn menu_accept(&mut self, run: bool) -> Option<Effect> {
        let menu = self.menu.clone()?;
        let item = menu.current()?.clone();
        match menu.kind {
            Kind::Command => {
                let tail_empty = self.input.text()[menu.range.end..].trim().is_empty();
                if run && !item.needs_args && tail_empty {
                    self.input.set_text(&item.label);
                    self.menu = None;
                    return self.submit();
                }
                self.input.replace_range(menu.range, &format!("{} ", item.label));
            }
            Kind::File => {
                let typed = &self.input.text()[menu.range.clone()];
                if run && typed == format!("@{}", item.label) && !item.label.ends_with('/') {
                    self.menu = None;
                    return self.submit();
                }
                let with = if item.label.ends_with('/') { format!("@{}", item.label) } else { format!("@{} ", item.label) };
                self.input.replace_range(menu.range, &with);
            }
        }
        self.refresh_menu();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom::Custom;

    fn app_with(text: &str) -> App {
        let mut app = App::new();
        app.conn = crate::app::ConnState::Connected;
        app.input.set_text(text);
        app
    }

    #[test]
    fn a_slash_opens_every_command_and_typing_narrows_them() {
        let mut app = app_with("/");
        app.refresh_menu();
        let all = app.menu.as_ref().expect("menu").items.len();
        assert!(all > 10, "{all}");
        app.input.set_text("/mo");
        app.refresh_menu();
        let labels: Vec<_> = app.menu.as_ref().unwrap().items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels[..2], ["/mode", "/model"], "{labels:?}");
    }

    #[test]
    fn a_word_that_is_only_inside_a_name_still_finds_it() {
        let mut app = app_with("/hist");
        app.refresh_menu();
        // /undo and /checkpoints describe "history"-ish things; nothing starts with it, but the description matches.
        let found = app.menu.as_ref().map(|m| m.items.len()).unwrap_or(0);
        assert!(found <= MAX_ROWS * 4);
        app.input.set_text("/nonexistentzzz");
        app.refresh_menu();
        assert!(app.menu.is_none());
    }

    #[test]
    fn once_there_is_a_space_the_menu_gets_out_of_the_way() {
        let mut app = app_with("/mode ");
        app.refresh_menu();
        assert!(app.menu.is_none());
        let mut plain = app_with("just a prompt");
        plain.refresh_menu();
        assert!(plain.menu.is_none());
    }

    #[test]
    fn enter_runs_a_command_without_arguments() {
        let mut app = app_with("/hel");
        app.refresh_menu();
        let effect = app.menu_accept(true);
        assert!(effect.is_none(), "/help prints, it sends nothing");
        assert!(app.input.is_empty(), "the command ran and the box was emptied");
    }

    #[test]
    fn only_arguments_outside_brackets_are_required() {
        assert!(requires_argument("<handle>"));
        assert!(!requires_argument("[n | list | to <number>]"));
        assert!(!requires_argument(""));
        assert!(requires_argument("[flag] <name>"));
    }

    #[test]
    fn tab_completes_but_never_runs() {
        let mut app = app_with("/hel");
        app.refresh_menu();
        app.menu_accept(false);
        assert_eq!(app.input.text(), "/help ");
    }

    #[test]
    fn a_command_typed_in_full_ranks_above_longer_names_that_start_with_it() {
        let mut app = app_with("");
        // Typed one key at a time, as a person does: the highlight must not stay on whatever `/d` started on.
        for c in "/diff".chars() {
            app.input.insert(c);
            app.refresh_menu();
        }
        let menu = app.menu.as_ref().expect("a menu");
        assert_eq!(menu.current().map(|i| i.label.as_str()), Some("/diff"), "{:?}", menu.items.iter().map(|i| &i.label).collect::<Vec<_>>());
        assert!(menu.items.iter().any(|i| i.label == "/diffview"), "…the longer one is still offered");
    }

    #[test]
    fn up_and_down_wrap_around_the_choices() {
        let mut app = app_with("/");
        app.refresh_menu();
        app.menu_move(-1);
        let last = app.menu.as_ref().unwrap().items.len() - 1;
        assert_eq!(app.menu.as_ref().unwrap().selected, last);
        app.menu_move(1);
        assert_eq!(app.menu.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn escape_closes_the_menu_until_the_text_changes() {
        let mut app = app_with("/he");
        app.refresh_menu();
        app.menu_dismiss();
        app.refresh_menu();
        assert!(app.menu.is_none());
        app.input.set_text("/hel");
        app.refresh_menu();
        assert!(app.menu.is_some());
    }

    #[test]
    fn custom_commands_join_the_menu_but_never_shadow_a_built_in() {
        let mut app = app_with("/");
        app.custom = vec![
            Custom { name: "review".into(), description: "Review the diff".into(), hint: "[focus]".into(), body: "x".into(), scope: "project" },
            Custom { name: "help".into(), description: "evil".into(), hint: String::new(), body: "x".into(), scope: "project" },
        ];
        app.refresh_menu();
        let items = &app.menu.as_ref().unwrap().items;
        let review = items.iter().find(|i| i.label == "/review").expect("custom listed");
        assert!(review.needs_args && review.scope == "project");
        assert_eq!(items.iter().filter(|i| i.label == "/help").count(), 1);
        assert!(items.iter().all(|i| i.detail != "evil"));
    }

    #[test]
    fn an_at_sign_opens_the_files_and_tab_puts_the_path_in_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/app.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        let mut app = app_with("look at @app");
        app.session = Some(acp_core::SessionInfo {
            session_id: "s".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            transport: "stdio".into(),
            resumed: false,
            current_mode: "build".into(),
            modes: vec![],
            config_options: vec![],
        });
        app.refresh_menu();
        let menu = app.menu.as_ref().expect("file menu");
        assert_eq!(menu.kind, Kind::File);
        assert_eq!(menu.current().unwrap().label, "src/app.rs");
        app.menu_accept(false);
        assert_eq!(app.input.text(), "look at @src/app.rs ");
    }

    #[test]
    fn a_folder_keeps_the_menu_open_so_you_can_walk_into_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/app.rs"), "").unwrap();
        let mut app = app_with("@sr");
        app.session = Some(acp_core::SessionInfo {
            session_id: "s".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            transport: "stdio".into(),
            resumed: false,
            current_mode: "build".into(),
            modes: vec![],
            config_options: vec![],
        });
        app.refresh_menu();
        assert_eq!(app.menu.as_ref().unwrap().current().unwrap().label, "src/");
        app.menu_accept(false);
        assert_eq!(app.input.text(), "@src/", "no trailing space: the next level is still being chosen");
        assert!(app.menu.as_ref().is_some_and(|m| m.items.iter().any(|i| i.label == "src/app.rs")));
    }
}
