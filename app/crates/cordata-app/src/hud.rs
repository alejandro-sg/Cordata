//! The turn divider: a thin rule with its label in it, between turns.

use cordata_theme::tokens as t;
use gpui::{App, Div, ParentElement, SharedString, Styled, div, px};

use crate::theme::line;
use crate::ui;

/// A thin rule with a label in it, between turns: `── 14:02 · turn 3 ──`.
pub fn divider(label: &str, cx: &App) -> Div {
    let rule = || div().flex_1().h(px(1.)).bg(line(cx, t::LINE_SUBTLE));
    div().flex().items_center().gap(px(8.)).py(px(6.)).child(rule()).child(ui::meta(SharedString::from(label.to_owned()), cx)).child(rule())
}
