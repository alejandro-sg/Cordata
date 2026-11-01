//! The HUD vocabulary the panes share, on the design tokens: badges for modes and states, the context gauge, framed
//! cards with a `// TITLE` heading, turn dividers, the haze. Colour is never the only signal: every badge says its word.

use cordata_theme::tokens::{self as t, HAZE_AMBER, HAZE_TEAL, Tracking, tracked_text};
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, Div, IntoElement, ParentElement, SharedString, Styled, div, linear_color_stop, linear_gradient, px, relative, rgb};

use crate::theme::{c, line, mono, sans, tk};
use crate::ui;

/// `⌜CORDATA⌝`: 12px proportional sans with crisp thin-space tracking, amber; the brackets in dim amber.
pub fn bracket_title(text: &str, cx: &App) -> Div {
    let tk = tk(cx);
    div()
        .flex()
        .items_center()
        .gap(px(2.))
        .whitespace_nowrap()
        .font_family(sans(cx))
        .text_size(px(t::TEXT_LOGO))
        .font_weight(gpui::FontWeight::BOLD)
        .child(div().font_family(mono(cx)).text_color(c(tk.amber_dim)).child("⌜"))
        .child(div().text_color(c(tk.amber)).child(SharedString::from(tracked_text(text, Tracking::Label, true))))
        .child(div().font_family(mono(cx)).text_color(c(tk.amber_dim)).child("⌝"))
}

/// The context gauge: label, an 80×3 bar (amber fill = share used), the value.
pub fn gauge(label: &str, fraction: Option<f64>, value: &str, cx: &App) -> Div {
    let tk = tk(cx);
    let fill = match fraction {
        Some(f) if f >= 0.9 => tk.danger,
        _ => tk.amber,
    };
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .whitespace_nowrap()
        .font_family(mono(cx))
        .text_size(px(t::TEXT_STATUS))
        .child(div().text_color(c(tk.text_label)).child(SharedString::from(label.to_ascii_uppercase())))
        .child(
            div()
                .relative()
                .w(px(t::CONTEXT_BAR_W))
                .h(px(t::CONTEXT_BAR_H))
                .rounded_full()
                .bg(line(cx, t::LINE_CONTROL_STRONG))
                .child(div().absolute().top_0().bottom_0().left_0().rounded_full().w(relative(fraction.unwrap_or(0.0).clamp(0.0, 1.0) as f32)).bg(c(fill))),
        )
        .child(div().text_color(c(tk.text_secondary)).child(SharedString::from(value.to_owned())))
}

/// A thin rule with a label in it, between turns: `── 14:02 · turn 3 ──`.
pub fn divider(label: &str, cx: &App) -> Div {
    let rule = || div().flex_1().h(px(1.)).bg(line(cx, t::LINE_SUBTLE));
    div().flex().items_center().gap(px(8.)).py(px(6.)).child(rule()).child(ui::meta(SharedString::from(label.to_owned()), cx)).child(rule())
}

/// The ambient haze over the conversation column: sodium amber rising from the bottom-left, teal falling from the
/// top-right, each fading out by 60%. GPUI paints linear gradients only, so the reference's radial ellipses are two
/// angled linear washes. Non-interactive; plain ground when off (the setting, Reduce Transparency, Reduce Motion).
pub fn haze(on: bool, cx: &App) -> AnyElement {
    let ground = c(tk(cx).bg_main);
    let wash = |(hex, alpha): (u32, f32)| {
        let mut colour: gpui::Hsla = rgb(hex).into();
        colour.a = alpha;
        colour
    };
    let clear = |(hex, _): (u32, f32)| {
        let mut colour: gpui::Hsla = rgb(hex).into();
        colour.a = 0.;
        colour
    };
    div()
        .absolute()
        .inset_0()
        .bg(ground)
        .when(on, |this| {
            this.child(div().absolute().inset_0().bg(linear_gradient(35., linear_color_stop(wash(HAZE_AMBER), 0.0), linear_color_stop(clear(HAZE_AMBER), 0.6))))
                .child(div().absolute().inset_0().bg(linear_gradient(215., linear_color_stop(wash(HAZE_TEAL), 0.0), linear_color_stop(clear(HAZE_TEAL), 0.6))))
        })
        .into_any_element()
}

/// A section label (older panes).
pub fn label(text: &str, cx: &App) -> Div {
    ui::section_label(text, cx)
}

