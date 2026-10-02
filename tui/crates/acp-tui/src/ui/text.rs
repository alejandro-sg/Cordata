//! Wrapping/padding helpers shared by the renderers.

use unicode_width::UnicodeWidthStr;

/// Word-wrap `text` to `width` columns, preserving blank lines and breaking
/// over-long words (URLs, base64) rather than overflowing.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let options = textwrap::Options::new(width).break_words(true);
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            out.push(String::new());
        } else {
            out.extend(
                textwrap::wrap(line, &options)
                    .into_iter()
                    .map(std::borrow::Cow::into_owned),
            );
        }
    }
    out
}

pub fn pad_to(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
}

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Truncate to `max` display columns, appending `…` when cut.
pub fn truncate(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_keeps_blank_lines_and_breaks_long_words() {
        assert_eq!(wrap("ab\n\ncd", 10), ["ab", "", "cd"]);
        assert!(wrap(&"x".repeat(25), 10).iter().all(|l| l.len() <= 10));
    }

    #[test]
    fn truncate_respects_width() {
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("hi", 8), "hi");
    }
}
