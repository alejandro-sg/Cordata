//! Motion. Nothing in the window moves on a timer: every movement is started by something that happened, so a thread
//! that has gone quiet looks quiet (`app/assets/brand/README.md`, "Motion"). There are four movements, each with one
//! meaning, and no loop (a fifth, `flow`, is the same idea for orchestrated runs: see `FLOW`):
//!
//! | movement | started by | says |
//! |---|---|---|
//! | beat | an event from the agent | this is alive; how bright it is says how recently |
//! | tug | events on the thread you are looking at | work is passing along the cord |
//! | arrive | a gate opening | the agent has come to you |
//! | settle | a new block in the transcript | this is what just came in |
//!
//! All of it is off under macOS Reduce Motion: the still picture carries the same state by shape and colour.

use std::cell::Cell;
use std::time::{Duration, Instant};

use gpui::{Animation, AnimationExt as _, AnyElement, App, ElementId, IntoElement, SharedString, Styled};

/// How long a beat takes to fall back to rest.
pub const BEAT: Duration = Duration::from_millis(900);
/// What a live thing rests at between events. An event lifts it to 1.
pub const BEAT_REST: f32 = cordata_theme::tokens::LIVE_REST;
/// One pulse along the header's cord.
pub const TUG: Duration = Duration::from_millis(640);
/// The pulse's length, as a share of the cord.
pub const TUG_LEN: f32 = 0.18;
/// The node coming to the gate, and the gate's rule drawing.
pub const ARRIVE: Duration = Duration::from_millis(240);
/// A new block coming to rest.
pub const SETTLE: Duration = Duration::from_millis(200);
/// How far a new block rises, in px.
pub const SETTLE_RISE: f32 = 6.;

/// The cord drawing from a finished step to the step a poll just saw start.
pub const FLOW: Duration = Duration::from_millis(480);

/// Whether the window may move: off when macOS Reduce Motion is on. Every movement asks this first.
pub fn on(cx: &App) -> bool {
    !cx.try_global::<crate::platform::Display>().is_some_and(|d| d.reduce_motion)
}

/// An animation that starts over when `seq` changes and otherwise holds its last frame.
fn keyed(id: impl Into<ElementId>, seq: u64) -> ElementId {
    ElementId::NamedInteger(SharedString::from(format!("{:?}", id.into())), seq)
}

/// A beat: `el` is lifted to full strength when `seq` changes and falls back to `BEAT_REST`. Events arriving faster
/// than it falls keep it lit, so it never blinks; when they stop, it dims and stays dim.
pub fn beat<E: Styled + IntoElement + 'static>(el: E, id: impl Into<ElementId>, seq: u64, cx: &App) -> AnyElement {
    if !on(cx) {
        return el.into_any_element();
    }
    el.with_animation(keyed(id, seq), Animation::new(BEAT).with_easing(gpui::ease_out_quint()), |el, t| el.opacity(1. - (1. - BEAT_REST) * t)).into_any_element()
}

/// One pass of `el` (a `TUG_LEN`-wide segment, absolutely placed) from the cord's left end off its right, started
/// when `seq` changes.
pub fn tug<E: Styled + IntoElement + 'static>(el: E, id: impl Into<ElementId>, seq: u64) -> AnyElement {
    el.with_animation(keyed(id, seq), Animation::new(TUG).with_easing(gpui::ease_in_out), |el, t| el.left(gpui::relative(-TUG_LEN + (1. + TUG_LEN) * t))).into_any_element()
}

/// One run of `shape` from 0 to 1, started when `seq` changes and held at its last frame after. For the cord
/// between two orchestrated steps, which is drawn in three straight pieces that fill one after the other.
pub fn flow<E: IntoElement + 'static>(el: E, id: impl Into<ElementId>, seq: u64, shape: impl Fn(E, f32) -> E + 'static) -> AnyElement {
    el.with_animation(keyed(id, seq), Animation::new(FLOW).with_easing(gpui::ease_in_out), shape).into_any_element()
}

/// How much of a piece is drawn when `t` of the whole path is: the piece starts `before` along a path `total` long
/// and is `len` long itself. Pieces fill in order, each only once the one before is whole.
pub fn piece(t: f32, before: f32, len: f32, total: f32) -> f32 {
    if len <= 0.0 || total <= 0.0 {
        return 0.0;
    }
    ((t * total - before) / len).clamp(0.0, 1.0)
}

/// Turns a stream of events into tugs: a new one starts only when the last has run its length, so a burst of events
/// is one clean pass and not a stutter.
#[derive(Debug, Default)]
pub struct Tugs {
    seen: Cell<u64>,
    seq: Cell<u64>,
    started: Cell<Option<Instant>>,
}

impl Tugs {
    /// The tug to show for a thread at `version` (its event count) at `now`.
    pub fn at(&self, version: u64, now: Instant) -> u64 {
        if version != self.seen.replace(version) && self.started.get().is_none_or(|s| now.duration_since(s) >= TUG) {
            self.seq.set(self.seq.get() + 1);
            self.started.set(Some(now));
        }
        self.seq.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tug_starts_on_an_event_and_never_without_one() {
        let (tugs, t0) = (Tugs::default(), Instant::now());
        let first = tugs.at(1, t0);
        assert_eq!(first, 1, "the first event tugs");
        assert_eq!(tugs.at(1, t0 + TUG * 10), first, "no event, no tug, however long it has been");
    }

    #[test]
    fn a_burst_is_one_tug_and_the_next_waits_for_it_to_finish() {
        let (tugs, t0) = (Tugs::default(), Instant::now());
        let first = tugs.at(1, t0);
        assert_eq!(tugs.at(2, t0 + TUG / 4), first);
        assert_eq!(tugs.at(3, t0 + TUG / 2), first);
        assert_eq!(tugs.at(4, t0 + TUG), first + 1, "an event after the pass ended starts the next");
    }

    #[test]
    fn the_pieces_of_a_cord_fill_in_order_and_end_whole() {
        let (a, b, c) = (10.0, 30.0, 10.0);
        let total = a + b + c;
        assert_eq!((piece(0.0, 0.0, a, total), piece(0.0, a, b, total), piece(0.0, a + b, c, total)), (0.0, 0.0, 0.0));
        assert_eq!(piece(0.2, 0.0, a, total), 1.0, "the first piece is whole");
        assert_eq!(piece(0.2, a, b, total), 0.0, "before the second starts");
        assert!((piece(0.5, a, b, total) - 0.5).abs() < 1e-5);
        assert_eq!(piece(0.5, a + b, c, total), 0.0);
        assert_eq!((piece(1.0, 0.0, a, total), piece(1.0, a, b, total), piece(1.0, a + b, c, total)), (1.0, 1.0, 1.0));
        assert_eq!(piece(0.5, 0.0, 0.0, total), 0.0, "a piece with no length draws nothing");
    }

    #[test]
    fn a_beat_rests_dim_but_visible() {
        const { assert!(BEAT_REST >= 0.6 && BEAT_REST <= 0.8) };
    }
}
