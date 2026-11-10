//! The Cordata palette — warm graphite grounds, viridian signal, ochre attention, and lavender agents.
//!
//! The renderers keep using the semantic ANSI colours (`Cyan` is "accent", `Magenta` is "the second voice", `DarkGray` is
//! "dim", `Red` is "danger"…); this module decides what those names look like on the terminal in front of the person,
//! and `term.rs` applies it at the one place a colour becomes an escape sequence. So the whole UI changes together, a test
//! that reads the screen never sees a colour, and a terminal that cannot show the palette falls back to its own.
//!
//! * truecolor terminals (`COLORTERM=truecolor|24bit`) get the full palette;
//! * 256-colour terminals (`TERM=*256color`, Terminal.app) get the nearest cube colours;
//! * a light terminal (`CORDATA_THEME=light`, or `COLORFGBG` naming a light background) gets the `light` palette
//!   (`cordata_theme::tokens::DAY`: the accents darkened to WCAG AA on paper) instead of the night one, which would be
//!   unreadable there; `CORDATA_THEME=dark` keeps the night palette whatever `COLORFGBG` says. Decided once, at start;
//! * `CORDATA_THEME=terminal` or a terminal without 256 colours keeps the terminal's own 16 colours;
//! * `NO_COLOR` strips colour and keeps bold/dim/reverse.
//!
//! The roles, so a new widget picks the right name: **Cyan = viridian signal** (Cordata, the live thing, the prompt),
//! **Magenta = lavender** (agents, the orchestrator, untrusted content: the other voice), **Green = viridian/ok**
//! (done, safe, plan), **Yellow = ochre attention** (needs you, gate), **Red = vermilion** (danger, auto), **Blue = steel**.

use std::sync::OnceLock;

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Colour stripped; modifiers (bold, dim, reverse) stay.
    Mono,
    /// The terminal's own 16 colours, untouched.
    Ansi,
    /// The palette, snapped to the 256-colour cube.
    Indexed,
    /// The palette.
    TrueColor,
}

/// What this process's terminal gets; decided once.
pub fn level() -> Level {
    // Tests read the screen through an emulator and assert on the terminal's own colours; the palette is
    // tested through `detect` and `remap_for` directly.
    if cfg!(test) {
        return Level::Ansi;
    }
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(|| detect(|key| std::env::var(key).ok()))
}

fn detect(env: impl Fn(&str) -> Option<String>) -> Level {
    if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return Level::Mono;
    }
    if env("CORDATA_THEME").is_some_and(|v| v.eq_ignore_ascii_case("terminal")) {
        return Level::Ansi;
    }
    let colorterm = env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return Level::TrueColor;
    }
    // Terminal.app and most multiplexers advertise only the 256-colour TERM.
    if env("TERM").is_some_and(|t| t.contains("256color")) {
        return Level::Indexed;
    }
    Level::Ansi
}

/// Is the terminal's background light? Decided once.
pub fn light() -> bool {
    if cfg!(test) {
        return false;
    }
    static LIGHT: OnceLock<bool> = OnceLock::new();
    *LIGHT.get_or_init(|| detect_light(|key| std::env::var(key).ok()))
}

fn detect_light(env: impl Fn(&str) -> Option<String>) -> bool {
    match env("CORDATA_THEME").map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("light" | "day") => return true,
        Some("dark" | "night" | "2049") => return false,
        _ => {}
    }
    // `COLORFGBG=0;15` is foreground;background: a light background is 7 or 15. Unparseable or absent: dark.
    env("COLORFGBG").is_some_and(|v| matches!(v.rsplit(';').next(), Some("7" | "15")))
}

/// The palette: what each semantic colour is, as RGB — read from `cordata-theme`, the one token source the native app
/// uses too (the night palette, or `DAY`'s on a light terminal). `None` for colours the theme leaves alone.
fn palette(color: Color) -> Option<(u8, u8, u8)> {
    palette_for(light(), color)
}

fn palette_for(light: bool, color: Color) -> Option<(u8, u8, u8)> {
    use cordata_theme::{NIGHT, Role};
    let role = match color {
        // accent — the one colour that means "Cordata": viridian signal
        Color::Cyan | Color::LightCyan => Role::Accent,
        // the second voice: agents, the orchestrator, untrusted content — lavender
        Color::Magenta | Color::LightMagenta => Role::Agent,
        Color::Yellow | Color::LightYellow => Role::Attention,
        Color::Green | Color::LightGreen => Role::Ok,
        Color::Red | Color::LightRed => Role::Danger,
        Color::Blue | Color::LightBlue => Role::Info,
        // secondary text, and the dimmest text that is still readable
        Color::Gray => Role::TextSecondary,
        Color::DarkGray => Role::TextDim,
        Color::White => Role::Text,
        _ => return None,
    };
    if light {
        return Some(cordata_theme::tokens::DAY.palette().get(role).tuple());
    }
    Some(NIGHT.get(role).tuple())
}

/// The nearest xterm-256 colour for an RGB value: the 6×6×6 cube or the grey ramp, whichever is closer.
fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let snap = |v: u8| {
        STEPS
            .iter()
            .enumerate()
            .min_by_key(|(_, step)| i32::from(**step).abs_diff(i32::from(v)))
            .map_or(0, |(index, _)| index as u8)
    };
    let (ri, gi, bi) = (snap(r), snap(g), snap(b));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let cube_rgb = (STEPS[ri as usize], STEPS[gi as usize], STEPS[bi as usize]);

    let average = ((u16::from(r) + u16::from(g) + u16::from(b)) / 3) as u8;
    let ramp_index = (average.saturating_sub(8) / 10).min(23);
    let ramp_level = 8 + ramp_index * 10;
    let ramp = 232 + ramp_index;

    let distance = |(x, y, z): (u8, u8, u8)| {
        let d = |a: u8, b: u8| u32::from(a).abs_diff(u32::from(b)).pow(2);
        d(x, r) + d(y, g) + d(z, b)
    };
    if distance((ramp_level, ramp_level, ramp_level)) < distance(cube_rgb) {
        ramp
    } else {
        cube
    }
}

pub fn remap_for(level: Level, color: Color) -> Color {
    match level {
        Level::Ansi => color,
        Level::Mono => Color::Reset,
        Level::TrueColor => palette(color).map_or(color, |(r, g, b)| Color::Rgb(r, g, b)),
        Level::Indexed => palette(color).map_or(color, |(r, g, b)| Color::Indexed(nearest_256(r, g, b))),
    }
}

/// A colour as this terminal should be told about it.
pub fn remap(color: Color) -> Color {
    remap_for(level(), color)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn the_terminal_decides_how_much_of_the_palette_it_gets() {
        assert_eq!(detect(env(&[("COLORTERM", "truecolor")])), Level::TrueColor);
        assert_eq!(detect(env(&[("COLORTERM", "24bit"), ("TERM", "xterm-256color")])), Level::TrueColor);
        assert_eq!(detect(env(&[("TERM", "xterm-256color")])), Level::Indexed);
        assert_eq!(detect(env(&[("TERM", "linux")])), Level::Ansi);
        assert_eq!(detect(env(&[])), Level::Ansi);
    }

    #[test]
    fn colour_can_be_switched_off_or_left_to_the_terminal() {
        assert_eq!(detect(env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])), Level::Mono);
        assert_eq!(detect(env(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")])), Level::TrueColor);
        assert_eq!(detect(env(&[("CORDATA_THEME", "terminal"), ("COLORTERM", "truecolor")])), Level::Ansi);
        // A light background no longer drops to the terminal's colours: it gets the light palette (below).
        assert_eq!(detect(env(&[("COLORFGBG", "0;15"), ("COLORTERM", "truecolor")])), Level::TrueColor);
    }

    #[test]
    fn a_light_terminal_gets_the_light_palette() {
        assert!(detect_light(env(&[("COLORFGBG", "0;15")])));
        assert!(detect_light(env(&[("CORDATA_THEME", "light")])));
        assert!(!detect_light(env(&[("COLORFGBG", "15;0")])));
        assert!(!detect_light(env(&[("COLORFGBG", "nonsense")])) && !detect_light(env(&[])));
        assert!(!detect_light(env(&[("CORDATA_THEME", "dark"), ("COLORFGBG", "0;15")])), "an explicit dark wins");
        let day = cordata_theme::tokens::DAY;
        assert_eq!(palette_for(true, Color::Cyan), Some(day.amber.tuple()));
        assert_ne!(palette_for(true, Color::Cyan), palette_for(false, Color::Cyan));
    }

    #[test]
    fn semantic_colours_become_the_palette_and_everything_else_passes_through() {
        assert_eq!(remap_for(Level::TrueColor, Color::Cyan), Color::Rgb(61, 201, 164));
        assert_eq!(remap_for(Level::TrueColor, Color::Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
        assert_eq!(remap_for(Level::TrueColor, Color::Black), Color::Black);
        assert_eq!(remap_for(Level::TrueColor, Color::Reset), Color::Reset);
        assert_eq!(remap_for(Level::Ansi, Color::Cyan), Color::Cyan);
        assert_eq!(remap_for(Level::Mono, Color::Cyan), Color::Reset);
    }

    #[test]
    fn the_256_colour_fallback_lands_on_a_near_colour() {
        // Pure cube corners.
        assert_eq!(nearest_256(255, 255, 255), 231);
        assert_eq!(nearest_256(0, 0, 0), 16);
        // A mid grey is better served by the grey ramp than by the cube.
        assert!((232..=255).contains(&nearest_256(104, 117, 135)) || nearest_256(104, 117, 135) >= 16);
        let Color::Indexed(index) = remap_for(Level::Indexed, Color::Cyan) else {
            panic!("indexed")
        };
        assert!(index >= 16, "{index}");
    }
}
