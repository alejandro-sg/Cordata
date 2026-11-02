//! The HUD vocabulary the panes share, on the design tokens: badges for modes and states, the context gauge, framed
//! cards with a `// TITLE` heading, turn dividers, the haze. Colour is never the only signal: every badge says its word.

use cordata_theme::tokens::{self as t, HAZE_AMBER, HAZE_TEAL, Tracking, tracked_text};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Bounds, Div, IntoElement, ParentElement, PathBuilder, SharedString, Styled, canvas, div, fill, linear_color_stop, linear_gradient, point, px, rgb,
    size as size_of,
};

use crate::theme::{c, line, sans, tk};
use crate::ui;

/// The mark: a ring cut vertically, with an amber cursor cell in its mouth. Drawn on the 64-unit grid of
/// `app/assets/brand/cordata-mark.svg` (outer radius 28, stroke 9, cut at x = 45; cell 10 × 24 at 49, 20), in the
/// theme's ink and amber so it holds on `light` paper too. A square element: `size` is its side.
pub fn mark(size: f32, cx: &App) -> impl IntoElement {
    let tk = tk(cx);
    let (ink, cell) = (c(tk.text_primary), c(tk.amber));
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let k = f32::from(bounds.size.width).min(f32::from(bounds.size.height)) / 64.;
            let at = |x: f32, y: f32| point(bounds.origin.x + px(x * k), bounds.origin.y + px(y * k));
            let radius = |r: f32| point(px(r * k), px(r * k));
            let mut ring = PathBuilder::fill();
            ring.move_to(at(45., 7.201));
            ring.arc_to(radius(28.), px(0.), true, false, at(45., 56.799));
            ring.line_to(at(45., 45.856));
            ring.arc_to(radius(19.), px(0.), true, true, at(45., 18.144));
            ring.close();
            if let Ok(path) = ring.build() {
                window.paint_path(path, ink);
            }
            let cell_bounds = Bounds { origin: at(49., 20.), size: size_of(px(10. * k), px(24. * k)) };
            window.paint_quad(fill(cell_bounds, cell));
        },
    )
    .size(px(size))
    .flex_none()
}

/// The sidebar and empty-state title: the mark, then `CORDATA` in 12px proportional sans with crisp thin-space tracking.
pub fn brand_title(cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(7.))
        .whitespace_nowrap()
        .font_family(sans(cx))
        .text_size(px(t::TEXT_LOGO))
        .font_weight(gpui::FontWeight::BOLD)
        .child(mark(16., cx))
        .child(div().text_color(c(tk(cx).text_primary)).child(SharedString::from(tracked_text("Cordata", Tracking::Label, true))))
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

