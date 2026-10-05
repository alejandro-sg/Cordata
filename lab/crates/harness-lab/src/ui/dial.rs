//! The radial Laya dial: one agent's destructiveness threshold, as a needle on an arc.
//!
//! The drawer used to offer four flat preset buttons (`0.30 | 0.50 | 0.65 | 0.80`). A threshold is a
//! continuous thing, so it is drawn as one: a 240° arc from [`DIAL_MIN`] to [`DIAL_MAX`], open at the
//! bottom, with the needle at the agent's own dial. The `[` and `]` keys and `set_dial`'s own clamp
//! are unchanged — this is a second, finer way to the same value, not a new value.
//!
//! The angle arithmetic is pure and unit-tested; the drawing is not.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{Bounds, IntoElement, Pixels, Point, Window, canvas, div, fill, point, prelude::*, px, size};

use super::kit::mono;
use crate::theme::{self, Theme};

/// The dial's range. The daemon clamps to the same range on its side (and to the gate at 0.80).
pub const DIAL_MIN: f64 = 0.30;
pub const DIAL_MAX: f64 = 0.80;
/// The arc runs from 150° to 390° (30°): 240° of travel with the opening at the bottom, which is how
/// a physical dial is laid out. Angles are screen angles — y grows downward.
const START_DEG: f32 = 150.0;
const SWEEP_DEG: f32 = 240.0;
/// The dial's box, and the radius the needle and the arc are drawn at inside it.
pub const DIAL_SIZE: f32 = 96.0;
const RADIUS: f32 = 38.0;

/// Where a value sits on the arc, in screen degrees.
pub fn dial_angle(value: f64) -> f32 {
    let fraction = (value.clamp(DIAL_MIN, DIAL_MAX) - DIAL_MIN) / (DIAL_MAX - DIAL_MIN);
    START_DEG + fraction as f32 * SWEEP_DEG
}

/// The value an angle means. An angle inside the opening (the gap at the bottom) snaps to the end it
/// is nearer, so a drag that wanders off the end stays at the end rather than flipping to the other.
pub fn dial_value_from_angle(angle_deg: f32) -> f64 {
    let mut relative = (angle_deg - START_DEG) % 360.0;
    if relative < 0.0 {
        relative += 360.0;
    }
    let fraction = if relative <= SWEEP_DEG {
        relative / SWEEP_DEG
    } else if relative <= SWEEP_DEG + (360.0 - SWEEP_DEG) / 2.0 {
        1.0
    } else {
        0.0
    };
    let value = DIAL_MIN + fraction as f64 * (DIAL_MAX - DIAL_MIN);
    (value * 100.0).round() / 100.0
}

/// The value under the pointer, given the dial's centre. This is the whole of the drag: one `atan2`.
pub fn dial_value_at(pointer: Point<Pixels>, centre: Point<Pixels>) -> f64 {
    let dx = f32::from(pointer.x - centre.x);
    let dy = f32::from(pointer.y - centre.y);
    dial_value_from_angle(dy.atan2(dx).to_degrees())
}

/// A point on the needle, `fraction` of the way out from the centre.
pub fn dial_point(value: f64, centre: Point<Pixels>, fraction: f32) -> Point<Pixels> {
    let angle = dial_angle(value).to_radians();
    point(centre.x + px(angle.cos() * RADIUS * fraction), centre.y + px(angle.sin() * RADIUS * fraction))
}

/// The dial. `dragging` only changes how the read-out looks — the value is already the live one.
pub fn laya_dial(pod: &harness_proto::v1::Pod, enabled: bool, dragging: bool, bounds_slot: &Rc<Cell<Option<Bounds<Pixels>>>>, theme: &Theme) -> impl IntoElement {
    let value = if pod.dial > 0.0 { pod.dial.clamp(DIAL_MIN, DIAL_MAX) } else { pod.dial_default.clamp(DIAL_MIN, DIAL_MAX) };
    let hue = if enabled { theme.warn } else { theme.faint };
    let track = Theme::tint(hue, 0.18);
    let angle = dial_angle(value);
    let theme = *theme;
    // Record where the dial was painted. A pointer position arrives in window coordinates and the
    // drag needs an angle about the dial's centre; the paint closure is the one place that knows it.
    let slot = bounds_slot.clone();

    // The arc is a ring of dots for the same reason the memory ring is: gpui 0.2.2 has no arc
    // primitive, and at this radius 60 dots of 4px are a continuous band.
    let arc = canvas(
        |bounds: Bounds<Pixels>, _w: &mut Window, _c: &mut gpui::App| bounds,
        move |bounds: Bounds<Pixels>, _: Bounds<Pixels>, window: &mut Window, _: &mut gpui::App| {
            slot.set(Some(bounds));
            let centre = point(bounds.origin.x + bounds.size.width / 2.0, bounds.origin.y + bounds.size.height / 2.0);
            const DOTS: usize = 60;
            for i in 0..DOTS {
                let at = START_DEG + SWEEP_DEG * (i as f32 + 0.5) / DOTS as f32;
                let lit = at <= angle;
                let rad = at.to_radians();
                let dot = point(centre.x + px(rad.cos() * RADIUS), centre.y + px(rad.sin() * RADIUS));
                window.paint_quad(fill(Bounds { origin: point(dot.x - px(2.), dot.y - px(2.)), size: size(px(4.), px(4.)) }, if lit { hue } else { track }).corner_radii(px(2.)));
            }
            // The needle: dots along the ray, so it needs no rotation.
            for step in 0..=8 {
                let at = dial_point(value, centre, step as f32 / 8.0);
                let dot = point(at.x - px(1.5), at.y - px(1.5));
                window.paint_quad(fill(Bounds { origin: dot, size: size(px(3.), px(3.)) }, if enabled { theme.text } else { theme.dim }));
            }
            // The hub.
            window.paint_quad(fill(Bounds { origin: point(centre.x - px(4.), centre.y - px(4.)), size: size(px(8.), px(8.)) }, theme.card).corner_radii(px(4.)));
        },
    )
    .size_full();

    div()
        .debug_selector(|| "laya-dial".to_owned())
        .relative()
        .flex_none()
        .size(px(DIAL_SIZE))
        .child(arc)
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(mono(format!("{value:.2}"), if enabled { theme::threat_color(Some(value), &theme) } else { theme.faint }))
                .child(div().text_size(px(9.)).text_color(if dragging { theme.warn } else { theme.faint }).child(if dragging { "release to set" } else { "drag, or [ ]" })),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_and_its_angle_agree() {
        for value in [DIAL_MIN, 0.5, 0.65, DIAL_MAX] {
            assert!((dial_value_from_angle(dial_angle(value)) - value).abs() < 0.011, "{value} did not survive a round trip");
        }
        assert_eq!(dial_angle(DIAL_MIN), START_DEG);
        assert_eq!(dial_angle(DIAL_MAX), START_DEG + SWEEP_DEG);
        assert_eq!(dial_angle(0.0), START_DEG, "below the range clamps to the low end");
        assert_eq!(dial_angle(2.0), START_DEG + SWEEP_DEG, "…and above it to the high end");
    }

    #[test]
    fn the_opening_at_the_bottom_snaps_to_the_nearer_end() {
        // Straight down is the middle of the opening, past the high end.
        assert_eq!(dial_value_from_angle(90.0), DIAL_MAX);
        assert_eq!(dial_value_from_angle(60.0), DIAL_MAX, "just past the end is still the end");
        // The other side of the opening is nearer the low end.
        assert_eq!(dial_value_from_angle(140.0), DIAL_MIN);
        assert_eq!(dial_value_from_angle(-180.0), dial_value_from_angle(180.0), "and angles wrap rather than escaping the range");
        for angle in [-720.0, 0.0, 45.0, 170.0, 359.0, 1080.0] {
            let value = dial_value_from_angle(angle);
            assert!((DIAL_MIN..=DIAL_MAX).contains(&value), "{angle}° gave {value}, outside the range");
        }
    }

    #[test]
    fn a_pointer_drag_reads_the_angle_from_the_centre() {
        let centre = point(px(100.), px(100.));
        // Squarely right of the centre is on the far side of the sweep from the start.
        let right = dial_value_at(point(px(200.), px(100.)), centre);
        assert!((right - 0.74).abs() < 0.011, "expected 0.74, got {right}");
        // Down is the opening: the nearest end.
        assert_eq!(dial_value_at(point(px(100.), px(300.)), centre), DIAL_MAX);
        // The needle at a value points at that value's angle.
        for value in [DIAL_MIN, 0.55, DIAL_MAX] {
            let at = dial_point(value, centre, 1.0);
            assert!((dial_value_at(at, centre) - value).abs() < 0.02, "the needle at {value} reads back as {}", dial_value_at(at, centre));
        }
        assert_eq!(dial_point(DIAL_MIN, centre, 0.0), centre, "the needle starts at the hub");
    }
}
