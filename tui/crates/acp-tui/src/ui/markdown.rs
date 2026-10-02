//! A deliberately small markdown renderer for agent replies: headings, bullet
//! and numbered lists, block quotes, fenced code, and inline `code` / **bold**.
//! Not a full CommonMark parser — it exists so a reply reads like a reply
//! instead of a wall of `**` and backticks. Anything it doesn't recognise
//! falls through as plain wrapped text, so it can only ever make output
//! prettier, never lose any.
//!
//! Wrapping happens before inline styling, so an inline span that happens to
//! straddle a wrap boundary loses its style on the second row; the text is intact.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::hard_wrap;

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

/// Split one already-wrapped row into styled spans for `code` and **bold**.
fn inline_spans(row: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut bold = false;
    let chars: Vec<char> = row.chars().collect();
    let mut i = 0;

    let flush = |plain: &mut String, spans: &mut Vec<Span<'static>>, bold: bool| {
        if !plain.is_empty() {
            let style = if bold {
                base.add_modifier(Modifier::BOLD)
            } else {
                base
            };
            spans.push(Span::styled(std::mem::take(plain), style));
        }
    };

    while i < chars.len() {
        if chars[i] == '`'
            && let Some(close) = chars[i + 1..].iter().position(|&c| c == '`')
            && close > 0
        {
            flush(&mut plain, &mut spans, bold);
            let code: String = chars[i + 1..i + 1 + close].iter().collect();
            spans.push(Span::styled(code, Style::new().fg(Color::Yellow)));
            i += close + 2;
        } else if chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            flush(&mut plain, &mut spans, bold);
            bold = !bold;
            i += 2;
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    flush(&mut plain, &mut spans, bold);
    spans
}

fn push_wrapped(
    out: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    first: &str,
    rest: &str,
    base: Style,
) {
    let options = textwrap::Options::new(width.max(1))
        .initial_indent(first)
        .subsequent_indent(rest)
        .break_words(true);
    for row in textwrap::wrap(text, &options) {
        out.push(Line::from(inline_spans(&row, base)));
    }
}

pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;

    for raw in text.split('\n') {
        let line = raw.trim_end();
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") {
            in_code = !in_code;
            let lang = trimmed.trim_start_matches('`').trim();
            out.push(Line::styled(
                if !in_code {
                    "└─".to_owned()
                } else if lang.is_empty() {
                    "┌─".to_owned()
                } else {
                    format!("┌─ {lang}")
                },
                dim(),
            ));
            continue;
        }
        if in_code {
            let code_style = Style::new().fg(Color::Cyan);
            for row in hard_wrap(line, width.saturating_sub(2).max(1)) {
                out.push(Line::from(vec![
                    Span::styled("│ ", dim()),
                    Span::styled(row, code_style),
                ]));
            }
            continue;
        }
        if line.is_empty() {
            out.push(Line::raw(""));
            continue;
        }

        let indent = line.len() - trimmed.len();
        let pad = " ".repeat(indent);

        if let Some(heading) = trimmed
            .strip_prefix('#')
            .map(|rest| rest.trim_start_matches('#'))
            .filter(|rest| rest.starts_with(' '))
        {
            let style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            push_wrapped(&mut out, heading.trim(), width, "", "", style);
        } else if let Some(item) = ["- ", "* ", "+ "]
            .iter()
            .find_map(|marker| trimmed.strip_prefix(marker))
        {
            push_wrapped(
                &mut out,
                item,
                width,
                &format!("{pad}• "),
                &format!("{pad}  "),
                Style::new(),
            );
        } else if let Some((number, item)) = trimmed
            .split_once(". ")
            .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        {
            let marker = format!("{pad}{number}. ");
            let hang = " ".repeat(marker.chars().count());
            push_wrapped(&mut out, item, width, &marker, &hang, Style::new());
        } else if let Some(quoted) = trimmed.strip_prefix("> ") {
            push_wrapped(
                &mut out,
                quoted,
                width,
                "▎ ",
                "▎ ",
                Style::new().fg(Color::Gray).add_modifier(Modifier::ITALIC),
            );
        } else {
            push_wrapped(&mut out, line, width, "", "", Style::new());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn style_of(line: &Line<'_>, text: &str) -> Option<Style> {
        line.spans
            .iter()
            .find(|s| s.content == text)
            .map(|s| s.style)
    }

    #[test]
    fn headings_lists_and_quotes() {
        let lines = render("# Title\n- one\n  - nested\n1. first\n> quoted", 40);
        assert_eq!(
            plain(&lines),
            ["Title", "• one", "  • nested", "1. first", "▎ quoted"]
        );
        assert!(
            style_of(&lines[0], "Title")
                .unwrap()
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn inline_code_and_bold_lose_their_markers() {
        let lines = render("run `cargo test` for **all** crates", 60);
        assert_eq!(plain(&lines), ["run cargo test for all crates"]);
        assert_eq!(
            style_of(&lines[0], "cargo test").unwrap().fg,
            Some(Color::Yellow)
        );
        assert!(
            style_of(&lines[0], "all")
                .unwrap()
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn fenced_code_is_boxed_unwrapped_and_not_inline_styled() {
        let lines = render("text\n```rust\nlet x = `not code` * 2;\n```\nafter", 40);
        assert_eq!(
            plain(&lines),
            [
                "text",
                "┌─ rust",
                "│ let x = `not code` * 2;",
                "└─",
                "after"
            ]
        );
    }

    #[test]
    fn an_unterminated_fence_while_streaming_keeps_rendering_as_code() {
        assert_eq!(
            plain(&render("```\nlet a = 1;", 40)),
            ["┌─", "│ let a = 1;"]
        );
    }

    #[test]
    fn long_lines_wrap_with_a_hanging_indent_and_never_exceed_the_width() {
        let lines = plain(&render(
            "- the quick brown fox jumps over the lazy dog again and again",
            24,
        ));
        assert!(lines.len() > 1 && lines[1].starts_with("  "), "{lines:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 24), "{lines:?}");
        assert!(
            plain(&render(&"x".repeat(90), 30))
                .iter()
                .all(|l| l.chars().count() <= 30)
        );
    }

    #[test]
    fn unrecognised_text_passes_through_and_stray_markers_stay() {
        assert_eq!(
            plain(&render("2 * 3 = 6 and a lone ` tick", 60)),
            ["2 * 3 = 6 and a lone ` tick"]
        );
        assert_eq!(plain(&render("a\n\nb", 20)), ["a", "", "b"]);
    }
}
