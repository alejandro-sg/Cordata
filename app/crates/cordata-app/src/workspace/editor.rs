//! The light editor in the dock (docs/app.md §3 D4): the project's files (a tree, or a fuzzy list while a filter is
//! typed — the same match as `@` mentions), open files in tabs (gpui-component's editor: tree-sitter highlighting, find,
//! line numbers), ⌘S to save, squiggles from the backend's language servers (`_cordata/lsp/diagnostics` — the same
//! trust-gated servers the agent's tools use: none starts in an untrusted folder, and the app installs none), and "Open
//! in Zed" for anything more.
//!
//! A file is shown as it is on disk — it is the person's own file, edited in place — except that a file that looks like
//! a secret (`.env`, keys) is not opened here at all, so "a key is never drawn" still holds (THREAT_MODEL.md). A file
//! changed on disk while open (an agent's edit) reloads itself when it has no unsaved edits, and says so when it has.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use cordata_theme::Role;
use cordata_view::tree::{Row, Tree};
use cordata_view::{mentions, shown};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Subscription,
    Window, div, px, relative, uniform_list,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_component::input::{Editor, EditorState, Input, InputEvent, InputState, Position};

use super::{DockTab, Workspace};
use crate::hud;
use crate::theme::{color, tint};

/// Prefix of the tags of `_cordata/lsp/diagnostics` requests (one tag per request, so answers never cross).
pub const LSP_TAG: &str = "gui-lsp:";
/// Larger files open in Zed.
const MAX_OPEN_BYTES: u64 = 2 * 1024 * 1024;
/// Quiet time after a keystroke before the buffer is checked.
const CHECK_AFTER: Duration = Duration::from_millis(700);
const ROW_HEIGHT: f32 = 22.;

pub struct Buffer {
    /// Relative to the project.
    pub rel: String,
    /// Absolute and canonical (what the backend answers with).
    pub path: PathBuf,
    pub state: Entity<EditorState>,
    mtime: Option<SystemTime>,
    pub dirty: bool,
    /// Changed on disk while it had unsaved edits.
    pub conflict: bool,
    /// Bumped on every edit; a check's answer is applied only to the text it was asked about.
    edits: u64,
    pub errors: usize,
    pub warnings: usize,
    _subscription: Subscription,
}

#[derive(Default)]
pub struct Files {
    pub tree: Option<Tree>,
    loading: bool,
    pub buffers: Vec<Buffer>,
    pub active: usize,
    pub notice: Option<String>,
    /// The language server answering for the active file, or why none is.
    pub servers: Option<String>,
    /// Tag → (path, edits) of each check in flight.
    pending: std::collections::HashMap<String, (PathBuf, u64)>,
    next_check: u64,
    /// × on a buffer with unsaved edits: the second click discards them.
    closing: Option<usize>,
}

impl Files {
    pub fn active(&self) -> Option<&Buffer> {
        self.buffers.get(self.active)
    }

    pub fn dirty(&self) -> usize {
        self.buffers.iter().filter(|b| b.dirty).count()
    }
}

/// The highlighter's language for a file name (gpui-component's names; plain text otherwise).
pub fn language_for(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    if matches!(name, "Makefile" | "makefile" | "GNUmakefile") {
        return "make";
    }
    match name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).as_deref() {
        Some("rs") => "rust",
        Some("py" | "pyi") => "python",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "tsx",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("go") => "go",
        Some("c" | "h") => "c",
        Some("cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx") => "cpp",
        Some("css" | "scss") => "css",
        Some("html" | "htm") => "html",
        Some("java") => "java",
        Some("md" | "markdown" | "mdx") => "markdown",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("json" | "jsonc") => "json",
        Some("sh" | "bash" | "zsh") => "bash",
        Some("sql") => "sql",
        Some("swift") => "swift",
        Some("diff" | "patch") => "diff",
        Some("mk") => "make",
        _ => "text",
    }
}

/// Why a file is not opened in the app (`None`: it can be).
pub fn refuse(rel: &str, path: &Path, root: &Path) -> Option<String> {
    if mentions::looks_secret(rel) {
        return Some(format!("{rel} looks like a secret — it is not shown here (Open in Zed)"));
    }
    if !path.starts_with(root) {
        return Some(format!("{rel} is outside the project"));
    }
    match std::fs::metadata(path) {
        Err(error) => Some(format!("{rel}: {error}")),
        Ok(meta) if meta.is_dir() => Some(format!("{rel} is a folder")),
        Ok(meta) if meta.len() > MAX_OPEN_BYTES => Some(format!("{rel} is {} KB — larger files open in Zed", meta.len() / 1024)),
        Ok(_) => None,
    }
}

/// Write `text` to `path` the safe way: a sibling temporary file, its mode copied, renamed over the original.
pub fn write_atomically(path: &Path, text: &str) -> std::io::Result<Option<SystemTime>> {
    let parent = path.parent().ok_or_else(|| std::io::Error::other("no parent folder"))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = parent.join(format!(".{name}.cordata-save"));
    std::fs::write(&temp, text)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&temp, meta.permissions());
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(std::fs::metadata(path).and_then(|m| m.modified()).ok())
}

/// The `zed` command line, where Zed installs it.
fn zed_cli() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut candidates = vec![PathBuf::from("/usr/local/bin/zed"), PathBuf::from("/opt/homebrew/bin/zed"), home.join(".local/bin/zed")];
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("zed")));
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn severity(code: u64) -> DiagnosticSeverity {
    match code {
        1 => DiagnosticSeverity::Error,
        2 => DiagnosticSeverity::Warning,
        3 => DiagnosticSeverity::Info,
        _ => DiagnosticSeverity::Hint,
    }
}

fn position(value: &serde_json::Value) -> Position {
    let at = |key: &str| value.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    Position::new(at("line"), at("character"))
}

impl Workspace {
    pub(super) fn root(&self) -> PathBuf {
        self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone())
    }

    // ------------------------------------------------------------------------------------------------ the tree

    pub(super) fn load_tree(&mut self, cx: &mut Context<Self>) {
        if self.files.loading {
            return;
        }
        self.files.loading = true;
        let root = self.root();
        cx.spawn(async move |this, cx| {
            let entries = cx.background_executor().spawn(async move { mentions::index(&root) }).await;
            let _ = this.update(cx, |this, cx| {
                this.files.loading = false;
                match this.files.tree.as_mut() {
                    Some(tree) => tree.set_entries(entries),
                    None => {
                        let mut tree = Tree::new(entries);
                        tree.set_filter(&this.file_filter.read(cx).value());
                        this.files.tree = Some(tree);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn on_file_filter(&mut self, input: &Entity<InputState>, event: &InputEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            InputEvent::Change => {
                let query = input.read(cx).value();
                if let Some(tree) = self.files.tree.as_mut() {
                    tree.set_filter(&query);
                }
                cx.notify();
            }
            InputEvent::PressEnter { .. } => {
                let first = self.files.tree.as_ref().and_then(|t| t.rows().iter().find(|r| !r.dir)).map(|r| r.path.clone());
                if let Some(path) = first {
                    self.open_file(&path, None, window, cx);
                }
            }
            _ => {}
        }
    }

    fn click_row(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        if row.dir {
            if let Some(tree) = self.files.tree.as_mut() {
                tree.toggle(&row.path);
            }
            cx.notify();
        } else {
            self.open_file(&row.path, None, window, cx);
        }
    }

    // ------------------------------------------------------------------------------------------------ buffers

    /// Open `rel` (relative to the project) in a tab, or show its tab; `line` (0-based) moves the cursor there.
    pub(super) fn open_file(&mut self, rel: &str, line: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        self.dock_tab = DockTab::Editor;
        let root = self.root();
        let path = root.join(rel).canonicalize().unwrap_or_else(|_| root.join(rel));
        if let Some(index) = self.files.buffers.iter().position(|b| b.path == path) {
            self.files.active = index;
            if let Some(line) = line {
                self.files.buffers[index].state.update(cx, |state, cx| state.set_cursor_position(Position::new(line, 0), window, cx));
            }
            cx.notify();
            return;
        }
        if let Some(why) = refuse(rel, &path, &root) {
            self.files.notice = Some(why);
            cx.notify();
            return;
        }
        let text = match std::fs::read(&path) {
            Ok(bytes) if bytes.contains(&0) => Err(format!("{rel} is a binary file")),
            Ok(bytes) => String::from_utf8(bytes).map_err(|_| format!("{rel} is not UTF-8 text")),
            Err(error) => Err(format!("{rel}: {error}")),
        };
        let text = match text {
            Ok(text) => text,
            Err(why) => {
                self.files.notice = Some(why);
                cx.notify();
                return;
            }
        };
        let language = language_for(rel);
        let state = cx.new(|cx| EditorState::new(window, cx).language(language).line_number(true).searchable(true).default_value(text));
        if let Some(line) = line {
            state.update(cx, |state, cx| state.set_cursor_position(Position::new(line, 0), window, cx));
        }
        let subscription = cx.subscribe_in(&state, window, Self::on_buffer_event);
        self.files.buffers.push(Buffer {
            rel: rel.to_owned(),
            mtime: std::fs::metadata(&path).and_then(|m| m.modified()).ok(),
            path: path.clone(),
            state: state.clone(),
            dirty: false,
            conflict: false,
            edits: 0,
            errors: 0,
            warnings: 0,
            _subscription: subscription,
        });
        self.files.active = self.files.buffers.len() - 1;
        self.files.notice = None;
        if let Some(tree) = self.files.tree.as_mut() {
            tree.reveal(rel);
        }
        state.update(cx, |state, cx| state.focus(window, cx));
        self.check_buffer(&path, None, cx);
        cx.notify();
    }

    fn on_buffer_event(&mut self, state: &Entity<EditorState>, event: &InputEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(event, InputEvent::Change) {
            return;
        }
        let Some(buffer) = self.files.buffers.iter_mut().find(|b| &b.state == state) else { return };
        buffer.dirty = true;
        buffer.edits += 1;
        let (path, edits) = (buffer.path.clone(), buffer.edits);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHECK_AFTER).await;
            let _ = this.update(cx, |this, cx| this.check_buffer(&path, Some(edits), cx));
        })
        .detach();
        cx.notify();
    }

    /// Ask the language server about `path`'s buffer — its unsaved text when it has edits. `edits`: only if the buffer
    /// has not changed since (a debounced check).
    fn check_buffer(&mut self, path: &Path, edits: Option<u64>, cx: &mut Context<Self>) {
        let Some(buffer) = self.files.buffers.iter().find(|b| b.path == path) else { return };
        if edits.is_some_and(|e| e != buffer.edits) {
            return;
        }
        let mut params = serde_json::json!({ "path": buffer.path.to_string_lossy() });
        if buffer.dirty {
            params["text"] = serde_json::Value::String(buffer.state.read(cx).value().to_string());
        }
        self.files.next_check += 1;
        let tag = format!("{LSP_TAG}{}", self.files.next_check);
        if self.session.read(cx).request("_cordata/lsp/diagnostics", params, &tag) {
            self.files.pending.insert(tag, (buffer.path.clone(), buffer.edits));
        }
    }

    /// An answer to [`Self::check_buffer`].
    pub(super) fn on_lsp(&mut self, tag: &str, cx: &mut Context<Self>) {
        let result = self.session.update(cx, |session, _| session.data.remove(tag));
        let Some((path, edits)) = self.files.pending.remove(tag) else { return };
        let value = match result {
            Some(Ok(value)) => value,
            Some(Err(error)) => {
                self.files.servers = Some(format!("language server: {}", shown(&error)));
                cx.notify();
                return;
            }
            None => return,
        };
        let active = self.files.active().is_some_and(|b| b.path == path);
        if value.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            if active {
                self.files.servers = Some(match value.get("needs").and_then(|v| v.as_str()) {
                    Some("trust") => "no squiggles: language servers start only in a trusted folder".to_owned(),
                    _ => shown(value.get("reason").and_then(|v| v.as_str()).unwrap_or("no diagnostics")),
                });
            }
            cx.notify();
            return;
        }
        let Some(buffer) = self.files.buffers.iter_mut().find(|b| b.path == path) else { return };
        if buffer.edits != edits {
            return; // typed since: a newer check is on its way
        }
        let rows = value.get("diagnostics").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let (mut errors, mut warnings) = (0, 0);
        buffer.state.update(cx, |state, cx| {
            let text = state.text().clone();
            if let Some(set) = state.diagnostics_mut() {
                set.reset(&text);
                for row in &rows {
                    let level = row.get("severity").and_then(|v| v.as_u64()).unwrap_or(1);
                    match level {
                        1 => errors += 1,
                        2 => warnings += 1,
                        _ => {}
                    }
                    let start = position(row.get("start").unwrap_or(&serde_json::Value::Null));
                    let end = position(row.get("end").unwrap_or(&serde_json::Value::Null));
                    let mut diagnostic = Diagnostic::new(start..end, shown(row.get("message").and_then(|v| v.as_str()).unwrap_or(""))).with_severity(severity(level));
                    if let Some(source) = row.get("source").and_then(|v| v.as_str()) {
                        diagnostic = diagnostic.with_source(source.to_owned());
                    }
                    set.push(diagnostic);
                }
            }
            cx.notify();
        });
        (buffer.errors, buffer.warnings) = (errors, warnings);
        if active {
            self.files.servers = Some(match value.get("server").and_then(|v| v.as_str()) {
                Some(server) => format!("{server} · {errors} error{} · {warnings} warning{}", if errors == 1 { "" } else { "s" }, if warnings == 1 { "" } else { "s" }),
                None => "no language server for this file".to_owned(),
            });
        }
        cx.notify();
    }

    pub(super) fn save_active(&mut self, cx: &mut Context<Self>) {
        let Some(buffer) = self.files.buffers.get_mut(self.files.active) else { return };
        let text = buffer.state.read(cx).value().to_string();
        match write_atomically(&buffer.path, &text) {
            Ok(mtime) => {
                buffer.mtime = mtime;
                buffer.dirty = false;
                buffer.conflict = false;
                self.files.notice = Some(format!("saved {}", buffer.rel));
                let path = buffer.path.clone();
                self.check_buffer(&path, None, cx);
            }
            Err(error) => self.files.notice = Some(format!("{}: {error}", buffer.rel)),
        }
        cx.notify();
    }

    /// Reload `index` from disk, keeping the cursor's line.
    pub(super) fn reload_buffer(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(buffer) = self.files.buffers.get_mut(index) else { return };
        let Ok(text) = std::fs::read_to_string(&buffer.path) else {
            self.files.notice = Some(format!("{} can no longer be read", buffer.rel));
            return;
        };
        buffer.state.update(cx, |state, cx| {
            let at = state.cursor_position();
            state.set_value(text, window, cx);
            state.set_cursor_position(at, window, cx);
        });
        buffer.mtime = std::fs::metadata(&buffer.path).and_then(|m| m.modified()).ok();
        buffer.dirty = false;
        buffer.conflict = false;
        let path = buffer.path.clone();
        self.check_buffer(&path, None, cx);
        cx.notify();
    }

    /// Files changed on disk under open buffers (the agent's edits, a `git` command): reloaded when clean, flagged
    /// when they have unsaved edits.
    pub(super) fn poll_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut reload = Vec::new();
        for (index, buffer) in self.files.buffers.iter_mut().enumerate() {
            let now = std::fs::metadata(&buffer.path).and_then(|m| m.modified()).ok();
            if now.is_none() || now == buffer.mtime {
                continue;
            }
            if buffer.dirty {
                if !buffer.conflict {
                    buffer.conflict = true;
                    cx.notify();
                }
            } else {
                reload.push(index);
            }
        }
        for index in reload {
            self.reload_buffer(index, window, cx);
        }
    }

    fn close_buffer(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(buffer) = self.files.buffers.get(index) else { return };
        if buffer.dirty && self.files.closing != Some(index) {
            self.files.closing = Some(index);
            self.files.notice = Some(format!("{} has unsaved edits — × again discards them", buffer.rel));
            cx.notify();
            return;
        }
        self.files.closing = None;
        self.files.buffers.remove(index);
        self.files.pending.retain(|_, (path, _)| self.files.buffers.iter().any(|b| &b.path == path));
        if self.files.active >= self.files.buffers.len() {
            self.files.active = self.files.buffers.len().saturating_sub(1);
        }
        self.files.notice = None;
        cx.notify();
    }

    pub(super) fn close_active_buffer(&mut self, cx: &mut Context<Self>) {
        self.close_buffer(self.files.active, cx);
    }

    pub(super) fn open_in_zed(&mut self, cx: &mut Context<Self>) {
        let target = match self.files.active() {
            Some(buffer) => {
                let at = buffer.state.read(cx).cursor_position();
                format!("{}:{}:{}", buffer.path.display(), at.line + 1, at.character + 1)
            }
            None => self.root().display().to_string(),
        };
        let spawned = match zed_cli() {
            Some(cli) => std::process::Command::new(cli).arg(&target).stdin(std::process::Stdio::null()).spawn(),
            None => std::process::Command::new("/usr/bin/open").args(["-a", "Zed", target.split(':').next().unwrap_or(&target)]).spawn(),
        };
        match spawned {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(error) => {
                self.files.notice = Some(format!("Zed: {error}"));
                cx.notify();
            }
        }
    }

    // ------------------------------------------------------------------------------------------------ drawing

    pub(super) fn files_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows: Rc<Vec<Row>> = Rc::new(self.files.tree.as_ref().map(|t| t.rows().to_vec()).unwrap_or_default());
        let open: Vec<PathBuf> = self.files.buffers.iter().map(|b| b.path.clone()).collect();
        let root = self.root();
        let this = cx.entity().downgrade();
        let count = rows.len();
        let list = uniform_list("file-tree", count, move |range, _window, cx| {
            range
                .filter_map(|ix| rows.get(ix).cloned().map(|row| (ix, row)))
                .map(|(ix, row)| {
                    let is_open = !row.dir && open.contains(&root.join(&row.path));
                    let glyph = if row.dir { if row.open { "▾" } else { "▸" } } else { " " };
                    let role = if row.dir { Role::TextSecondary } else if is_open { Role::Accent } else { Role::Text };
                    let this = this.clone();
                    let click = row.clone();
                    div()
                        .id(("file-row", ix))
                        .h(px(ROW_HEIGHT))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .pl(px(6. + row.depth as f32 * 12.))
                        .pr(px(6.))
                        .rounded(px(2.))
                        .cursor_pointer()
                        .text_xs()
                        .text_color(color(cx, role))
                        .hover(|el| el.bg(tint(cx, Role::Accent, 0.08)))
                        .child(div().w(px(10.)).text_color(color(cx, Role::TextDim)).child(glyph))
                        .child(div().truncate().child(SharedString::from(row.name.clone())))
                        .on_click(move |_: &ClickEvent, window, cx| {
                            let _ = this.update(cx, |this, cx| this.click_row(&click, window, cx));
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0();
        let empty = match (&self.files.tree, self.files.loading) {
            (None, _) | (_, true) if count == 0 => Some("Reading the project's files…"),
            (Some(tree), _) if count == 0 && !tree.filter().is_empty() => Some("No file matches."),
            (Some(_), _) if count == 0 => Some("No files."),
            _ => None,
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_1().p(px(2.)).rounded(px(3.)).border_1().border_color(color(cx, Role::Hairline)).child(Input::new(&self.file_filter).appearance(false)))
                    .child(Button::new("files-refresh").label("↻").ghost().on_click(cx.listener(|this, _, _, cx| this.load_tree(cx)))),
            )
            .when_some(self.files.notice.clone(), |el, notice| el.child(div().text_xs().text_color(color(cx, Role::Attention)).child(SharedString::from(shown(&notice)))))
            .when_some(empty, |el, text| el.child(div().text_xs().text_color(color(cx, Role::TextDim)).child(text)))
            .child(list)
            .into_any_element()
    }

    pub(super) fn editor_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(buffer) = self.files.active() else {
            return div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .text_xs()
                .text_color(color(cx, Role::TextDim))
                .child("No file open. Pick one in Files (⌘P), or click a hunk in Review.")
                .when_some(self.files.notice.clone(), |el, notice| el.child(div().text_color(color(cx, Role::Attention)).child(SharedString::from(shown(&notice)))))
                .into_any_element();
        };
        let tabs: Vec<AnyElement> = self
            .files
            .buffers
            .iter()
            .enumerate()
            .map(|(index, b)| {
                let selected = index == self.files.active;
                let name = b.rel.rsplit('/').next().unwrap_or(&b.rel).to_owned();
                let marks = format!("{}{}", if b.dirty { " ●" } else { "" }, if b.errors > 0 { format!(" ✕{}", b.errors) } else { String::new() });
                div()
                    .id(("buffer-tab", index))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(8.))
                    .py(px(3.))
                    .border_b_2()
                    .border_color(if selected { color(cx, Role::Accent) } else { tint(cx, Role::Accent, 0.0) })
                    .text_xs()
                    .text_color(color(cx, if selected { Role::Text } else { Role::TextSecondary }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.files.active = index;
                        this.files.closing = None;
                        cx.notify();
                    }))
                    .child(SharedString::from(name))
                    .when(!marks.is_empty(), |el| el.child(div().text_color(color(cx, if b.errors > 0 { Role::Danger } else { Role::Attention })).child(SharedString::from(marks))))
                    .child(
                        div()
                            .id(("buffer-close", index))
                            .px(px(2.))
                            .text_color(color(cx, Role::TextDim))
                            .hover(|el| el.text_color(color(cx, Role::Danger)))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_buffer(index, cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        let language = language_for(&buffer.rel);
        div()
            .id("editor-pane")
            .key_context("CordataEditor")
            .flex()
            .flex_col()
            .size_full()
            .gap(px(6.))
            .child(div().id("buffer-tabs").flex().overflow_x_scroll().border_b_1().border_color(color(cx, Role::Hairline)).children(tabs))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(color(cx, Role::TextSecondary)).child(SharedString::from(format!("{} · {language}", buffer.rel))))
                    .when(buffer.dirty, |el| el.child(Button::new("buffer-save").label("Save ⌘S").primary().on_click(cx.listener(|this, _, _, cx| this.save_active(cx)))))
                    .child(Button::new("open-in-zed").label("Open in Zed").ghost().on_click(cx.listener(|this, _, _, cx| this.open_in_zed(cx)))),
            )
            .when(buffer.conflict, |el| {
                let index = self.files.active;
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .p(px(6.))
                        .rounded(px(3.))
                        .border_1()
                        .border_color(color(cx, Role::Attention))
                        .bg(tint(cx, Role::Attention, 0.1))
                        .text_xs()
                        .child(div().flex_1().text_color(color(cx, Role::Attention)).child("Changed on disk while you were editing."))
                        .child(Button::new("conflict-reload").label("Reload theirs").ghost().on_click(cx.listener(move |this, _, window, cx| this.reload_buffer(index, window, cx))))
                        .child(Button::new("conflict-keep").label("Keep mine (save)").on_click(cx.listener(|this, _, _, cx| this.save_active(cx)))),
                )
            })
            .when_some(self.files.notice.clone(), |el, notice| el.child(div().text_xs().text_color(color(cx, Role::Attention)).child(SharedString::from(shown(&notice)))))
            .child(
                div()
                    .id("editor")
                    .role(gpui::Role::Group)
                    .aria_label("Editor")
                    .flex_1()
                    .min_h_0()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(color(cx, Role::Hairline))
                    .bg(color(cx, Role::Surface))
                    .child(Editor::new(&buffer.state).bordered(false).h(relative(1.)).font_family(crate::theme::mono(cx))),
            )
            .child(div().text_xs().text_color(color(cx, if buffer.errors > 0 { Role::Danger } else { Role::TextDim })).child(SharedString::from(self.files.servers.clone().unwrap_or_else(|| "checking…".into()))))
            .into_any_element()
    }

    pub(super) fn dock_label(&self, tab: DockTab) -> String {
        match tab {
            DockTab::Mission => "mission".into(),
            DockTab::Files => "files".into(),
            DockTab::Editor => match (self.files.buffers.len(), self.files.dirty()) {
                (0, _) => "editor".into(),
                (n, 0) => format!("editor {n}"),
                (n, _) => format!("editor {n}●"),
            },
            DockTab::Review => match self.review.hunks() {
                0 => "review".into(),
                n => format!("review {n}"),
            },
            DockTab::Terminal => "terminal".into(),
            DockTab::Browser => "browser".into(),
            DockTab::Models => "models".into(),
            DockTab::Settings => {
                if self.settings_view.trusted {
                    "settings".into()
                } else {
                    "settings ⚠".into()
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn add_test_buffer(&mut self, rel: &str, path: PathBuf, state: Entity<EditorState>) {
        self.files.buffers.push(Buffer {
            rel: rel.to_owned(),
            path,
            state,
            mtime: None,
            dirty: false,
            conflict: false,
            edits: 0,
            errors: 0,
            warnings: 0,
            _subscription: Subscription::new(|| {}),
        });
        self.files.active = self.files.buffers.len() - 1;
    }
}

/// A frame for one of the dock's panes.
pub(super) fn pane(title: &str, body: AnyElement, cx: &mut Context<Workspace>) -> AnyElement {
    div().flex().flex_col().size_full().min_h_0().gap(px(6.)).child(hud::label(title, cx)).child(div().flex_1().min_h_0().child(body)).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_follow_the_file_name() {
        assert_eq!(language_for("src/main.rs"), "rust");
        assert_eq!(language_for("a/b/Makefile"), "make");
        assert_eq!(language_for("web/App.TSX"), "tsx");
        assert_eq!(language_for("notes"), "text");
    }

    #[test]
    fn secrets_big_files_and_escapes_are_not_opened() {
        let dir = tempfile_dir();
        std::fs::write(dir.join(".env"), "KEY=1").unwrap();
        std::fs::write(dir.join("ok.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("big.log"), vec![b'a'; (MAX_OPEN_BYTES + 1) as usize]).unwrap();
        assert!(refuse(".env", &dir.join(".env"), &dir).unwrap().contains("secret"));
        assert!(refuse("big.log", &dir.join("big.log"), &dir).unwrap().contains("Zed"));
        assert!(refuse("../x", Path::new("/etc/hosts"), &dir).unwrap().contains("outside"));
        assert!(refuse("ok.rs", &dir.join("ok.rs"), &dir).is_none());
        let saved = write_atomically(&dir.join("ok.rs"), "fn main() { println!(); }").unwrap();
        assert!(saved.is_some());
        assert_eq!(std::fs::read_to_string(dir.join("ok.rs")).unwrap(), "fn main() { println!(); }");
        assert!(!dir.join(".ok.rs.cordata-save").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cordata-editor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }
}
