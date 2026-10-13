//! The diff review (F4 or `/diffview`): what the working tree changed, file by file and hunk by hunk.
//!
//! Left, the changed files with their `+added −removed`; right, the selected file's patch with old and new line
//! numbers in the gutter. ↑/↓ (or `j`/`k`) move in the focused pane, ←/→ or Tab switch panes, `]`/`[` jump a file,
//! `n`/`N` a hunk, the wheel scrolls whatever is under it and a click on a file opens it. The data is one `git diff
//! HEAD` run by the event loop (`Effect::Review`), so opening it never blocks a keypress.

use cordata_view::key::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::{Kind, Outcome, chrome, contains, dim, panel, put};
use crate::app::{App, Effect, sanitize};
use crate::ui::text::{display_width, truncate};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    /// Every line of this file's patch, header lines included.
    pub lines: Vec<String>,
}

enum State {
    Loading,
    Ready(Vec<FileDiff>),
    Failed(String),
}

pub struct Review {
    state: State,
    selected: usize,
    list_scroll: usize,
    scroll: usize,
    /// Which hunk of the selected file `n` / `N` last landed on.
    hunk: usize,
    patch_focus: bool,
    list_hits: Vec<(Rect, usize)>,
    list_area: Rect,
    patch_area: Rect,
    patch_rows: usize,
}

impl Review {
    pub fn loading() -> Self {
        Self { state: State::Loading, selected: 0, list_scroll: 0, scroll: 0, hunk: 0, patch_focus: false, list_hits: Vec::new(), list_area: Rect::default(), patch_area: Rect::default(), patch_rows: 10 }
    }

    /// The `git diff` has come back.
    pub fn load(&mut self, outcome: &crate::shell::Outcome) {
        if outcome.timed_out {
            self.state = State::Failed("git diff timed out".into());
        } else if outcome.code.is_some_and(|code| code != 0) {
            self.state = State::Failed(sanitize(outcome.output.trim()).lines().take(3).collect::<Vec<_>>().join(" · "));
        } else {
            self.state = State::Ready(parse(&outcome.output));
        }
        self.selected = 0;
        self.scroll = 0;
        self.list_scroll = 0;
    }

    fn files(&self) -> &[FileDiff] {
        match &self.state {
            State::Ready(files) => files,
            _ => &[],
        }
    }

    fn select(&mut self, index: usize) {
        let last = self.files().len().saturating_sub(1);
        let index = index.min(last);
        if index != self.selected {
            self.selected = index;
            self.scroll = 0;
            self.hunk = 0;
        }
    }

    fn hunks(&self) -> Vec<usize> {
        self.files().get(self.selected).map(|f| f.lines.iter().enumerate().filter(|(_, l)| l.starts_with("@@")).map(|(i, _)| i).collect()).unwrap_or_default()
    }

    /// Land the view on hunk `index` of the selected file (the render clamps the scroll to what fits).
    fn show_hunk(&mut self, index: usize) {
        let hunks = self.hunks();
        self.hunk = index.min(hunks.len().saturating_sub(1));
        self.scroll = hunks.get(self.hunk).copied().unwrap_or(0);
    }

    fn jump_hunk(&mut self, forward: bool) {
        let count = self.hunks().len();
        if forward {
            if self.hunk + 1 < count {
                self.show_hunk(self.hunk + 1);
            } else if self.selected + 1 < self.files().len() {
                // Past the last hunk: carry on into the next file's first one.
                self.select(self.selected + 1);
                self.show_hunk(0);
            }
        } else if self.hunk > 0 {
            self.show_hunk(self.hunk - 1);
        } else if self.selected > 0 {
            self.select(self.selected - 1);
            let last = self.hunks().len().saturating_sub(1);
            self.show_hunk(last);
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let len = self.files().get(self.selected).map_or(0, |f| f.lines.len());
        let max = len.saturating_sub(self.patch_rows.max(1));
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    pub fn on_key(&mut self, _app: &mut App, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.patch_rows.max(2) as isize - 1;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(4) => Outcome::close(None),
            KeyCode::Char('c') if ctrl => Outcome::close(None),
            KeyCode::Char('k') if ctrl => Outcome::switch(Kind::Palette),
            KeyCode::Char('r') => {
                self.state = State::Loading;
                Outcome { close: false, effect: Some(Effect::Review), open: None }
            }
            KeyCode::Tab => {
                self.patch_focus = !self.patch_focus;
                Outcome::stay()
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.patch_focus = true;
                Outcome::stay()
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.patch_focus = false;
                Outcome::stay()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.patch_focus {
                    self.scroll_by(-1);
                } else {
                    self.select(self.selected.saturating_sub(1));
                }
                Outcome::stay()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.patch_focus {
                    self.scroll_by(1);
                } else {
                    self.select(self.selected + 1);
                }
                Outcome::stay()
            }
            KeyCode::PageUp => {
                self.scroll_by(-page);
                Outcome::stay()
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.scroll_by(page);
                Outcome::stay()
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll = 0;
                Outcome::stay()
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll_by(isize::MAX / 2);
                Outcome::stay()
            }
            KeyCode::Char(']') => {
                self.select(self.selected + 1);
                Outcome::stay()
            }
            KeyCode::Char('[') => {
                self.select(self.selected.saturating_sub(1));
                Outcome::stay()
            }
            KeyCode::Char('n') => {
                self.jump_hunk(true);
                Outcome::stay()
            }
            KeyCode::Char('N') => {
                self.jump_hunk(false);
                Outcome::stay()
            }
            _ => Outcome::stay(),
        }
    }

    pub fn on_mouse(&mut self, _app: &mut App, mouse: MouseEvent) -> Outcome {
        let on_list = contains(self.list_area, mouse.column, mouse.row);
        let on_patch = contains(self.patch_area, mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, index)) = self.list_hits.iter().find(|(rect, _)| contains(*rect, mouse.column, mouse.row)) {
                    self.select(*index);
                    self.patch_focus = false;
                } else if on_patch {
                    self.patch_focus = true;
                }
            }
            MouseEventKind::ScrollDown if on_list => self.select(self.selected + 1),
            MouseEventKind::ScrollUp if on_list => self.select(self.selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            _ => {}
        }
        Outcome::stay()
    }

    pub fn render(&mut self, app: &App, buffer: &mut Buffer, area: Rect) {
        let body = chrome(buffer, area, app, "REVIEW", "↑↓ move · ←→ pane · [ ] file · n N hunk · wheel/click · r reload · Ctrl+K deck · Esc close");
        self.list_hits.clear();
        match &self.state {
            State::Loading => {
                put(buffer, body.x + 2, body.y + 1, vec![Span::styled("reading the working tree…", dim())], body.width);
                return;
            }
            State::Failed(reason) => {
                put(buffer, body.x + 2, body.y + 1, vec![Span::styled(format!("git diff failed: {reason}"), Style::new().fg(Color::Red))], body.width);
                return;
            }
            State::Ready(files) if files.is_empty() => {
                put(buffer, body.x + 2, body.y + 1, vec![Span::styled("◉ the working tree matches HEAD — nothing to review", Style::new().fg(Color::Green))], body.width);
                return;
            }
            State::Ready(_) => {}
        }

        let list_width = (body.width / 3).clamp(24, 40).min(body.width.saturating_sub(20));
        let list_outer = Rect::new(body.x, body.y, list_width, body.height);
        let patch_outer = Rect::new(body.x + list_width, body.y, body.width - list_width, body.height);
        let files: Vec<FileDiff> = self.files().to_vec();
        let (added, removed): (usize, usize) = files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
        let title = format!("FILES {} · +{added} −{removed}", files.len());
        let inner = panel(buffer, list_outer, &title, !self.patch_focus);
        self.list_area = list_outer;

        let rows = inner.height as usize;
        if self.selected < self.list_scroll {
            self.list_scroll = self.selected;
        } else if self.selected >= self.list_scroll + rows {
            self.list_scroll = self.selected + 1 - rows;
        }
        for (offset, file) in files.iter().enumerate().skip(self.list_scroll).take(rows) {
            let y = inner.y + (offset - self.list_scroll) as u16;
            let selected = offset == self.selected;
            let stats = format!("+{} −{}", file.added, file.removed);
            let room = (inner.width as usize).saturating_sub(display_width(&stats) + 4);
            let name = short_name(&file.path, room);
            let pad = " ".repeat(room.saturating_sub(display_width(&name)) + 1);
            let name_style = if selected { Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::REVERSED) } else { Style::new().fg(Color::White) };
            put(
                buffer,
                inner.x,
                y,
                vec![
                    Span::styled(if selected { "▸ " } else { "  " }, Style::new().fg(Color::Cyan)),
                    Span::styled(name, name_style),
                    Span::raw(pad),
                    Span::styled(format!("+{}", file.added), Style::new().fg(Color::Green)),
                    Span::styled(format!(" −{}", file.removed), Style::new().fg(Color::Red)),
                ],
                inner.width,
            );
            self.list_hits.push((Rect::new(inner.x, y, inner.width, 1), offset));
        }

        let Some(file) = files.get(self.selected) else { return };
        let hunks = file.lines.iter().filter(|l| l.starts_with("@@")).count();
        let inner = panel(buffer, patch_outer, &format!("{} · {hunks} hunk{}", truncate(&file.path, patch_outer.width.saturating_sub(16) as usize), if hunks == 1 { "" } else { "s" }), self.patch_focus);
        self.patch_area = patch_outer;
        self.patch_rows = inner.height as usize;
        let max = file.lines.len().saturating_sub(self.patch_rows);
        self.scroll = self.scroll.min(max);
        let rendered = patch_lines(file, inner.width as usize);
        for (offset, spans) in rendered.into_iter().skip(self.scroll).take(self.patch_rows).enumerate() {
            put(buffer, inner.x, inner.y + offset as u16, spans, inner.width);
        }
        if max > 0 {
            let place = format!("{}/{}", (self.scroll + self.patch_rows).min(file.lines.len()), file.lines.len());
            buffer.set_string(patch_outer.x + patch_outer.width.saturating_sub(display_width(&place) as u16 + 3), patch_outer.y, format!(" {place} "), dim());
        }
    }
}

/// `src/deep/dir/file.rs` shortened from the left to fit: `…dir/file.rs`.
fn short_name(path: &str, room: usize) -> String {
    if display_width(path) <= room {
        return path.to_owned();
    }
    let tail: String = path.chars().rev().take(room.saturating_sub(1)).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

/// Split a unified diff into files (`diff --git a/x b/x` starts one).
pub fn parse(output: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b).to_owned();
            files.push(FileDiff { path, added: 0, removed: 0, lines: vec![line.to_owned()] });
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        if line.starts_with('+') && !line.starts_with("+++") {
            file.added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            file.removed += 1;
        }
        file.lines.push(line.to_owned());
    }
    files
}

/// The patch as rows: a gutter of old and new line numbers, then the line, coloured by what it does.
fn patch_lines(file: &FileDiff, width: usize) -> Vec<Vec<Span<'static>>> {
    let (mut old, mut new) = (0usize, 0usize);
    let text_width = width.saturating_sub(12);
    file.lines
        .iter()
        .map(|raw| {
            let line = sanitize(raw);
            if let Some(hunk) = line.strip_prefix("@@") {
                // `@@ -a,b +c,d @@ context`
                let numbers: Vec<usize> = hunk.split_whitespace().take(2).filter_map(|part| part.trim_start_matches(['-', '+']).split(',').next()?.parse().ok()).collect();
                if let [a, c] = numbers[..] {
                    old = a;
                    new = c;
                }
                return vec![
                    Span::styled(format!("{:>4} {:>4} │ ", "·", "·"), dim()),
                    Span::styled(truncate(&format!("@@{hunk}"), text_width), Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                ];
            }
            let (gutter_old, gutter_new, style) = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") || line.starts_with("index ") || line.starts_with("new file") || line.starts_with("deleted file") || line.starts_with("similarity") || line.starts_with("rename") || line.starts_with("Binary") {
                (String::new(), String::new(), Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD))
            } else if line.starts_with('+') {
                let n = new;
                new += 1;
                (String::new(), n.to_string(), Style::new().fg(Color::Green))
            } else if line.starts_with('-') {
                let o = old;
                old += 1;
                (o.to_string(), String::new(), Style::new().fg(Color::Red))
            } else {
                let (o, n) = (old, new);
                old += 1;
                new += 1;
                (o.to_string(), n.to_string(), Style::new().fg(Color::Gray))
            };
            vec![
                Span::styled(format!("{gutter_old:>4} {gutter_new:>4} │ "), dim()),
                Span::styled(truncate(&line, text_width), style),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::overlay::Overlay;

    const DIFF: &str = "diff --git a/src/app.rs b/src/app.rs\nindex 1..2 100644\n--- a/src/app.rs\n+++ b/src/app.rs\n@@ -10,3 +10,4 @@ fn main() {\n keep\n-old line\n+new line\n+another\n@@ -40,2 +41,2 @@\n keep two\n-gone\n+here\ndiff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-hello\n+hello world\n";

    fn ready() -> Overlay {
        let mut review = Review::loading();
        review.load(&crate::shell::Outcome { output: DIFF.into(), code: Some(0), timed_out: false, cut: false });
        Overlay::Review(review)
    }

    #[test]
    fn a_diff_is_split_into_files_with_their_counts() {
        let files = parse(DIFF);
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].path.as_str(), files[0].added, files[0].removed), ("src/app.rs", 3, 2));
        assert_eq!((files[1].path.as_str(), files[1].added, files[1].removed), ("README.md", 1, 1));
    }

    #[test]
    fn the_review_lists_files_and_shows_the_selected_patch_with_line_numbers() {
        let app = app();
        let shown = text(&ready().render(&app, 110, 30));
        for needle in ["REVIEW", "FILES 2 · +4 −3", "src/app.rs", "README.md", "+3 −2", "2 hunks", "@@ -10,3 +10,4 @@ fn main() {", "  10   10 │  keep", "  11      │ -old line", "      11 │ +new line"] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
    }

    #[test]
    fn keys_walk_files_and_hunks_and_the_mouse_clicks_and_scrolls() {
        let mut app = app();
        let mut overlay = ready();
        overlay.render(&app, 110, 12);
        overlay.on_key(&mut app, key(KeyCode::Char('n')));
        let second = text(&overlay.render(&app, 110, 12));
        assert!(second.contains("@@ -40,2 +41,2 @@"), "n jumps to the next hunk:\n{second}");
        overlay.on_key(&mut app, key(KeyCode::Char('n')));
        let next_file = text(&overlay.render(&app, 110, 12));
        assert!(next_file.contains("hello world"), "past the last hunk it carries on into the next file:\n{next_file}");
        overlay.on_key(&mut app, key(KeyCode::Char('N')));
        let back = text(&overlay.render(&app, 110, 12));
        assert!(back.contains("@@ -40,2 +41,2 @@"), "N goes back a hunk, across files:\n{back}");

        let buffer = overlay.render(&app, 110, 12);
        let (column, row) = find(&buffer, "README.md").unwrap();
        overlay.on_mouse(&mut app, click(column + 1, row));
        let clicked = text(&overlay.render(&app, 110, 12));
        assert!(clicked.contains("README.md · 1 hunk") && clicked.contains("hello world"), "a click opens the file:\n{clicked}");

        overlay.on_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 3, row));
        let up = text(&overlay.render(&app, 110, 12));
        assert!(up.contains("src/app.rs · 2 hunks"), "the wheel over the list moves the selection:\n{up}");
        assert!(overlay.on_key(&mut app, key(KeyCode::Esc)).close);
    }

    #[test]
    fn empty_loading_and_failed_say_so_and_r_reloads() {
        let app = app();
        let mut loading = Overlay::Review(Review::loading());
        assert!(text(&loading.render(&app, 90, 12)).contains("reading the working tree"));
        let mut review = Review::loading();
        review.load(&crate::shell::Outcome { output: String::new(), code: Some(0), timed_out: false, cut: false });
        assert!(text(&Overlay::Review(review).render(&app, 90, 12)).contains("nothing to review"));
        let mut review = Review::loading();
        review.load(&crate::shell::Outcome { output: "fatal: not a git repository".into(), code: Some(128), timed_out: false, cut: false });
        assert!(text(&Overlay::Review(review).render(&app, 90, 12)).contains("git diff failed: fatal: not a git repository"));
        let mut app = app;
        let outcome = ready().on_key(&mut app, key(KeyCode::Char('r')));
        assert_eq!(outcome.effect, Some(Effect::Review), "r asks the event loop for a fresh diff");
    }

    #[test]
    fn untracked_new_files_are_parsed_into_review() {
        const UNTRACKED_DIFF: &str = "diff --git a/new_file.rs b/new_file.rs\nnew file mode 100644\n--- /dev/null\n+++ b/new_file.rs\n@@ -0,0 +1,3 @@\n+fn new_fn() {\n+    println!(\"hello\");\n+}\n";
        let files = parse(UNTRACKED_DIFF);
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].path.as_str(), files[0].added, files[0].removed), ("new_file.rs", 3, 0));
    }
}
