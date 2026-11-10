//! The rail and the side panel are as wide as the person drags them: a 7px grip on each inner edge, invisible until
//! hovered (then a 1px signal line, the column's edge made live), double-click for the default. Widths are kept in
//! prefs and clamped so the conversation never falls under `CONVERSATION_MIN_W`.

use cordata_theme::tokens as t;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, InteractiveElement as _, IntoElement, MouseButton, MouseMoveEvent, ParentElement, StatefulInteractiveElement as _, Styled, div, px};

use super::{Workspace, side_panel};
use crate::theme::{amber, c, tk};
use crate::ui;

pub const RAIL_MIN_W: f32 = 180.;
pub const RAIL_MAX_W: f32 = 440.;
pub const DOCK_MIN_W: f32 = 280.;
pub const DOCK_MAX_W: f32 = 900.;
/// What the conversation keeps however the columns are dragged.
pub const CONVERSATION_MIN_W: f32 = 420.;
pub(super) const GRIP_W: f32 = 7.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Rail,
    Dock,
}

/// The rail's width: the person's, within bounds.
pub fn rail_width(chosen: Option<f32>) -> f32 {
    chosen.unwrap_or(t::SIDEBAR_W).clamp(RAIL_MIN_W, RAIL_MAX_W)
}

/// The side panel's width: the person's (bounded by what the window leaves after the rail and the conversation), or
/// the tab's default.
pub fn dock_width(chosen: Option<f32>, tab: super::DockTab, window_w: f32, rail_w: f32) -> f32 {
    let room = (window_w - rail_w - CONVERSATION_MIN_W).max(DOCK_MIN_W);
    chosen.map_or_else(|| side_panel::panel_width(tab, window_w), |w| w.clamp(DOCK_MIN_W, DOCK_MAX_W.min(room)))
}

impl Workspace {
    pub(super) fn rail_w(&self) -> f32 {
        if self.sidebar_open { rail_width(self.prefs.rail_w) } else { 0. }
    }

    pub(super) fn dock_w(&self, window_w: f32) -> f32 {
        if self.panel_open { dock_width(self.prefs.dock_w, self.dock_tab, window_w, self.rail_w()) } else { 0. }
    }

    /// The drag in progress: follow the pointer, or end when the button is no longer down.
    pub(super) fn on_resize_move(&mut self, event: &MouseMoveEvent, window_w: f32, cx: &mut Context<Self>) {
        let Some(edge) = self.resizing else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.end_resize(cx);
            return;
        }
        let x = event.position.x.as_f32();
        match edge {
            Edge::Rail => {
                let room = window_w - self.dock_w(window_w) - CONVERSATION_MIN_W;
                self.prefs.rail_w = Some(x.clamp(RAIL_MIN_W, RAIL_MAX_W.min(room.max(RAIL_MIN_W))));
            }
            Edge::Dock => self.prefs.dock_w = Some(dock_width(Some(window_w - x), self.dock_tab, window_w, self.rail_w())),
        }
        cx.notify();
    }

    pub(super) fn end_resize(&mut self, cx: &mut Context<Self>) {
        if self.resizing.take().is_some() {
            self.prefs.save();
            cx.notify();
        }
    }

    /// The grips, laid over the column edges (absolute, so they take no width).
    pub(super) fn resize_grips(&self, window_w: f32, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut grips = Vec::new();
        // Edge::Rail is rendered directly on the Logbook drawer overlay slide when sidebar_open is true.
        if self.panel_open {
            grips.push(self.grip(Edge::Dock, self.dock_w(window_w) - GRIP_W / 2., false, cx));
        }
        grips
    }

    pub(super) fn grip(&self, edge: Edge, offset: f32, from_left: bool, cx: &mut Context<Self>) -> AnyElement {
        let active = self.resizing == Some(edge);
        let signal = c(tk(cx).amber);
        let faint = amber(cx, 0.55);
        let (id, label) = match edge {
            Edge::Rail => ("grip-rail", "Drag to resize the sidebar · double-click to reset"),
            Edge::Dock => ("grip-dock", "Drag to resize the panel · double-click to reset"),
        };
        div()
            .id(id)
            .role(gpui::Role::Splitter)
            .aria_label(label)
            .tooltip(ui::tip(label))
            .absolute()
            .top_0()
            .bottom_0()
            .w(px(GRIP_W))
            .when(from_left, |el| el.left(px(offset)))
            .when(!from_left, |el| el.right(px(offset)))
            .cursor_col_resize()
            .flex()
            .justify_center()
            .group(id)
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .when(active, |el| el.bg(signal))
                    .when(!active, |el| el.group_hover(id, |s| s.bg(faint))),
            )
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                if event.click_count >= 2 {
                    match edge {
                        Edge::Rail => this.prefs.rail_w = None,
                        Edge::Dock => this.prefs.dock_w = None,
                    }
                    this.prefs.save();
                } else {
                    this.resizing = Some(edge);
                }
                cx.notify();
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::DockTab;

    #[test]
    fn widths_stay_in_bounds_and_leave_the_conversation_room() {
        assert_eq!(rail_width(None), t::SIDEBAR_W);
        assert_eq!((rail_width(Some(20.)), rail_width(Some(9000.))), (RAIL_MIN_W, RAIL_MAX_W));
        assert_eq!(dock_width(None, DockTab::Files, 1600., 232.), side_panel::panel_width(DockTab::Files, 1600.));
        assert_eq!(dock_width(Some(2000.), DockTab::Files, 1400., 232.), 1400. - 232. - CONVERSATION_MIN_W);
        assert_eq!(dock_width(Some(100.), DockTab::Files, 1400., 232.), DOCK_MIN_W);
    }
}
