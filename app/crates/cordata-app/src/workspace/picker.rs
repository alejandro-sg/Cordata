//! The project picker (⌘O, or a click on the project card): a 360px popover under the card. Search the recent
//! projects or paste a path; open a folder with the system panel, clone a repository, or start an empty project.

use std::path::{Path, PathBuf};

use cordata_theme::tokens as t;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, AppContext as _, Context, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement, PathPromptOptions, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px, relative};
use gpui_component::input::Input;

use super::{Overlay, Workspace, tilde};
use crate::prefs;
use crate::theme::{amber, c, line, mono, tk};
use crate::ui::{self, Kind};

/// What the picker's search box holds: a filter, or a path to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Filter(String),
    Path(PathBuf),
}

/// `~/x`, `/x`, `./x` and `../x` are paths (resolved against `cwd`); anything else filters.
pub fn parse_query(text: &str, cwd: &Path) -> Query {
    let text = text.trim();
    if text.starts_with('~') || text.starts_with('/') || text.starts_with('.') {
        let expanded = match text.strip_prefix('~') {
            Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest.trim_start_matches('/')),
            None if text.starts_with('/') => PathBuf::from(text),
            None => cwd.join(text),
        };
        return Query::Path(expanded);
    }
    Query::Filter(text.to_lowercase())
}

/// A repository URL `git clone` may be given: https, ssh or scp-style; never an option, never `ext::`.
pub fn valid_clone_url(url: &str) -> bool {
    let url = url.trim();
    !url.is_empty()
        && !url.starts_with('-')
        && !url.contains(char::is_whitespace)
        && !url.to_ascii_lowercase().starts_with("ext::")
        && (url.starts_with("https://") || url.starts_with("ssh://") || (url.starts_with("git@") && url.contains(':')))
}

/// `https://github.com/acme/widget.git` → `widget`.
pub fn repo_name(url: &str) -> String {
    let tail = url.trim().trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or("repository");
    let name = tail.strip_suffix(".git").unwrap_or(tail);
    let clean: String = name.chars().filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
    if clean.is_empty() || clean.starts_with('.') { "repository".into() } else { clean }
}

impl Workspace {
    pub(super) fn picker_keys(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.close_overlay(Overlay::Picker, window, cx),
            "enter" => {
                let text = self.picker_input.read(cx).value().to_string();
                match parse_query(&text, &self.cwd) {
                    Query::Path(path) => self.switch_project(path, window, cx),
                    Query::Filter(_) => {
                        if let Some(path) = self.picker_matches(cx).get(self.picker_ix).map(|r| r.path.clone()) {
                            self.switch_project(path, window, cx);
                        }
                    }
                }
            }
            "down" => self.picker_ix = (self.picker_ix + 1).min(self.picker_matches(cx).len().saturating_sub(1)),
            "up" => self.picker_ix = self.picker_ix.saturating_sub(1),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn picker_matches(&self, cx: &Context<Self>) -> Vec<prefs::RecentProject> {
        let text = self.picker_input.read(cx).value().to_string();
        let mut rows = self.prefs.recents.clone();
        if !rows.iter().any(|r| r.path == self.cwd) {
            rows.insert(0, prefs::RecentProject { path: self.cwd.clone(), opened: prefs::now() });
        }
        match parse_query(&text, &self.cwd) {
            Query::Filter(filter) if !filter.is_empty() => rows.into_iter().filter(|r| r.path.to_string_lossy().to_lowercase().contains(&filter)).collect(),
            Query::Filter(_) => rows,
            Query::Path(_) => Vec::new(),
        }
    }

    /// The system's folder panel (directories only).
    pub(super) fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = window;
        let answer = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Open".into()) });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = answer.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update_in(cx, |this, window, cx| this.switch_project(path, window, cx));
            }
        })
        .detach();
    }

    /// A new empty folder, chosen with the system's save panel.
    fn new_empty_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let parent = self.cwd.parent().map(Path::to_path_buf).unwrap_or_else(|| self.cwd.clone());
        let answer = cx.prompt_for_new_path(&parent, Some("new-project"));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = answer.await {
                let _ = this.update_in(cx, |this, window, cx| match std::fs::create_dir_all(&path) {
                    Ok(()) => this.switch_project(path, window, cx),
                    Err(error) => this.picker_note = Some((format!("Could not create {}: {error}", tilde(&path)), true)),
                });
            }
        })
        .detach();
    }

    /// `git clone` into the destination, off the UI thread; opens the clone when it lands.
    pub(super) fn start_clone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.clone_url.read(cx).value().trim().to_owned();
        let dest_text = self.clone_dest.read(cx).value().trim().to_owned();
        if !valid_clone_url(&url) {
            self.clone_note = Some(("Use an https://, ssh:// or git@host:path URL.".into(), true));
            cx.notify();
            return;
        }
        let dest = match parse_query(if dest_text.is_empty() { "~" } else { &dest_text }, &self.cwd) {
            Query::Path(p) => p,
            Query::Filter(_) => self.cwd.join(&dest_text),
        };
        let dest = if dest.is_dir() { dest.join(repo_name(&url)) } else { dest };
        if dest.exists() {
            self.clone_note = Some((format!("{} already exists.", tilde(&dest)), true));
            cx.notify();
            return;
        }
        self.clone_note = Some((format!("Cloning into {}…", tilde(&dest)), false));
        cx.notify();
        let job = cx.background_executor().spawn({
            let dest = dest.clone();
            async move {
                std::process::Command::new("git")
                    .args(["-c", "protocol.ext.allow=never", "-c", "protocol.file.allow=never", "clone", "--", &url])
                    .arg(&dest)
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .stdin(std::process::Stdio::null())
                    .output()
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(out) if out.status.success() => {
                        this.clone_note = None;
                        this.close_overlay(Overlay::Clone, window, cx);
                        this.switch_project(dest, window, cx);
                    }
                    Ok(out) => {
                        let err = String::from_utf8_lossy(&out.stderr);
                        let line = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("git clone failed");
                        this.clone_note = Some((cordata_view::shown(line), true));
                    }
                    Err(error) => this.clone_note = Some((format!("git could not start: {error}"), true)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn picker(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let text = self.picker_input.read(cx).value().to_string();
        let query = parse_query(&text, &self.cwd);
        let rows = self.picker_matches(cx);
        let now = prefs::now();
        let path_row = match &query {
            Query::Path(path) => {
                let path = path.clone();
                let shown_path = tilde(&path);
                let exists = path.is_dir();
                Some(
                    ui::row("picker-open-path", true, cx)
                        .on_click(cx.listener(move |this, _, window, cx| this.switch_project(path.clone(), window, cx)))
                        .child(div().text_color(c(tk.amber)).child("↳"))
                        .child(div().flex_shrink_0().text_size(px(t::TEXT_UI)).text_color(c(tk.text_primary)).child("Open this folder"))
                        .child(div().flex_1().min_w_0().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(if exists { tk.text_muted } else { tk.danger })).child(SharedString::from(if exists { shown_path } else { format!("{shown_path} (not found)") })))
                        .child(ui::meta("↵", cx))
                        .into_any_element(),
                )
            }
            Query::Filter(_) => None,
        };
        let list: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let current = row.path == self.cwd;
                let path = row.path.clone();
                let name = row.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| tilde(&row.path));
                ui::row(("picker-row", index), current || index == self.picker_ix, cx)
                    .when(current, |r| r.bg(amber(cx, t::AMBER_FILL_MID)))
                    .on_click(cx.listener(move |this, _, window, cx| this.switch_project(path.clone(), window, cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().truncate().text_size(px(t::TEXT_UI)).text_color(c(tk.text_primary)).child(SharedString::from(name)))
                            .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_LABEL)).text_color(c(tk.text_faint)).child(SharedString::from(tilde(&row.path)))),
                    )
                    .child(ui::meta(SharedString::from(if current { "current".to_owned() } else { super::age(now.saturating_sub(row.opened) as f64) }), cx))
                    .into_any_element()
            })
            .collect();
        let no_match = list.is_empty() && path_row.is_none();
        let top = t::HEADER_H + 34.;
        let max_h = (window.viewport_size().height.as_f32() - top - 16.).max(240.);
        let footer_action = |id: &'static str, label: &'static str, key: &'static str, cx: &mut Context<Self>| {
            ui::row(id, false, cx).h(px(32.)).text_size(px(t::TEXT_UI)).text_color(c(tk.text_secondary)).child(div().flex_1().child(label)).when(!key.is_empty(), |r| r.child(ui::meta(key, cx)))
        };
        div()
            .id("project-picker")
            .role(gpui::Role::Dialog)
            .aria_label("Switch project")
            .key_context("Picker")
            .capture_key_down(cx.listener(Self::picker_keys))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| this.close_overlay(Overlay::Picker, window, cx)))
            .absolute()
            .top(px(top))
            .left(px(12.))
            .w(px(t::PICKER_W))
            .max_h(px(max_h))
            .flex()
            .flex_col()
            .child(
                ui::popover(cx)
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .max_h(px(max_h))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(42.))
                            .flex_shrink_0()
                            .px(px(12.))
                            .border_b_1()
                            .border_color(line(cx, t::LINE_DEFAULT))
                            .child(div().text_color(c(tk.text_faint)).child("⌕"))
                            .child(div().flex_1().text_size(px(t::TEXT_UI)).child(Input::new(&self.picker_input).appearance(false))),
                    )
                    .child(
                        div()
                            .id("picker-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p(px(6.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .children(path_row)
                            .when(!list.is_empty(), |l| l.child(div().px(px(8.)).pt(px(6.)).pb(px(4.)).child(ui::section_label("recent projects", cx))))
                            .children(list)
                            .when(no_match, |l| l.child(div().px(px(10.)).py(px(14.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("No match. Paste a full path to open a folder.")))
                            .when_some(self.picker_note.clone(), |l, (note, error)| l.child(div().px(px(10.)).py(px(6.)).text_size(px(t::TEXT_SMALL)).text_color(c(if error { tk.danger } else { tk.text_muted })).child(SharedString::from(note)))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_shrink_0()
                            .p(px(6.))
                            .border_t_1()
                            .border_color(line(cx, t::LINE_DEFAULT))
                            .child(footer_action("picker-open-folder", "Open folder…", "⌘O", cx).on_click(cx.listener(|this, _, window, cx| this.choose_folder(window, cx))))
                            .child(footer_action("picker-clone", "Clone repository…", "", cx).on_click(cx.listener(|this, _, window, cx| {
                                this.close_overlay(Overlay::Picker, window, cx);
                                this.open_overlay(Overlay::Clone, window, cx);
                            })))
                            .child(footer_action("picker-new", "New empty project", "", cx).on_click(cx.listener(|this, _, window, cx| this.new_empty_project(window, cx)))),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn clone_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let field = |label: &'static str, input: &gpui::Entity<gpui_component::input::InputState>, cx: &mut Context<Self>| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(ui::section_label(label, cx))
                .child(div().h(px(32.)).flex().items_center().px(px(10.)).rounded(px(t::RADIUS_CARD)).bg(c(tk.bg_window)).border_1().border_color(line(cx, t::LINE_CONTROL_STRONG)).font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).child(Input::new(input).appearance(false)))
        };
        div()
            .size_full()
            .flex()
            .items_start()
            .justify_center()
            .pt(relative(0.16))
            .child(
                div()
                    .id("clone-sheet")
                    .role(gpui::Role::Dialog)
                    .aria_label("Clone repository")
                    .key_context("CloneSheet")
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| match event.keystroke.key.as_str() {
                        "escape" => {
                            this.close_overlay(Overlay::Clone, window, cx);
                            cx.stop_propagation();
                        }
                        "enter" => {
                            this.start_clone(window, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }))
                    .w(px(480.))
                    .max_w(relative(0.9))
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .p(px(20.))
                    .rounded(px(t::RADIUS_MODAL))
                    .bg(c(tk.bg_input))
                    .border_1()
                    .border_color(line(cx, t::LINE_CONTROL_STRONG))
                    .shadow(crate::theme::modal_shadow(cx))
                    .child(div().text_size(px(t::TEXT_SETTINGS_TITLE)).font_weight(gpui::FontWeight::MEDIUM).text_color(c(tk.text_primary)).child("Clone repository"))
                    .child(field("repository url", &self.clone_url, cx))
                    .child(field("destination", &self.clone_dest, cx))
                    .when_some(self.clone_note.clone(), |sheet, (note, error)| sheet.child(div().text_size(px(t::TEXT_SMALL)).text_color(c(if error { tk.danger } else { tk.text_muted })).child(SharedString::from(note))))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .justify_end()
                            .child(ui::button("clone-cancel", "Cancel", Kind::Ghost, cx).on_click(cx.listener(|this, _, window, cx| this.close_overlay(Overlay::Clone, window, cx))))
                            .child(ui::button("clone-go", "Clone ↵", Kind::Primary, cx).on_click(cx.listener(|this, _, window, cx| this.start_clone(window, cx)))),
                    ),
            )
            .into_any_element()
    }

    /// Fresh state for the clone sheet each time it opens.
    pub(super) fn reset_clone_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let parent = self.cwd.parent().map(tilde).unwrap_or_else(|| "~".into());
        self.clone_url.update(cx, |s, cx| s.set_value("", window, cx));
        self.clone_dest.update(cx, |s, cx| s.set_value(parent, window, cx));
        self.clone_note = None;
        let _ = cx.new(|_| ());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_filters_are_told_apart() {
        let cwd = Path::new("/work/app");
        assert_eq!(parse_query("cordata", cwd), Query::Filter("cordata".into()));
        assert_eq!(parse_query("/tmp/x", cwd), Query::Path(PathBuf::from("/tmp/x")));
        assert_eq!(parse_query("./sub", cwd), Query::Path(PathBuf::from("/work/app/./sub")));
        assert!(matches!(parse_query("~/code", cwd), Query::Path(p) if p.ends_with("code")));
    }

    #[test]
    fn clone_urls_are_checked() {
        assert!(valid_clone_url("https://github.com/acme/widget.git"));
        assert!(valid_clone_url("git@github.com:acme/widget.git"));
        assert!(!valid_clone_url("--upload-pack=touch /tmp/x"));
        assert!(!valid_clone_url("ext::sh -c touch% /tmp/pwned"));
        assert!(!valid_clone_url("file:///etc"));
        assert!(!valid_clone_url("https://a b"));
        assert_eq!(repo_name("https://github.com/acme/widget.git"), "widget");
        assert_eq!(repo_name("git@github.com:acme/w.git"), "w");
        assert_eq!(repo_name("https://x/..git"), "repository");
    }
}
