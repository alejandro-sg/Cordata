//! The HUD vocabulary, in GPUI (docs/app.md §4b): pills `▐ PLAN ▌`, segmented gauges `▰▰▱▱`, the bracketed title
//! `⌜ C O R D A T A ⌝`, hairline frames `┌╴// TITLE ──┐`, and the haze — all from what GPUI paints (quads, two-stop
//! linear gradients, borders). Colour is never the only signal: every pill says its word.

use cordata_theme::{Role, segbar, tracked};
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, Div, IntoElement, ParentElement, SharedString, Styled, div, linear_color_stop, linear_gradient, px};

use crate::theme::{color, tint};

/// `▐ PLAN ▌` — a mode, a state, a risk band: the word in tracked caps on a tinted plate, edged in its colour.
pub fn pill(label: &str, role: Role, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .px(px(6.))
        .py(px(1.))
        .rounded(px(2.))
        .border_1()
        .border_color(tint(cx, role, 0.65))
        .bg(tint(cx, role, 0.12))
        .text_color(color(cx, role))
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(SharedString::from(format!("▐ {} ▌", label.to_uppercase())))
}

/// `⌜ C O R D A T A ⌝`.
pub fn bracket_title(text: &str, cx: &App) -> Div {
    div().text_color(color(cx, Role::Accent)).text_sm().font_weight(gpui::FontWeight::BOLD).child(SharedString::from(format!("⌜ {} ⌝", tracked(text))))
}

/// A gauge: label, `▰▰▰▱▱`, value. The role follows the level (calm, attention past `warn`, danger past `alarm`).
pub fn gauge(label: &str, fraction: Option<f64>, value: &str, cx: &App) -> Div {
    let role = match fraction {
        Some(f) if f >= 0.9 => Role::Danger,
        Some(f) if f >= 0.75 => Role::Attention,
        Some(_) => Role::Accent,
        None => Role::TextDim,
    };
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .text_xs()
        .child(div().text_color(color(cx, Role::TextDim)).child(SharedString::from(tracked(label))))
        .child(div().text_color(color(cx, role)).child(SharedString::from(segbar(fraction.unwrap_or(0.0), 8))))
        .child(div().text_color(color(cx, Role::TextSecondary)).child(SharedString::from(value.to_owned())))
}

/// A hairline-framed panel with a `// TITLE` header.
pub fn frame(title: &str, body: impl IntoElement, cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .border_1()
        .border_color(color(cx, Role::Hairline))
        .rounded(px(3.))
        .bg(color(cx, Role::Surface))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(10.))
                .py(px(5.))
                .border_b_1()
                .border_color(color(cx, Role::Hairline))
                .text_xs()
                .text_color(color(cx, Role::TextDim))
                .child(SharedString::from(format!("// {}", tracked(title)))),
        )
        .child(div().p(px(10.)).child(body))
}

/// A thin rule with a label in it, between turns: `── 14:02 · turn 3 ──`.
pub fn divider(label: &str, cx: &App) -> Div {
    let line = || div().flex_1().h(px(1.)).bg(color(cx, Role::Hairline));
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .py(px(6.))
        .child(line())
        .child(div().text_xs().text_color(color(cx, Role::TextDim)).child(SharedString::from(label.to_owned())))
        .child(line())
}

/// The haze behind everything: sodium amber rising from the bottom-left, teal falling from the top-right, each a
/// two-stop gradient at a few percent — volumetric light without a shader. Plain ground when `atmosphere` is off
/// (Reduce Transparency / Reduce Motion).
pub fn haze(atmosphere: bool, cx: &App) -> AnyElement {
    let ground = color(cx, Role::Ground);
    if !atmosphere {
        return div().absolute().inset_0().bg(ground).into_any_element();
    }
    let transparent = tint(cx, Role::Ground, 0.0);
    div()
        .absolute()
        .inset_0()
        .bg(ground)
        .child(div().absolute().inset_0().bg(linear_gradient(35., linear_color_stop(tint(cx, Role::Accent, 0.07), 0.0), linear_color_stop(transparent, 0.55))))
        .child(div().absolute().inset_0().bg(linear_gradient(215., linear_color_stop(tint(cx, Role::Agent, 0.06), 0.0), linear_color_stop(transparent, 0.5))))
        .into_any_element()
}

/// A small caps label, as used over every section of the rail and the dock.
pub fn label(text: &str, cx: &App) -> Div {
    div().text_xs().text_color(color(cx, Role::TextDim)).child(SharedString::from(tracked(text)))
}

/// `when` for a role: the attention dot on a session row (running, needs you, done).
pub fn dot(role: Option<Role>, cx: &App) -> Div {
    div().w(px(6.)).h(px(6.)).rounded_full().when_some(role, |this, role| this.bg(color(cx, role)))
}
