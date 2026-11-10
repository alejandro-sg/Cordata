//! The small controls every surface shares, drawn from the tokens: section labels, key caps, status dots, badges,
//! chips, buttons, the switch, the segmented control, the select trigger. Each returns a styled element; the caller
//! adds ids, handlers and accessibility labels. No colour or size is written here except through `theme`/`tokens`.

use cordata_theme::tokens::{self as t};
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, AnyView, App, Div, ElementId, FontWeight, IntoElement as _, InteractiveElement as _, ParentElement, SharedString, Stateful, StatefulInteractiveElement as _, Styled, Window, div, px};
use gpui_component::tooltip::Tooltip;

use crate::theme::{amber, c, ca, line, mono, tk};

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
        Dot::Live => base.bg(c(tk.amber)),
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

/// What a thread is doing, said by the mark (`app/assets/brand/README.md`, "State"): the loop is the sandbox the agent
/// works in, the gap is the gate where you stand, and the node is the live thing. Each state differs in shape and
/// place as well as colour. Used wherever a thread's state is shown — the rail, the gate, the receipt, the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkState {
    /// The open loop alone, dim: nothing happening.
    Idle,
    /// The node inside the loop: working, in the sandbox.
    Working,
    /// The node in the gap: at the gate, waiting on you.
    NeedsYou,
    /// No node; the loop closes: done.
    Done,
    /// A slash across the gap: something failed.
    Error,
}

const LOOP_OPEN: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" fill="none"><path d="M47.606 17.948A21 21 0 1 0 47.606 46.052" stroke="currentColor" stroke-width="7" stroke-linecap="round"/></svg>"#;
const LOOP_CLOSED: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" fill="none"><circle cx="32" cy="32" r="21" stroke="currentColor" stroke-width="7"/></svg>"#;
const NODE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><circle cx="32" cy="32" r="7" fill="currentColor"/></svg>"#;
const SLASH: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" fill="none"><path d="M49.5 39.5L56.5 24.5" stroke="currentColor" stroke-width="5.5" stroke-linecap="round"/></svg>"#;
/// How far the gate is from the loop's centre, as a share of the mark's width (21 of the 64 grid).
const GATE: f32 = 21. / 64.;

fn mark_layer(svg: &'static [u8], side: f32, colour: gpui::Hsla) -> gpui::Svg {
    gpui::svg().data(svg).absolute().top(px(0.)).left(px(0.)).size(px(side)).text_color(colour)
}

/// The mark in its state, still. `size` is the text size it sits beside.
pub fn mark(id: impl Into<ElementId>, state: MarkState, size: f32, cx: &App) -> AnyElement {
    mark_at(id, state, size, None, cx)
}

/// The mark for a thread whose events we see: `beat` is the thread's event count, and a working node lights on each
/// one and dims between them (`motion::beat`). A thread that has stopped talking looks like it.
pub fn mark_live(id: impl Into<ElementId>, state: MarkState, size: f32, beat: u64, cx: &App) -> AnyElement {
    mark_at(id, state, size, Some(beat), cx)
}

fn mark_at(id: impl Into<ElementId>, state: MarkState, size: f32, beat: Option<u64>, cx: &App) -> AnyElement {
    let tk = tk(cx);
    // Never under 14px: below that the node and the gap close up.
    let side = (size * 1.3).round().max(14.);
    let (ring, ink) = match state {
        MarkState::Idle => (LOOP_OPEN, ca(tk.text_primary, t::MARK_IDLE)),
        MarkState::Done => (LOOP_CLOSED, c(tk.text_secondary)),
        _ => (LOOP_OPEN, c(tk.text_primary)),
    };
    let el = div().relative().flex_shrink_0().size(px(side)).child(mark_layer(ring, side, ink));
    match state {
        MarkState::Working => {
            let node = mark_layer(NODE, side, c(tk.amber));
            el.child(match beat {
                Some(beat) => crate::motion::beat(node, id, beat, cx),
                None => node.into_any_element(),
            })
        }
        // The node comes from inside the loop to the gate: the agent has come to you.
        MarkState::NeedsYou => {
            let (node, reach) = (mark_layer(NODE, side, c(tk.attention)), side * GATE);
            el.child(if motion(cx) {
                gpui::AnimationExt::with_animation(node, id, gpui::Animation::new(crate::motion::ARRIVE).with_easing(gpui::ease_out_quint()), move |el, t| el.left(px(reach * t))).into_any_element()
            } else {
                node.left(px(reach)).into_any_element()
            })
        }
        MarkState::Error => el.child(mark_layer(SLASH, side, c(tk.danger))),
        MarkState::Idle | MarkState::Done => el,
    }
    .into_any_element()
}

/// Whether the window may animate: off when macOS Reduce Motion is on. Every animation asks this first.
pub fn motion(cx: &App) -> bool {
    crate::motion::on(cx)
}

/// A 24px icon action: one of Cordata's icons (`icons.rs`), muted until hovered. Every action in the
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

/// A popover surface: control border, tight shadow.
pub fn popover(cx: &App) -> Div {
    div()
        .rounded(px(t::RADIUS_POPOVER))
        .border_1()
        .border_color(line(cx, t::LINE_CONTROL_STRONG))
        .bg(crate::theme::c(crate::theme::tk(cx).bg_input))
        .shadow(crate::theme::modal_shadow(cx))
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

/// The logo: the loop in the ink and the node at the gate in the signal colour, flat — no badge, glass or shadow.
/// `size` is the line it sits on.
pub fn cordata_logo(size: f32, cx: &App) -> AnyElement {
    let tk = tk(cx);
    let side = (size * 0.78).round();
    div()
        .relative()
        .flex_shrink_0()
        .size(px(side))
        .child(mark_layer(LOOP_OPEN, side, c(tk.text_primary)))
        .child(mark_layer(NODE, side, c(tk.amber)).left(px(side * GATE)))
        .into_any_element()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerSide {
    Left,
    Right,
}

/// A drawer surface (Logbook ⌘B from left, Tool sheet ⌘\ from right): tight low shadow,
/// panel background, separating hairline, and 240 ms slide animation under motion.
pub fn drawer(id: impl Into<ElementId>, side: DrawerSide, width: f32, cx: &App) -> Stateful<Div> {
    let tk = tk(cx);
    let is_left = matches!(side, DrawerSide::Left);
    div()
        .id(id)
        .w(px(width))
        .h_full()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .bg(c(tk.bg_panel))
        .when(is_left, |d| d.border_r_1().border_color(line(cx, t::LINE_SUBTLE)))
        .when(!is_left, |d| d.border_l_1().border_color(line(cx, t::LINE_DEFAULT)))
        .shadow(crate::theme::modal_shadow(cx))
}

/// A ticker displaying rolling numeric values (NumberFlow-style) with 220 ms tick animation.
#[allow(dead_code)]
pub fn ticker(id: impl Into<ElementId>, value: i64, prefix: Option<&str>, cx: &App) -> Stateful<Div> {
    let tk = tk(cx);
    let label = match prefix {
        Some(p) => format!("{p}{value}"),
        None => format!("{value}"),
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .font_family(mono(cx))
        .text_size(px(t::TEXT_META))
        .text_color(c(tk.text_faint))
        .child(SharedString::from(label))
}

/// A notice toast for the bottom-right notice stack.
#[allow(dead_code)]
pub struct NoticeToast {
    pub id: ElementId,
    pub message: SharedString,
    pub tone: Tone,
}

/// Stacked notice toasts (Sonner-style) pinned to bottom-right.
#[allow(dead_code)]
pub fn notices(stack: Vec<NoticeToast>, cx: &App) -> AnyElement {
    if stack.is_empty() {
        return div().into_any_element();
    }
    let tk = tk(cx);
    let count = stack.len();
    let toasts: Vec<AnyElement> = stack
        .into_iter()
        .enumerate()
        .rev()
        .map(|(ix, toast)| {
            let offset = ((count - 1 - ix) as f32) * 6.;
            let border_col = match toast.tone {
                Tone::Amber => ca(tk.attention, 0.4),
                Tone::Neutral => line(cx, t::LINE_CONTROL_STRONG),
            };
            div()
                .id(toast.id)
                .absolute()
                .bottom(px(offset))
                .right(px(0.))
                .min_w(px(240.))
                .max_w(px(360.))
                .p(px(10.))
                .rounded(px(t::RADIUS_CARD))
                .bg(c(tk.bg_input))
                .border_1()
                .border_color(border_col)
                .shadow(crate::theme::modal_shadow(cx))
                .text_size(px(t::TEXT_SMALL))
                .text_color(c(tk.text_primary))
                .child(toast.message)
                .into_any_element()
        })
        .collect();

    div()
        .id("notices-stack")
        .absolute()
        .bottom(px(24.))
        .right(px(24.))
        .children(toasts)
        .into_any_element()
}

#[cfg(test)]
mod legibility {
    /// No view sets a literal text size under the minimum (`TEXT_MIN`): a size that small is not read. Sizes that
    /// scale with the zoom go through a clamp, so only literals in `text_size(px(N))` are checked.
    fn sources() -> Vec<std::path::PathBuf> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        files
    }

    /// No emoji anywhere in the window's source (`app/assets/brand/README.md`): the marks and the icon set are the
    /// pictures. Test data that needs one writes it as an escape.
    #[test]
    fn no_view_draws_an_emoji() {
        let mut found = Vec::new();
        for file in sources() {
            for (n, line) in std::fs::read_to_string(&file).unwrap().lines().enumerate() {
                if line.chars().any(|c| matches!(c, '\u{1F300}'..='\u{1FAFF}' | '\u{26A1}' | '\u{2B50}' | '\u{2705}' | '\u{274C}')) {
                    found.push(format!("{}:{}", file.display(), n + 1));
                }
            }
        }
        assert!(found.is_empty(), "emoji in:\n{}", found.join("\n"));
    }

    #[test]
    fn no_view_sets_text_under_the_minimum() {
        let files = sources();
        let mut small = Vec::new();
        for file in files.iter().filter(|f| !f.ends_with("ui.rs")) {
            let source = std::fs::read_to_string(file).unwrap();
            for (n, line) in source.lines().enumerate() {
                let mut rest = line;
                while let Some(at) = rest.find("text_size(px(") {
                    rest = &rest[at + "text_size(px(".len()..];
                    let number: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
                    if number.parse::<f32>().is_ok_and(|v| v < cordata_theme::tokens::TEXT_MIN) {
                        small.push(format!("{}:{} text_size(px({number}))", file.display(), n + 1));
                    }
                }
            }
        }
        assert!(small.is_empty(), "text under {}px:\n{}", cordata_theme::tokens::TEXT_MIN, small.join("\n"));
    }
}
