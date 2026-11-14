//! Line comments on a diff, sent as one batch of direction to re-run the step that wrote it (docs/orchestrator.md).
//!
//! A person selects lines of a hunk, writes what is wrong, and repeats; nothing runs until they send. The batch becomes
//! the `--direction` text of `cordata-run resume <run> --redo-step <step>`: the step's worker runs again with every
//! comment as the person's words, and the step's checks and the run's acceptance criteria run on the result.
//!
//! Pure: no git, no window. Line numbers are the **new side** of the diff (the file as it is now), so a worker can
//! find them; a selection that holds only deleted lines is placed where the deletion sits.

use crate::review::Hunk;
use crate::runs::Step;

/// What one direction may carry (`cordata-run`'s `DIRECTION_MAX_CHARS`): a longer batch is cut there, so it is sent in
/// parts instead.
pub const DIRECTION_LIMIT: usize = 2000;
/// One comment, cut here, so a few comments always fit in a batch.
pub const COMMENT_MAX_CHARS: usize = 400;

const HEADER: &str = "Review comments on the diff you wrote. Address each one and leave the rest as it is:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub file: String,
    /// First and last line of the selection, 1-based, inclusive, new side.
    pub start: usize,
    pub end: usize,
    pub comment: String,
}

impl Annotation {
    /// `path:12` or `path:12-14`.
    pub fn place(&self) -> String {
        if self.start == self.end { format!("{}:{}", self.file, self.start) } else { format!("{}:{}-{}", self.file, self.start, self.end) }
    }
}

/// The new-side line number each body line of `hunk` stands at: context and added lines their own, a deleted line the
/// number of the line that now follows it, a `\ No newline` marker none.
pub fn line_numbers(hunk: &Hunk) -> Vec<Option<usize>> {
    let mut next = hunk.new_start.max(1);
    hunk.lines
        .iter()
        .map(|line| match line.chars().next() {
            Some('\\') => None,
            Some('-') => Some(next),
            _ => {
                next += 1;
                Some(next - 1)
            }
        })
        .collect()
}

/// The new-side range covered by body lines `a` and `b` of `hunk` (either order), `None` when they hold no line.
pub fn selection_range(hunk: &Hunk, a: usize, b: usize) -> Option<(usize, usize)> {
    let numbers = line_numbers(hunk);
    let (lo, hi) = (a.min(b), a.max(b).min(numbers.len().checked_sub(1)?));
    let picked: Vec<usize> = numbers.get(lo..=hi)?.iter().flatten().copied().collect();
    Some((*picked.iter().min()?, *picked.iter().max()?))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Annotations {
    items: Vec<Annotation>,
}

/// What [`Annotations::batch`] made: the direction text and how many comments it carries (the first `sent` of
/// [`Annotations::ordered`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub text: String,
    pub sent: usize,
    pub left: usize,
}

impl Annotations {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Adds a comment on `start..=end` of `file` (either order). A blank comment or file adds nothing. A second comment on
    /// the very same lines replaces the first. `true` when something was added or replaced.
    pub fn add(&mut self, file: &str, start: usize, end: usize, comment: &str) -> bool {
        let comment: String = comment.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(COMMENT_MAX_CHARS).collect();
        if comment.is_empty() || file.trim().is_empty() || start == 0 || end == 0 {
            return false;
        }
        let (start, end) = (start.min(end), start.max(end));
        let item = Annotation { file: file.to_owned(), start, end, comment };
        match self.items.iter_mut().find(|a| a.file == item.file && a.start == start && a.end == end) {
            Some(existing) => *existing = item,
            None => self.items.push(item),
        }
        true
    }

    /// Removes the `index`th comment of [`Annotations::ordered`].
    pub fn remove(&mut self, index: usize) -> bool {
        let order = self.order();
        let Some(&at) = order.get(index) else { return false };
        self.items.remove(at);
        true
    }

    fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.items.len()).collect();
        order.sort_by(|&x, &y| (&self.items[x].file, self.items[x].start, self.items[x].end).cmp(&(&self.items[y].file, self.items[y].start, self.items[y].end)));
        order
    }

    /// The comments by file, then line: the order a batch is written in.
    pub fn ordered(&self) -> Vec<&Annotation> {
        self.order().into_iter().map(|i| &self.items[i]).collect()
    }

    /// Comments on `file`, by line.
    pub fn for_file(&self, file: &str) -> Vec<&Annotation> {
        self.ordered().into_iter().filter(|a| a.file == file).collect()
    }

    /// Everything that fits in `limit` characters as one direction. Comments past the limit are not cut mid-way: the
    /// text says how many were left out, and [`Batch::left`] has the count so the rest can go in the next batch.
    pub fn batch(&self, limit: usize) -> Option<Batch> {
        let all = self.ordered();
        if all.is_empty() {
            return None;
        }
        let mut text = HEADER.to_owned();
        let mut sent = 0;
        for (i, a) in all.iter().enumerate() {
            let line = format!("\n- {}: {}", a.place(), a.comment);
            let reserve = if i + 1 < all.len() { format!("\n(+{} more comments, sent next)", all.len() - i - 1).chars().count() } else { 0 };
            if text.chars().count() + line.chars().count() + reserve > limit {
                break;
            }
            text.push_str(&line);
            sent += 1;
        }
        if sent == 0 {
            return None;
        }
        let left = all.len() - sent;
        if left > 0 {
            text.push_str(&format!("\n(+{left} more comments, sent next)"));
        }
        Some(Batch { text, sent, left })
    }

    /// Drops the first `n` comments of [`Annotations::ordered`], the ones a batch carried.
    pub fn drop_sent(&mut self, n: usize) {
        for _ in 0..n.min(self.items.len()) {
            let first = self.order()[0];
            self.items.remove(first);
        }
    }
}

/// Which step of a run wrote these files: the one whose scope names most of them (the first on a tie), `None` when no
/// step's scope names any. A scope entry names a file exactly or as a directory prefix; globs are not read.
pub fn target_step<'a>(steps: &'a [Step], files: &[&str]) -> Option<&'a Step> {
    let named = |step: &Step, file: &str| {
        step.scope.iter().any(|entry| {
            let entry = entry.trim_start_matches("./").trim_end_matches('/');
            !entry.is_empty() && (file == entry || file.starts_with(&format!("{entry}/")))
        })
    };
    let mut best: Option<(&Step, usize)> = None;
    for step in steps {
        let n = files.iter().filter(|f| named(step, f)).count();
        if n > 0 && best.is_none_or(|(_, m)| n > m) {
            best = Some((step, n));
        }
    }
    best.map(|(s, _)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(new_start: usize, lines: &[&str]) -> Hunk {
        Hunk { header: String::new(), old_start: 1, old_lines: 0, new_start, new_lines: 0, lines: lines.iter().map(|s| (*s).to_owned()).collect() }
    }

    fn step(id: &str, scope: &[&str]) -> Step {
        Step { id: id.into(), name: id.into(), status: "completed".into(), worker: None, attempts: 1, scope: scope.iter().map(|s| (*s).to_owned()).collect(), deps: Vec::new(), last_event_at: None, blocked_reason: None, behavior: String::new(), because: String::new() }
    }

    #[test]
    fn line_numbers_follow_the_new_side() {
        let h = hunk(10, &[" keep", "-old", "+new one", "+new two", "\\ No newline at end of file", " tail"]);
        assert_eq!(line_numbers(&h), [Some(10), Some(11), Some(11), Some(12), None, Some(13)]);
    }

    #[test]
    fn a_selection_is_a_new_side_range_in_either_direction() {
        let h = hunk(10, &[" keep", "-old", "+new one", "+new two", " tail"]);
        assert_eq!(selection_range(&h, 2, 3), Some((11, 12)));
        assert_eq!(selection_range(&h, 3, 2), Some((11, 12)));
        assert_eq!(selection_range(&h, 1, 1), Some((11, 11)), "a deleted line sits where it was removed");
        assert_eq!(selection_range(&h, 0, 99), Some((10, 13)), "past the end is clamped");
        assert_eq!(selection_range(&hunk(1, &[]), 0, 0), None);
        assert_eq!(selection_range(&hunk(1, &["\\ No newline at end of file"]), 0, 0), None);
    }

    #[test]
    fn blank_comments_add_nothing_and_a_repeat_on_the_same_lines_replaces() {
        let mut a = Annotations::default();
        assert!(!a.add("src/a.rs", 3, 3, "   "));
        assert!(!a.add(" ", 3, 3, "x"));
        assert!(!a.add("src/a.rs", 0, 3, "x"));
        assert!(a.add("src/a.rs", 5, 3, "  rename\n this  "));
        assert_eq!(a.ordered()[0].comment, "rename this");
        assert_eq!((a.ordered()[0].start, a.ordered()[0].end), (3, 5));
        assert!(a.add("src/a.rs", 3, 5, "better name"));
        assert_eq!(a.len(), 1);
        assert_eq!(a.ordered()[0].comment, "better name");
        assert_eq!(a.ordered()[0].place(), "src/a.rs:3-5");
    }

    #[test]
    fn a_batch_is_ordered_by_file_then_line_and_carries_every_comment() {
        let mut a = Annotations::default();
        a.add("src/b.rs", 9, 9, "second file");
        a.add("src/a.rs", 20, 22, "later");
        a.add("src/a.rs", 4, 4, "earlier");
        let b = a.batch(DIRECTION_LIMIT).unwrap();
        assert_eq!((b.sent, b.left), (3, 0));
        let lines: Vec<&str> = b.text.lines().collect();
        assert_eq!(lines[1..], ["- src/a.rs:4: earlier", "- src/a.rs:20-22: later", "- src/b.rs:9: second file"]);
        assert!(Annotations::default().batch(DIRECTION_LIMIT).is_none());
    }

    #[test]
    fn a_batch_over_the_limit_splits_between_comments_and_the_rest_goes_next() {
        let mut a = Annotations::default();
        for i in 1..=8 {
            a.add("src/a.rs", i, i, &"x".repeat(300));
        }
        let first = a.batch(DIRECTION_LIMIT).unwrap();
        assert!(first.text.chars().count() <= DIRECTION_LIMIT, "{}", first.text.chars().count());
        assert!(first.sent >= 1 && first.left == 8 - first.sent);
        assert!(first.text.ends_with(&format!("(+{} more comments, sent next)", first.left)));
        assert!(first.text.contains("- src/a.rs:1: "), "the earliest comments go first");
        a.drop_sent(first.sent);
        assert_eq!(a.len(), first.left);
        assert_eq!(a.ordered()[0].start, first.sent + 1);
        let mut total = first.sent;
        while let Some(next) = a.batch(DIRECTION_LIMIT) {
            total += next.sent;
            a.drop_sent(next.sent);
        }
        assert_eq!((total, a.len()), (8, 0), "every comment goes out exactly once");
        let mut tiny = Annotations::default();
        tiny.add("f", 1, 1, "does not fit");
        assert!(tiny.batch(10).is_none());
    }

    #[test]
    fn remove_and_clear_follow_the_displayed_order() {
        let mut a = Annotations::default();
        a.add("b.rs", 1, 1, "b");
        a.add("a.rs", 1, 1, "a");
        assert!(a.remove(0));
        assert_eq!(a.ordered()[0].file, "b.rs");
        assert!(!a.remove(5));
        a.clear();
        assert!(a.is_empty());
    }

    #[test]
    fn the_step_that_wrote_most_of_the_files_is_the_target() {
        let steps = [step("s1", &["src/api/"]), step("s2", &["src/ui", "README.md"]), step("s3", &["docs/"])];
        assert_eq!(target_step(&steps, &["src/ui/a.rs", "src/ui/b.rs", "src/api/x.rs"]).map(|s| s.id.as_str()), Some("s2"));
        assert_eq!(target_step(&steps, &["README.md"]).map(|s| s.id.as_str()), Some("s2"));
        assert_eq!(target_step(&steps, &["src/uix/a.rs"]), None, "a prefix is a directory, not a substring");
        assert_eq!(target_step(&steps, &["elsewhere.rs"]), None);
    }
}
