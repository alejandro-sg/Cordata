//! Cordata's colours — one source for the terminal and the native app (docs/app.md §4b).
//!
//! A Blade Runner 2049 night: sodium-vapour amber against cold electric teal on near-black. Every colour is a *role*,
//! so a widget asks for "the accent" or "untrusted content", never for a hex value, and both front ends change
//! together. The terminal maps its semantic ANSI names onto these roles (`tui/.../theme.rs`); the app reads them as
//! GPUI colours (`Rgb::to_u32`). No dependencies: this crate is data and arithmetic.
//!
//! The rules the tests hold the palette to:
//!
//! * every text role is WCAG AA (≥ 4.5:1) on every ground it is drawn on; the dim role, which only ever carries
//!   secondary text, is AA as well — a palette that is only beautiful at a distance is not one to work in;
//! * the high-contrast palette (macOS "Increase Contrast") is AAA (≥ 7:1) for the main text roles;
//! * colour is never the only signal (a mode is also its name, a risk also its word) — that is the widgets' half of
//!   the bargain, and why roles have names.

/// A colour as 8-bit sRGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn hex(value: u32) -> Self {
        Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }

    /// `0xRRGGBB`, what GPUI's `rgb()` takes.
    pub const fn to_u32(self) -> u32 {
        ((self.0 as u32) << 16) | ((self.1 as u32) << 8) | self.2 as u32
    }

    pub const fn tuple(self) -> (u8, u8, u8) {
        (self.0, self.1, self.2)
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f64 {
        let channel = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.039_28 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(self.0) + 0.7152 * channel(self.1) + 0.0722 * channel(self.2)
    }

    /// WCAG contrast ratio between two colours (1.0 – 21.0).
    pub fn contrast(self, other: Rgb) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// `self` laid over `ground` at `alpha` (0–1): what a translucent fill looks like once drawn.
    pub fn over(self, ground: Rgb, alpha: f64) -> Rgb {
        let mix = |a: u8, b: u8| (f64::from(a) * alpha + f64::from(b) * (1.0 - alpha)).round().clamp(0.0, 255.0) as u8;
        Rgb(mix(self.0, ground.0), mix(self.1, ground.1), mix(self.2, ground.2))
    }

    pub fn css(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }
}

/// What a colour is *for*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The one colour that means "Cordata": the prompt, the live thing, build mode. Sodium amber.
    Accent,
    /// The other voice: agents, the orchestrator, delegated and untrusted content. Electric teal.
    Agent,
    /// Needs a look: approvals waiting, edit mode, warnings. Pale gold.
    Attention,
    /// Done, safe, plan mode. Cold mint.
    Ok,
    /// Danger: auto mode, failures, destructive actions. Vermilion.
    Danger,
    /// Information, links, the steel of the HUD line-work.
    Info,
    /// Body text.
    Text,
    /// Secondary text: labels, metadata.
    TextSecondary,
    /// The dimmest text that is still text: timestamps, hints.
    TextDim,
    /// The window's ground.
    Ground,
    /// Panels and cards on the ground.
    Surface,
    /// A card on a panel, a popover.
    Raised,
    /// Hairline frames and separators.
    Hairline,
    /// Selected text and rows.
    Selection,
}

impl Role {
    pub const ALL: [Role; 14] = [
        Role::Accent,
        Role::Agent,
        Role::Attention,
        Role::Ok,
        Role::Danger,
        Role::Info,
        Role::Text,
        Role::TextSecondary,
        Role::TextDim,
        Role::Ground,
        Role::Surface,
        Role::Raised,
        Role::Hairline,
        Role::Selection,
    ];

    /// Roles that carry text (or a glyph that must be read).
    pub const INK: [Role; 9] = [Role::Accent, Role::Agent, Role::Attention, Role::Ok, Role::Danger, Role::Info, Role::Text, Role::TextSecondary, Role::TextDim];

    /// Roles text is drawn on.
    pub const GROUNDS: [Role; 3] = [Role::Ground, Role::Surface, Role::Raised];
}

/// A full set of role colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub accent: Rgb,
    pub agent: Rgb,
    pub attention: Rgb,
    pub ok: Rgb,
    pub danger: Rgb,
    pub info: Rgb,
    pub text: Rgb,
    pub text_secondary: Rgb,
    pub text_dim: Rgb,
    pub ground: Rgb,
    pub surface: Rgb,
    pub raised: Rgb,
    pub hairline: Rgb,
    pub selection: Rgb,
}

/// The palette.
pub const NIGHT: Palette = Palette {
    accent: Rgb::hex(0xFF9E3D),
    agent: Rgb::hex(0x48D6E0),
    attention: Rgb::hex(0xFFD682),
    ok: Rgb::hex(0x76DEBA),
    danger: Rgb::hex(0xFF604A),
    info: Rgb::hex(0x74A0D6),
    text: Rgb::hex(0xEEF1F5),
    text_secondary: Rgb::hex(0xAAB4BE),
    text_dim: Rgb::hex(0x7D8A99),
    ground: Rgb::hex(0x07090C),
    surface: Rgb::hex(0x0D1117),
    raised: Rgb::hex(0x151B23),
    hairline: Rgb::hex(0x232B36),
    selection: Rgb::hex(0x3A2A14),
};

/// The palette for macOS "Increase Contrast": brighter ink, a blacker ground, visible hairlines.
pub const NIGHT_HIGH_CONTRAST: Palette = Palette {
    accent: Rgb::hex(0xFFB066),
    agent: Rgb::hex(0x7DE8F0),
    attention: Rgb::hex(0xFFE3A6),
    ok: Rgb::hex(0x9BEFD0),
    danger: Rgb::hex(0xFF8A78),
    info: Rgb::hex(0xA3C4EC),
    text: Rgb::hex(0xFFFFFF),
    text_secondary: Rgb::hex(0xD5DCE3),
    text_dim: Rgb::hex(0xB3BEC9),
    ground: Rgb::hex(0x000000),
    surface: Rgb::hex(0x06080B),
    raised: Rgb::hex(0x0E1218),
    hairline: Rgb::hex(0x5A6878),
    selection: Rgb::hex(0x5A3F1C),
};

impl Palette {
    pub const fn get(&self, role: Role) -> Rgb {
        match role {
            Role::Accent => self.accent,
            Role::Agent => self.agent,
            Role::Attention => self.attention,
            Role::Ok => self.ok,
            Role::Danger => self.danger,
            Role::Info => self.info,
            Role::Text => self.text,
            Role::TextSecondary => self.text_secondary,
            Role::TextDim => self.text_dim,
            Role::Ground => self.ground,
            Role::Surface => self.surface,
            Role::Raised => self.raised,
            Role::Hairline => self.hairline,
            Role::Selection => self.selection,
        }
    }

    /// The palette to use given the system's accessibility flags.
    pub const fn for_contrast(increase_contrast: bool) -> &'static Palette {
        if increase_contrast { &NIGHT_HIGH_CONTRAST } else { &NIGHT }
    }
}

/// The colour of a session mode (`plan` · `build` · `edit` · `auto`), as a role. Never shown without the mode's name.
pub fn mode_role(mode: &str) -> Role {
    match mode {
        "plan" => Role::Ok,
        "build" => Role::Accent,
        "edit" => Role::Attention,
        "auto" => Role::Danger,
        _ => Role::TextSecondary,
    }
}

/// The colour of a Laya risk band (`low` · `medium` · `high`), as a role.
pub fn risk_role(band: &str) -> Role {
    match band {
        "high" => Role::Danger,
        "medium" | "elevated" => Role::Attention,
        "low" => Role::Ok,
        _ => Role::TextSecondary,
    }
}

/// The type the app ships, by name (bundled, OFL; docs/app.md §4b). Front ends without them fall back to their own.
pub mod fonts {
    /// Monospace: code, the HUD, tracked caps.
    pub const MONO: &str = "JetBrains Mono";
    /// Grotesk: body text and labels.
    pub const SANS: &str = "Inter";
}

/// Letter-spaced capitals, the HUD's voice: `CORDATA` → `C O R D A T A`.
pub fn tracked(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for (index, ch) in text.chars().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.extend(ch.to_uppercase());
    }
    out
}

/// A segmented bar for a fraction (`▰▰▰▱▱`), the HUD's gauge.
pub fn segbar(fraction: f64, cells: usize) -> String {
    let lit = ((fraction.clamp(0.0, 1.0) * cells as f64).round() as usize).min(cells);
    "▰".repeat(lit) + &"▱".repeat(cells - lit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit(palette: &Palette, text_min: f64, dim_min: f64) -> Vec<String> {
        let mut failures = Vec::new();
        for ground in Role::GROUNDS {
            for ink in Role::INK {
                let ratio = palette.get(ink).contrast(palette.get(ground));
                let min = if matches!(ink, Role::TextDim | Role::TextSecondary) { dim_min } else { text_min };
                if ratio < min {
                    failures.push(format!("{ink:?} on {ground:?}: {ratio:.2} < {min}"));
                }
            }
        }
        failures
    }

    #[test]
    fn every_text_role_is_wcag_aa_on_every_ground() {
        let failures = audit(&NIGHT, 4.5, 4.5);
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn the_high_contrast_palette_is_aaa_for_main_text_and_aa_everywhere() {
        let failures = audit(&NIGHT_HIGH_CONTRAST, 7.0, 4.5);
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn selected_text_stays_readable() {
        for palette in [&NIGHT, &NIGHT_HIGH_CONTRAST] {
            let ratio = palette.text.contrast(palette.selection);
            assert!(ratio >= 4.5, "text on selection {ratio:.2}");
        }
    }

    #[test]
    fn hairlines_are_visible_but_quiet() {
        let ratio = NIGHT.hairline.contrast(NIGHT.ground);
        assert!((1.3..3.0).contains(&ratio), "a hairline is seen, not read ({ratio:.2})");
        assert!(NIGHT_HIGH_CONTRAST.hairline.contrast(NIGHT_HIGH_CONTRAST.ground) >= 3.0, "with Increase Contrast it meets the 3:1 for UI parts");
    }

    #[test]
    fn the_accent_and_the_agent_colour_are_told_apart() {
        // Amber vs teal is the palette's one distinction that carries meaning (ours vs delegated/untrusted).
        let (a, t) = (NIGHT.accent, NIGHT.agent);
        let hue_gap = |x: Rgb, y: Rgb| (i32::from(x.0) - i32::from(y.0)).abs() + (i32::from(x.2) - i32::from(y.2)).abs();
        assert!(hue_gap(a, t) > 300, "amber and teal are far apart in red and blue");
    }

    #[test]
    fn modes_and_risks_map_to_roles() {
        assert_eq!(mode_role("plan"), Role::Ok);
        assert_eq!(mode_role("build"), Role::Accent);
        assert_eq!(mode_role("edit"), Role::Attention);
        assert_eq!(mode_role("auto"), Role::Danger);
        assert_eq!(risk_role("high"), Role::Danger);
        assert_eq!(NIGHT.get(Role::Accent).css(), "#FF9E3D");
        assert_eq!(Rgb::hex(0xFF9E3D).to_u32(), 0xFF9E3D);
    }

    #[test]
    fn hud_vocabulary() {
        assert_eq!(tracked("Cordata"), "C O R D A T A");
        assert_eq!(segbar(0.5, 4), "▰▰▱▱");
        assert_eq!(segbar(2.0, 3), "▰▰▰");
    }
}
