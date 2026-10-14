//! ⌘I in the editor (Copilot's inline chat): select code, say what to change, and the session's model rewrites the
//! selection in place as one undoable edit (⌘Z). Nothing is saved until you save. Plus the ghost-text provider's
//! switch: buffers get the local Ollama completions when a model is chosen in Settings.

use std::rc::Rc;

use cordata_theme::tokens as t;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, Entity, InteractiveElement as _, IntoElement, KeyDownEvent, StatefulInteractiveElement as _, ParentElement, SharedString, Styled, Window, div, px};
use gpui_component::input::{EditorState, Input};
use serde_json::{Value, json};

use super::Workspace;
use super::completions::OllamaInline;
use crate::theme::{amber, c, line, mono, tk};
use crate::ui::{self, Kind};

pub const EDIT_TAG: &str = "gui-inline-edit";

/// An ⌘I edit in flight: which buffer, the selection it was asked about, and what it held.
pub struct Pending {
    pub state: Entity<EditorState>,
    pub range: std::ops::Range<usize>,
    pub original: String,
    pub asking: bool,
}

impl Workspace {
    /// Give `state` the ghost-text provider when a completion model is set (or take it away).
    pub(super) fn apply_completions(&self, state: &Entity<EditorState>, cx: &mut Context<Self>) {
        let model = self.prefs.completion_model.clone();
        state.update(cx, |s, _| {
            s.lsp_mut().completion_provider = if model.is_empty() { None } else { Some(Rc::new(OllamaInline { model })) };
        });
    }

    pub(super) fn apply_completions_to_all(&self, cx: &mut Context<Self>) {
        let states: Vec<Entity<EditorState>> = self.files.buffers.iter().map(|b| b.state.clone()).collect();
        for state in &states {
            self.apply_completions(state, cx);
        }
    }

    /// ⌘I: open the instruction bar for the active buffer's selection (or the current line when nothing is selected).
    pub(super) fn begin_inline_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(buffer) = self.files.active() else { return };
        let state = buffer.state.clone();
        let (range, original) = state.update(cx, |s, cx| {
            let mut range = s.selected_range();
            if range.is_empty() {
                let text = s.value().to_string();
                let at = range.start.min(text.len());
                let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
                let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
                range = start..end;
                s.set_selected_range(range.clone(), cx);
            }
            let text = s.value().to_string();
            let original = text.get(range.clone()).unwrap_or_default().to_owned();
            (range, original)
        });
        self.inline_pending = Some(Pending { state, range, original, asking: false });
        self.inline_input.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    fn submit_inline_edit(&mut self, cx: &mut Context<Self>) {
        let instruction = self.inline_input.read(cx).value().trim().to_owned();
        let Some(pending) = self.inline_pending.as_mut() else { return };
        if instruction.is_empty() || pending.asking {
            return;
        }
        let text = pending.state.read(cx).value().to_string();
        let before = text.get(..pending.range.start).unwrap_or_default().to_owned();
        let after = text.get(pending.range.end..).unwrap_or_default().to_owned();
        let (path, language) = self.files.active().map(|b| (b.rel.clone(), super::editor::language_for(&b.rel).to_owned())).unwrap_or_default();
        pending.asking = true;
        self.session.read(cx).request("_cordata/edit/inline", json!({ "code": pending.original, "before": before, "after": after, "instruction": instruction, "path": path, "language": language }), EDIT_TAG);
        cx.notify();
    }

    /// The model's replacement: applied only if the selection still holds what was sent (else it would clobber edits).
    pub(super) fn on_inline_edit(&mut self, value: Value, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.inline_pending.take() else { return };
        let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let code = value.get("code").and_then(Value::as_str);
        match (ok, code) {
            (true, Some(code)) => {
                let still = pending.state.read(cx).value().to_string().get(pending.range.clone()).map(str::to_owned);
                if still.as_deref() != Some(pending.original.as_str()) {
                    self.files.notice = Some("The selection changed while the model worked — nothing applied.".into());
                } else {
                    let code = code.to_owned();
                    pending.state.update(cx, |s, cx| {
                        s.set_selected_range(pending.range.clone(), cx);
                        s.replace(code, window, cx);
                        s.focus(window, cx);
                    });
                    self.mark_edited(&pending.state);
                    self.files.notice = Some("Edited — ⌘Z undoes it; nothing is saved until you save.".into());
                }
            }
            _ => self.files.notice = Some(value.get("reason").and_then(Value::as_str).unwrap_or("the edit failed").to_owned()),
        }
        cx.notify();
    }

    fn inline_keys(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "enter" => {
                self.submit_inline_edit(cx);
                cx.stop_propagation();
            }
            "escape" => {
                if let Some(p) = self.inline_pending.take() {
                    p.state.update(cx, |s, cx| s.focus(window, cx));
                }
                cx.stop_propagation();
                cx.notify();
            }
            _ => {}
        }
    }

    /// The bar above the editor while an ⌘I edit is open.
    pub(super) fn inline_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.inline_pending.as_ref()?;
        let tk = tk(cx);
        let lines = pending.original.lines().count().max(1);
        Some(
            div()
                .id("inline-edit")
                .role(gpui::Role::Dialog)
                .aria_label("Edit selection")
                .capture_key_down(cx.listener(Self::inline_keys))
                .flex()
                .items_center()
                .gap(px(8.))
                .p(px(6.))
                .rounded(px(t::RADIUS_CARD))
                .border_1()
                .border_color(amber(cx, t::AMBER_BORDER_STRONG))
                .bg(c(tk.bg_input))
                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.amber)).child("⌘I"))
                .child(div().flex_1().text_size(px(t::TEXT_UI)).child(Input::new(&self.inline_input).appearance(false)))
                .child(ui::meta(SharedString::from(format!("{lines} line{}", if lines == 1 { "" } else { "s" })), cx))
                .when(pending.asking, |r| r.child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.amber)).child("editing…")))
                .when(!pending.asking, |r| r.child(ui::button("inline-go", "Edit ↵", Kind::Tint, cx).h(px(24.)).on_click(cx.listener(|this, _, _, cx| this.submit_inline_edit(cx)))))
                .child(div().h(px(18.)).w(px(1.)).bg(line(cx, t::LINE_CONTROL)))
                .child(ui::meta("esc", cx))
                .into_any_element(),
        )
    }
}
