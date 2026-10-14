//! History (the side panel): the conversation's checkpoints and the turns that changed files, with the three ways
//! back that Claude, Codex and Copilot offer — go back (the agent forgets the later turns), fork (a new conversation
//! from that point; this one keeps going) and undo a turn's file changes (previewed first, then applied).
//!
//! Everything is the backend's (`_cordata/conversation/*`, `_cordata/checkpoints/*`): this pane only asks and shows.

use cordata_theme::tokens as t;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px};
use serde_json::{Value, json};

use super::{Workspace, age, now_secs};
use crate::session::Target;
use crate::theme::{c, line, mono, tk};
use crate::ui::{self, Kind};

pub const CONVERSATION_TAG: &str = "gui-history-conversation";
pub const FILES_TAG: &str = "gui-history-files";
pub const REWIND_TAG: &str = "gui-history-rewind";
pub const FORK_TAG: &str = "gui-history-fork";
pub const UNDO_TAG: &str = "gui-history-undo";

#[derive(Default)]
pub struct History {
    pub checkpoints: Vec<Value>,
    pub turns: u64,
    pub file_turns: Vec<Value>,
    /// A previewed file undo: (turn, the dry run's answer).
    pub preview: Option<(String, Value)>,
    pub note: Option<(String, bool)>,
}

impl History {
    pub fn take_conversation(&mut self, value: &Value) {
        self.checkpoints = value.get("checkpoints").and_then(Value::as_array).cloned().unwrap_or_default();
        self.turns = value.get("turns").and_then(Value::as_u64).unwrap_or(0);
    }

    pub fn take_files(&mut self, value: &Value) {
        self.file_turns = value.get("turns").and_then(Value::as_array).cloned().unwrap_or_default();
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

impl Workspace {
    pub(super) fn load_history(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        session.request("_cordata/conversation/list", json!({ "limit": 30 }), CONVERSATION_TAG);
        session.request("_cordata/checkpoints/list", json!({ "limit": 15 }), FILES_TAG);
    }

    /// Back to checkpoint `index` (newest first): the agent forgets what came after; files are untouched.
    fn rewind(&mut self, index: u64, cx: &mut Context<Self>) {
        self.session.read(cx).request("_cordata/conversation/rewind", json!({ "index": index }), REWIND_TAG);
        self.history.note = Some(("Going back…".into(), false));
        cx.notify();
    }

    /// A new conversation from before turn `turn`; this one is left as it is.
    fn fork(&mut self, turn: u64, cx: &mut Context<Self>) {
        self.session.read(cx).request("_cordata/conversation/fork", json!({ "turn": turn }), FORK_TAG);
        self.history.note = Some(("Forking…".into(), false));
        cx.notify();
    }

    fn undo_files(&mut self, turn: &str, apply: bool, cx: &mut Context<Self>) {
        self.session.read(cx).request("_cordata/checkpoints/undo", json!({ "turn": turn, "apply": apply }), UNDO_TAG);
        if apply {
            self.history.preview = None;
        }
        cx.notify();
    }

    /// The answers to this pane's requests (routed from `on_data`).
    pub(super) fn on_history_data(&mut self, tag: &str, value: Value, window: &mut Window, cx: &mut Context<Self>) {
        let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let reason = || str_of(&value, "reason").to_owned();
        match tag {
            CONVERSATION_TAG => self.history.take_conversation(&value),
            FILES_TAG => self.history.take_files(&value),
            REWIND_TAG => {
                self.history.note = Some(if ok {
                    let forgot = value.get("forgot").and_then(Value::as_array).map_or(0, Vec::len);
                    (format!("Went back: the agent forgot {forgot} message{}. The transcript above still shows them; files are unchanged.", if forgot == 1 { "" } else { "s" }), false)
                } else {
                    (reason(), true)
                });
                self.load_history(cx);
            }
            FORK_TAG => {
                if let (true, Some(id)) = (ok, value.get("sessionId").and_then(Value::as_str)) {
                    self.history.note = Some(("Forked — opening it.".into(), false));
                    self.open_fork(id.to_owned(), window, cx);
                } else {
                    self.history.note = Some((reason(), true));
                }
            }
            UNDO_TAG => {
                let dry = value.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
                if !ok {
                    self.history.note = Some((reason(), true));
                } else if dry {
                    let turn = str_of(&value, "turn").to_owned();
                    self.history.preview = Some((turn, value));
                } else {
                    let count = value.get("count").and_then(Value::as_u64).unwrap_or(0);
                    self.history.note = Some((format!("Restored {count} file{} to before that turn.", if count == 1 { "" } else { "s" }), false));
                    self.load_history(cx);
                    self.load_review(cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Open a forked session: on the hub it becomes a conversation of its own; over stdio it is resumed by id.
    fn open_fork(&mut self, session_id: String, window: &mut Window, cx: &mut Context<Self>) {
        match &self.connection.transport {
            acp_core::BackendTransport::Hub { daemon, .. } => {
                let (daemon, cwd) = (daemon.clone(), self.cwd.clone());
                let title = format!("fork of {}", self.session.read(cx).view.title.clone().unwrap_or_else(|| "conversation".into()));
                let job = cx.global::<crate::session::Runtime>().0.spawn(async move { acp_core::hub::adopt(&daemon, &session_id, &cwd, &title).await });
                cx.spawn_in(window, async move |this, cx| {
                    let result = job.await;
                    let _ = this.update_in(cx, |this, window, cx| match result {
                        Ok(Ok(conv)) => {
                            this.open(Target::Conversation(conv.handle), window, cx);
                            this.refresh_listing(cx);
                        }
                        Ok(Err(error)) => this.history.note = Some((error.to_string(), true)),
                        Err(_) => {}
                    });
                })
                .detach();
            }
            _ => self.open(Target::Resume(session_id), window, cx),
        }
    }

    pub(super) fn history_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let now = now_secs();
        let busy = self.session.read(cx).view.turn_active;
        let checkpoints: Vec<AnyElement> = self
            .history
            .checkpoints
            .iter()
            .enumerate()
            .map(|(row, cp)| {
                let index = cp.get("index").and_then(Value::as_u64).unwrap_or(0);
                let turn = cp.get("turn").and_then(Value::as_u64).unwrap_or(0);
                let prompt = shown(str_of(cp, "prompt"));
                let at = cp.get("at").and_then(Value::as_f64).unwrap_or(now);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .p(px(10.))
                    .rounded(px(t::RADIUS_CARD))
                    .border_1()
                    .border_color(line(cx, t::LINE_DEFAULT))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(ui::meta(SharedString::from(format!("turn {turn}")), cx))
                            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TEXT_UI)).text_color(c(tk.text_primary)).child(SharedString::from(if prompt.is_empty() { "(checkpoint)".into() } else { prompt })))
                            .child(ui::meta(SharedString::from(age(now - at)), cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(ui::button(("hist-back", row), "Go back here", Kind::Outline, cx).h(px(24.)).text_size(px(t::TEXT_SMALL)).when(busy, |b| b.opacity(0.5)).tooltip(ui::tip("The agent forgets the later turns; files stay as they are")).on_click(cx.listener(move |this, _, _, cx| this.rewind(index, cx))))
                            .when(turn > 0, |r| r.child(ui::button(("hist-fork", row), "Fork from here", Kind::Ghost, cx).h(px(24.)).text_size(px(t::TEXT_SMALL)).tooltip(ui::tip("A new conversation from before this turn")).on_click(cx.listener(move |this, _, _, cx| this.fork(turn, cx))))),
                    )
                    .into_any_element()
            })
            .collect();
        let files: Vec<AnyElement> = self
            .history
            .file_turns
            .iter()
            .enumerate()
            .map(|(row, ft)| {
                let turn = str_of(ft, "turn").to_owned();
                let undone = ft.get("undone").and_then(Value::as_bool).unwrap_or(false);
                let tools = ft.get("tools").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).take(3).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                let ts = ft.get("ts").and_then(Value::as_f64).unwrap_or(now);
                let previewing = self.history.preview.as_ref().filter(|(t, _)| *t == turn).map(|(_, v)| v.clone());
                let undo_turn = turn.clone();
                let apply_turn = turn.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .py(px(8.))
                    .border_b_1()
                    .border_color(line(cx, t::LINE_SUBTLE))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().min_w_0().flex_1().truncate().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(if undone { tk.text_faint } else { tk.text_secondary })).child(SharedString::from(format!("{} · {}", turn.chars().take(10).collect::<String>(), if tools.is_empty() { "edits".into() } else { tools }))))
                            .child(ui::meta(SharedString::from(if undone { "undone".to_owned() } else { age(now - ts) }), cx))
                            .when(!undone && previewing.is_none(), |r| r.child(ui::button(("hist-undo", row), "Undo files…", Kind::Ghost, cx).h(px(22.)).text_size(px(t::TEXT_SMALL)).on_click(cx.listener(move |this, _, _, cx| this.undo_files(&undo_turn.clone(), false, cx))))),
                    )
                    .when_some(previewing, |col, preview| {
                        let count = preview.get("count").and_then(Value::as_u64).unwrap_or(0);
                        let names: Vec<String> = preview.get("files").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).take(8).map(str::to_owned).collect()).unwrap_or_default();
                        let edited = preview.get("edited_since").and_then(Value::as_array).map_or(0, Vec::len);
                        col.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(6.))
                                .p(px(8.))
                                .rounded(px(t::RADIUS_ROW))
                                .bg(line(cx, t::FILL_FAINT))
                                .child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_primary)).child(SharedString::from(format!("{count} file{} go back to before this turn{}", if count == 1 { "" } else { "s" }, if edited > 0 { format!(" — {edited} were edited since, those edits are lost") } else { String::new() }))))
                                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child(SharedString::from(shown(&names.join("\n")))))
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(6.))
                                        .child(ui::button(("hist-apply", row), "Restore files", Kind::Danger, cx).h(px(24.)).text_size(px(t::TEXT_SMALL)).on_click(cx.listener(move |this, _, _, cx| this.undo_files(&apply_turn.clone(), true, cx))))
                                        .child(ui::button(("hist-cancel", row), "Cancel", Kind::Ghost, cx).h(px(24.)).text_size(px(t::TEXT_SMALL)).on_click(cx.listener(|this, _, _, cx| {
                                            this.history.preview = None;
                                            cx.notify();
                                        }))),
                                ),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let empty_cp = checkpoints.is_empty();
        let empty_files = files.is_empty();
        div()
            .id("history-scroll")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(div().flex().items_center().justify_between().child(ui::heading("history", cx)).child(ui::button("hist-refresh", "Refresh", Kind::Ghost, cx).h(px(22.)).text_size(px(t::TEXT_SMALL)).on_click(cx.listener(|this, _, _, cx| this.load_history(cx)))))
            .when_some(self.history.note.clone(), |col, (note, error)| col.child(div().text_size(px(t::TEXT_SMALL)).text_color(c(if error { tk.danger } else { tk.teal })).child(SharedString::from(shown(&note)))))
            .when(busy, |col| col.child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.amber)).child("A turn is running — going back and restoring files wait until it ends.")))
            .child(ui::section_label("conversation", cx))
            .when(empty_cp, |col| col.child(ui::empty("no checkpoints yet", "Each turn leaves one; come back after the first reply.", cx)))
            .children(checkpoints)
            .child(div().pt(px(8.)).child(ui::section_label("file changes", cx)))
            .when(empty_files, |col| col.child(ui::empty("no file changes", "Turns that edit files can be undone here.", cx)))
            .children(files)
            .into_any_element()
    }
}
