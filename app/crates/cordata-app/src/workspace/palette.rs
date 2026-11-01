//! The command palette (⌘K): every window command and the project's conversations, filtered fuzzily as you type;
//! ↑/↓ move, ↵ runs, esc closes. The `/` menu in the composer stays for the harness's slash commands.

use cordata_theme::tokens as t;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px, relative};
use gpui_component::input::Input;

use super::{DockTab, Overlay, Workspace};
use crate::session::Target;
use crate::theme::{c, line, mono, tk};
use crate::ui;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    NewConversation,
    ResumeLast,
    ToggleSidebar,
    TogglePanel,
    OpenTerminal,
    Tab(DockTab),
    TrustProject,
    RevokeTrust,
    SwitchProject,
    OpenFolder,
    Settings,
    CycleMode,
    StopRun,
    Attach,
    Worktree,
    Review(bool),
    SettingsAt(super::settings_modal::Section),
    Open(Target),
}

#[derive(Debug, Clone)]
pub struct Item {
    pub group: &'static str,
    pub label: String,
    pub shortcut: &'static str,
    pub action: Action,
}

/// A subsequence match of `query` in `text` (case-insensitive), scored: consecutive letters and letters at word
/// starts count more, an earlier first match more. `None` when the letters are not all there in order.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let query: Vec<char> = query.trim().to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if query.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut qi = 0usize;
    let mut last: Option<usize> = None;
    let mut first: Option<usize> = None;
    for (i, &ch) in chars.iter().enumerate() {
        if qi < query.len() && ch == query[qi] {
            first.get_or_insert(i);
            let at_word = i == 0 || !chars[i - 1].is_alphanumeric();
            score += 1 + if at_word { 6 } else { 0 } + if last == Some(i.wrapping_sub(1)) { 4 } else { 0 };
            last = Some(i);
            qi += 1;
        }
    }
    (qi == query.len()).then(|| score - first.unwrap_or(0) as i32 / 4)
}

impl Workspace {
    fn palette_items(&self, cx: &gpui::App) -> Vec<Item> {
        let trusted = self.settings_view.trusted;
        let mut items = vec![
            Item { group: "chat", label: "New conversation".into(), shortcut: "⌘N", action: Action::NewConversation },
            Item { group: "chat", label: "Resume last conversation".into(), shortcut: "", action: Action::ResumeLast },
            Item { group: "chat", label: "Cycle permission mode".into(), shortcut: "⇧Tab", action: Action::CycleMode },
            Item { group: "chat", label: "Stop the run".into(), shortcut: "esc esc", action: Action::StopRun },
            Item { group: "panel", label: "Toggle side panel".into(), shortcut: "⌘\\", action: Action::TogglePanel },
            Item { group: "panel", label: "Show or hide project sidebar".into(), shortcut: "⌘B", action: Action::ToggleSidebar },
            Item { group: "panel", label: "Open terminal".into(), shortcut: "⌃`", action: Action::OpenTerminal },
            Item { group: "panel", label: "Mission control".into(), shortcut: "⌘⌥1", action: Action::Tab(DockTab::Mission) },
            Item { group: "panel", label: "Files".into(), shortcut: "⌘P", action: Action::Tab(DockTab::Files) },
            Item { group: "panel", label: "Editor".into(), shortcut: "⌘⌥3", action: Action::Tab(DockTab::Editor) },
            Item { group: "panel", label: "Review changes".into(), shortcut: "⌘⌥4", action: Action::Tab(DockTab::Review) },
            Item { group: "panel", label: "Browser agent".into(), shortcut: "⌘⌥6", action: Action::Tab(DockTab::Browser) },
            Item { group: "panel", label: "Models & voice".into(), shortcut: "⌘⌥7", action: Action::Tab(DockTab::Models) },
            Item { group: "chat", label: "History: go back, fork, undo files".into(), shortcut: "", action: Action::Tab(DockTab::History) },
            Item { group: "chat", label: "Attach files…".into(), shortcut: "", action: Action::Attach },
            Item { group: "chat", label: "New conversation in a worktree…".into(), shortcut: "", action: Action::Worktree },
            Item { group: "proj", label: "Review uncommitted changes".into(), shortcut: "", action: Action::Review(false) },
            Item { group: "proj", label: "Review this branch".into(), shortcut: "", action: Action::Review(true) },
            Item { group: "proj", label: "Commit, push, pull request…".into(), shortcut: "", action: Action::Tab(DockTab::Review) },
            if trusted {
                Item { group: "proj", label: "Revoke trust for this project".into(), shortcut: "", action: Action::RevokeTrust }
            } else {
                Item { group: "proj", label: "Trust this project…".into(), shortcut: "", action: Action::TrustProject }
            },
            Item { group: "proj", label: "Switch project…".into(), shortcut: "⌘O", action: Action::SwitchProject },
            Item { group: "proj", label: "Open folder…".into(), shortcut: "", action: Action::OpenFolder },
            Item { group: "app", label: "Settings".into(), shortcut: "⌘,", action: Action::Settings },
            Item { group: "app", label: "Customize: instructions, commands, skills…".into(), shortcut: "", action: Action::SettingsAt(super::settings_modal::Section::Customize) },
        ];
        items.extend(
            self.listing
                .iter()
                .filter(|row| !self.sessions.iter().any(|s| super::matches_target(s.read(cx), &row.target)))
                .take(30)
                .map(|row| Item { group: "chat", label: shown(&row.title), shortcut: "", action: Action::Open(row.target.clone()) }),
        );
        items
    }

    /// The items for the current query, best first (ties keep their order).
    pub(super) fn palette_matches(&self, cx: &gpui::App) -> Vec<Item> {
        let query = self.palette_input.read(cx).value().to_string();
        let mut scored: Vec<(i32, usize, Item)> = self.palette_items(cx).into_iter().enumerate().filter_map(|(i, item)| fuzzy_score(&query, &item.label).map(|s| (s, i, item))).collect();
        if !query.trim().is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        scored.into_iter().map(|(_, _, item)| item).collect()
    }

    pub(super) fn run_palette(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        self.close_overlay(Overlay::Palette, window, cx);
        match action {
            Action::NewConversation => self.new_session(&super::NewSession, window, cx),
            Action::ResumeLast => {
                if let Some(row) = self.listing.iter().find(|row| !self.sessions.iter().any(|s| super::matches_target(s.read(cx), &row.target))).cloned() {
                    self.open(row.target, window, cx);
                }
            }
            Action::ToggleSidebar => self.toggle_sidebar(&super::ToggleSidebar, window, cx),
            Action::TogglePanel => self.toggle_panel(&super::TogglePanel, window, cx),
            Action::OpenTerminal => self.toggle_terminal(&super::ToggleTerminal, window, cx),
            Action::Tab(tab) => {
                self.set_panel(true, cx);
                self.switch_dock(tab, cx);
            }
            Action::TrustProject => self.ask_trust(window, cx),
            Action::RevokeTrust => self.revoke_trust(None, cx),
            Action::SwitchProject => self.open_overlay(Overlay::Picker, window, cx),
            Action::OpenFolder => self.choose_folder(window, cx),
            Action::Settings => self.open_overlay(Overlay::Settings, window, cx),
            Action::CycleMode => self.cycle_mode(&super::CycleMode, window, cx),
            Action::StopRun => self.session.update(cx, |s, cx| s.cancel(cx)),
            Action::Attach => self.choose_attachments(window, cx),
            Action::Worktree => self.begin_worktree(window, cx),
            Action::SettingsAt(section) => {
                self.settings_section = section;
                self.open_overlay(Overlay::Settings, window, cx);
            }
            Action::Review(branch) => self.request_review(branch, window, cx),
            Action::Open(target) => self.open(target, window, cx),
        }
    }

    pub(super) fn palette_keys(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.palette_matches(cx).len();
        match event.keystroke.key.as_str() {
            "down" => self.palette_ix = if count == 0 { 0 } else { (self.palette_ix + 1) % count },
            "up" => self.palette_ix = if count == 0 { 0 } else { (self.palette_ix + count - 1) % count },
            "enter" => {
                if let Some(item) = self.palette_matches(cx).into_iter().nth(self.palette_ix) {
                    self.run_palette(item.action, window, cx);
                }
            }
            "escape" => self.close_overlay(Overlay::Palette, window, cx),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn palette(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let items = self.palette_matches(cx);
        let selected = self.palette_ix.min(items.len().saturating_sub(1));
        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                let action = item.action.clone();
                ui::row(("palette-row", index), index == selected, cx)
                    .h(px(34.))
                    .on_click(cx.listener(move |this, _, window, cx| this.run_palette(action.clone(), window, cx)))
                    .child(div().w(px(44.)).flex_shrink_0().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.text_faint)).child(SharedString::from(item.group.to_uppercase())))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_UI)).text_color(c(if index == selected { tk.text_primary } else { tk.text_secondary })).child(SharedString::from(item.label)))
                    .when(!item.shortcut.is_empty(), |r| r.child(ui::meta(item.shortcut, cx)))
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        let top = window.viewport_size().height.as_f32() * t::PALETTE_TOP_VH;
        div()
            .absolute()
            .top(px(top))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .id("palette")
                    .role(gpui::Role::Dialog)
                    .aria_label("Command palette")
                    .key_context("Palette")
                    .capture_key_down(cx.listener(Self::palette_keys))
                    .on_mouse_down_out(cx.listener(|this, _, window, cx| this.close_overlay(Overlay::Palette, window, cx)))
                    .w(px(t::PALETTE_W))
                    .max_w(relative(0.9))
                    .flex()
                    .flex_col()
                    .rounded(px(t::RADIUS_MODAL))
                    .bg(c(tk.bg_input))
                    .border_1()
                    .border_color(line(cx, t::LINE_CONTROL_STRONG))
                    .shadow(crate::theme::modal_shadow(cx))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .h(px(48.))
                            .px(px(16.))
                            .border_b_1()
                            .border_color(line(cx, t::LINE_DEFAULT))
                            .child(div().font_family(mono(cx)).text_size(px(t::TEXT_BODY)).text_color(c(tk.amber)).child("›"))
                            .child(div().flex_1().text_size(px(t::TEXT_BODY)).child(Input::new(&self.palette_input).appearance(false)))
                            .child(ui::keycap("esc", cx)),
                    )
                    .child(
                        div()
                            .id("palette-list")
                            .role(gpui::Role::ListBox)
                            .max_h(px(380.))
                            .overflow_y_scroll()
                            .p(px(6.))
                            .children(rows)
                            .when(empty, |list| list.child(div().px(px(10.)).py(px(12.)).text_size(px(t::TEXT_UI)).text_color(c(tk.text_muted)).child("No matching command or conversation."))),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;

    #[test]
    fn fuzzy_matches_in_order_and_prefers_word_starts() {
        assert!(fuzzy_score("tsp", "Toggle side panel").is_some());
        assert!(fuzzy_score("pst", "Toggle side panel").is_none());
        assert!(fuzzy_score("", "anything").is_some());
        let word = fuzzy_score("set", "Settings").unwrap();
        let scattered = fuzzy_score("set", "Reset the test").unwrap();
        assert!(word > scattered, "{word} > {scattered}");
        assert!(fuzzy_score("NEW", "New conversation").is_some(), "case-insensitive");
    }
}
