//! The two instruments: the Laya threat gauge and the unified-memory map.
//!
//! The threat gauge is a real painted element — a `canvas` that draws 50 lit/unlit segments, a
//! needle at the live score and a marker on the router's own 0.80 gate — rather than a styled box.
//! With no reading it paints an empty track and no needle: never a green 0.0, which would read as clearance.

use gpui::{Bounds, IntoElement, Pixels, RenderOnce, Window, canvas, div, fill, point, prelude::*, px, relative, size};
use harness_proto::v1::OmlxState;

use super::kit::{self, mono};
use crate::theme::{self, Theme};

/// Below this much free model memory the window says so on its own.
pub const MEMORY_ALARM_GB: f64 = 6.0;

const SEGMENTS: usize = 50;
const GAP: f32 = 2.0;

/// Which hue a segment at `fraction` (0..1) is painted in when lit.
pub fn segment_color(fraction: f64, gate: f64, theme: &Theme) -> gpui::Rgba {
    if fraction >= gate {
        theme.danger
    } else if fraction >= 0.5 {
        theme.warn
    } else {
        theme.ok
    }
}

/// How many of `SEGMENTS` are lit for `score`. A score of exactly 1.0 lights all of them, any
/// reading above zero lights at least one, and no reading lights none.
pub fn lit_segments(score: Option<f64>) -> usize {
    match score {
        None => 0,
        Some(s) if s <= 0.0 => 0,
        Some(s) => ((s.clamp(0.0, 1.0) * SEGMENTS as f64).round() as usize).clamp(1, SEGMENTS),
    }
}

#[derive(IntoElement)]
pub struct ThreatGauge {
    pub score: Option<f64>,
    pub gate: f64,
    pub theme: Theme,
    /// Just the painted track — for the banner, where the number is printed beside it.
    pub compact: bool,
}

impl RenderOnce for ThreatGauge {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let Self { score, gate, theme, compact } = self;
        let lit = lit_segments(score);
        let value = score.map(|s| s.clamp(0.0, 1.0) as f32);
        let needle_color = theme.text;
        let track = canvas(
            |bounds: Bounds<Pixels>, _window: &mut Window, _cx: &mut gpui::App| bounds,
            move |bounds: Bounds<Pixels>, _state: Bounds<Pixels>, window: &mut Window, _cx: &mut gpui::App| {
                let width = f32::from(bounds.size.width);
                let seg_w = ((width - GAP * (SEGMENTS as f32 - 1.0)) / SEGMENTS as f32).max(1.0);
                let top = bounds.origin.y + px(8.);
                for i in 0..SEGMENTS {
                    let x = bounds.origin.x + px(i as f32 * (seg_w + GAP));
                    let cell = Bounds { origin: point(x, top), size: size(px(seg_w), px(16.)) };
                    let fraction = (i as f64 + 0.5) / SEGMENTS as f64;
                    let color = if i < lit { segment_color(fraction, gate, &theme) } else { Theme::tint(segment_color(fraction, gate, &theme), 0.13) };
                    window.paint_quad(fill(cell, color).corner_radii(px(1.5)));
                }
                // The router's own destructive gate: a hairline through the track, always drawn.
                let gate_x = bounds.origin.x + px(width * gate as f32);
                window.paint_quad(fill(Bounds { origin: point(gate_x - px(0.5), bounds.origin.y), size: size(px(1.5), px(32.)) }, theme.danger));
                // The needle: where the live reading sits.
                if let Some(v) = value {
                    let x = bounds.origin.x + px((width - 2.0) * v);
                    window.paint_quad(fill(Bounds { origin: point(x, bounds.origin.y + px(2.)), size: size(px(2.), px(28.)) }, needle_color));
                }
            },
        )
        .h(px(32.))
        .w_full();

        if compact {
            return div().debug_selector(|| "threat-gauge".to_owned()).child(track).into_any_element();
        }
        div()
            .debug_selector(|| "threat-gauge".to_owned())
            .flex()
            .flex_col()
            .gap_1()
            .child(track)
            .child(
                div()
                    .relative()
                    .h(px(14.))
                    .w_full()
                    .child(mono("0.0", theme.faint).absolute().left_0())
                    .child(mono("0.5", theme.faint).absolute().left(relative(0.5)))
                    .child(mono(format!("GATE {gate:.2}"), theme.danger).absolute().right(relative(1.0 - gate as f32)))
                    .child(mono("1.0", theme.faint).absolute().right_0().mr(px(-0.))),
            )
            .into_any_element()
    }
}

/// The colours the memory map cycles through for resident models.
pub fn model_color(index: usize, theme: &Theme) -> gpui::Rgba {
    [theme.accent, theme.violet, theme.ok, theme.warn][index % 4]
}

/// What each resident thing holds, as fractions of the budget: oMLX models in order, then whatever
/// else is resident (Ollama), then nothing — the rest of the bar is free. Pure, so it is tested.
pub fn memory_segments(omlx: &OmlxState) -> Vec<(String, f64, usize)> {
    let budget = omlx.budget_gb.max(0.001);
    let mut out = Vec::new();
    let mut held = 0.0;
    for (index, model) in omlx.models.iter().filter(|m| m.loaded).enumerate() {
        out.push((model.id.clone(), (model.size_gb / budget).clamp(0.0, 1.0), index));
        held += model.size_gb;
    }
    let other = (omlx.used_gb - held).max(0.0);
    if other > 0.05 {
        out.push(("other residents".to_owned(), (other / budget).clamp(0.0, 1.0), usize::MAX));
    }
    // Never wider than the bar, whatever the arithmetic upstream said.
    let total: f64 = out.iter().map(|s| s.1).sum();
    if total > 1.0 {
        for segment in &mut out {
            segment.1 /= total;
        }
    }
    out
}

/// The unified-memory bar: one segment per resident model against the harness's budget.
pub fn memory_bar(omlx: &OmlxState, theme: &Theme) -> gpui::Div {
    let danger = omlx.free_gb < MEMORY_ALARM_GB;
    let mut bar = div()
        .debug_selector(|| "memory-bar".to_owned())
        .flex()
        .flex_row()
        .h(px(14.))
        .w_full()
        .rounded(px(4.))
        .overflow_hidden()
        .bg(theme.card)
        .border_1()
        .border_color(if danger { theme.danger } else { theme.border });
    for (_, fraction, index) in memory_segments(omlx) {
        let color = if index == usize::MAX { theme.dim } else { model_color(index, theme) };
        bar = bar.child(div().h_full().w(relative(fraction as f32)).bg(color).border_r_1().border_color(theme.bg));
    }
    bar
}

/// The unified-memory ring: the same data as `memory_bar`, wound into a circle — one arc per
/// resident model against the harness's budget, with the free GB in the middle.
///
/// It is painted as a ring of dots rather than with arc geometry: gpui 0.2.2 has no arc primitive on
/// `PathBuilder`, and 96 dots of 4px overlap into a continuous band. The track behind them carries
/// the alarm: under [`MEMORY_ALARM_GB`] free, the unlit dots go crimson.
pub fn memory_ring(omlx: &OmlxState, theme: &Theme, hovered: bool) -> gpui::Div {
    const DOTS: usize = 96;
    const RING: f32 = 78.0;
    let segments = memory_segments(omlx);
    let danger = omlx.free_gb < MEMORY_ALARM_GB;
    let track = if danger { Theme::tint(theme.danger, 0.4) } else { theme.card };
    let free = free_color(omlx.free_gb, theme);

    // Which arc each dot belongs to, walking the segments across the circle once. `usize::MAX` is
    // the unlit track. The first dot of a new arc is left out, which is the gap between segments.
    let mut dots: Vec<(usize, gpui::Rgba)> = Vec::with_capacity(DOTS);
    for i in 0..DOTS {
        let at = (i as f64 + 0.5) / DOTS as f64;
        let mut start = 0.0;
        let mut hit = None;
        for (n, (_, fraction, index)) in segments.iter().enumerate() {
            if at >= start && at < start + fraction {
                hit = Some((n, *index));
                break;
            }
            start += fraction;
        }
        dots.push(match hit {
            Some((n, index)) => (n, if index == usize::MAX { theme.dim } else { model_color(index, theme) }),
            None => (usize::MAX, track),
        });
    }

    let ring = canvas(
        |bounds: Bounds<Pixels>, _w: &mut Window, _c: &mut gpui::App| bounds,
        move |bounds: Bounds<Pixels>, _: Bounds<Pixels>, window: &mut Window, _: &mut gpui::App| {
            let centre = point(bounds.origin.x + bounds.size.width / 2.0, bounds.origin.y + bounds.size.height / 2.0);
            let radius = f32::from(bounds.size.width) / 2.0 - 5.0;
            let mut previous = usize::MAX;
            for (i, (arc, color)) in dots.iter().enumerate() {
                let gap = *arc != previous;
                previous = *arc;
                if gap && i > 0 && *arc != usize::MAX {
                    continue;
                }
                let angle = i as f32 / DOTS as f32 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                let at = point(centre.x + px(angle.cos() * radius), centre.y + px(angle.sin() * radius));
                window.paint_quad(fill(Bounds { origin: point(at.x - px(2.), at.y - px(2.)), size: size(px(4.), px(4.)) }, *color).corner_radii(px(2.)));
            }
        },
    )
    .size_full();

    let middle = div().absolute().top_0().left_0().size_full().flex().items_center().justify_center().child(if hovered {
        kit::button("Unload", theme.danger, true, theme, "unload-memory-ring").into_any_element()
    } else {
        div().flex().flex_col().items_center().child(mono(format!("{:.0}", omlx.free_gb), free)).child(div().text_size(px(9.)).text_color(theme.faint).child("GB free")).into_any_element()
    });

    div()
        .debug_selector(|| "memory-ring".to_owned())
        .relative()
        .flex_none()
        .size(px(RING))
        .child(ring)
        .child(middle)
}

pub fn free_color(free_gb: f64, theme: &Theme) -> gpui::Rgba {
    if free_gb < MEMORY_ALARM_GB {
        theme.danger
    } else if free_gb < 12.0 {
        theme.warn
    } else {
        theme.ok
    }
}

/// `used / budget` as the headline line under the bar.
pub fn memory_summary(omlx: &OmlxState, theme: &Theme) -> gpui::Div {
    use gpui::FontWeight;
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_4()
        .child(kit::readout("budget", theme::human_gb(omlx.budget_gb), theme.dim, theme))
        .child(kit::readout("resident", theme::human_gb(omlx.used_gb), theme.text, theme))
        .child(kit::readout("free", theme::human_gb(omlx.free_gb), free_color(omlx.free_gb, theme), theme))
        .child(kit::readout("oMLX", if omlx.up { "up" } else { "down" }, if omlx.up { theme.ok } else { theme.danger }, theme).font_weight(FontWeight::MEDIUM))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_proto::v1::OmlxModel;

    #[test]
    fn no_reading_lights_nothing_and_a_reading_lights_something() {
        assert_eq!(lit_segments(None), 0);
        assert_eq!(lit_segments(Some(0.0)), 0);
        assert_eq!(lit_segments(Some(0.001)), 1, "any non-zero reading is visible");
        assert_eq!(lit_segments(Some(0.5)), 25);
        assert_eq!(lit_segments(Some(1.0)), SEGMENTS);
        assert_eq!(lit_segments(Some(7.0)), SEGMENTS, "an out-of-range score cannot overflow the track");
    }

    #[test]
    fn the_gate_is_where_the_red_starts() {
        let t = Theme::dark();
        assert_eq!(segment_color(0.2, 0.8, &t), t.ok);
        assert_eq!(segment_color(0.6, 0.8, &t), t.warn);
        assert_eq!(segment_color(0.8, 0.8, &t), t.danger);
        assert_eq!(segment_color(0.99, 0.8, &t), t.danger);
    }

    #[test]
    fn the_memory_bar_never_overflows() {
        let state = OmlxState {
            budget_gb: 38.0,
            used_gb: 60.0,
            free_gb: -22.0,
            models: vec![
                OmlxModel { id: "a".into(), loaded: true, size_gb: 30.0, pinned: false },
                OmlxModel { id: "b".into(), loaded: true, size_gb: 20.0, pinned: false },
                OmlxModel { id: "c".into(), loaded: false, size_gb: 99.0, pinned: false },
            ],
            ..Default::default()
        };
        let segments = memory_segments(&state);
        let total: f64 = segments.iter().map(|s| s.1).sum();
        assert!(total <= 1.0 + 1e-9, "the segments sum to {total}");
        assert_eq!(segments.len(), 3, "two loaded models and the 'other residents' remainder; the inactive one holds nothing");
        assert!(segments.iter().all(|s| s.0 != "c"));
    }

    #[test]
    fn an_empty_pool_draws_an_empty_bar() {
        assert!(memory_segments(&OmlxState { budget_gb: 38.0, ..Default::default() }).is_empty());
    }
}
