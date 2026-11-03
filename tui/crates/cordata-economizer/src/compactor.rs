//! Dual head/tail compaction of long tool output, and one-line digests for old results.

use std::path::{Path, PathBuf};

use crate::profile::Profile;

/// Share of the kept characters taken from the start; the rest comes from the end (1,500 + 2,500 at 4,000).
const HEAD_SHARE: (usize, usize) = (3, 8);
/// Aged results shorter than this are left alone.
const AGE_MIN_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    pub text: String,
    /// Where the full output was written, when it was compacted and the write worked.
    pub offloaded: Option<PathBuf>,
}

fn take_head(text: &str, chars: usize) -> &str {
    text.char_indices().nth(chars).map_or(text, |(i, _)| &text[..i])
}

fn take_tail(text: &str, chars: usize) -> &str {
    let total = text.chars().count();
    text.char_indices().nth(total.saturating_sub(chars)).map_or("", |(i, _)| &text[i..])
}

/// Output over `limit` characters keeps its first and last parts (two thirds of `limit` in all);
/// the full text goes to `offload_dir/<id>.txt`.
pub fn compact(output: &str, limit: usize, offload_dir: &Path) -> Compacted {
    let total = output.chars().count();
    if total <= limit {
        return Compacted { text: output.to_owned(), offloaded: None };
    }
    let keep = limit * 2 / 3;
    let head_len = keep * HEAD_SHARE.0 / HEAD_SHARE.1;
    let tail_len = keep - head_len;
    let file = offload_dir.join(format!("{}.txt", crate::short_id(output)));
    let written = std::fs::create_dir_all(offload_dir).and_then(|()| std::fs::write(&file, output)).is_ok();
    let handle = if written {
        format!("full output offloaded to {} — read it with read_file offset/limit", file.display())
    } else {
        "truncated, offload failed".to_owned()
    };
    let text = format!(
        "{}\n…[{} of {total} chars omitted; {handle}]…\n{}",
        take_head(output, head_len),
        total - head_len - tail_len,
        take_tail(output, tail_len)
    );
    Compacted { text, offloaded: written.then_some(file) }
}

/// One line standing in for an old result: its first meaningful line and its size.
pub fn digest(output: &str) -> String {
    let first = output.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("(empty)");
    let first: String = first.chars().take(100).collect();
    format!("{first} …[aged: {} lines, {} chars; run the tool again if you need it]", output.lines().count(), output.chars().count())
}

/// The replacement for a result that is `turns_old` turns old, or `None` to keep it as it is.
pub fn age(tool: &str, output: &str, turns_old: usize, profile: &Profile) -> Option<String> {
    let is_read = matches!(tool, "read_file" | "code_search" | "code_outline" | "code_map");
    let eligible = turns_old > profile.age_after_turns && (!profile.age_reads_only || is_read);
    (eligible && output.chars().count() >= AGE_MIN_CHARS && !output.contains("…[aged:")).then(|| digest(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let out = compact("hello", 6_000, dir.path());
        assert_eq!(out, Compacted { text: "hello".into(), offloaded: None });
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn long_output_keeps_head_and_tail_and_offloads_the_whole() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("START{}END", "m".repeat(20_000));
        let out = compact(&body, 6_000, dir.path());
        assert!(out.text.starts_with("START") && out.text.ends_with("END"));
        assert!(out.text.chars().count() < 4_300, "{}", out.text.chars().count());
        let file = out.offloaded.expect("offloaded");
        assert_eq!(std::fs::read_to_string(file).unwrap(), body);
        assert!(out.text.contains("offloaded to"));
    }

    #[test]
    fn head_is_1500_and_tail_2500_at_the_standard_limit() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (0..10_000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let out = compact(&body, 6_000, dir.path());
        let (head, rest) = out.text.split_once('\n').unwrap();
        assert_eq!(head.chars().count(), 1_500);
        assert_eq!(rest.rsplit_once('\n').unwrap().1.chars().count(), 2_500);
    }

    #[test]
    fn multibyte_text_is_cut_on_char_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let out = compact(&"é€😀".repeat(5_000), 3_000, dir.path());
        assert!(out.text.contains("omitted"));
    }

    #[test]
    fn same_output_offloads_to_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let body = "z".repeat(9_000);
        assert_eq!(compact(&body, 3_000, dir.path()).offloaded, compact(&body, 3_000, dir.path()).offloaded);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn digests_are_one_line() {
        let d = digest("\n\nerror[E0432]: unresolved import\n  --> src/lib.rs\n");
        assert!(d.starts_with("error[E0432]: unresolved import") && !d.contains('\n'));
    }

    #[test]
    fn aging_follows_the_provider_profile() {
        let big = "line\n".repeat(100);
        let ollama = Profile::for_model("llama3");
        let claude = Profile::for_model("claude-x");
        let gemini = Profile::for_model("gemini-x");
        assert!(age("run_command", &big, 2, &ollama).is_some());
        assert!(age("run_command", &big, 2, &claude).is_none());
        assert!(age("run_command", &big, 4, &claude).is_some());
        assert!(age("run_command", &big, 4, &gemini).is_none()); // Gemini ages raw reads only
        assert!(age("read_file", &big, 4, &gemini).is_some());
        assert!(age("read_file", "tiny", 9, &ollama).is_none());
        let aged = age("read_file", &big, 4, &gemini).unwrap();
        assert!(age("read_file", &aged, 9, &gemini).is_none(), "ageing is idempotent");
    }
}
