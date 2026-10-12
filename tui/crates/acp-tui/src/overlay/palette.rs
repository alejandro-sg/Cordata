//! The command deck (Ctrl+K): every mode, model, view, session and slash command in one searchable list.
//!
//! Type to filter (every word must appear, in the label, the detail or the group; a label that starts with the word
//! ranks first), ↑/↓ or the wheel to move, Enter or a click to run, Esc to close. With nothing typed the list is
//! grouped, and what is already in force (the mode, the model) wears `◉`. A command that needs an argument is not
//! run: it is put in the prompt box, ready to be completed.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::{Kind, Outcome, chrome, contains, dim, put, rule};
use crate::app::{App, Effect, SwitchTarget};
use crate::commands;
use crate::ui::footer::mode_color;
use crate::ui::text::{display_width, truncate};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Mode(String),
    Config { id: String, value: String },
    /// A line to run as if typed (`/diff`).
    Run(String),
    /// A line to leave in the prompt box for the person to finish (`/goal `).
    Insert(String),
    Open(Kind),
    Switch(usize),
    NewSession,
}

#[derive(Debug, Clone)]
struct Entry {
    group: &'static str,
    label: String,
    detail: String,
    current: bool,
    action: Action,
}

#[derive(Debug, Clone, Copy)]
enum Row {
    Header(&'static str),
    Item(usize),
}

pub struct Palette {
    query: String,
    entries: Vec<Entry>,
    rows: Vec<Row>,
    /// Index into `rows` of the highlighted item.
    selected: usize,
    scroll: usize,
    /// Where each visible item was drawn last frame, for the mouse.
    hits: Vec<(Rect, usize)>,
    list_height: usize,
}

impl Palette {
    pub fn new(app: &App) -> Self {
        let mut palette = Self { query: String::new(), entries: entries(app), rows: Vec::new(), selected: 0, scroll: 0, hits: Vec::new(), list_height: 10 };
        palette.refilter();
        palette
    }

    fn refilter(&mut self) {
        let words: Vec<String> = self.query.split_whitespace().map(str::to_lowercase).collect();
        self.rows.clear();
        if words.is_empty() {
            let mut last = "";
            for (index, entry) in self.entries.iter().enumerate() {
                if entry.group != last {
                    self.rows.push(Row::Header(entry.group));
                    last = entry.group;
                }
                self.rows.push(Row::Item(index));
            }
        } else {
            let mut scored: Vec<(i32, usize)> = self.entries.iter().enumerate().filter_map(|(index, entry)| score(&words, entry).map(|s| (s, index))).collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            self.rows.extend(scored.into_iter().map(|(_, index)| Row::Item(index)));
        }
        self.selected = self.first_item().unwrap_or(0);
        self.scroll = 0;
    }

    fn first_item(&self) -> Option<usize> {
        self.rows.iter().position(|row| matches!(row, Row::Item(_)))
    }

    fn selected_entry(&self) -> Option<&Entry> {
        match self.rows.get(self.selected)? {
            Row::Item(index) => self.entries.get(*index),
            Row::Header(_) => None,
        }
    }

    fn step(&mut self, delta: isize) {
        let mut at = self.selected as isize;
        loop {
            at += delta.signum();
            if at < 0 || at as usize >= self.rows.len() {
                return;
            }
            if matches!(self.rows[at as usize], Row::Item(_)) {
                self.selected = at as usize;
                // Land on the item with its group header in view when it is the first of its group.
                return;
            }
        }
    }

    fn step_by(&mut self, delta: isize) {
        for _ in 0..delta.unsigned_abs() {
            self.step(delta);
        }
    }

    fn run(&mut self, app: &mut App) -> Outcome {
        let Some(entry) = self.selected_entry().cloned() else {
            return Outcome::stay();
        };
        match entry.action {
            Action::Mode(id) => {
                app.pending_mode = Some(id.clone());
                Outcome::close(Some(Effect::SetMode(id)))
            }
            Action::Config { id, value } => Outcome::close(Some(Effect::SetConfig { id, value })),
            Action::Run(line) => Outcome::close(app.run_line(line)),
            Action::Insert(text) => {
                if app.can_type() {
                    app.input.set_text(&text);
                    app.refresh_menu();
                }
                Outcome::close(None)
            }
            Action::Open(kind) => Outcome::switch(kind),
            Action::Switch(number) => Outcome::close(Some(Effect::SwitchSession(SwitchTarget::Slot(number)))),
            Action::NewSession => Outcome::close(Some(Effect::NewSession)),
        }
    }

    pub fn on_key(&mut self, app: &mut App, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => Outcome::close(None),
            KeyCode::Char('k' | 'c') if ctrl => Outcome::close(None),
            KeyCode::Enter => self.run(app),
            KeyCode::Up => {
                self.step(-1);
                Outcome::stay()
            }
            KeyCode::Down => {
                self.step(1);
                Outcome::stay()
            }
            KeyCode::Char('p') if ctrl => {
                self.step(-1);
                Outcome::stay()
            }
            KeyCode::Char('n') if ctrl => {
                self.step(1);
                Outcome::stay()
            }
            KeyCode::PageUp => {
                self.step_by(-(self.list_height as isize));
                Outcome::stay()
            }
            KeyCode::PageDown => {
                self.step_by(self.list_height as isize);
                Outcome::stay()
            }
            KeyCode::Home => {
                self.selected = self.first_item().unwrap_or(0);
                Outcome::stay()
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.refilter();
                Outcome::stay()
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refilter();
                Outcome::stay()
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.refilter();
                Outcome::stay()
            }
            _ => Outcome::stay(),
        }
    }

    pub fn on_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Outcome {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.step_by(-3);
                Outcome::stay()
            }
            MouseEventKind::ScrollDown => {
                self.step_by(3);
                Outcome::stay()
            }
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                if let Some(row) = self.hit(mouse.column, mouse.row) {
                    self.selected = row;
                }
                Outcome::stay()
            }
            MouseEventKind::Down(MouseButton::Left) => match self.hit(mouse.column, mouse.row) {
                Some(row) => {
                    self.selected = row;
                    self.run(app)
                }
                None => Outcome::stay(),
            },
            _ => Outcome::stay(),
        }
    }

    fn hit(&self, column: u16, row: u16) -> Option<usize> {
        self.hits.iter().find(|(rect, _)| contains(*rect, column, row)).map(|(_, index)| *index)
    }

    pub fn render(&mut self, app: &App, buffer: &mut Buffer, area: Rect) {
        let body = chrome(buffer, area, app, "COMMAND DECK", "↑↓ move · Enter run · Esc close · type to filter · click or wheel");
        let width = body.width as usize;

        // The query line.
        let count = self.rows.iter().filter(|row| matches!(row, Row::Item(_))).count();
        let found = format!("{count} {}", if count == 1 { "entry" } else { "entries" });
        let prompt_cap = width.saturating_sub(display_width(&found) + 6);
        let mut line = vec![
            Span::styled(" ❯ ", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(truncate(&self.query, prompt_cap)),
            Span::styled("▏", Style::new().fg(Color::Cyan)),
        ];
        let used = 3 + display_width(&truncate(&self.query, prompt_cap)) + 1;
        if used + display_width(&found) + 1 < width {
            line.push(Span::raw(" ".repeat(width - used - display_width(&found) - 1)));
            line.push(Span::styled(found, dim()));
        }
        put(buffer, body.x, body.y, line, body.width);
        rule(buffer, body.y + 1, body);

        let list = Rect::new(body.x, body.y + 2, body.width, body.height.saturating_sub(2));
        self.list_height = list.height as usize;
        self.hits.clear();
        if self.rows.is_empty() || count == 0 {
            put(buffer, list.x, list.y, vec![Span::styled("   nothing matches — Esc closes", dim())], list.width);
            return;
        }

        // Keep the selection in view (and its group header, when it is the first item under one).
        let height = list.height as usize;
        if self.selected < self.scroll {
            self.scroll = self.selected;
            if self.scroll > 0 && matches!(self.rows[self.scroll - 1], Row::Header(_)) {
                self.scroll -= 1;
            }
        } else if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }

        let label_width = self.entries.iter().map(|e| display_width(&e.label)).max().unwrap_or(8).clamp(8, 26);
        for (offset, row) in self.rows.iter().enumerate().skip(self.scroll).take(height) {
            let y = list.y + (offset - self.scroll) as u16;
            match row {
                Row::Header(group) => {
                    put(buffer, list.x, y, vec![Span::styled(format!(" ╴// {group}"), Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD))], list.width);
                }
                Row::Item(index) => {
                    let entry = &self.entries[*index];
                    let selected = offset == self.selected;
                    let marker = if selected { " ▸ " } else { "   " };
                    let label_style = if selected {
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::REVERSED)
                    } else {
                        Style::new().fg(Color::White)
                    };
                    let label = truncate(&entry.label, label_width);
                    let pad = " ".repeat(label_width.saturating_sub(display_width(&label)) + 2);
                    let tag = if self.query.is_empty() { String::new() } else { format!("  [{}]", entry.group.to_lowercase()) };
                    let current = if entry.current { " ◉ " } else { "   " };
                    let room = width.saturating_sub(3 + label_width + 2 + 3 + display_width(&tag));
                    let mut spans = vec![
                        Span::styled(marker, Style::new().fg(Color::Cyan)),
                        Span::styled(label, label_style),
                        Span::raw(pad),
                        Span::styled(truncate(&entry.detail, room), dim()),
                        Span::styled(tag, Style::new().fg(Color::DarkGray)),
                    ];
                    let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
                    if used + 3 <= width {
                        spans.push(Span::raw(" ".repeat(width - used - 3)));
                        spans.push(Span::styled(current, Style::new().fg(mode_color("plan"))));
                    }
                    put(buffer, list.x, y, spans, list.width);
                    self.hits.push((Rect::new(list.x, y, list.width, 1), offset));
                }
            }
        }
    }
}

/// Every word must be found; a label that starts with it ranks first, then a label that contains it, then the detail
/// or the group.
fn score(words: &[String], entry: &Entry) -> Option<i32> {
    let label = entry.label.to_lowercase();
    let detail = entry.detail.to_lowercase();
    let group = entry.group.to_lowercase();
    let mut total = 0;
    for word in words {
        let bare = word.trim_start_matches('/');
        let name = label.trim_start_matches('/');
        total += if name == bare {
            150
        } else if name.starts_with(bare) {
            100 - name.chars().count().min(40) as i32 // a shorter name that starts with it is the likelier meaning
        } else if label.contains(bare) {
            60
        } else if detail.contains(bare) {
            20
        } else if group.contains(bare) {
            10
        } else if is_subsequence(bare, &label) {
            5
        } else {
            return None;
        };
    }
    Some(total)
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut rest = haystack.chars();
    needle.chars().all(|c| rest.any(|h| h == c))
}

fn entries(app: &App) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut add = |group: &'static str, label: &str, detail: &str, current: bool, action: Action| {
        out.push(Entry { group, label: label.to_owned(), detail: detail.to_owned(), current, action });
    };

    for mode in &app.modes {
        let detail = mode.description.clone().unwrap_or_default();
        add("MODE", &mode.id, &detail, mode.id == app.mode, Action::Mode(mode.id.clone()));
    }
    for (id, group) in [("model", "MODEL"), ("temperature", "TEMPERATURE")] {
        if let Some(option) = app.config(id) {
            for choice in &option.choices {
                let detail = if choice.name == choice.value { String::new() } else { choice.value.clone() };
                add(group, &choice.name, &detail, choice.value == option.current_value, Action::Config { id: id.to_owned(), value: choice.value.clone() });
            }
        }
    }
    add("VIEW", "panes", "Blade Runner 2049 workspace — conversation, agents, handoffs, telemetry (F3)", false, Action::Open(Kind::Panes));
    add("VIEW", "hud", "session dashboard — context, usage, tasks, agents, activity", false, Action::Open(Kind::Hud));
    add("VIEW", "diffview", "the working tree's changes: files, hunks, mouse (F4)", false, Action::Open(Kind::Review));
    add("VIEW", "lab", "the Experimental Lab: pods, A2A links, threat gauge (F2)", false, Action::Run("/lab".into()));
    if app.tabs.len() > 1 {
        for tab in &app.tabs {
            add("SESSION", &format!("session {}", tab.number), if tab.active { "shown now" } else { "switch to it" }, tab.active, Action::Switch(tab.number));
        }
    }
    add("SESSION", "new session", "another backend in this project (Ctrl+N)", false, Action::NewSession);

    for (usage, detail) in commands::HELP {
        let (label, hint) = usage.split_once(' ').map_or((*usage, ""), |(l, h)| (l, h));
        let needs_args = crate::menu::requires_argument(hint);
        let action = if needs_args { Action::Insert(format!("{label} ")) } else { Action::Run(label.to_owned()) };
        let shown = if hint.is_empty() { (*detail).to_owned() } else { format!("{hint}  {detail}") };
        add("COMMAND", label, &shown, false, action);
    }
    for custom in &app.custom {
        add("COMMAND", &format!("/{}", custom.name), "custom command", false, Action::Insert(format!("/{} ", custom.name)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::overlay::Overlay;

    fn open(app: &App) -> Palette {
        Palette::new(app)
    }

    #[test]
    fn it_lists_modes_models_views_and_commands_grouped_with_what_is_current_marked() {
        let app = app();
        let buffer = Overlay::Palette(open(&app)).render(&app, 100, 90);
        let shown = text(&buffer);
        for needle in ["COMMAND DECK", "╴// MODE", "plan", "Discuss and plan only", "╴// MODEL", "kimi-k2.7-code", "glm-5.3-flash", "╴// VIEW", "hud", "diffview", "╴// COMMAND", "/diff"] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
        let (_, build_row) = find(&buffer, "Tools run after your approval").expect("build row");
        assert!(shown.lines().nth(build_row as usize).unwrap().contains('◉'), "the mode in force is marked");
        let (_, model_row) = find(&buffer, "glm-5.3-flash").unwrap();
        assert!(!shown.lines().nth(model_row as usize).unwrap().contains('◉'), "…and the other model is not");
    }

    #[test]
    fn typing_filters_by_every_word_and_ranks_a_label_that_starts_with_it_first() {
        let mut app = app();
        let mut palette = open(&app);
        for k in typed("diff") {
            palette.on_key(&mut app, k);
        }
        let first = palette.selected_entry().expect("a match");
        assert_eq!(first.label, "/diff", "the label that starts with the word wins");
        for k in typed(" zzzz") {
            palette.on_key(&mut app, k);
        }
        assert!(palette.selected_entry().is_none(), "a word nothing contains matches nothing");
        let shown = text(&Overlay::Palette(palette).render(&app, 100, 20));
        assert!(shown.contains("nothing matches"), "{shown}");
    }

    #[test]
    fn enter_runs_a_mode_a_model_a_command_or_opens_a_view() {
        let mut app = app();
        let mut palette = open(&app);
        for k in typed("plan") {
            palette.on_key(&mut app, k);
        }
        let outcome = palette.on_key(&mut app, key(KeyCode::Enter));
        assert!(outcome.close && outcome.effect == Some(Effect::SetMode("plan".into())), "a mode");
        assert_eq!(app.pending_mode.as_deref(), Some("plan"), "the footer shows the change as pending");

        let mut palette = open(&app);
        for k in typed("glm") {
            palette.on_key(&mut app, k);
        }
        let outcome = palette.on_key(&mut app, key(KeyCode::Enter));
        assert_eq!(outcome.effect, Some(Effect::SetConfig { id: "model".into(), value: "glm-5.3-flash".into() }), "a model");

        let mut palette = open(&app);
        for k in typed("hud") {
            palette.on_key(&mut app, k);
        }
        let outcome = palette.on_key(&mut app, key(KeyCode::Enter));
        assert!(outcome.close && outcome.open == Some(Kind::Hud), "a view");

        let mut palette = open(&app);
        for k in typed("/open") {
            palette.on_key(&mut app, k);
        }
        let outcome = palette.on_key(&mut app, key(KeyCode::Enter));
        assert!(outcome.close && outcome.effect.is_none());
        assert!(app.input.text().starts_with("/open"), "a command that needs an argument is left in the prompt to finish: {:?}", app.input.text());
    }

    #[test]
    fn the_mouse_hovers_clicks_and_scrolls() {
        let mut app = app();
        let mut overlay = Overlay::Palette(open(&app));
        let buffer = overlay.render(&app, 100, 40);
        let (column, row) = find(&buffer, "glm-5.3-flash").unwrap();

        let hover = overlay.on_mouse(&mut app, mouse(MouseEventKind::Moved, column, row));
        assert!(!hover.close, "hovering never runs anything");
        let after = text(&overlay.render(&app, 100, 40));
        assert!(after.lines().nth(row as usize).unwrap().contains('▸'), "the hovered row is the selected one:\n{after}");

        let outcome = overlay.on_mouse(&mut app, click(column, row));
        assert_eq!(outcome.effect, Some(Effect::SetConfig { id: "model".into(), value: "glm-5.3-flash".into() }), "a click runs the row");

        let mut overlay = Overlay::Palette(open(&app));
        let before = text(&overlay.render(&app, 100, 40));
        overlay.on_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 10, 10));
        let scrolled = text(&overlay.render(&app, 100, 40));
        assert_ne!(before, scrolled, "the wheel moves the selection");
        let nothing = overlay.on_mouse(&mut app, click(0, 0));
        assert!(!nothing.close && nothing.effect.is_none(), "a click on the chrome does nothing");
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_selection_in_view() {
        let mut app = app();
        let mut overlay = Overlay::Palette(open(&app));
        overlay.render(&app, 100, 14);
        for _ in 0..40 {
            overlay.on_key(&mut app, key(KeyCode::Down));
        }
        let shown = text(&overlay.render(&app, 100, 14));
        assert!(shown.contains('▸'), "the selection is on screen after moving far down:\n{shown}");
        for _ in 0..80 {
            overlay.on_key(&mut app, key(KeyCode::Up));
        }
        let top = text(&overlay.render(&app, 100, 14));
        assert!(top.contains("╴// MODE"), "…and back at the top the first group header shows again:\n{top}");
    }

    #[test]
    fn escape_and_ctrl_k_close_it() {
        let mut app = app();
        let mut palette = open(&app);
        assert!(palette.on_key(&mut app, key(KeyCode::Esc)).close);
        let ctrl_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert!(palette.on_key(&mut app, ctrl_k).close);
    }
}
