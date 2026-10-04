//! The diff viewer: what an edit tool call changes, as coloured unified-diff
//! rows with line numbers — in the approval panel (before you allow it) and in
//! the tool card (after it ran).
//!
//! Input is either the backend's ACP `diff` content (old text read from disk
//! before the call, new text) or a unified patch (`apply_patch`, the self-heal
//! loop's proposals). Old/new pairs are diffed here line by line: common
//! prefix and suffix are trimmed, then an LCS over what's left (bounded — a huge
//! rewrite falls back to "everything removed, everything added", which is what
//! it is anyway).
//!
//! ```text
//! ✎ src/lib.rs  +2 −2
//!    5   pub fn broken() -> i32 {
//!    6 − let s: String = double(2);
//!    6 + let s: i32 = double(2);
//! ```

use acp_core::FileDiff;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::text::{display_width, truncate};

const CONTEXT: usize = 3;
/// Cells in the LCS table before giving up on a minimal diff.
const MAX_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Keep,
    Del,
    Add,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub op: Op,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub text: String,
}

fn split(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Every line of `old` and `new`, aligned (Keep / Del / Add), 1-based numbers.
pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let a = split(old);
    let b = split(new);
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (mid_a, mid_b) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);

    let mut ops: Vec<Op> = vec![Op::Keep; prefix];
    ops.extend(middle_ops(mid_a, mid_b));
    ops.extend(std::iter::repeat_n(Op::Keep, suffix));

    let (mut i, mut j) = (0usize, 0usize);
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            Op::Keep => {
                out.push(DiffLine {
                    op,
                    old_no: Some(i + 1),
                    new_no: Some(j + 1),
                    text: b[j].to_owned(),
                });
                i += 1;
                j += 1;
            }
            Op::Del => {
                out.push(DiffLine {
                    op,
                    old_no: Some(i + 1),
                    new_no: None,
                    text: a[i].to_owned(),
                });
                i += 1;
            }
            Op::Add => {
                out.push(DiffLine {
                    op,
                    old_no: None,
                    new_no: Some(j + 1),
                    text: b[j].to_owned(),
                });
                j += 1;
            }
        }
    }
    out
}

fn middle_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 || (n + 1) * (m + 1) > MAX_CELLS {
        let mut ops = vec![Op::Del; n];
        ops.extend(std::iter::repeat_n(Op::Add, m));
        return ops;
    }
    // lcs[i][j] = LCS length of a[i..] and b[j..]
    let width = m + 1;
    let mut lcs = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * width + j] = if a[i] == b[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::with_capacity(n + m);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Keep);
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
            ops.push(Op::Del);
            i += 1;
        } else {
            ops.push(Op::Add);
            j += 1;
        }
    }
    ops.extend(std::iter::repeat_n(Op::Del, n - i));
    ops.extend(std::iter::repeat_n(Op::Add, m - j));
    ops
}

/// Changed lines with `CONTEXT` lines around them; `None` marks a gap.
pub fn hunk_rows(lines: &[DiffLine]) -> Vec<Option<&DiffLine>> {
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.op != Op::Keep)
        .map(|(index, _)| index)
        .collect();
    let mut keep = vec![false; lines.len()];
    for &index in &changed {
        let start = index.saturating_sub(CONTEXT);
        let end = (index + CONTEXT + 1).min(lines.len());
        keep[start..end].iter_mut().for_each(|k| *k = true);
    }
    let mut out = Vec::new();
    let mut gap = false;
    for (index, line) in lines.iter().enumerate() {
        if keep[index] {
            if gap && !out.is_empty() {
                out.push(None);
            }
            gap = false;
            out.push(Some(line));
        } else {
            gap = true;
        }
    }
    out
}

pub fn stats(lines: &[DiffLine]) -> (usize, usize) {
    lines.iter().fold((0, 0), |(add, del), line| match line.op {
        Op::Add => (add + 1, del),
        Op::Del => (add, del + 1),
        Op::Keep => (add, del),
    })
}

/// A path relative to the session's directory when it is inside it.
pub fn short(path: &str, cwd: &str) -> String {
    let cwd = cwd.trim_end_matches('/');
    match path
        .strip_prefix(cwd)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        Some(rel) if !cwd.is_empty() => rel.to_owned(),
        _ => crate::ui::footer::short_path(path),
    }
}

fn header(
    path: &str,
    new_file: bool,
    added: usize,
    removed: usize,
    width: usize,
) -> Vec<Span<'static>> {
    let label = if new_file { "  (new file)" } else { "" };
    let counts = format!("  +{added} −{removed}");
    let room = width.saturating_sub(2 + display_width(&counts) + display_width(label));
    vec![
        Span::styled("✎ ", Style::new().fg(Color::Cyan)),
        Span::styled(
            truncate(path, room.max(8)),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(label, Style::new().fg(Color::Gray)),
        Span::styled(format!("  +{added}"), Style::new().fg(Color::Green)),
        Span::styled(format!(" −{removed}"), Style::new().fg(Color::Red)),
    ]
}

fn row(line: &DiffLine, width: usize) -> Vec<Span<'static>> {
    let number = match line.op {
        Op::Del => line.old_no,
        _ => line.new_no,
    }
    .map_or_else(|| "    ".to_owned(), |n| format!("{n:>4}"));
    let (sign, style) = match line.op {
        Op::Keep => ("  ", Style::new().fg(Color::Gray)),
        Op::Del => (" −", Style::new().fg(Color::Red)),
        Op::Add => (" +", Style::new().fg(Color::Green)),
    };
    let text = crate::app::sanitize(&line.text).replace('\t', "    ");
    let room = width.saturating_sub(4 + 2 + 1);
    vec![
        Span::styled(number, Style::new().fg(Color::DarkGray)),
        Span::styled(sign, style.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(truncate(&text, room), style),
    ]
}

/// Rows (spans, unbordered) for one file's change. `max_rows` caps the body;
/// the remainder becomes a "+N more" row.
pub fn file_rows(
    diff: &FileDiff,
    cwd: &str,
    width: usize,
    max_rows: Option<usize>,
) -> Vec<Vec<Span<'static>>> {
    let lines = line_diff(diff.old_text.as_deref().unwrap_or(""), &diff.new_text);
    let (added, removed) = stats(&lines);
    let mut rows = vec![header(
        &short(&diff.path, cwd),
        diff.old_text.is_none(),
        added,
        removed,
        width,
    )];
    if added + removed == 0 {
        rows.push(vec![Span::styled(
            "  (no changes)",
            Style::new().fg(Color::Gray),
        )]);
        return rows;
    }
    let body = hunk_rows(&lines);
    let limit = max_rows.unwrap_or(usize::MAX);
    for item in body.iter().take(limit) {
        rows.push(match item {
            Some(line) => row(line, width),
            None => vec![Span::styled("     ⋯", Style::new().fg(Color::DarkGray))],
        });
    }
    if body.len() > limit {
        rows.push(vec![Span::styled(
            format!(
                "     … +{} more diff lines (Ctrl+E expands)",
                body.len() - limit
            ),
            Style::new().fg(Color::Gray).add_modifier(Modifier::ITALIC),
        )]);
    }
    rows
}

/// A unified patch (``apply_patch`` input, a self-heal proposal) as coloured
/// rows: headers bold, hunks cyan, removals red, additions green.
pub fn patch_rows(patch: &str, width: usize, max_rows: Option<usize>) -> Vec<Vec<Span<'static>>> {
    let lines: Vec<&str> = patch.lines().collect();
    let limit = max_rows.unwrap_or(usize::MAX);
    let mut rows: Vec<Vec<Span<'static>>> = lines
        .iter()
        .take(limit)
        .map(|line| {
            let style = if line.starts_with("+++") || line.starts_with("---") {
                Style::new().add_modifier(Modifier::BOLD)
            } else if line.starts_with("@@") {
                Style::new().fg(Color::Cyan)
            } else if line.starts_with('+') {
                Style::new().fg(Color::Green)
            } else if line.starts_with('-') {
                Style::new().fg(Color::Red)
            } else {
                Style::new().fg(Color::Gray)
            };
            vec![Span::styled(
                truncate(&crate::app::sanitize(line), width.max(8)),
                style,
            )]
        })
        .collect();
    if lines.len() > limit {
        rows.push(vec![Span::styled(
            format!("… +{} more lines", lines.len() - limit),
            Style::new().fg(Color::Gray).add_modifier(Modifier::ITALIC),
        )]);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(lines: &[DiffLine]) -> String {
        lines
            .iter()
            .map(|l| match l.op {
                Op::Keep => ' ',
                Op::Del => '-',
                Op::Add => '+',
            })
            .collect()
    }

    #[test]
    fn a_one_line_change_is_one_removal_and_one_addition() {
        let lines = line_diff("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(ops(&lines), " -+ ");
        assert_eq!(
            (lines[1].old_no, lines[2].new_no, lines[2].text.as_str()),
            (Some(2), Some(2), "B")
        );
        assert_eq!(stats(&lines), (1, 1));
    }

    #[test]
    fn insertions_deletions_and_new_files() {
        assert_eq!(ops(&line_diff("a\nc\n", "a\nb\nc\n")), " + ");
        assert_eq!(ops(&line_diff("a\nb\nc\n", "a\nc\n")), " - ");
        assert_eq!(ops(&line_diff("", "x\ny\n")), "++");
        assert_eq!(ops(&line_diff("same\n", "same\n")), " ");
        // LCS keeps the common middle line rather than replacing everything.
        assert_eq!(ops(&line_diff("x\nkeep\ny\n", "p\nkeep\nq\n")), "-+ -+");
    }

    #[test]
    fn hunks_keep_three_lines_of_context_and_mark_gaps() {
        let old: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let new: String = (1..=20)
            .map(|i| match i {
                2 => "two\n".to_owned(),
                18 => "eighteen\n".to_owned(),
                i => format!("{i}\n"),
            })
            .collect();
        let lines = line_diff(&old, &new);
        let rows = hunk_rows(&lines);
        assert!(
            rows.iter().any(Option::is_none),
            "a gap between the two hunks"
        );
        let shown: Vec<&str> = rows.iter().flatten().map(|l| l.text.as_str()).collect();
        assert!(shown.contains(&"5") && !shown.contains(&"10"), "{shown:?}");
    }

    #[test]
    fn rows_render_header_numbers_and_signs_within_width() {
        let diff = FileDiff {
            path: "/proj/src/lib.rs".into(),
            old_text: Some("fn a() {}\nlet s: String = x;\n".into()),
            new_text: "fn a() {}\nlet s: i32 = x;\n".into(),
        };
        let rows = file_rows(&diff, "/proj", 40, None);
        let text: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            text[0].starts_with("✎ src/lib.rs") && text[0].contains("+1") && text[0].contains("−1")
        );
        assert!(text.iter().any(|t| t.starts_with("   2 −")));
        assert!(text.iter().any(|t| t.starts_with("   2 +")));
        assert!(text.iter().all(|t| display_width(t) <= 40), "{text:#?}");
    }

    #[test]
    fn long_diffs_are_capped_with_a_more_row() {
        let new: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let diff = FileDiff {
            path: "/p/new.txt".into(),
            old_text: None,
            new_text: new,
        };
        let rows = file_rows(&diff, "/p", 60, Some(10));
        let last: String = rows
            .last()
            .unwrap()
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(last.contains("+40 more diff lines"), "{last}");
        let first: String = rows[0].iter().map(|s| s.content.as_ref()).collect();
        assert!(first.contains("(new file)") && first.contains("+50"));
    }

    #[test]
    fn huge_rewrites_fall_back_without_quadratic_blowup() {
        let old: String = (0..3000).map(|i| format!("a{i}\n")).collect();
        let new: String = (0..3000).map(|i| format!("b{i}\n")).collect();
        let lines = line_diff(&old, &new);
        assert_eq!(stats(&lines), (3000, 3000));
    }

    #[test]
    fn patches_are_coloured_by_line_kind() {
        let rows = patch_rows("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n", 40, None);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[3][0].style.fg, Some(Color::Red));
        assert_eq!(rows[4][0].style.fg, Some(Color::Green));
        assert_eq!(rows[2][0].style.fg, Some(Color::Cyan));
    }
}
