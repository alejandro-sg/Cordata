//! The small controls every surface shares, drawn from the tokens: section labels, key caps, status dots, badges,
//! chips, buttons, the switch, the segmented control, the select trigger. Each returns a styled element; the caller
//! adds ids, handlers and accessibility labels. No colour or size is written here except through `theme`/`tokens`.

use cordata_theme::tokens::{self as t, Tracking, tracked_text};
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyView, App, Div, ElementId, FontWeight, InteractiveElement as _, ParentElement, SharedString, Stateful, StatefulInteractiveElement as _, Styled, Window, div, px, relative};
use gpui_component::tooltip::Tooltip;

use crate::theme::{amber, amber_glow, c, ca, inset_ring, line, mono, teal, tk};

/// `PROJECT`: 10px mono, tracked capitals, the label colour.
pub fn section_label(text: &str, cx: &App) -> Div {
    div()
        .flex_shrink_0()
        .whitespace_nowrap()
        .font_family(mono(cx))
        .text_size(px(t::TEXT_LABEL))
        .font_weight(FontWeight::MEDIUM)
        .text_color(c(tk(cx).text_label))
        .child(SharedString::from(tracked_text(text, Tracking::Label, true)))
}

/// `// MISSION CONTROL`: a primary panel heading, the slashes in amber.
pub fn heading(text: &str, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .whitespace_nowrap()
        .font_family(mono(cx))
        .text_size(px(t::TEXT_LABEL))
        .font_weight(FontWeight::MEDIUM)
        .child(div().text_color(c(tk(cx).amber)).child("//"))
        .child(div().text_color(c(tk(cx).text_secondary)).child(SharedString::from(tracked_text(text, Tracking::Label, true))))
}

/// Small mono meta text (shortcut hints, ages, counts).
pub fn meta(text: impl Into<SharedString>, cx: &App) -> Div {
    div().flex_shrink_0().whitespace_nowrap().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk(cx).text_faint)).child(text.into())
}

/// `⌘K` in a key cap: 1px border, 2px bottom border, 4px radius.
pub fn keycap(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex_shrink_0()
        .whitespace_nowrap()
        .px(px(4.))
        .rounded(px(t::RADIUS_CAP))
        .border_1()
        .border_b_2()
        .border_color(line(cx, t::LINE_CONTROL_STRONG))
        .font_family(mono(cx))
        .text_size(px(t::TEXT_LABEL))
        .text_color(c(tk(cx).text_muted))
        .child(text.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    /// Grey, hollow: connecting.
    Hollow,
    /// Amber with a glow: live, running.
    Live,
    /// Amber, still: waiting on you.
    Amber,
    /// Teal: idle, done, trusted, connected.
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
        Dot::Amber => base.bg(c(tk.amber)),
        Dot::Ok => base.bg(c(tk.teal)),
        Dot::Danger => base.bg(c(tk.danger)),
        Dot::Muted => base.bg(c(tk.text_faint)),
    }
}

/// The assistant / approval marker: a diamond in amber (GPUI does not rotate boxes, so it is the glyph).
pub fn diamond(cx: &App) -> Div {
    div().flex_shrink_0().text_size(px(t::TEXT_LABEL)).line_height(relative(1.)).text_color(c(tk(cx).amber)).child("◆")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Amber,
    Teal,
    Danger,
    Neutral,
}

/// `◆ UNTRUSTED`, `● TRUSTED`, `ASK`: a 3px-radius badge in a tint of its tone.
pub fn badge(text: &str, tone: Tone, cx: &App) -> Div {
    let tk = tk(cx);
    let (ink, fill, edge) = match tone {
        Tone::Amber => (c(tk.amber), amber(cx, t::AMBER_FILL_MID), amber(cx, t::AMBER_BORDER)),
        Tone::Teal => (c(tk.teal), teal(cx, t::TEAL_FILL), teal(cx, t::TEAL_BORDER)),
        Tone::Danger => (c(tk.danger), ca(tk.danger, t::DANGER_FILL), ca(tk.danger, t::DANGER_BORDER)),
        Tone::Neutral => (c(tk.text_secondary), line(cx, t::FILL_FAINT), line(cx, t::LINE_CONTROL)),
    };
    div()
        .flex_shrink_0()
        .whitespace_nowrap()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(t::RADIUS_BADGE))
        .border_1()
        .border_color(edge)
        .bg(fill)
        .font_family(mono(cx))
        .text_size(px(t::TEXT_TAG))
        .font_weight(FontWeight::MEDIUM)
        .text_color(ink)
        .child(SharedString::from(tracked_text(text, Tracking::Label, true)))
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

/// One option of a segmented control; amber tint on the selected one. The caller lays them in a row
/// (`segmented_row`) and adds the click.
pub fn segment(id: impl Into<ElementId>, label: &str, selected: bool, cx: &App) -> Stateful<Div> {
    let tk = tk(cx);
    let hover = line(cx, t::HOVER);
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
        .font_family(mono(cx))
        .text_size(px(t::TEXT_META))
        .when(selected, |s| s.bg(amber(cx, t::AMBER_FILL_STRONG)).text_color(c(tk.amber)).shadow(inset_ring(amber(cx, t::AMBER_BORDER))))
        .when(!selected, move |s| s.text_color(c(tk.text_muted)).hover(move |s| s.bg(hover)))
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
        .bg(c(tk(cx).bg_input))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .role(gpui::Role::ComboBox)
        .child(div().whitespace_nowrap().truncate().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk(cx).text_primary)).child(SharedString::from(value.to_owned())))
        .child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk(cx).text_muted)).child("⌄"))
}

/// A popover surface: input ground, control border, 8px radius, soft shadow.
pub fn popover(cx: &App) -> Div {
    div()
        .rounded(px(t::RADIUS_POPOVER))
        .border_1()
        .border_color(line(cx, t::LINE_CONTROL_STRONG))
        .bg(c(tk(cx).bg_input))
        .shadow(crate::theme::soft_shadow(cx))
}

/// A tooltip builder for `.tooltip(…)`: the words, and the shortcut when there is one.
pub fn tip(text: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |window, cx| Tooltip::new(text).build(window, cx)
}

/// One row of a menu, list or palette: hover fill, selected amber tint.
pub fn row(id: impl Into<ElementId>, selected: bool, cx: &App) -> Stateful<Div> {
    let hover = line(cx, t::HOVER);
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(10.))
        .py(px(6.))
        .rounded(px(t::RADIUS_ROW))
        .cursor_pointer()
        .when(selected, |s| s.bg(amber(cx, t::AMBER_FILL_MID)))
        .when(!selected, move |s| s.hover(move |s| s.bg(hover)))
}

/// Empty state for a side-panel tab: a centred label and a one-line hint.
pub fn empty(label: &str, hint: &str, cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .py(px(40.))
        .px(px(16.))
        .child(section_label(label, cx))
        .child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk(cx).text_muted)).text_center().child(SharedString::from(hint.to_owned())))
}

/// `$ pnpm test` blocks, code.
pub fn code_block(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .px(px(12.))
        .py(px(10.))
        .rounded(px(t::RADIUS_CARD))
        .bg(c(tk(cx).bg_window))
        .border_1()
        .border_color(line(cx, t::LINE_DEFAULT))
        .font_family(mono(cx))
        .text_size(px(t::TEXT_SMALL))
        .text_color(c(tk(cx).text_body))
        .child(text.into())
}
