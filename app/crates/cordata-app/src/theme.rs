//! `cordata-theme`'s roles as GPUI colours, and as the component library's theme.
//!
//! The components (inputs, buttons, lists, the editor) read gpui-component's `Theme`; it is built here from the same
//! roles the terminal uses, so there is one palette. Increase Contrast swaps in the high-contrast palette.

use std::rc::Rc;

use cordata_theme::{Palette, Role};
use gpui::{App, Hsla, Rgba, rgb};
use gpui_component::theme::{Theme, ThemeConfig, ThemeMode};

/// The palette in force (the window reads the system's Increase Contrast flag once at start; a change applies on the
/// next launch — GPUI does not report it live).
pub struct Active(pub &'static Palette);

impl gpui::Global for Active {}

pub fn palette(cx: &App) -> &'static Palette {
    cx.try_global::<Active>().map_or(&cordata_theme::NIGHT, |active| active.0)
}

pub fn color(cx: &App, role: Role) -> Hsla {
    rgba(palette(cx).get(role)).into()
}

pub fn rgba(c: cordata_theme::Rgb) -> Rgba {
    rgb(c.to_u32())
}

/// `role` at `alpha` (0–1), for washes and glows.
pub fn tint(cx: &App, role: Role, alpha: f32) -> Hsla {
    let mut c: Hsla = color(cx, role);
    c.a = alpha;
    c
}

/// Install the palette and the component theme. Call once, after `gpui_component::init`.
pub fn install(increase_contrast: bool, cx: &mut App) {
    let palette = Palette::for_contrast(increase_contrast);
    cx.set_global(Active(palette));
    let config = Rc::new(component_theme(palette));
    Theme::update(cx, |theme| {
        theme.apply_config(&config);
        theme.mode = ThemeMode::Dark;
    });
    Theme::change(ThemeMode::Dark, None, cx);
}

/// The component library's colours, from the roles. Keys are gpui-component's theme schema.
fn component_theme(p: &Palette) -> ThemeConfig {
    let hex = |role: Role| p.get(role).css();
    let wash = |role: Role, alpha: u8| format!("{}{:02X}", p.get(role).css(), alpha);
    let colors: serde_json::Map<String, serde_json::Value> = [
        ("background", hex(Role::Ground)),
        ("foreground", hex(Role::Text)),
        ("border", hex(Role::Hairline)),
        ("input.border", hex(Role::Hairline)),
        ("ring", hex(Role::Accent)),
        ("caret", hex(Role::Accent)),
        ("selection.background", hex(Role::Selection)),
        ("accent.background", hex(Role::Raised)),
        ("accent.foreground", hex(Role::Text)),
        ("muted.background", hex(Role::Surface)),
        ("muted.foreground", hex(Role::TextSecondary)),
        ("popover.background", hex(Role::Raised)),
        ("popover.foreground", hex(Role::Text)),
        ("primary.background", hex(Role::Accent)),
        ("primary.foreground", hex(Role::Ground)),
        ("primary.hover.background", wash(Role::Accent, 0xE0)),
        ("primary.active.background", wash(Role::Accent, 0xC0)),
        ("secondary.background", hex(Role::Raised)),
        ("secondary.foreground", hex(Role::Text)),
        ("secondary.hover.background", hex(Role::Hairline)),
        ("secondary.active.background", hex(Role::Hairline)),
        ("danger.background", hex(Role::Danger)),
        ("danger.foreground", hex(Role::Ground)),
        ("warning.background", hex(Role::Attention)),
        ("warning.foreground", hex(Role::Ground)),
        ("success.background", hex(Role::Ok)),
        ("success.foreground", hex(Role::Ground)),
        ("info.background", hex(Role::Info)),
        ("info.foreground", hex(Role::Ground)),
        ("link", hex(Role::Agent)),
        ("link.hover", hex(Role::Accent)),
        ("link.active", hex(Role::Accent)),
        ("list.background", hex(Role::Ground)),
        ("list.hover.background", hex(Role::Surface)),
        ("list.active.background", wash(Role::Accent, 0x22)),
        ("list.active.border", hex(Role::Accent)),
        ("list.even.background", hex(Role::Ground)),
        ("list.head.background", hex(Role::Surface)),
        ("sidebar.background", hex(Role::Surface)),
        ("sidebar.foreground", hex(Role::TextSecondary)),
        ("sidebar.border", hex(Role::Hairline)),
        ("sidebar.accent.background", hex(Role::Raised)),
        ("sidebar.accent.foreground", hex(Role::Text)),
        ("sidebar.primary.background", hex(Role::Accent)),
        ("sidebar.primary.foreground", hex(Role::Ground)),
        ("scrollbar.background", "#00000000".to_owned()),
        ("scrollbar.thumb.background", wash(Role::TextDim, 0x55)),
        ("scrollbar.thumb.hover.background", wash(Role::TextDim, 0x99)),
        ("tab_bar.background", hex(Role::Surface)),
        ("title_bar.background", hex(Role::Ground)),
        ("title_bar.border", hex(Role::Hairline)),
        ("progress.bar.background", hex(Role::Accent)),
        ("skeleton.background", hex(Role::Raised)),
        ("switch.background", hex(Role::Raised)),
        ("slider.background", hex(Role::Accent)),
        ("slider.thumb.background", hex(Role::Text)),
        ("chart.1", hex(Role::Accent)),
        ("chart.2", hex(Role::Agent)),
        ("chart.3", hex(Role::Ok)),
        ("chart.4", hex(Role::Attention)),
        ("chart.5", hex(Role::Info)),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), serde_json::Value::String(value)))
    .collect();
    let config = serde_json::json!({
        "name": "Cordata Night",
        "mode": "dark",
        "font.size": 14.0,
        "mono_font.size": 13.0,
        "radius": 4,
        "radius.lg": 6,
        "shadow": false,
        "colors": colors,
    });
    serde_json::from_value(config).unwrap_or_else(|error| {
        eprintln!("cordata: the component theme did not parse ({error}); using the library's dark theme");
        ThemeConfig { mode: ThemeMode::Dark, ..Default::default() }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_component_theme_parses_from_the_roles() {
        for palette in [&cordata_theme::NIGHT, &cordata_theme::NIGHT_HIGH_CONTRAST] {
            let config = component_theme(palette);
            assert_eq!(config.name.as_ref(), "Cordata Night");
            assert!(config.mode.is_dark());
            assert_eq!(config.colors.background.as_ref().map(|s| s.to_string()), Some(palette.ground.css()));
            assert_eq!(config.colors.primary.as_ref().map(|s| s.to_string()), Some(palette.accent.css()));
        }
    }
}
