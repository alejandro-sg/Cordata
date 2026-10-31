//! The native app's design tokens (the "Cordata Harness" reference design): every colour, size, radius, type size and
//! tracking a view may use. Views never write a literal — they ask for a token here, through `cordata-app`'s
//! `theme` module. Four themes share one shape: three dark ones (`2049`, `graphite`, `dim`) with the same accents, and
//! `light` (`DAY`) — warm paper, charcoal ink, the accents darkened until they hold WCAG AA on paper (AAA with Increase
//! Contrast). `system` in the settings picks `DAY` or `2049` from the macOS appearance (cordata-app's `theme`).
//!
//! Lines are white at an alpha (the `LINE_*` constants), tints are an accent at an alpha (`AMBER_*`, `TEAL_*`): both
//! are drawn translucent over whatever ground they sit on, so one value works on every surface.

use crate::{Palette, Rgb};

/// The colours of one theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    pub name: &'static str,
    /// Window / outermost.
    pub bg_window: Rgb,
    /// The conversation column.
    pub bg_main: Rgb,
    /// Sidebar, side panel, status bar.
    pub bg_panel: Rgb,
    /// The settings navigation.
    pub bg_nav_deep: Rgb,
    /// Inputs, popovers, the palette, the composer.
    pub bg_input: Rgb,
    /// Titles, active items.
    pub text_primary: Rgb,
    /// Message body.
    pub text_body: Rgb,
    /// Secondary values.
    pub text_secondary: Rgb,
    /// Hints, inactive tabs.
    pub text_muted: Rgb,
    /// The quieter hint.
    pub text_muted_2: Rgb,
    /// Section labels.
    pub text_label: Rgb,
    /// Shortcuts, meta.
    pub text_faint: Rgb,
    /// The faintest meta.
    pub text_faint_2: Rgb,
    /// Primary accent, focus, "needs you".
    pub amber: Rgb,
    pub amber_hover: Rgb,
    /// Inline code, commands.
    pub amber_soft: Rgb,
    /// Shortcut text on amber, the logo's brackets.
    pub amber_dim: Rgb,
    pub amber_dim_2: Rgb,
    /// Text on an amber fill.
    pub amber_on_fill: Rgb,
    /// Healthy / done / trusted / connected.
    pub teal: Rgb,
    /// Destructive actions.
    pub danger: Rgb,
    /// The steel of HUD line-work (gauges, links in older panes). Not in the reference; kept for the panes it predates.
    pub info: Rgb,
    /// What `LINE_*` and `HOVER_*` are drawn in.
    pub ink: Rgb,
    /// A dark ground (white lines, light text) or a light one (charcoal lines, dark text).
    pub is_dark: bool,
    /// What a modal's or a popover's drop shadow is drawn in — never the window ground, which is near-white in `light`.
    pub shadow: Rgb,
}

pub const T2049: Tokens = Tokens {
    name: "2049",
    bg_window: Rgb::hex(0x050607),
    bg_main: Rgb::hex(0x07080A),
    bg_panel: Rgb::hex(0x0A0B0D),
    bg_nav_deep: Rgb::hex(0x08090B),
    bg_input: Rgb::hex(0x0D0E11),
    text_primary: Rgb::hex(0xECE7DE),
    text_body: Rgb::hex(0xD9D3C8),
    text_secondary: Rgb::hex(0xA9A49B),
    text_muted: Rgb::hex(0x8A8780),
    text_muted_2: Rgb::hex(0x7A766F),
    text_label: Rgb::hex(0x6B6862),
    text_faint: Rgb::hex(0x5F5C56),
    text_faint_2: Rgb::hex(0x4F4C47),
    amber: Rgb::hex(0xE8A254),
    amber_hover: Rgb::hex(0xF0B46E),
    amber_soft: Rgb::hex(0xF3C38A),
    amber_dim: Rgb::hex(0x9C7445),
    amber_dim_2: Rgb::hex(0x7A5A36),
    amber_on_fill: Rgb::hex(0x1A1208),
    teal: Rgb::hex(0x5FB3B3),
    danger: Rgb::hex(0xE56050),
    info: Rgb::hex(0x7F9BB8),
    ink: Rgb::hex(0xFFFFFF),
    is_dark: true,
    shadow: Rgb::hex(0x000000),
};

/// Neutral greys instead of the warm near-black; same accents.
pub const GRAPHITE: Tokens = Tokens {
    name: "graphite",
    bg_window: Rgb::hex(0x0B0C0E),
    bg_main: Rgb::hex(0x101113),
    bg_panel: Rgb::hex(0x131416),
    bg_nav_deep: Rgb::hex(0x0F1012),
    bg_input: Rgb::hex(0x17181B),
    text_primary: Rgb::hex(0xEAEAEA),
    text_body: Rgb::hex(0xD6D6D6),
    text_secondary: Rgb::hex(0xA8A8A8),
    text_muted: Rgb::hex(0x8E8E8E),
    text_muted_2: Rgb::hex(0x7C7C7C),
    text_label: Rgb::hex(0x6E6E6E),
    text_faint: Rgb::hex(0x626262),
    text_faint_2: Rgb::hex(0x515151),
    ..T2049
};

/// A lifted ground for bright rooms; brighter muted text so it stays readable on it.
pub const DIM: Tokens = Tokens {
    name: "dim",
    bg_window: Rgb::hex(0x121417),
    bg_main: Rgb::hex(0x15171A),
    bg_panel: Rgb::hex(0x191B1F),
    bg_nav_deep: Rgb::hex(0x16181B),
    bg_input: Rgb::hex(0x1E2025),
    text_primary: Rgb::hex(0xF0EBE2),
    text_body: Rgb::hex(0xDED8CD),
    text_secondary: Rgb::hex(0xB3AEA5),
    text_muted: Rgb::hex(0x9A968E),
    text_muted_2: Rgb::hex(0x8A867F),
    text_label: Rgb::hex(0x7A766F),
    text_faint: Rgb::hex(0x6E6A64),
    text_faint_2: Rgb::hex(0x5E5A55),
    ..T2049
};

/// Warm paper and charcoal ink for daylight and macOS Light appearance. Every accent is darkened to hold WCAG AA (4.5:1)
/// on every ground, and text on an amber fill holds AAA; `high_contrast` lifts the text toward the ink (darker), so
/// Increase Contrast reaches AAA (7:1) — the tests below compute both.
pub const DAY: Tokens = Tokens {
    name: "light",
    bg_window: Rgb::hex(0xECE8E1),
    bg_main: Rgb::hex(0xF7F6F2),
    bg_panel: Rgb::hex(0xEFECE6),
    bg_nav_deep: Rgb::hex(0xE6E2DA),
    bg_input: Rgb::hex(0xFFFFFF),
    text_primary: Rgb::hex(0x16181D),
    text_body: Rgb::hex(0x262930),
    text_secondary: Rgb::hex(0x474A52),
    text_muted: Rgb::hex(0x585B62),
    text_muted_2: Rgb::hex(0x63666C),
    text_label: Rgb::hex(0x6A6C72),
    text_faint: Rgb::hex(0x76787D),
    text_faint_2: Rgb::hex(0x8A8C90),
    amber: Rgb::hex(0x7F470B),
    amber_hover: Rgb::hex(0x8F520F),
    amber_soft: Rgb::hex(0x6E3D08),
    amber_dim: Rgb::hex(0xA87A4A),
    amber_dim_2: Rgb::hex(0xC29A70),
    amber_on_fill: Rgb::hex(0xFFFFFF),
    teal: Rgb::hex(0x0D6B73),
    danger: Rgb::hex(0xA8231B),
    info: Rgb::hex(0x3B5878),
    ink: Rgb::hex(0x16181D),
    is_dark: false,
    shadow: Rgb::hex(0x2A2620),
};

pub const THEMES: [&Tokens; 4] = [&T2049, &GRAPHITE, &DIM, &DAY];

impl Tokens {
    /// A theme by its settings name (`day` and `light` are the same theme; `night` is `2049`); unknown names get
    /// `2049`. `system` is resolved by the app against the OS appearance before it gets here (`for_appearance`).
    pub fn named(name: &str) -> &'static Tokens {
        match name {
            "day" | "light" => &DAY,
            "night" => &T2049,
            _ => THEMES.iter().copied().find(|t| t.name == name).unwrap_or(&T2049),
        }
    }

    /// The theme a `system` preference resolves to: `DAY` under a light appearance, `2049` under a dark one.
    pub fn for_appearance(dark: bool) -> &'static Tokens {
        if dark { &T2049 } else { &DAY }
    }

    /// macOS "Increase Contrast": every text step brighter, so labels and meta read as text.
    pub fn high_contrast(self) -> Tokens {
        let lift = |c: Rgb, by: f64| self.ink.over(c, by);
        Tokens {
            text_primary: lift(self.text_primary, 0.6),
            text_body: lift(self.text_body, 0.5),
            text_secondary: lift(self.text_secondary, 0.4),
            text_muted: lift(self.text_muted, 0.35),
            text_muted_2: lift(self.text_muted_2, 0.35),
            text_label: lift(self.text_label, 0.35),
            text_faint: lift(self.text_faint, 0.35),
            text_faint_2: lift(self.text_faint_2, 0.35),
            ..self
        }
    }

    /// The role palette the older panes (editor, review, terminal, …) and the component library read, from these
    /// tokens, so every pane changes with the theme.
    pub fn palette(&self) -> Palette {
        Palette {
            accent: self.amber,
            agent: self.teal,
            attention: self.amber,
            ok: self.teal,
            danger: self.danger,
            info: self.info,
            text: self.text_primary,
            text_secondary: self.text_secondary,
            text_dim: self.text_muted,
            ground: self.bg_main,
            surface: self.bg_panel,
            raised: self.bg_input,
            hairline: self.ink.over(self.bg_panel, LINE_HAIRLINE_OPAQUE),
            selection: self.amber.over(self.bg_main, 0.22),
        }
    }
}

// ------------------------------------------------------------------------------------------------ alphas

/// Row dividers.
pub const LINE_SUBTLE: f32 = 0.06;
/// Column borders, headers.
pub const LINE_DEFAULT: f32 = 0.08;
/// Chip and button borders.
pub const LINE_CONTROL: f32 = 0.10;
pub const LINE_CONTROL_STRONG: f32 = 0.14;
/// What an opaque hairline is mixed at (the role palette has no alpha).
pub const LINE_HAIRLINE_OPAQUE: f64 = 0.13;
/// Faint fills: cards, the user's bubble.
pub const FILL_FAINT: f32 = 0.03;
/// Hover on rows and ghost buttons.
pub const HOVER: f32 = 0.045;
pub const HOVER_STRONG: f32 = 0.07;

pub const AMBER_FILL_FAINT: f32 = 0.04;
pub const AMBER_FILL: f32 = 0.05;
pub const AMBER_FILL_MID: f32 = 0.08;
pub const AMBER_FILL_STRONG: f32 = 0.12;
pub const AMBER_FILL_HOVER: f32 = 0.16;
pub const AMBER_BORDER_FAINT: f32 = 0.14;
pub const AMBER_BORDER: f32 = 0.25;
pub const AMBER_BORDER_STRONG: f32 = 0.35;
pub const AMBER_GLOW: f32 = 0.7;
pub const TEAL_FILL: f32 = 0.08;
pub const TEAL_BORDER: f32 = 0.25;
pub const DANGER_FILL: f32 = 0.08;
pub const DANGER_BORDER: f32 = 0.35;

/// The modal scrim, over `bg_window`.
pub const SCRIM: f32 = 0.65;
pub const MODAL_SHADOW: f32 = 0.65;

/// The haze: amber from the bottom-left, teal from the top-right.
pub const HAZE_AMBER: (u32, f32) = (0xC46E28, 0.10);
pub const HAZE_TEAL: (u32, f32) = (0x468C96, 0.07);

// ------------------------------------------------------------------------------------------------ metrics (px)

pub const HEADER_H: f32 = 52.;
pub const STATUS_H: f32 = 28.;
pub const SIDEBAR_W: f32 = 256.;
pub const PANEL_MIN_W: f32 = 300.;
pub const PANEL_MAX_W: f32 = 380.;
/// The side panel's share of the window, clamped to `PANEL_MIN_W..=PANEL_MAX_W`.
pub const PANEL_VW: f32 = 0.28;
/// Editor and Review need room for code: the same clamp, wider.
pub const PANEL_WIDE_MIN_W: f32 = 380.;
pub const PANEL_WIDE_MAX_W: f32 = 560.;
pub const PANEL_WIDE_VW: f32 = 0.38;
pub const CONTENT_MAX_W: f32 = 768.;
pub const PALETTE_W: f32 = 580.;
/// The palette's distance from the top, as a share of the window height.
pub const PALETTE_TOP_VH: f32 = 0.14;
pub const PICKER_W: f32 = 368.;
pub const SETTINGS_W: f32 = 960.;
pub const SETTINGS_H: f32 = 680.;
pub const SETTINGS_NAV_W: f32 = 220.;
pub const SETTINGS_HEADER_H: f32 = 56.;
pub const SEND_BUTTON: f32 = 32.;
pub const SWITCH_W: f32 = 34.;
pub const SWITCH_H: f32 = 20.;
pub const SELECT_MIN_W: f32 = 150.;
pub const CONTEXT_BAR_W: f32 = 80.;
pub const CONTEXT_BAR_H: f32 = 3.;
pub const DOT: f32 = 6.;
pub const DOT_SMALL: f32 = 5.;
pub const DIAMOND: f32 = 6.;
pub const GLOW_BLUR: f32 = 8.;
pub const COMPOSER_RING: f32 = 4.;
pub const GUTTER: f32 = 16.;
pub const ROW_PAD_Y: f32 = 14.;

/// Window size rules.
pub const MIN_WINDOW_W: f32 = 900.;
pub const MIN_WINDOW_H: f32 = 640.;
/// Under this width the side panel starts collapsed (until the person chooses).
pub const PANEL_AUTO_COLLAPSE_W: f32 = 1100.;
/// Under this width the terminal tab moves into the overflow menu.
pub const TERMINAL_OVERFLOW_W: f32 = 1240.;
/// Conversation column widths under which the header drops the branch chip, then shortens the model chip.
pub const HEADER_HIDE_BRANCH_W: f32 = 560.;
pub const HEADER_SHORT_MODEL_W: f32 = 480.;

pub const RADIUS_BADGE: f32 = 4.;
pub const RADIUS_ROW: f32 = 6.;
pub const RADIUS_CONTROL: f32 = 6.;
pub const RADIUS_CARD: f32 = 8.;
pub const RADIUS_POPOVER: f32 = 10.;
pub const RADIUS_MODAL: f32 = 12.;
pub const RADIUS_CAP: f32 = 4.;

// ------------------------------------------------------------------------------------------------ type (px)

pub const TEXT_BODY: f32 = 14.;
pub const LINE_HEIGHT_BODY: f32 = 1.6;
pub const TEXT_TITLE: f32 = 13.5;
pub const TEXT_UI: f32 = 13.;
pub const TEXT_SMALL: f32 = 12.;
pub const TEXT_META: f32 = 11.;
pub const TEXT_STATUS: f32 = 10.5;
pub const TEXT_LABEL: f32 = 10.;
pub const TEXT_TAG: f32 = 9.5;
pub const TEXT_SETTINGS_TITLE: f32 = 15.;
pub const TEXT_EMPTY: f32 = 22.;
pub const TEXT_LOGO: f32 = 12.;

/// Letter-spacing, as GPUI has none: thin spaces between the letters (`tracked_text`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracking {
    /// 0.16–0.18em: section labels, the status bar.
    Label,
    /// 0.32em: the logo.
    Logo,
}

/// `text` letter-spaced for `tracking`: U+2009 (≈0.2em) or U+2009+U+200A (≈0.3em) between characters; a space stays
/// a wider gap.
pub fn tracked_text(text: &str, tracking: Tracking, upper: bool) -> String {
    let gap = match tracking {
        Tracking::Label => "\u{2009}",
        Tracking::Logo => "\u{2009}\u{200A}",
    };
    let mut out = String::with_capacity(text.len() * 4);
    let mut previous: Option<char> = None;
    for ch in text.chars() {
        if previous.is_some_and(|p| p != ' ') && ch != ' ' {
            out.push_str(gap);
        }
        previous = Some(ch);
        if upper {
            out.extend(ch.to_uppercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Fonts. The app bundles Geist when its files are present (`app/assets/fonts`); otherwise the system's.
pub mod fonts {
    pub const SANS: &str = "Geist";
    pub const MONO: &str = "Geist Mono";
    pub const SANS_FALLBACK: &str = ".SystemUIFont";
    pub const MONO_FALLBACKS: [&str; 2] = ["SF Mono", "Menlo"];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_text_is_aa_on_every_ground_in_every_theme() {
        for theme in THEMES {
            for t in [*theme, theme.high_contrast()] {
                for ground in [t.bg_window, t.bg_main, t.bg_panel, t.bg_nav_deep, t.bg_input] {
                    for (name, ink) in [("primary", t.text_primary), ("body", t.text_body), ("secondary", t.text_secondary), ("muted", t.text_muted)] {
                        let ratio = ink.contrast(ground);
                        assert!(ratio >= 4.5, "{} {name} on {}: {ratio:.2}", t.name, ground.css());
                    }
                }
            }
        }
    }

    #[test]
    fn light_is_aa_everywhere_and_aaa_with_increase_contrast() {
        let grounds = |t: &Tokens| [t.bg_window, t.bg_main, t.bg_panel, t.bg_nav_deep, t.bg_input];
        for ground in grounds(&DAY) {
            for (name, accent) in [("amber", DAY.amber), ("teal", DAY.teal), ("danger", DAY.danger), ("info", DAY.info)] {
                let ratio = accent.contrast(ground);
                assert!(ratio >= 4.5, "light {name} on {}: {ratio:.2}", ground.css());
            }
        }
        let hc = DAY.high_contrast();
        for ground in grounds(&hc) {
            for (name, ink) in [("primary", hc.text_primary), ("body", hc.text_body), ("secondary", hc.text_secondary), ("muted", hc.text_muted)] {
                let ratio = ink.contrast(ground);
                assert!(ratio >= 7.0, "light high-contrast {name} on {}: {ratio:.2}", ground.css());
            }
        }
        assert!(DAY.amber_on_fill.contrast(DAY.amber) >= 7.0);
        assert!(!DAY.is_dark && THEMES.iter().filter(|t| t.is_dark).count() == 3);
    }

    #[test]
    fn a_shadow_is_darker_than_the_ground_it_falls_on() {
        for t in THEMES {
            assert!(t.shadow.luminance() < t.bg_window.luminance() || t.shadow == Rgb::hex(0), "{}: shadow {} on {}", t.name, t.shadow.css(), t.bg_window.css());
            assert!(t.shadow.contrast(t.bg_main) >= 3.0 || t.is_dark, "{}: the shadow must read on paper", t.name);
        }
        assert_eq!(Tokens::named("light").name, "light");
        assert_eq!(Tokens::named("day").name, "light");
        assert_eq!(Tokens::for_appearance(false).name, "light");
        assert_eq!(Tokens::for_appearance(true).name, "2049");
    }

    #[test]
    fn accents_read_on_their_grounds_and_on_fill() {
        for t in THEMES {
            assert!(t.amber.contrast(t.bg_main) >= 4.5, "{} amber", t.name);
            assert!(t.teal.contrast(t.bg_panel) >= 4.5, "{} teal", t.name);
            assert!(t.amber_on_fill.contrast(t.amber) >= 7.0, "{} text on amber", t.name);
        }
    }

    #[test]
    fn the_reference_values_are_the_2049_theme() {
        assert_eq!(T2049.amber.css(), "#E8A254");
        assert_eq!(T2049.bg_main.css(), "#07080A");
        assert_eq!(Tokens::named("graphite").name, "graphite");
        assert_eq!(Tokens::named("nonsense").name, "2049");
    }

    #[test]
    fn the_derived_palette_follows_the_tokens() {
        let p = T2049.palette();
        assert_eq!(p.accent, T2049.amber);
        assert_eq!(p.ground, T2049.bg_main);
        assert!(p.text.contrast(p.selection) >= 4.5);
    }

    #[test]
    fn tracking_spaces_letters_not_words() {
        assert_eq!(tracked_text("ab c", Tracking::Label, true), "A\u{2009}B C");
        assert_eq!(tracked_text("", Tracking::Logo, false), "");
    }
}
