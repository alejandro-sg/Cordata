//! The small controls every surface shares, drawn from the tokens: section labels, key caps, status dots, badges,
//! chips, buttons, the switch, the segmented control, the select trigger. Each returns a styled element; the caller
//! adds ids, handlers and accessibility labels. No colour or size is written here except through `theme`/`tokens`.

use cordata_theme::tokens::{self as t};
use cordata_theme::Rgb;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, AnyView, App, Div, ElementId, FontWeight, IntoElement as _, InteractiveElement as _, ParentElement, SharedString, Stateful, StatefulInteractiveElement as _, Styled, Window, div, px};
use gpui_component::tooltip::Tooltip;

use crate::theme::{amber, amber_glow, c, ca, line, mono, tk};

/// Small mono meta text (shortcut hints, ages, counts).
pub fn meta(text: impl Into<SharedString>, cx: &App) -> Div {
    div().flex_shrink_0().whitespace_nowrap().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk(cx).text_faint)).child(text.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    /// Grey, hollow: connecting.
    Hollow,
    /// Amber with a glow: live, running.
    Live,
    /// Amber, still: waiting on you.
    Amber,
    /// Viridian, still: healthy, done, connected. (Lavender is for delegated or untrusted work, not for "fine".)
    Ok,
    /// The failure colour.
    Danger,
    /// A quiet grey.
    Muted,
}

pub fn dot(kind: Dot, cx: &App) -> Div {
    let tk = tk(cx);
    let base = div().flex_shrink_0().size(px(t::DOT)).rounded_full();
    match kind {
        Dot::Hollow => base.border_1().border_color(c(tk.text_muted)),
        Dot::Live => base.bg(c(tk.amber)).shadow(amber_glow(cx)),
        Dot::Amber => base.bg(c(tk.attention)),
        Dot::Ok => base.bg(c(tk.amber)),
        Dot::Danger => base.bg(c(tk.danger)),
        Dot::Muted => base.bg(c(tk.text_faint)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Amber,
    Neutral,
}

/// `approval`, `queued`: a word, not a box. `Amber` is the "needs you" tone (the attention colour on a faint tint);
/// `Neutral` is just quieter text.
pub fn badge(text: &str, tone: Tone, cx: &App) -> Div {
    let tk = tk(cx);
    let base = div().flex_shrink_0().whitespace_nowrap().px(px(5.)).py(px(1.)).rounded(px(t::RADIUS_BADGE)).text_size(px(t::TEXT_META));
    match tone {
        Tone::Amber => {
            let mut fill = c(tk.attention);
            fill.a = 0.12;
            base.bg(fill).font_weight(FontWeight::MEDIUM).text_color(c(tk.attention))
        }
        Tone::Neutral => base.text_color(c(tk.text_muted)),
    }
    .child(SharedString::from(text.to_lowercase()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Amber fill, dark text.
    Primary,
    /// Amber tint and border.
    Tint,
    /// Amber outline.
    AmberOutline,
    /// A hairline outline.
    Outline,
    /// Text only.
    Ghost,
    /// The danger outline.
    Danger,
}

/// A button: never wraps, 5px radius, hover per kind. The caller adds `on_click`.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, kind: Kind, cx: &App) -> Stateful<Div> {
    let tk = tk(cx);
    let base = div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .h(px(28.))
        .px(px(10.))
        .rounded(px(t::RADIUS_CONTROL))
        .whitespace_nowrap()
        .cursor_pointer()
        .text_size(px(t::TEXT_UI))
        .child(label.into());
    match kind {
        Kind::Primary => {
            let hover = c(tk.amber_hover);
            base.bg(c(tk.amber)).text_color(c(tk.amber_on_fill)).font_weight(FontWeight::MEDIUM).hover(move |s| s.bg(hover))
        }
        Kind::Tint => {
            let hover = amber(cx, t::AMBER_FILL_HOVER);
            base.bg(amber(cx, t::AMBER_FILL_MID)).border_1().border_color(amber(cx, t::AMBER_BORDER_STRONG)).text_color(c(tk.amber)).hover(move |s| s.bg(hover))
        }
        Kind::AmberOutline => {
            let hover = amber(cx, t::AMBER_FILL_MID);
            base.border_1().border_color(amber(cx, t::AMBER_BORDER_STRONG)).text_color(c(tk.amber)).hover(move |s| s.bg(hover))
        }
        Kind::Outline => {
            let hover = line(cx, t::HOVER_STRONG);
            base.border_1().border_color(line(cx, t::LINE_CONTROL_STRONG)).text_color(c(tk.text_primary)).hover(move |s| s.bg(hover))
        }
        Kind::Ghost => {
            let (hover, ink) = (line(cx, t::HOVER), c(tk.text_primary));
            base.text_color(c(tk.text_muted)).hover(move |s| s.bg(hover).text_color(ink))
        }
        Kind::Danger => {
            let hover = ca(tk.danger, t::DANGER_FILL);
            base.border_1().border_color(ca(tk.danger, t::DANGER_BORDER)).text_color(c(tk.danger)).hover(move |s| s.bg(hover))
        }
    }
}

/// A small icon-only control (⚙, ⋯, ✕): needs `tooltip` and an accessibility label from the caller.
pub fn icon_button(id: impl Into<ElementId>, glyph: &'static str, cx: &App) -> Stateful<Div> {
    let (hover, ink) = (line(cx, t::HOVER_STRONG), c(tk(cx).text_primary));
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(24.))
        .rounded(px(t::RADIUS_CONTROL))
        .cursor_pointer()
        .text_size(px(t::TEXT_UI))
        .text_color(c(tk(cx).text_muted))
        .hover(move |s| s.bg(hover).text_color(ink))
        .role(gpui::Role::Button)
        .child(glyph)
}

/// Whether the window may animate: off when macOS Reduce Motion is on. Every animation asks this first.
pub fn motion(cx: &App) -> bool {
    !cx.try_global::<crate::platform::Display>().is_some_and(|d| d.reduce_motion)
}

/// A 24px icon action: one Lucide icon (the set embedded in `main.rs`), muted until hovered. Every action in the
/// chrome uses these, so they share one stroke weight, size and baseline.
pub fn icon_action(id: impl Into<ElementId>, icon: gpui_kit_assets::IconName, cx: &App) -> Stateful<Div> {
    let (hover, ink) = (line(cx, t::HOVER_STRONG), c(tk(cx).text_primary));
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(24.))
        .rounded(px(t::RADIUS_CONTROL))
        .cursor_pointer()
        .text_color(c(tk(cx).text_muted))
        .hover(move |s| s.bg(hover).text_color(ink))
        .role(gpui::Role::Button)
        .child(gpui_component::Icon::new(icon).size(px(15.)))
}

/// A header chip (`⎇ main`, `opus-4 · 200k ⌄`): mono, hairline outline.
pub fn chip(id: impl Into<ElementId>, text: impl Into<SharedString>, cx: &App) -> Stateful<Div> {
    let hover = line(cx, t::HOVER_STRONG);
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(4.))
        .h(px(24.))
        .px(px(8.))
        .rounded(px(t::RADIUS_CONTROL))
        .border_1()
        .border_color(line(cx, t::LINE_CONTROL))
        .whitespace_nowrap()
        .font_family(mono(cx))
        .text_size(px(t::TEXT_META))
        .text_color(c(tk(cx).text_secondary))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(text.into())
}

/// The switch: 34×20, amber when on with a dark knob.
pub fn switch(id: impl Into<ElementId>, on: bool, cx: &App) -> Stateful<Div> {
    let tk = tk(cx);
    let knob = px(t::SWITCH_H - 6.);
    div()
        .id(id)
        .flex_shrink_0()
        .relative()
        .w(px(t::SWITCH_W))
        .h(px(t::SWITCH_H))
        .rounded_full()
        .cursor_pointer()
        .role(gpui::Role::Switch)
        .aria_toggled(on.into())
        .when(on, |s| s.bg(c(tk.amber)))
        .when(!on, |s| s.bg(line(cx, t::LINE_CONTROL_STRONG)).border_1().border_color(line(cx, t::LINE_CONTROL)))
        .child(
            div()
                .absolute()
                .top(px(3.))
                .left(if on { px(t::SWITCH_W - 3.) - knob } else { px(3.) })
                .size(knob)
                .rounded_full()
                .bg(if on { c(tk.amber_on_fill) } else { c(tk.text_muted) }),
        )
}

/// One option of a segmented control: the selected one is a stronger fill in the primary ink — no accent tint, no
/// ring. The caller lays them in a row (`segmented_row`) and adds the click.
pub fn segment(id: impl Into<ElementId>, label: &str, selected: bool, cx: &App) -> Stateful<Div> {
    let tk = tk(cx);
    let (hover, strong, ink) = (line(cx, t::HOVER), line(cx, t::HOVER_STRONG + 0.03), c(tk.text_primary));
    div()
        .id(id)
        .flex_shrink_0()
        .px(px(10.))
        .h(px(24.))
        .flex()
        .items_center()
        .rounded(px(t::RADIUS_ROW))
        .whitespace_nowrap()
        .cursor_pointer()
        .text_size(px(t::TEXT_SMALL))
        .when(selected, move |s| s.bg(strong).text_color(ink).font_weight(FontWeight::MEDIUM))
        .when(!selected, move |s| s.text_color(c(tk.text_muted)).hover(move |s| s.bg(hover).text_color(ink)))
        .role(gpui::Role::RadioButton)
        .aria_selected(selected)
        .child(SharedString::from(label.to_owned()))
}

pub fn segmented_row(cx: &App) -> Div {
    div().flex().flex_shrink_0().gap(px(2.)).p(px(2.)).rounded(px(t::RADIUS_CONTROL)).border_1().border_color(line(cx, t::LINE_CONTROL))
}

/// A select's trigger: the mono value and `⌄`, at least 150px.
pub fn select_trigger(id: impl Into<ElementId>, value: &str, cx: &App) -> Stateful<Div> {
    let hover = line(cx, t::HOVER_STRONG);
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .min_w(px(t::SELECT_MIN_W))
        .h(px(28.))
        .px(px(10.))
        .rounded(px(t::RADIUS_CONTROL))
        .border_1()
        .border_color(line(cx, t::LINE_CONTROL_STRONG))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .role(gpui::Role::ComboBox)
        .child(div().whitespace_nowrap().truncate().text_size(px(t::TEXT_SMALL)).text_color(c(tk(cx).text_primary)).child(SharedString::from(value.to_owned())))
        .child(div().flex_shrink_0().text_color(c(tk(cx).text_faint)).child(gpui_component::Icon::new(gpui_kit_assets::IconName::ChevronDown).size(px(13.))))
}

/// A popover surface: frosted liquid glass ground, control border, 8px radius, specular shadow.
pub fn popover(cx: &App) -> Div {
    div()
        .rounded(px(t::RADIUS_POPOVER))
        .border_1()
        .border_color(line(cx, t::LINE_CONTROL_STRONG))
        .bg(crate::theme::glass_modal_bg(cx))
        .shadow(crate::theme::soft_shadow(cx))
}

/// A tooltip builder for `.tooltip(…)`: the words, and the shortcut when there is one.
pub fn tip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text: SharedString = text.into();
    move |window, cx| Tooltip::new(text.clone()).build(window, cx)
}

/// One row of a menu, list or palette: a hover fill; the selected row is a stronger fill with the rail's 2px signal
/// edge mark — never an accent flood.
pub fn row(id: impl Into<ElementId>, selected: bool, cx: &App) -> Stateful<Div> {
    let (hover, strong, mark) = (line(cx, t::HOVER), line(cx, t::HOVER_STRONG), c(tk(cx).amber));
    div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(10.))
        .py(px(6.))
        .rounded(px(t::RADIUS_ROW))
        .cursor_pointer()
        .when(selected, move |s| s.bg(strong).child(edge_mark(mark)))
        .when(!selected, move |s| s.hover(move |s| s.bg(hover)))
}

/// The 2px edge mark at a row's left (the rail's thread state, a selected row): absolutely placed, no layout shift.
pub fn edge_mark(colour: gpui::Hsla) -> Div {
    div().absolute().left(px(0.)).top(px(7.)).bottom(px(7.)).w(px(2.)).rounded_full().bg(colour)
}

/// A quiet group label (`Threads`, `Recent`): sentence case, faint, small — the rail's voice, not tracked capitals.
pub fn quiet_label(text: impl Into<SharedString>, cx: &App) -> Div {
    div().flex_shrink_0().whitespace_nowrap().text_size(px(t::TEXT_META)).text_color(c(tk(cx).text_faint)).child(text.into())
}

/// A link-weight action (`keep`, `revert`, `copy`): faint text that lights on hover; `danger` hovers in the danger
/// colour. The after-reply links' form, for panes that need small actions without buttons.
pub fn text_action(id: impl Into<ElementId>, label: impl Into<SharedString>, danger: bool, cx: &App) -> Stateful<Div> {
    let tk = tk(cx);
    let hover = c(if danger { tk.danger } else { tk.text_primary });
    div()
        .id(id)
        .flex_shrink_0()
        .whitespace_nowrap()
        .cursor_pointer()
        .role(gpui::Role::Button)
        .text_size(px(t::TEXT_SMALL))
        .text_color(c(tk.text_faint))
        .hover(move |s| s.text_color(hover))
        .child(label.into())
}

/// Empty state for a side-panel tab: what is missing, and a one-line hint — plain text, left on the pane's edge.
pub fn empty(label: &str, hint: &str, cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .py(px(16.))
        .child(div().text_size(px(t::TEXT_UI)).text_color(c(tk(cx).text_secondary)).child(SharedString::from(label.to_owned())))
        .child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk(cx).text_muted)).child(SharedString::from(hint.to_owned())))
}

/// `$ pnpm test` blocks, code.
pub fn code_block(text: impl Into<SharedString>, cx: &App) -> Div {
    let tk = tk(cx);
    div()
        .px(px(12.))
        .py(px(10.))
        .rounded(px(t::RADIUS_CARD))
        .bg(c(tk.bg_input))
        .border_1()
        .border_color(line(cx, t::LINE_CONTROL_STRONG))
        .font_family(mono(cx))
        .text_size(px(crate::theme::code_size(cx)))
        .text_color(c(tk.text_primary))
        .child(text.into())
}

const MARK_RING_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" fill="currentColor"><path d="M45 7.201A28 28 0 1 0 45 56.799V45.856A19 19 0 1 1 45 18.144Z"/></svg>"#;
const MARK_CELL_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" fill="currentColor"><path d="M49 20h10v24H49Z"/></svg>"#;

/// The Cordata brand mark, adapting to light and dark themes.
#[allow(dead_code)]
pub fn cordata_mark(size: f32, cx: &App) -> AnyElement {
    let t = tk(cx);
    let ring_color = c(t.text_primary);
    let cell_color = c(t.amber);
    div()
        .relative()
        .size(px(size))
        .child(
            div()
                .absolute()
                .inset_0()
                .child(gpui::svg().data(MARK_RING_SVG).size_full().text_color(ring_color)),
        )
        .child(
            div()
                .absolute()
                .inset_0()
                .child(gpui::svg().data(MARK_CELL_SVG).size_full().text_color(cell_color)),
        )
        .into_any_element()
}

/// The Cordata logo with subtle Apple liquid glass treatment (frosted squircle, specular rim, ambient drop shadow).
pub fn cordata_logo(size: f32, cx: &App) -> AnyElement {
    let t = tk(cx);
    let mark_size = (size * 0.58).round();

    // Subtle Apple liquid glass materials for light and dark
    let badge_bg = if t.is_dark {
        ca(Rgb::hex(0x1B1F27), 0.72)
    } else {
        ca(Rgb::hex(0xF8FAFC), 0.82)
    };
    let border_col = if t.is_dark {
        ca(Rgb::hex(0xFFFFFF), 0.16)
    } else {
        ca(Rgb::hex(0x000000), 0.08)
    };
    let specular = if t.is_dark {
        ca(Rgb::hex(0xFFFFFF), 0.40)
    } else {
        ca(Rgb::hex(0xFFFFFF), 0.95)
    };
    let inner_bounce = if t.is_dark {
        ca(t.amber, 0.20)
    } else {
        ca(Rgb::hex(0x000000), 0.04)
    };
    let shadow_ambient = ca(t.shadow, if t.is_dark { 0.42 } else { 0.14 });
    let shadow_contact = ca(t.shadow, if t.is_dark { 0.22 } else { 0.07 });

    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(size * 0.24))
        .bg(badge_bg)
        .border_1()
        .border_color(border_col)
        .shadow(vec![
            // Top specular liquid glass reflection
            gpui::BoxShadow {
                color: specular,
                offset: gpui::point(px(0.), px(1.)),
                blur_radius: px(0.5),
                spread_radius: px(0.),
                inset: true,
            },
            // Bottom inner refraction bounce
            gpui::BoxShadow {
                color: inner_bounce,
                offset: gpui::point(px(0.), px(-1.)),
                blur_radius: px(1.5),
                spread_radius: px(0.),
                inset: true,
            },
            // Ambient elevation shadow
            gpui::BoxShadow {
                color: shadow_ambient,
                offset: gpui::point(px(0.), px((size * 0.08).max(1.))),
                blur_radius: px((size * 0.25).max(3.)),
                spread_radius: px(0.),
                inset: false,
            },
            // Contact shadow
            gpui::BoxShadow {
                color: shadow_contact,
                offset: gpui::point(px(0.), px(1.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
                inset: false,
            },
        ])
        .child(cordata_mark(mark_size, cx))
        .into_any_element()
}
