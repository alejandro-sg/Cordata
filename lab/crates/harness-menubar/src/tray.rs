//! The status item: what it draws, and what its clicks mean.
//!
//! Deliberately dumb. The *state* comes from [`harness_client::deck`], derived from the same model
//! the Lab draws; this module only turns that into an icon, swaps it when it changes, and hands
//! clicks back to the caller as plain values.
//!
//! Two decisions worth stating, because both are about not lying to a person:
//!
//! * **The idle icon is templated and the other two are not.** A template image is tinted by the
//!   system, so the idle glyph is legible on a light menu bar and a dark one; a *meaningful* colour
//!   (cyan for work, amber for "needs you") has to be its own pixels or the menu bar would tint the
//!   meaning away.
//! * **The badge is a number in the tooltip, not a number drawn on the icon.** At menu-bar size a
//!   digit is illegible and, on a busy day, becomes noise a person learns to ignore — the opposite
//!   of what the number is for.
//!
//! The glyphs are drawn here rather than loaded from files: a 22-point circle is a dozen lines of
//! arithmetic, and an app with no bundle should not have to find an image inside one.

use harness_client::deck::TrayState;
use harness_lab::theme::Theme;
use tray_icon::{Icon, Rect, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// The menu bar draws at 22 points; providing 2× makes it sharp on a retina display.
const GLYPH: u32 = 44;

/// A click on the status item, in the terms the deck needs: which button, and where the icon is —
/// which is the anchor the deck is positioned against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrayClick {
    pub rect: Rect,
}

/// A glyph for one state, as RGBA. Pure, so the shape of each state is testable without a menu bar.
///
/// The shapes differ, not just the colours — an accessible reading of the menu bar (and a
/// monochrome screenshot) must still tell the three states apart.
pub fn glyph(state: TrayState) -> Vec<u8> {
    let theme = Theme::dark();
    // A thin ring, a filled disc, and a filled disc with a bar cut through it — three shapes, so
    // the states survive a monochrome menu bar and a greyscale screenshot.
    let (color, ring, bar) = match state {
        TrayState::Idle => (theme.dim, true, false),
        TrayState::Executing => (theme.accent, false, false),
        TrayState::ActionRequired => (theme.warn, false, true),
    };
    let mut rgba = vec![0u8; (GLYPH * GLYPH * 4) as usize];
    let center = GLYPH as f32 / 2.0;
    let outer = if ring { GLYPH as f32 * 0.30 } else { GLYPH as f32 * 0.36 };
    let inner = GLYPH as f32 * 0.24;
    let ink = (channel(color.r), channel(color.g), channel(color.b));
    for y in 0..GLYPH {
        for x in 0..GLYPH {
            let (dx, dy) = (x as f32 + 0.5 - center, y as f32 + 0.5 - center);
            let distance = (dx * dx + dy * dy).sqrt();
            // Antialiased edges: a hard-edged circle at 22 pt looks like a square with its corners
            // bitten off.
            let coverage = (outer + 0.5 - distance).clamp(0.0, 1.0);
            let hollow = if ring { (inner - distance).clamp(0.0, 1.0) } else { 1.0 };
            let alpha = coverage * hollow;
            if alpha <= 0.0 {
                continue;
            }
            // The bar and the dot under it are the page colour, so they read as cut out of the disc.
            let barred = bar && dx.abs() < 3.0 && ((dy > -13.0 && dy < -3.0) || (dy > 4.0 && dy < 11.0));
            let (r, g, b) = if barred { (0x08, 0x0b, 0x10) } else { ink };
            let index = ((y * GLYPH + x) * 4) as usize;
            rgba[index] = r;
            rgba[index + 1] = g;
            rgba[index + 2] = b;
            rgba[index + 3] = (alpha.clamp(0.0, 1.0) * 255.0) as u8;
        }
    }
    rgba
}

/// `Rgba`'s channels are 0..1 floats; tray-icon wants bytes.
fn channel(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// A templated state is one the system may tint; a state whose colour *is* the message is not.
pub fn is_templated(state: TrayState) -> bool {
    matches!(state, TrayState::Idle)
}

/// The status item, held for the life of the app. Dropping it removes the icon, so it lives in a
/// field of whatever owns the app — never in a local.
pub struct Tray {
    icon: TrayIcon,
    shown: Option<TrayState>,
}

impl Tray {
    /// Create the status item. Must be called on the main thread, with the app already up.
    pub fn install() -> Result<Self, String> {
        // The idle glyph is the templated one: it is the state that is on screen almost all the
        // time, and templating is what keeps it legible on a light menu bar and a dark one.
        let icon = TrayIconBuilder::new()
            .with_icon_templated(icon_for(TrayState::Idle)?)
            .with_tooltip("cordata")
            .build()
            .map_err(|error| format!("the status item could not be created: {error}"))?;
        Ok(Self { icon, shown: Some(TrayState::Idle) })
    }

    /// Show a state, if it is not the one already shown. Rebuilding the icon every tick would make
    /// the menu bar flicker for no reason, so the last state is remembered here.
    pub fn show(&mut self, state: TrayState, waiting: usize) -> Result<bool, String> {
        let tooltip = if waiting > 0 {
            format!("cordata — {} ({} waiting)", state.label(), waiting)
        } else {
            format!("cordata — {}", state.label())
        };
        let changed = self.shown != Some(state);
        if changed {
            let redrawn = if is_templated(state) {
                self.icon.set_icon_templated(Some(icon_for(state)?))
            } else {
                self.icon.set_icon(Some(icon_for(state)?))
            };
            redrawn.map_err(|error| format!("the status item could not be redrawn: {error}"))?;
            self.shown = Some(state);
        }
        // The tooltip carries the count even when the state is unchanged: going from one waiting
        // agent to two is news, and the icon alone cannot say it.
        let _ = self.icon.set_tooltip(Some(tooltip));
        Ok(changed)
    }

    /// The state being shown, so the caller can skip a redraw it does not need.
    pub fn shown_state(&self) -> Option<TrayState> {
        self.shown
    }

    /// The status item's own rectangle in screen coordinates, for a hotkey press that has no click
    /// to anchor to. `None` when the platform will not say — the caller then falls back to the
    /// display's corner rather than not opening at all.
    pub fn icon_rect(&self) -> Option<Rect> {
        self.icon.rect()
    }

    /// Drain clicks that arrived since the last look. Polling rather than a callback: the
    /// event-handler API is built around a winit event loop this app does not have, and polling
    /// keeps the tray out of the render path entirely.
    pub fn clicks(&self) -> Vec<TrayClick> {
        let receiver = TrayIconEvent::receiver();
        let mut clicks = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let TrayIconEvent::Click { rect, button: tray_icon::MouseButton::Left, button_state: tray_icon::MouseButtonState::Up, .. } = event {
                clicks.push(TrayClick { rect });
            }
        }
        clicks
    }
}

/// Holds the status item for the life of the app. A `Global` is how a GPUI app keeps something
/// that is not owned by any window; a local would drop the tray and remove the icon.
pub struct TrayHandle(pub Tray);

impl gpui::Global for TrayHandle {}

fn icon_for(state: TrayState) -> Result<Icon, String> {
    Icon::from_rgba(glyph(state), GLYPH, GLYPH).map_err(|error| format!("the {0} glyph is malformed: {error}", state.label()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_has_a_glyph_of_the_right_shape() {
        for state in [TrayState::Idle, TrayState::Executing, TrayState::ActionRequired] {
            let pixels = glyph(state);
            assert_eq!(pixels.len(), (GLYPH * GLYPH * 4) as usize, "{state:?} glyph is the wrong size");
            assert!(pixels.chunks(4).any(|p| p[3] > 200), "{state:?} glyph is invisible");
            // Nothing outside the circle: a square would say the icon was drawn wrong, not meant.
            assert_eq!(pixels[3], 0, "{state:?} glyph has an opaque corner");
        }
    }

    #[test]
    fn the_states_are_told_apart_without_colour() {
        let idle = glyph(TrayState::Idle);
        let executing = glyph(TrayState::Executing);
        let asking = glyph(TrayState::ActionRequired);
        let opaque = |pixels: &[u8]| pixels.chunks(4).filter(|p| p[3] > 128).count();
        // Idle is a ring, so it is mostly empty; the other two are discs, so they are mostly full.
        assert!(opaque(&idle) < opaque(&executing) / 2, "idle ({}) must read as a ring next to executing ({})", opaque(&idle), opaque(&executing));
        // The asking state differs from the working one by the bar cut out of it.
        assert_ne!(executing, asking, "the two filled states must not be the same pixels");
        // The bar is near-black pixels inside the disc — present in one, absent in the other.
        let dark = |pixels: &[u8]| pixels.chunks(4).filter(|p| p[3] > 200 && p[0] < 40 && p[1] < 40 && p[2] < 40).count();
        assert!(dark(&asking) > dark(&executing), "the asking glyph must have the darker bar drawn through it ({} vs {})", dark(&asking), dark(&executing));
    }

    #[test]
    fn only_the_idle_glyph_may_be_tinted_by_the_system() {
        assert!(is_templated(TrayState::Idle), "a templated idle glyph stays legible on either menu bar");
        assert!(!is_templated(TrayState::Executing), "cyan means work; the system must not tint it away");
        assert!(!is_templated(TrayState::ActionRequired), "amber means someone is waiting");
    }
}
