//! A deliberately small markdown renderer for agent replies: headings, bullet
//! and numbered lists, block quotes, rules, fenced code, GitHub tables, and
//! inline `code` / **bold**. Not a full CommonMark parser — it exists so a
//! reply reads like a reply instead of a wall of `**`, pipes and backticks.
//! Anything it doesn't recognise falls through as plain wrapped text, so it
//! can only ever make output prettier, never lose any.
//!
//! Models write tables with `<br>` and `•` bullets inside cells, which is a
//! wall of pipes in a terminal. A table that fits the width and has one-line
//! cells is drawn as an aligned grid; anything else is stacked, one block per
//! row (the first column as the label, the rest under it), which survives a
//! narrow terminal. `<br>` is a line break everywhere outside a code fence.
//! The Discord front door does the same conversion
//! — keep them in step.
//!
//! Wrapping happens before inline styling, so an inline span that happens to
//! straddle a wrap boundary loses its style on the second row; the text is intact.

use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;

use super::text::{display_width, hard_wrap};

static BR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>").unwrap());
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]\n]+)\]\((https?://[^)\s]+)\)").unwrap());

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

const BULLETS: [&str; 5] = ["- ", "* ", "+ ", "• ", "· "];

/// A bullet marker at the start of a table-cell part: "• x", "- x".
fn strip_bullet(part: &str) -> (&str, bool) {
    for marker in BULLETS {
        if let Some(rest) = part.strip_prefix(marker) {
            return (rest.trim_start(), true);
        }
    }
    (part, false)
}

fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn split_cells(line: &str) -> Vec<String> {
    let mut row = line.trim();
    row = row.strip_prefix('|').unwrap_or(row);
    row = row.strip_suffix('|').unwrap_or(row);
    row.split('|').map(|cell| cell.trim().to_owned()).collect()
}

fn is_table_sep(line: &str) -> bool {
    if !is_table_row(line) {
        return false;
    }
    let cells = split_cells(line);
    cells.len() >= 2
        && cells.iter().all(|cell| {
            let inner = cell.trim_matches(':');
            !inner.is_empty() && inner.chars().all(|c| c == '-')
        })
}

fn is_rule(trimmed: &str) -> bool {
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(first @ ('-' | '*' | '_')) => {
            trimmed.chars().count() >= 3 && chars.all(|c| c == first)
        }
        _ => false,
    }
}

/// A table cell as its separate lines, and whether it was written as a list.
fn cell_parts(cell: &str) -> (Vec<String>, bool) {
    let mut bulleted = false;
    let parts = BR
        .split(cell)
        .map(str::trim)
        .map(|part| {
            let (rest, was) = strip_bullet(part);
            bulleted |= was;
            rest.to_owned()
        })
        .filter(|part| !part.is_empty())
        .collect();
    (parts, bulleted)
}

fn bold_label(label: &str) -> String {
    let label = label.trim();
    if label.starts_with("**") && label.ends_with("**") && label.len() > 4 {
        label.to_owned()
    } else {
        format!("**{label}**")
    }
}

/// The visible width of a cell once its `**` and backticks are gone.
fn visible_width(cell: &str) -> usize {
    let text: String = inline_spans(cell, Style::new())
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    display_width(&text)
}

fn grid_row(out: &mut Vec<Line<'static>>, cells: &[String], widths: &[usize], base: Style) {
    let mut spans = vec![Span::styled("│ ", dim())];
    for (index, width) in widths.iter().enumerate() {
        let cell = cells.get(index).map(String::as_str).unwrap_or("");
        let mut cell_spans = inline_spans(cell, base);
        let used = visible_width(cell);
        cell_spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
        spans.extend(cell_spans);
        spans.push(Span::styled(
            if index + 1 == widths.len() {
                " │"
            } else {
                " │ "
            },
            dim(),
        ));
    }
    out.push(Line::from(spans));
}

fn render_table(
    header: &[String],
    rows: &[Vec<String>],
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let columns = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    let all: Vec<&[String]> = std::iter::once(header)
        .chain(rows.iter().map(Vec::as_slice))
        .collect();
    let single_line = all.iter().all(|row| {
        row.iter().all(|cell| {
            let (parts, bulleted) = cell_parts(cell);
            parts.len() <= 1 && !bulleted
        })
    });
    let widths: Vec<usize> = (0..columns)
        .map(|col| {
            all.iter()
                .map(|row| row.get(col).map_or(0, |cell| visible_width(cell)))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let grid_width = widths.iter().sum::<usize>() + 3 * columns + 1;

    if single_line && grid_width <= width {
        let head_style = Style::new().add_modifier(Modifier::BOLD);
        grid_row(out, header, &widths, head_style);
        let rule: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        out.push(Line::styled(format!("├{}┤", rule.join("┼")), dim()));
        for row in rows {
            grid_row(out, row, &widths, Style::new());
        }
        return;
    }

    // Stacked: the first column labels each block; a two-column table's header names add nothing.
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.push(Line::raw(""));
        }
        let label = row
            .first()
            .map(|cell| cell.trim())
            .filter(|cell| !cell.is_empty());
        let label_md = label.map(bold_label);
        if columns <= 2 {
            let (parts, bulleted) = cell_parts(row.get(1).map(String::as_str).unwrap_or(""));
            match (&label_md, parts.as_slice(), bulleted) {
                (Some(label), [only], false) => {
                    push_wrapped(
                        out,
                        &format!("{label} — {only}"),
                        width,
                        "",
                        "  ",
                        Style::new(),
                    );
                }
                _ => {
                    if let Some(label) = &label_md {
                        push_wrapped(out, label, width, "", "  ", Style::new());
                    }
                    for part in &parts {
                        push_wrapped(out, part, width, "  • ", "    ", Style::new());
                    }
                }
            }
        } else {
            if let Some(label) = &label_md {
                push_wrapped(out, label, width, "", "  ", Style::new());
            }
            for (name, cell) in header.iter().zip(row.iter()).skip(1) {
                let (parts, _) = cell_parts(cell);
                if !parts.is_empty() {
                    let line = format!(
                        "**{}:** {}",
                        name.trim_matches(['*', ' ']),
                        parts.join("; ")
                    );
                    push_wrapped(out, &line, width, "  • ", "    ", Style::new());
                }
            }
        }
    }
}

/// Normalise a reply before layout: `<br>` becomes a line break and `[text](url)` becomes `text (url)`,
/// except inside a code fence (left exactly as written) and inside a table row (its cells are split later,
/// where a `<br>` means "next line of this cell").
fn normalise(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            lines.push(raw.to_owned());
        } else if in_code {
            lines.push(raw.to_owned());
        } else {
            let linked = LINK.replace_all(raw, "$1 ($2)");
            if is_table_row(&linked) {
                lines.push(linked.into_owned());
            } else {
                lines.extend(BR.split(&linked).map(str::to_owned));
            }
        }
    }
    lines
}

pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;
    let lines = normalise(text);
    let mut index = 0;

    while index < lines.len() {
        let raw = lines[index].as_str();
        index += 1;
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
        if is_table_row(line) && lines.get(index).is_some_and(|next| is_table_sep(next)) {
            let header = split_cells(line);
            index += 1;
            let mut rows: Vec<Vec<String>> = Vec::new();
            while let Some(row) = lines.get(index).filter(|row| is_table_row(row)) {
                rows.push(split_cells(row));
                index += 1;
            }
            render_table(&header, &rows, width, &mut out);
            continue;
        }
        if is_rule(trimmed) {
            out.push(Line::styled("─".repeat(width.clamp(3, 48)), dim()));
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
        } else if let Some(item) = BULLETS
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

    /// What the first live reply looked like: a GitHub table with `<br>` and bullets inside its cells.
    const TABLE_REPLY: &str = "I’m a **coding agent**. Here’s a rundown:\n\n\
| Category | What I can do |\n|----------|---------------|\n\
| **Code & files** | • Edit files (`editor`).<br>• Read files (`file_read`).<br>• Apply diffs (`apply_patch`). |\n\
| **Shell** | • Run any shell command. |\n\
| **Plain** | Just a sentence |\n\nDone!";

    #[test]
    fn a_table_with_br_and_bullets_is_stacked_not_a_wall_of_pipes() {
        let lines = render(TABLE_REPLY, 60);
        let text = plain(&lines);
        assert!(
            text.iter().all(|l| !l.contains('|') && !l.contains("<br>")),
            "{text:?}"
        );
        assert_eq!(
            text,
            [
                "I’m a coding agent. Here’s a rundown:",
                "",
                "Code & files",
                "  • Edit files (editor).",
                "  • Read files (file_read).",
                "  • Apply diffs (apply_patch).",
                "",
                "Shell",
                "  • Run any shell command.",
                "",
                "Plain — Just a sentence",
                "",
                "Done!"
            ]
        );
        assert!(
            style_of(&lines[2], "Code & files")
                .unwrap()
                .add_modifier
                .contains(Modifier::BOLD),
            "the row label is bold"
        );
    }

    #[test]
    fn a_small_table_is_an_aligned_grid() {
        let lines = plain(&render(
            "| Name | Qty |\n|---|---:|\n| apple | 3 |\n| **fig** | 12 |",
            40,
        ));
        assert_eq!(
            lines,
            [
                "│ Name  │ Qty │",
                "├───────┼─────┤",
                "│ apple │ 3   │",
                "│ fig   │ 12  │"
            ]
        );
    }

    #[test]
    fn a_grid_too_wide_for_the_terminal_is_stacked_instead() {
        let wide = "| Tool | What it is for |\n|---|---|\n| shell | run any shell command with a timeout and a working directory |";
        let text = plain(&render(wide, 30));
        assert!(
            text.iter()
                .all(|l| !l.contains('│') && l.chars().count() <= 30),
            "{text:?}"
        );
        assert!(text[0].starts_with("shell — run any"), "{text:?}");
    }

    #[test]
    fn three_columns_keep_their_header_names() {
        let text = plain(&render(
            "| Tool | Cost | Use |\n|---|---|---|\n| shell | low<br>• ok | runs things a bit more |",
            24,
        ));
        assert!(text.iter().any(|l| l == "shell"), "{text:?}");
        assert!(
            text.iter().any(|l| l.starts_with("  • Cost: low; ok")),
            "{text:?}"
        );
    }

    #[test]
    fn br_outside_a_table_breaks_the_line_but_never_inside_a_code_fence() {
        assert_eq!(plain(&render("a<br>b<br/>c", 20)), ["a", "b", "c"]);
        assert_eq!(
            plain(&render("```\nx<br>y | z\n```", 20)),
            ["┌─", "│ x<br>y | z", "└─"]
        );
    }

    #[test]
    fn rules_bullets_and_links_read_cleanly() {
        let text = plain(&render(
            "---\n• one\nsee [the docs](https://x.dev/a) now",
            60,
        ));
        assert!(text[0].chars().all(|c| c == '─') && text[0].chars().count() >= 3);
        assert_eq!(text[1], "• one");
        assert_eq!(text[2], "see the docs (https://x.dev/a) now");
    }

    #[test]
    fn a_half_streamed_table_never_loses_text() {
        // Only the header so far: it is plain text until the separator arrives.
        assert_eq!(plain(&render("| a | b |", 20)), ["| a | b |"]);
        // Header + separator and a partial row: the row renders with what has arrived.
        let text = plain(&render("| a | b |\n|---|---|\n| **x** | par", 20));
        assert_eq!(text, ["│ a │ b   │", "├───┼─────┤", "│ x │ par │"]);
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
