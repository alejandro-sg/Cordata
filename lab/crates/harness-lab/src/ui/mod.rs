//! What is drawn, and what a keystroke does.
//!
//! The window answers four questions, in this order, top to bottom and left to right:
//!
//! 1. **What is the team doing, and is it all right?** — the header (connection, how much freedom
//!    the team has) and the mission banner (the task, one word for the team's state, Laya's last
//!    reading, memory left).
//! 2. **What is each agent doing?** — the canvas: a pod per agent, conduits between them.
//! 3. **What needs me, and what can I do about it?** — the side column: approvals and alerts first,
//!    then the selected agent and its controls, then tabs for the rest.
//! 4. **What do I want next?** — the steer bar.
//!
//! One rule shapes all of it: **the containment modal is the only thing that may take the window.**
//! It is a full-window scrim drawn at the highest `deferred` priority, opened by itself only for a
//! destructive reading; everything else waits in "Needs you" and never blocks.
//!
//! * [`kit`]      — captions, LEDs, chips, readouts, keycaps, buttons, tabs
//! * [`pods`]     — the per-agent row, the delegation order and the role table
//! * [`gauges`]   — the painted Laya threat gauge and the unified-memory bar
//! * [`overlays`] — the scrim, the containment modal, the approval cards and the diff viewer
//! * [`chrome`]   — header, mission banner, steer bar
//! * [`canvas`] / [`side`] — the team canvas and the side column
//! * [`dock`]     — the floating command deck: autonomy, steering, E-Stop
//! * [`dial`]     — the radial Laya dial in the side column

mod canvas;
mod chrome;
mod dock;
pub mod dial;
mod gauges;
mod kit;
mod overlays;
mod pods;
mod side;

use gpui::{Context, Window, deferred, div, prelude::*};

use crate::app::LabApp;
use kit::SANS;
pub use overlays::DiffOverlay;

impl Render for LabApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.redrawn += 1;
        if !self.focused_once {
            window.focus(&self.focus);
            self.focused_once = true;
        }
        // The title says what is wrong before what is on screen, so a window behind another one
        // still reports an offline daemon or an approval waiting.
        window.set_window_title(&crate::app::window_title(self));
        let theme = self.theme;
        let modal = self.modal_intervention().cloned();
        let overlay = self.diff_overlay_data();
        let mut root = div()
            .debug_selector(|| "lab-root".to_owned())
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg)
            .text_color(theme.text)
            .font_family(SANS)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| key(this, event, cx)))
            // Dragging is tracked at the root, which is always under the pointer: the dial itself
            // only sees moves while the pointer is over it, so a drag that leaves the dial would
            // otherwise stick. The value is only sent while a drag is live.
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| this.drag_dial(event.position, cx)))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(|this, _event: &gpui::MouseUpEvent, _window, cx| this.end_dial_drag(cx)))
            .child(self.header(cx))
            .child(self.mission_banner(cx))
            .child(div().debug_selector(|| "lab-body".to_owned()).flex().flex_row().flex_1().min_h_0().child(self.canvas_board(cx)).child(div().w(gpui::px(1.)).h_full().bg(theme.border)).child(self.side(cx)))
            .child(self.command_deck(cx));

        if let Some(overlay) = overlay {
            root = root.child(deferred(overlays::scrim("diff-scrim", overlays::diff_overlay(overlay, theme, cx), theme)).with_priority(1));
        }
        if let Some(open) = modal {
            let waiting = self.model.interventions.len();
            root = root.child(deferred(overlays::scrim("modal-scrim", overlays::containment_modal(&open, waiting, theme, cx), theme)).with_priority(2));
        }
        root
    }
}

// ------------------------------------------------------------------------ keys

/// The keys. While the steer bar has the keyboard, every printable key goes into it — so typing
/// "yes, never run that" can never approve anything.
pub fn key(app: &mut LabApp, event: &gpui::KeyDownEvent, cx: &mut Context<LabApp>) {
    let stroke = &event.keystroke;
    let (shift, platform) = (stroke.modifiers.shift, stroke.modifiers.platform);
    // Quit is handled here rather than as a window action: the root element holds focus, so the
    // same handler that answers an approval can close the window, and there is no second place a
    // key can be bound.
    if platform && stroke.key == "q" {
        app.quit(cx);
        return;
    }
    if app.composing {
        match stroke.key.as_str() {
            "enter" => app.steer(cx),
            "escape" => app.end_composing(cx),
            "backspace" => app.backspace(cx),
            "v" if platform => app.paste(cx),
            _ => {
                if !platform
                    && !stroke.modifiers.control
                    && let Some(typed) = &stroke.key_char
                {
                    app.type_text(typed, cx);
                }
            }
        }
        return;
    }
    if platform && stroke.key == "k" {
        app.begin_composing(cx);
        return;
    }
    match stroke.key.as_str() {
        "/" => app.begin_composing(cx),
        "m" => app.cycle_mode(cx),
        "y" => app.approve(cx),
        "n" => app.reject(cx),
        "k" => app.isolate(cx),
        "u" => app.unload_memory(shift, cx),
        "d" => app.fetch_diff(None, cx),
        // The modal first (esc sends it to the tray); the diff viewer only when no modal is up.
        "escape" => {
            if !app.send_to_tray(cx) {
                app.close_diff(cx);
            }
        }
        "r" | "enter" => app.review(None, cx),
        "down" => app.step_pod(1, cx),
        "up" => app.step_pod(-1, cx),
        "tab" => app.next_session(cx),
        "1" => app.set_effort("low", cx),
        "2" => app.set_effort("medium", cx),
        "3" => app.set_effort("high", cx),
        "4" => app.set_effort("max", cx),
        "[" => app.nudge_dial(-0.05, cx),
        "]" => app.nudge_dial(0.05, cx),
        _ => {}
    }
}

impl LabApp {
    /// The diff viewer's data, cloned out of `self` so the element tree owns everything it draws.
    fn diff_overlay_data(&self) -> Option<DiffOverlay> {
        self.diff.as_ref().map(|diff| DiffOverlay {
            id: diff.id.clone().into(),
            source: diff.source.clone().into(),
            files: diff.files.iter().cloned().map(gpui::SharedString::from).collect(),
            lines: diff.lines,
            truncated: diff.truncated,
            rendered: diff.rendered.clone(),
            error: diff.error.clone().map(gpui::SharedString::from),
        })
    }

    /// Fill in a diff body the client fetched.
    pub fn set_diff_body(&mut self, id: &str, unified: String) {
        if let Some(diff) = self.diff.as_mut()
            && diff.id == id
        {
            diff.set_body(unified);
        }
    }

    pub fn set_diff_error(&mut self, id: &str, error: String) {
        if let Some(diff) = self.diff.as_mut()
            && diff.id == id
        {
            diff.error = Some(error);
        }
    }

    /// The threat of the session in focus, for the gauge.
    pub fn threat(&self) -> Option<harness_proto::v1::Threat> {
        self.model.lab().and_then(|lab| lab.threat.clone())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_keyboard_list_says_the_keys_the_app_actually_handles() {
        // The System tab's list is documentation; this keeps it from drifting away from `key()`.
        let keys = super::side::KEYS;
        for expected in ["/", "m", "y", "n", "k", "r", "esc", "tab", "1-4", "[", "]", "d", "u", "q"] {
            assert!(keys.iter().any(|(combo, _)| combo.contains(expected)), "the keyboard list does not mention {expected}");
        }
    }
}
