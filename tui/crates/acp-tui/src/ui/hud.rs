//! HUD primitives: the small pieces of the Cordata look that more than one widget draws.
//!
//! The vocabulary is a night-time interface — thin rules, corner brackets, segmented gauges and one slow scanning
//! light — rather than boxes and emoji. Everything here is a pure function of its arguments (the tick comes from
//! `App::spinner`), draws only characters present in every terminal font that has box drawing, and degrades by plain
//! truncation, so a narrow terminal loses decoration before it loses information.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

#[cfg(test)]
use super::text::display_width;

/// A tab-shaped label: `▐ PLAN ▌` — the ends in the colour, the word reversed out of it.
pub fn pill(label: &str, color: Color) -> Vec<Span<'static>> {
    vec![
        Span::styled("▐", Style::new().fg(color)),
        Span::styled(format!(" {label} "), Style::new().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD)),
        Span::styled("▌", Style::new().fg(color)),
    ]
}

/// A segmented gauge: `▰▰▰▱▱▱` — `value` in 0.0..=1.0 over `width` cells. Segments, not a smooth fill, because a tick
/// mark you can count ("four of six") is easier to read at a glance than a fraction of a cell.
pub fn segbar(value: f64, width: usize) -> String {
    let filled = ((value.clamp(0.0, 1.0)) * width as f64).round() as usize;
    format!("{}{}", "▰".repeat(filled.min(width)), "▱".repeat(width - filled.min(width)))
}

/// The working indicator: a bright cell sweeping back and forth along a rail, `⟦··█▓░·····⟧`, with a short fading
/// tail. `tick` advances every 100 ms. Returns `width` columns in total, brackets included.
pub fn scanner(tick: usize, width: usize) -> Vec<Span<'static>> {
    let rail = width.saturating_sub(2).max(3);
    // Ping-pong: 0..rail-1 then back down, so the light turns around instead of wrapping.
    let period = (rail - 1) * 2;
    let phase = tick % period.max(1);
    let (head, forward) = if phase < rail { (phase, true) } else { (period - phase, false) };
    let mut cells = Vec::with_capacity(rail);
    for i in 0..rail {
        // Distance behind the head along its direction of travel.
        let behind = if forward { head.checked_sub(i) } else { i.checked_sub(head) };
        let (glyph, bright) = match behind {
            Some(0) => ("█", true),
            Some(1) => ("▓", true),
            Some(2) => ("▒", false),
            Some(3) => ("░", false),
            _ => ("·", false),
        };
        cells.push(Span::styled(
            glyph,
            if bright { Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD) } else { Style::new().fg(if glyph == "·" { Color::DarkGray } else { Color::Cyan }) },
        ));
    }
    let mut out = vec![Span::styled("⟦", Style::new().fg(Color::DarkGray))];
    out.extend(cells);
    out.push(Span::styled("⟧", Style::new().fg(Color::DarkGray)));
    out
}

/// `⌜ text ⌝` corner brackets around a name, the way the title cards frame theirs.
pub fn bracketed(text: &str, color: Color) -> Vec<Span<'static>> {
    vec![
        Span::styled("⌜ ", Style::new().fg(Color::DarkGray)),
        Span::styled(text.to_owned(), Style::new().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(" ⌝", Style::new().fg(Color::DarkGray)),
    ]
}

/// Letter-spaced capitals, `C O R D A T A`: the wide tracking of the film's titles.
pub fn tracked(text: &str) -> String {
    text.chars().map(|c| c.to_ascii_uppercase().to_string()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_scanner_is_always_the_width_asked_for_and_turns_around() {
        for width in [8usize, 12, 20] {
            let mut seen_head = Vec::new();
            for tick in 0..60 {
                let spans = scanner(tick, width);
                assert_eq!(display_width(&text(&spans)), width, "tick {tick}");
                let rail: Vec<char> = text(&spans).chars().skip(1).collect();
                seen_head.push(rail.iter().position(|c| *c == '█').expect("a head"));
            }
            assert_eq!(*seen_head.iter().min().unwrap(), 0, "reaches the left end");
            assert_eq!(*seen_head.iter().max().unwrap(), width - 3, "reaches the right end, then turns");
        }
    }

    #[test]
    fn gauges_and_pills_have_the_width_they_claim() {
        assert_eq!(segbar(0.5, 6), "▰▰▰▱▱▱");
        assert_eq!(segbar(0.0, 4), "▱▱▱▱");
        assert_eq!(segbar(7.0, 4), "▰▰▰▰");
        assert_eq!(display_width(&text(&pill("PLAN", Color::Green))), 8);
        assert_eq!(tracked("cordata"), "C O R D A T A");
    }
}
