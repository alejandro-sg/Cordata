//! The one theme module: the design tokens (`cordata_theme::tokens`) as GPUI colours, the fonts the window uses, and
//! the component library's theme built from the same tokens. Views read colours only through here — `tk(cx)` for a
//! token, `line`/`amber`/`teal` for the translucent ones, `color(cx, Role)` for the older role-based panes.
//!
//! The theme (`system` · `light` · `2049` · `graphite` · `dim`) can change while the app runs (Settings → Appearance).
//! `system` follows macOS Light/Dark live: the window's appearance observer (`main.rs`, GPUI's
//! `observe_window_appearance`) calls `appearance_changed`, which reinstalls only when the resolved theme actually
//! changes — one reinstall per real switch, however many notifications arrive. Increase Contrast is read once at launch.

use std::rc::Rc;

use cordata_theme::tokens::{self, Tokens, fonts};
use cordata_theme::{Palette, Rgb};
use gpui::{App, BoxShadow, Hsla, Rgba, SharedString, WindowAppearance, point, px, rgb};
use gpui_component::theme::{Theme, ThemeConfig, ThemeMode};

/// What is in force.
pub struct Active {
    pub tokens: Tokens,
    pub sans: SharedString,
    pub mono: SharedString,
    /// The reading serif the agent's prose is set in (`fonts::VOICE`), or the sans when none is installed.
    pub voice: SharedString,
    pub increase_contrast: bool,
    /// The preference as written (`system`, or a theme name) — what an appearance change is resolved against.
    pub choice: SharedString,
}

impl gpui::Global for Active {}

fn active(cx: &App) -> Option<&Active> {
    cx.try_global::<Active>()
}

/// The token set in force.
pub fn tk(cx: &App) -> Tokens {
    active(cx).map_or(tokens::T2049, |a| a.tokens)
}

pub fn sans(cx: &App) -> SharedString {
    active(cx).map_or_else(|| SharedString::from(fonts::SANS), |a| a.sans.clone())
}

pub fn mono(cx: &App) -> SharedString {
    active(cx).map_or_else(|| SharedString::from(fonts::MONO[fonts::MONO.len() - 1]), |a| a.mono.clone())
}

/// The agent's reading face.
pub fn voice(cx: &App) -> SharedString {
    active(cx).map_or_else(|| SharedString::from(fonts::VOICE[0]), |a| a.voice.clone())
}

/// The code font size (Settings → Appearance), for code blocks and the editor.
pub fn code_size(cx: &App) -> f32 {
    cx.try_global::<crate::prefs::Prefs>().map_or(tokens::TEXT_UI, |p| p.code_font_size)
}

pub fn rgba(c: Rgb) -> Rgba {
    rgb(c.to_u32())
}

/// A token as a GPUI colour.
pub fn c(value: Rgb) -> Hsla {
    rgba(value).into()
}

/// A token at `alpha` (0–1).
pub fn ca(value: Rgb, alpha: f32) -> Hsla {
    let mut out = c(value);
    out.a = alpha;
    out
}

/// A line or a hover fill: the theme's ink at `alpha` (`tokens::LINE_*`, `tokens::HOVER*`).
pub fn line(cx: &App, alpha: f32) -> Hsla {
    ca(tk(cx).ink, alpha)
}

pub fn amber(cx: &App, alpha: f32) -> Hsla {
    ca(tk(cx).amber, alpha)
}

/// The one shadow, for floating layers only (the palette, popovers, modals, drawers): tight and low, so the layer
/// reads as lifted without haze. The chrome itself casts nothing — hairlines do the separating.
pub fn modal_shadow(cx: &App) -> Vec<BoxShadow> {
    let t = tk(cx);
    // The shadow token, never the window ground: on paper that is near-white and the modal would cast a white halo.
    let alpha = if t.is_dark { tokens::MODAL_SHADOW } else { tokens::MODAL_SHADOW_LIGHT };
    vec![BoxShadow { color: ca(t.shadow, alpha), offset: point(px(0.), px(tokens::MODAL_SHADOW_Y)), blur_radius: px(tokens::MODAL_SHADOW_BLUR), spread_radius: px(0.), inset: false }]
}

/// A 1px border drawn inside the box (the active row, an active settings item): no layout shift, no stripe.
pub fn inset_ring(color: Hsla) -> Vec<BoxShadow> {
    vec![BoxShadow { color, offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(1.), inset: true }]
}

/// A 1px line inside the bottom edge (the active tab).
pub fn inset_bottom(color: Hsla) -> Vec<BoxShadow> {
    vec![BoxShadow { color, offset: point(px(0.), px(-1.)), blur_radius: px(0.), spread_radius: px(0.), inset: true }]
}

/// Choose the families from the faces macOS ships: SF Pro (`.SystemUIFont`) for the sans, SF Mono then Menlo for the
/// mono, Charter → Iowan Old Style → Georgia for the agent's voice. Nothing is bundled or fetched; a face the system
/// lacks falls through to the next, and the last of each list is always present on macOS.
pub fn load_fonts(cx: &mut App) -> (SharedString, SharedString, SharedString) {
    let names = cx.text_system().all_font_names();
    let (sans, mono, voice) = pick_fonts(|name| names.iter().any(|n| n == name));
    (SharedString::from(sans), SharedString::from(mono), SharedString::from(voice))
}

/// The families `load_fonts` settles on, given what is installed. `.SystemUIFont` is a GPUI alias rather than a listed
/// family, so the sans is never looked up: it always resolves.
fn pick_fonts(has: impl Fn(&str) -> bool) -> (&'static str, &'static str, &'static str) {
    let sans = fonts::SANS;
    let mono = fonts::MONO.into_iter().find(|f| has(f)).unwrap_or(fonts::MONO[fonts::MONO.len() - 1]);
    let voice = fonts::VOICE.into_iter().find(|f| has(f)).unwrap_or(sans);
    (sans, mono, voice)
}

/// Is this appearance a dark one?
pub fn is_dark(appearance: WindowAppearance) -> bool {
    matches!(appearance, WindowAppearance::Dark | WindowAppearance::VibrantDark)
}

/// The tokens a preference names under an appearance: `system` follows it, anything else is that theme.
pub fn resolve(choice: &str, dark: bool) -> &'static Tokens {
    if choice == "system" { Tokens::for_appearance(dark) } else { Tokens::named(choice) }
}

/// Install the tokens and the component theme. Call once after `gpui_component::init`, and again (`set_theme`) when
/// the theme setting changes. `system` is resolved against the app's current appearance.
pub fn install(increase_contrast: bool, theme: &str, cx: &mut App) {
    let dark = is_dark(cx.window_appearance());
    install_resolved(increase_contrast, theme, dark, cx);
}

fn install_resolved(increase_contrast: bool, choice: &str, dark: bool, cx: &mut App) {
    let (sans, mono, voice) = match active(cx) {
        Some(a) => (a.sans.clone(), a.mono.clone(), a.voice.clone()),
        None => load_fonts(cx),
    };
    let base = *resolve(choice, dark);
    let tokens = if increase_contrast { base.high_contrast() } else { base };
    let palette = tokens.palette();
    let mode = if tokens.is_dark { ThemeMode::Dark } else { ThemeMode::Light };
    let config = Rc::new(component_theme(&tokens, &palette, &sans, &mono));
    cx.set_global(Active { tokens, sans, mono, voice, increase_contrast, choice: SharedString::from(choice.to_owned()) });
    // The library's mode first — `Theme::change` loads its stock colours for that mode — then ours over them; the other
    // order threw our colours away.
    Theme::change(mode, None, cx);
    Theme::update(cx, |theme| {
        theme.apply_config(&config);
        theme.mode = mode;
    });
}

/// The window's appearance changed (System Settings → Appearance, or Auto at dusk). Reinstalls — and redraws every
/// window — only when the preference is `system` and the resolved theme is a different one; returns whether it did.
pub fn appearance_changed(appearance: WindowAppearance, cx: &mut App) -> bool {
    let Some(current) = active(cx) else { return false };
    if current.choice.as_ref() != "system" {
        return false;
    }
    let dark = is_dark(appearance);
    if current.tokens.is_dark == dark {
        return false;
    }
    let (increase_contrast, choice) = (current.increase_contrast, current.choice.to_string());
    install_resolved(increase_contrast, &choice, dark, cx);
    cx.refresh_windows();
    true
}

pub fn set_theme(theme: &str, cx: &mut App) {
    let increase_contrast = active(cx).is_some_and(|a| a.increase_contrast);
    install(increase_contrast, theme, cx);
    cx.refresh_windows();
}

/// The component library's colours, from the tokens. Keys are gpui-component's theme schema.
fn component_theme(t: &Tokens, p: &Palette, sans: &str, mono: &str) -> ThemeConfig {
    let hex = |c: Rgb| c.css();
    let wash = |c: Rgb, alpha: u8| format!("{}{:02X}", c.css(), alpha);
    let hairline = hex(p.hairline);
    let colors: serde_json::Map<String, serde_json::Value> = [
        ("background", hex(t.bg_main)),
        ("foreground", hex(t.text_primary)),
        ("border", hairline.clone()),
        ("input.border", hairline.clone()),
        ("ring", hex(t.amber)),
        ("caret", hex(t.amber)),
        ("selection.background", hex(p.selection)),
        ("accent.background", hex(t.bg_input)),
        ("accent.foreground", hex(t.text_primary)),
        ("muted.background", hex(t.bg_panel)),
        ("muted.foreground", hex(t.text_muted)),
        ("popover.background", hex(t.bg_input)),
        ("popover.foreground", hex(t.text_primary)),
        ("primary.background", hex(t.amber)),
        ("primary.foreground", hex(t.amber_on_fill)),
        ("primary.hover.background", hex(t.amber_hover)),
        ("primary.active.background", wash(t.amber, 0xD0)),
        ("secondary.background", hex(t.bg_input)),
        ("secondary.foreground", hex(t.text_primary)),
        ("secondary.hover.background", hairline.clone()),
        ("secondary.active.background", hairline.clone()),
        ("danger.background", hex(t.danger)),
        ("danger.foreground", hex(if t.is_dark { t.bg_window } else { t.bg_input })),
        ("warning.background", hex(t.amber)),
        ("warning.foreground", hex(t.amber_on_fill)),
        ("success.background", hex(t.teal)),
        ("success.foreground", hex(if t.is_dark { t.bg_window } else { t.bg_input })),
        ("info.background", hex(t.info)),
        ("info.foreground", hex(if t.is_dark { t.bg_window } else { t.bg_input })),
        ("link", hex(t.teal)),
        ("link.hover", hex(t.amber)),
        ("link.active", hex(t.amber)),
        ("list.background", hex(t.bg_main)),
        ("list.hover.background", hex(t.bg_panel)),
        ("list.active.background", wash(t.amber, 0x22)),
        ("list.active.border", hex(t.amber)),
        ("list.even.background", hex(t.bg_main)),
        ("list.head.background", hex(t.bg_panel)),
        ("sidebar.background", hex(t.bg_panel)),
        ("sidebar.foreground", hex(t.text_secondary)),
        ("sidebar.border", hairline.clone()),
        ("sidebar.accent.background", hex(t.bg_input)),
        ("sidebar.accent.foreground", hex(t.text_primary)),
        ("sidebar.primary.background", hex(t.amber)),
        ("sidebar.primary.foreground", hex(t.amber_on_fill)),
        ("scrollbar.background", "#00000000".to_owned()),
        ("scrollbar.thumb.background", wash(t.text_muted, 0x44)),
        ("scrollbar.thumb.hover.background", wash(t.text_muted, 0x88)),
        ("tab_bar.background", hex(t.bg_panel)),
        ("title_bar.background", hex(t.bg_panel)),
        ("title_bar.border", hairline),
        ("progress.bar.background", hex(t.amber)),
        ("skeleton.background", hex(t.bg_input)),
        ("switch.background", hex(t.bg_input)),
        ("slider.background", hex(t.amber)),
        ("slider.thumb.background", hex(t.text_primary)),
        ("chart.1", hex(t.amber)),
        ("chart.2", hex(t.teal)),
        ("chart.3", hex(t.amber_soft)),
        ("chart.4", hex(t.danger)),
        ("chart.5", hex(t.info)),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), serde_json::Value::String(value)))
    .collect();
    let config = serde_json::json!({
        "name": "Cordata",
        "mode": if t.is_dark { "dark" } else { "light" },
        "font.family": sans,
        "font.size": tokens::TEXT_BODY,
        "mono_font.family": mono,
        "mono_font.size": tokens::TEXT_UI,
        "radius": tokens::RADIUS_CONTROL as usize,
        "radius.lg": tokens::RADIUS_CARD as usize,
        "shadow": false,
        "colors": colors,
    });
    serde_json::from_value(config).unwrap_or_else(|error| {
        eprintln!("cordata: the component theme did not parse ({error}); using the library's dark theme");
        ThemeConfig { mode: if t.is_dark { ThemeMode::Dark } else { ThemeMode::Light }, ..Default::default() }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_component_theme_parses_from_the_tokens() {
        for theme in tokens::THEMES {
            let p = theme.palette();
            let config = component_theme(theme, &p, fonts::SANS, fonts::MONO[0]);
            assert_eq!(config.mode.is_dark(), theme.is_dark, "{}", theme.name);
            assert_eq!(config.colors.background.as_ref().map(|s| s.to_string()), Some(theme.bg_main.css()));
            assert_eq!(config.colors.primary.as_ref().map(|s| s.to_string()), Some(theme.amber.css()));
        }
    }

    #[test]
    fn system_follows_the_appearance_and_a_named_theme_does_not() {
        assert_eq!(resolve("system", false).name, "light");
        assert_eq!(resolve("system", true).name, "2049");
        assert_eq!(resolve("graphite", false).name, "graphite");
        assert_eq!(resolve("light", true).name, "light");
        assert!(is_dark(WindowAppearance::VibrantDark) && !is_dark(WindowAppearance::VibrantLight));
    }

    #[test]
    fn the_fonts_are_the_systems_and_fall_through_in_order() {
        assert_eq!(pick_fonts(|_| true), (".SystemUIFont", "SF Mono", "Charter"));
        assert_eq!(pick_fonts(|f| f == "Menlo" || f == "Georgia"), (".SystemUIFont", "Menlo", "Georgia"));
        // Nothing listed at all: the mono's last resort and the sans for the voice.
        assert_eq!(pick_fonts(|_| false), (".SystemUIFont", "Menlo", ".SystemUIFont"));
    }
}
