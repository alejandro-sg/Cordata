//! The palette, and the pure mappings from daemon state to what gets drawn.
//!
//! Everything here is deterministic and I/O-free, so it is unit-tested directly (the drawing
//! code around it is not). The colours follow the TUI's Lab view (`tui/crates/acp-tui/src/ui/lab.rs`)
//! so the two front ends read the same: a pod is green when it is working, amber when it is
//! waiting on a human, red when it has been E-Stopped.

use gpui::{Rgba, rgb};

/// The window's colours. One instance, built once, copied by value into every element.
///
/// A control-room palette: near-black blue surfaces separated by hairlines, one cyan accent for
/// "this is live / selected", and the three traffic-light hues reserved for meaning — green is
/// working or safe, amber is waiting on a human or elevated, red is destructive or stopped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub bg: Rgba,
    pub panel: Rgba,
    pub card: Rgba,
    pub border: Rgba,
    pub line_strong: Rgba,
    pub text: Rgba,
    pub dim: Rgba,
    pub faint: Rgba,
    pub accent: Rgba,
    pub violet: Rgba,
    pub ok: Rgba,
    pub warn: Rgba,
    pub danger: Rgba,
    pub scrim: Rgba,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            bg: rgb(0x080b10),
            panel: rgb(0x0e131a),
            card: rgb(0x131a23),
            border: rgb(0x1d2631),
            line_strong: rgb(0x2a3544),
            text: rgb(0xe6edf5),
            dim: rgb(0x8fa0b5),
            faint: rgb(0x546276),
            accent: rgb(0x22d3ee),
            violet: rgb(0xa78bfa),
            ok: rgb(0x34d399),
            warn: rgb(0xfbbf24),
            danger: rgb(0xf87171),
            scrim: rgba(0x020408, 0.80),
        }
    }

    /// For a surface drawn over macOS's Liquid Glass: the fills go translucent so the glass is what
    /// is seen, and the hairlines go light, because a dark line on glass disappears. Text and the
    /// meaning colours are unchanged — glass is a material, not a new palette.
    pub fn glass(self) -> Self {
        // `bg` stays opaque: it is also the label colour of a filled button, and a transparent one
        // is a button with no words on it.
        Self {
            // Near-opaque: the glass shows at the edges and in the tint, never *through* the words —
            // at half opacity a terminal behind the deck reads as a second layer of text.
            panel: rgba(0x0e131a, 0.86),
            card: rgba(0x131a23, 0.92),
            border: rgba(0xffffff, 0.10),
            line_strong: rgba(0xffffff, 0.20),
            ..self
        }
    }

    /// For the same surface over gpui's own blur, where there is no glass (before macOS 26): more
    /// opaque fills and higher-contrast hairlines, rather than faking a refraction the platform
    /// does not draw.
    pub fn solid(self) -> Self {
        Self { panel: Self::tint(self.panel, 0.93), card: Self::tint(self.card, 0.97), line_strong: rgb(0x364457), ..self }
    }

    /// The same colour at a given opacity — the tinted fill behind a chip or a lit segment.
    pub fn tint(color: Rgba, alpha: f32) -> Rgba {
        Rgba { a: alpha, ..color }
    }
}

/// `rgb` with alpha, spelled once so the palette above reads at a glance.
pub const fn rgba(hex: u32, alpha: f32) -> Rgba {
    Rgba { r: ((hex >> 16) & 0xff) as f32 / 255.0, g: ((hex >> 8) & 0xff) as f32 / 255.0, b: (hex & 0xff) as f32 / 255.0, a: alpha }
}

/// A bare hex constant as an opaque colour — the form the role table uses.
pub const fn hex(value: u32) -> Rgba {
    rgba(value, 1.0)
}

/// The Laya destructiveness band a score falls in. Mirrors `backend/laya_risk.py`: the router's
/// own gate is 0.8, and anything at or above it is destructive enough to always ask.
pub fn band(score: f64) -> &'static str {
    if score >= 0.8 {
        "destructive"
    } else if score >= 0.5 {
        "elevated"
    } else {
        "safe"
    }
}

/// The colour of the gauge for a score. `None` is "Laya has not scored a turn" — a faint track,
/// never a green 0.0, which would read as clearance.
pub fn threat_color(score: Option<f64>, theme: &Theme) -> Rgba {
    match score {
        None => theme.faint,
        Some(score) if score >= 0.8 => theme.danger,
        Some(score) if score >= 0.5 => theme.warn,
        Some(_) => theme.ok,
    }
}

/// `0.42` — the gauge's numeric read-out, with `—` for "no reading".
pub fn score_text(score: Option<f64>) -> String {
    score.map(|s| format!("{s:.2}")).unwrap_or_else(|| "—".into())
}

/// A pod's state as the canvas draws it: the colour its border takes, and whether that border
/// breathes. `isolated` wins over `status`, because a pod E-stopped mid-loop reports its old status
/// until its next turn (the daemon's own caveat).
///
/// The hues are the ones the rest of the window already reserves for meaning — cyan is "live",
/// amber is "waiting on a human", crimson is "destructive or stopped" — so a pod's glow reads the
/// same as its chip and its ticks.
pub fn glow(pod: &harness_proto::v1::Pod, theme: &Theme) -> (Rgba, bool) {
    use harness_proto::v1::PodStatus;
    if pod.isolated {
        return (theme.danger, false);
    }
    match PodStatus::try_from(pod.status).unwrap_or_default() {
        PodStatus::Isolated => (theme.danger, false),
        PodStatus::Contained => (theme.warn, true),
        PodStatus::Synthesizing => (theme.accent, true),
        PodStatus::Executing => (theme.accent, false),
        PodStatus::Idle | PodStatus::Unspecified => (theme.border, false),
    }
}

/// Is a pod in a state that has something to show in a progress strip — i.e. is it working?
pub fn working(pod: &harness_proto::v1::Pod) -> bool {
    use harness_proto::v1::PodStatus;
    !pod.isolated && matches!(PodStatus::try_from(pod.status).unwrap_or_default(), PodStatus::Synthesizing | PodStatus::Executing)
}

/// Milliseconds as the cards show them: `820ms`, `3.4s`, `1m12s`.
pub fn human_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Gigabytes to one decimal, which is the precision the memory budget is decided at.
pub fn human_gb(gb: f64) -> String {
    format!("{gb:.1} GB")
}

/// Uptime as `3h04m`, `12m05s`, `42s`.
pub fn human_uptime(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3_600 {
        format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn glass_lets_the_material_through_and_the_fallback_does_not_pretend_to() {
        let dark = Theme::dark();
        let (glass, solid) = (dark.glass(), dark.solid());
        assert!(glass.panel.a < solid.panel.a && solid.panel.a < 1.0, "glass is the most translucent; the blur fallback is translucent but readable");
        assert!(glass.card.a < solid.card.a);
        assert_eq!((glass.text, glass.warn, glass.danger), (dark.text, dark.warn, dark.danger), "the meaning colours never change with the material");
        assert_eq!((solid.text, solid.accent), (dark.text, dark.accent));
        assert!(glass.border.r > 0.9 && glass.border.a < 0.2, "light hairlines on glass, where dark ones vanish");
        assert_eq!(glass.bg.a, 1.0, "bg is a filled button's label colour, so it never goes transparent");
    }

    use super::*;

    fn theme() -> Theme {
        Theme::dark()
    }

    #[test]
    fn the_bands_match_the_router_gate() {
        assert_eq!(band(0.0), "safe");
        assert_eq!(band(0.49), "safe");
        assert_eq!(band(0.5), "elevated");
        assert_eq!(band(0.79), "elevated");
        assert_eq!(band(0.8), "destructive");
        assert_eq!(band(1.0), "destructive");
    }

    #[test]
    fn no_reading_is_not_a_clearance() {
        let t = theme();
        assert_eq!(threat_color(None, &t), t.faint);
        assert_eq!(score_text(None), "—");
        assert_ne!(threat_color(None, &t), threat_color(Some(0.0), &t));
        assert_eq!(threat_color(Some(0.9), &t), t.danger);
    }

    #[test]
    fn durations_read_the_way_the_cards_need() {        assert_eq!(human_ms(820), "820ms");
        assert_eq!(human_ms(3_400), "3.4s");
        assert_eq!(human_ms(72_000), "1m12s");
        assert_eq!(human_uptime(42.0), "42s");
        assert_eq!(human_uptime(725.0), "12m05s");
        assert_eq!(human_uptime(11_040.0), "3h04m");
        assert_eq!(human_gb(8.41), "8.4 GB");
    }

    #[test]
    fn a_pod_glows_by_its_state_and_an_isolated_one_outranks_its_stale_status() {
        use harness_proto::v1::{Pod, PodStatus};
        let t = theme();
        let at = |status: PodStatus, isolated: bool| glow(&Pod { status: status as i32, isolated, ..Default::default() }, &t);
        assert_eq!(at(PodStatus::Idle, false), (t.border, false), "an idle pod is quiet");
        assert_eq!(at(PodStatus::Synthesizing, false), (t.accent, true), "thinking pulses cyan");
        assert_eq!(at(PodStatus::Executing, false), (t.accent, false), "running is solid cyan");
        assert_eq!(at(PodStatus::Contained, false), (t.warn, true), "waiting on a human pulses amber");
        assert_eq!(at(PodStatus::Isolated, false), (t.danger, false), "stopped is solid crimson");
        // The child only learns on its next loop turn, so `isolated` is authoritative.
        assert_eq!(at(PodStatus::Executing, true), (t.danger, false), "an E-stopped pod is crimson even before it notices");
        assert!(!working(&Pod { status: PodStatus::Idle as i32, ..Default::default() }));
        assert!(working(&Pod { status: PodStatus::Executing as i32, ..Default::default() }));
        assert!(!working(&Pod { status: PodStatus::Executing as i32, isolated: true, ..Default::default() }), "nothing is running inside a stopped pod");
    }
}
