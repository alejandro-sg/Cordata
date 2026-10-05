//! The small parts every panel is built from: captions, LEDs, chips, readouts, keycaps, buttons.
//!
//! Nothing here knows about the daemon. Each is a plain function returning an element, so a panel
//! reads as a sentence ("a panel titled X with a readout of Y and a button Z") and the look of the
//! whole window is decided in this one file.

use std::time::Duration;

use gpui::{Animation, AnimationExt, AnyElement, Div, ElementId, FontWeight, Rgba, SharedString, Stateful, div, prelude::*, pulsating_between, px};

use crate::theme::Theme;
use harness_client::Tone;

pub const MONO: &str = "Menlo";
pub const SANS: &str = "Helvetica Neue";

/// A tone (meaning) as a colour (hue).
pub fn tone_color(tone: Tone, theme: &Theme) -> Rgba {
    match tone {
        Tone::Info => theme.accent,
        Tone::Good => theme.ok,
        Tone::Warn => theme.warn,
        Tone::Bad => theme.danger,
        Tone::Dim => theme.faint,
    }
}

/// A tiny uppercase label: the "silkscreen" on a piece of lab equipment.
pub fn caption(value: &str, theme: &Theme) -> Div {
    div().text_size(px(10.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.faint).child(value.to_uppercase())
}

pub fn text(value: impl Into<SharedString>, color: Rgba) -> Div {
    div().text_xs().text_color(color).child(value.into())
}

pub fn mono(value: impl Into<SharedString>, color: Rgba) -> Div {
    div().font_family(MONO).text_xs().text_color(color).child(value.into())
}

/// A status light. `pulse` makes it breathe — used only for "something is happening or waiting",
/// so a still LED always means a settled state.
pub fn led(id: impl Into<ElementId>, color: Rgba, pulse: bool) -> AnyElement {
    let dot = div().flex_none().size(px(8.)).rounded_full().bg(color);
    if pulse {
        dot.with_animation(id, Animation::new(Duration::from_millis(1500)).repeat().with_easing(pulsating_between(0.25, 1.0)), |el, delta| el.opacity(delta)).into_any_element()
    } else {
        dot.into_any_element()
    }
}

/// A pill with a tinted fill: a status word, a band, a count.
pub fn chip(label: impl Into<SharedString>, color: Rgba) -> Div {
    div()
        .flex_none()
        .px_2()
        .py(px(1.))
        .rounded_full()
        .bg(Theme::tint(color, 0.12))
        .border_1()
        .border_color(Theme::tint(color, 0.45))
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(label.into())
}

/// A captioned number: `DAEMON` over `v1`.
pub fn readout(label: &str, value: impl Into<SharedString>, color: Rgba, theme: &Theme) -> Div {
    div().flex().flex_col().gap(px(1.)).child(caption(label, theme)).child(div().font_family(MONO).text_size(px(13.)).text_color(color).child(value.into()))
}

pub fn banner(message: impl Into<SharedString>, color: Rgba) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(Theme::tint(color, 0.10))
        .border_1()
        .border_color(Theme::tint(color, 0.5))
        .text_sm()
        .text_color(color)
        .child(message.into())
}

/// A key as printed on a keycap.
pub fn keycap(key: &str, theme: &Theme) -> Div {
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(4.))
        .bg(theme.card)
        .border_1()
        .border_color(theme.line_strong)
        .font_family(MONO)
        .text_size(px(10.))
        .text_color(theme.dim)
        .child(key.to_owned())
}

/// A click target. `filled` is the primary action in a group; the rest are outlined. The emergency
/// unload has no confirmation step on purpose: a dialog is exactly what fails when memory is the problem.
pub fn button(label: impl Into<SharedString>, color: Rgba, filled: bool, theme: &Theme, id: &'static str) -> Stateful<Div> {
    button_dyn(label, color, filled, theme, id.to_owned())
}

/// [`button`] with an id built at run time (one per tray card).
pub fn button_dyn(label: impl Into<SharedString>, color: Rgba, filled: bool, theme: &Theme, id: String) -> Stateful<Div> {
    let theme = *theme;
    let selector = format!("button-{id}");
    let base = div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .flex_none()
        .px_3()
        .py(px(5.))
        .rounded_md()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .border_1()
        .border_color(color);
    let base = if filled { base.bg(Theme::tint(color, 0.9)).text_color(theme.bg) } else { base.bg(Theme::tint(color, 0.08)).text_color(color) };
    base.hover(move |this| if filled { this.bg(color) } else { this.bg(Theme::tint(color, 0.22)) }).child(label.into())
}

/// One choice in a segmented control (`low | medium | high | max`).
pub fn segment(label: &str, active: bool, enabled: bool, color: Rgba, theme: &Theme, id: &'static str) -> Stateful<Div> {
    let theme = *theme;
    let (fg, bg, border) = if !enabled {
        (theme.faint, Theme::tint(theme.card, 0.5), theme.border)
    } else if active {
        (theme.bg, color, color)
    } else {
        (theme.dim, theme.card, theme.line_strong)
    };
    let seg = div()
        .id(SharedString::from(id))
        .debug_selector(move || format!("segment-{id}"))
        .flex_none()
        .px_3()
        .py(px(4.))
        .rounded(px(5.))
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .child(label.to_owned());
    if enabled && !active { seg.cursor_pointer().hover(move |this| this.border_color(color).text_color(color)) } else { seg }
}

/// One tab of the side column: a label, an optional count, an underline when it is the open one.
pub fn tab(label: &str, count: Option<usize>, active: bool, theme: &Theme, id: &'static str) -> Stateful<Div> {
    let theme = *theme;
    let t = div()
        .id(SharedString::from(id))
        .debug_selector(move || id.to_owned())
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .px_3()
        .py(px(6.))
        .text_sm()
        .font_weight(if active { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
        .text_color(if active { theme.text } else { theme.dim })
        .border_b_2()
        .border_color(if active { theme.accent } else { Theme::tint(theme.bg, 0.0) })
        .child(label.to_owned())
        .children(count.filter(|c| *c > 0).map(|c| chip(c.to_string(), if active { theme.accent } else { theme.faint })));
    if active { t } else { t.cursor_pointer().hover(move |this| this.text_color(theme.text)) }
}

/// `abcdef…` — one line, never longer than `max` characters.
pub fn truncate(value: &str, max: usize) -> String {
    let flat = value.replace('\n', " ");
    if flat.chars().count() <= max {
        flat
    } else {
        let head: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

pub fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Seconds ago, as a log column: `now`, `12s`, `4m`, `2h`.
pub fn ago(elapsed: Duration) -> String {
    let s = elapsed.as_secs();
    match s {
        0..=1 => "now".to_owned(),
        2..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h", s / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_never_grows_a_line() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("a\nb", 10), "a b");
        assert_eq!(truncate("abcdefghij", 5).chars().count(), 5);
        assert!(truncate("abcdefghij", 5).ends_with('…'));
    }

    #[test]
    fn a_log_column_reads_at_a_glance() {
        assert_eq!(ago(Duration::from_millis(300)), "now");
        assert_eq!(ago(Duration::from_secs(12)), "12s");
        assert_eq!(ago(Duration::from_secs(240)), "4m");
        assert_eq!(ago(Duration::from_secs(7300)), "2h");
    }
}
