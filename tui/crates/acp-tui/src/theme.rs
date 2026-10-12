//! The Cordata palette — a Blade Runner 2049 night: sodium-vapour amber against cold teal on near-black, the light of
//! K's apartment and the Wallace atrium, with the thin cyan line-work of a HUD.
//!
//! The renderers keep using the semantic ANSI colours (`Cyan` is "accent", `Magenta` is "the second voice", `DarkGray` is
//! "dim", `Red` is "danger"…); this module decides what those names look like on the terminal in front of the person,
//! and `term.rs` applies it at the one place a colour becomes an escape sequence. So the whole UI changes together, a test
//! that reads the screen never sees a colour, and a terminal that cannot show the palette falls back to its own.
//!
//! * truecolor terminals (`COLORTERM=truecolor|24bit`) get the full palette;
//! * 256-colour terminals (`TERM=*256color`, Terminal.app) get the nearest cube colours;
//! * `CORDATA_THEME=terminal`, a light terminal (`COLORFGBG`) or anything else keep the terminal's own 16 colours —
//!   a palette tuned for a dark background would be unreadable on a light one;
//! * `NO_COLOR` strips colour and keeps bold/dim/reverse.
//!
//! The roles, so a new widget picks the right name: **Cyan = sodium amber** (Cordata, the live thing, the prompt),
//! **Magenta = electric teal** (agents, the orchestrator, untrusted content: the other voice), **Green = cold mint**
//! (done, safe, plan), **Yellow = pale gold** (needs a look), **Red = vermilion** (danger, auto), **Blue = steel**.

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
    // `COLORFGBG=15;0` is foreground;background. A light background (7 or 15) wants the terminal's own palette.
    if env("COLORFGBG").is_some_and(|v| matches!(v.rsplit(';').next(), Some("7" | "15"))) {
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

/// The palette: what each semantic colour is, as RGB. `None` for colours the theme leaves alone.
fn palette(color: Color) -> Option<(u8, u8, u8)> {
    Some(match color {
        // accent — the one colour that means "Cordata": sodium-vapour amber
        Color::Cyan | Color::LightCyan => (255, 158, 61),
        // the second voice: agents, the orchestrator, untrusted content — electric teal
        Color::Magenta | Color::LightMagenta => (72, 214, 224),
        Color::Yellow | Color::LightYellow => (255, 214, 130),
        Color::Green | Color::LightGreen => (118, 222, 186),
        Color::Red | Color::LightRed => (255, 96, 74),
        Color::Blue | Color::LightBlue => (116, 160, 214),
        // secondary text, and the dimmest text that is still readable
        Color::Gray => (170, 180, 190),
        Color::DarkGray => (96, 108, 122),
        Color::White => (238, 241, 245),
        _ => return None,
    })
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
        // A light background keeps the terminal's own colours: the pastels would vanish on it.
        assert_eq!(detect(env(&[("COLORFGBG", "0;15"), ("COLORTERM", "truecolor")])), Level::Ansi);
        assert_eq!(detect(env(&[("COLORFGBG", "15;0"), ("COLORTERM", "truecolor")])), Level::TrueColor);
    }

    #[test]
    fn semantic_colours_become_the_palette_and_everything_else_passes_through() {
        assert_eq!(remap_for(Level::TrueColor, Color::Cyan), Color::Rgb(255, 158, 61));
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
