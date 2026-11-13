//! Reviewing what changed in the working tree, hunk by hunk (docs/app.md §3 D4: per-hunk agent-edit review).
//!
//! The agent's edits land in the working tree; this reads `git diff` (working tree against the index), splits it into
//! files and hunks, and acts on one hunk at a time: **keep** stages it (`git apply --cached`), so what is left in the
//! diff is what has not been reviewed; **revert** takes it back out of the file (`git apply -R`). Both apply a patch made
//! of that file's header and that one hunk, read from the same diff, so nothing else in the file moves.
//!
//! Git runs with the repository's code-running knobs off — no external diff drivers, no textconv filters, no fsmonitor
//! hook — because the folder may not be trusted and reading a diff must not run anything it configured
//! (THREAT_MODEL.md: an untrusted folder runs none of its own code).

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// A diff bigger than this is shown by its first files only.
const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@ …` line.
    pub header: String,
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    /// The hunk's body, each line with its ` `, `+`, `-` or `\` prefix.
    pub lines: Vec<String>,
}

impl Hunk {
    pub fn additions(&self) -> usize {
        self.lines.iter().filter(|l| l.starts_with('+')).count()
    }

    pub fn deletions(&self) -> usize {
        self.lines.iter().filter(|l| l.starts_with('-')).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Modified,
    Added,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileDiff {
    /// The path in the working tree (the new name of a rename), relative to the repository's top.
    pub path: String,
    /// The lines before the first hunk (`diff --git`, `index`, modes, `---`, `+++`).
    pub header: Vec<String>,
    pub hunks: Vec<Hunk>,
    pub binary: bool,
    pub status: Status,
}

impl FileDiff {
    pub fn additions(&self) -> usize {
        self.hunks.iter().map(Hunk::additions).sum()
    }

    pub fn deletions(&self) -> usize {
        self.hunks.iter().map(Hunk::deletions).sum()
    }

    /// This file's header and its hunk `index`, as a patch `git apply` takes.
    pub fn hunk_patch(&self, index: usize) -> Option<String> {
        let hunk = self.hunks.get(index)?;
        let mut out = String::new();
        for line in self.header.iter().chain(std::iter::once(&hunk.header)).chain(&hunk.lines) {
            out.push_str(line);
            out.push('\n');
        }
        Some(out)
    }

    /// The whole file's patch.
    pub fn patch(&self) -> String {
        let mut out = String::new();
        for line in &self.header {
            out.push_str(line);
            out.push('\n');
        }
        for hunk in &self.hunks {
            out.push_str(&hunk.header);
            out.push('\n');
            for line in &hunk.lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

}

/// Visual classification of a contiguous diff run inside a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
}

/// Contiguous diff lines of the same [`LineKind`], pre-sanitized and secret-scrubbed once off the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayRun {
    pub kind: LineKind,
    pub text: String,
}

const HUNK_PREVIEW_THRESHOLD: usize = 44;
const HUNK_PREVIEW_LINES: usize = 28;

/// Pre-scrubbed hunk ready for zero-regex 120Hz frame rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayHunk {
    pub header: String,
    pub new_start: usize,
    pub runs: Vec<DisplayRun>,
    pub preview_runs: Vec<DisplayRun>,
    pub hidden_lines: usize,
}

fn group_runs(lines: &[String]) -> Vec<DisplayRun> {
    let mut runs: Vec<DisplayRun> = Vec::new();
    for line in lines {
        let (pfx, rest) = line.split_at(line.chars().next().map_or(0, char::len_utf8));
        let kind = match pfx {
            "+" => LineKind::Add,
            "-" => LineKind::Del,
            _ => LineKind::Context,
        };
        let cleaned = crate::redact::scrub(&crate::app::sanitize(rest)).0.replace('\n', " ");
        let formatted = format!("{pfx}{cleaned}");
        match runs.last_mut() {
            Some(last) if last.kind == kind => {
                last.text.push('\n');
                last.text.push_str(&formatted);
            }
            _ => runs.push(DisplayRun { kind, text: formatted }),
        }
    }
    runs
}

/// Pre-scrubbed file diff ready for zero-regex 120Hz frame rendering in Change Sheet (`⌘D`) and Review (`⌘⌥4`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayFile {
    pub path: String,
    pub status: Status,
    pub binary: bool,
    pub additions: usize,
    pub deletions: usize,
    pub sensitive: bool,
    pub hunks: Vec<DisplayHunk>,
}

/// Builds pre-scrubbed, run-grouped [`DisplayFile`]s once off the UI thread so every 120Hz UI redraw does zero regex
/// scrubbing and lays out ~10x fewer GPUI elements on multi-thousand-line diffs.
pub fn display_files(files: &[FileDiff]) -> Vec<DisplayFile> {
    files
        .iter()
        .map(|file| {
            let hunks = file
                .hunks
                .iter()
                .map(|hunk| {
                    let runs = group_runs(&hunk.lines);
                    let (preview_runs, hidden_lines) = if hunk.lines.len() > HUNK_PREVIEW_THRESHOLD {
                        (group_runs(&hunk.lines[..HUNK_PREVIEW_LINES]), hunk.lines.len() - HUNK_PREVIEW_LINES)
                    } else {
                        (Vec::new(), 0)
                    };
                    DisplayHunk {
                        header: crate::redact::scrub(&crate::app::sanitize(&hunk.header)).0,
                        new_start: hunk.new_start,
                        runs,
                        preview_runs,
                        hidden_lines,
                    }
                })
                .collect();
            DisplayFile {
                path: crate::redact::scrub(&crate::app::sanitize(&file.path)).0,
                status: file.status.clone(),
                binary: file.binary,
                additions: file.additions(),
                deletions: file.deletions(),
                sensitive: crate::desk::is_sensitive_path(&file.path),
                hunks,
            }
        })
        .collect()
}

/// `git diff` output, split into files and hunks.
pub fn parse(patch: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            files.push(FileDiff { path: path_from_git_line(line).unwrap_or_default(), header: vec![line.to_owned()], ..Default::default() });
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        if let Some(hunk) = parse_hunk_header(line) {
            file.hunks.push(hunk);
            in_hunk = true;
            continue;
        }
        if in_hunk && matches!(line.chars().next(), Some(' ' | '+' | '-' | '\\') | None) {
            if let Some(hunk) = file.hunks.last_mut() {
                // `git diff` never emits an empty line inside a hunk; one here is a context line whose space was eaten.
                hunk.lines.push(if line.is_empty() { " ".to_owned() } else { line.to_owned() });
            }
            continue;
        }
        in_hunk = false;
        if let Some(path) = line.strip_prefix("+++ ") {
            if path != "/dev/null" {
                file.path = unquote(path).trim_start_matches("b/").to_owned();
            }
        } else if line.starts_with("new file mode") {
            file.status = Status::Added;
        } else if line.starts_with("deleted file mode") {
            file.status = Status::Deleted;
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.status = Status::Renamed;
            file.path = unquote(path);
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        }
        file.header.push(line.to_owned());
    }
    files
}

fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |text: &str| -> Option<(usize, usize)> {
        match text.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((text.parse().ok()?, 1)),
        }
    };
    let (old_start, old_lines) = range(old)?;
    let (new_start, new_lines) = range(new)?;
    Some(Hunk { header: line.to_owned(), old_start, old_lines, new_start, new_lines, lines: Vec::new() })
}

/// `diff --git a/x b/x` → `x` (the `b/` side). Quoted names are unquoted; a name with ` b/` in it falls back to the
/// `+++` line, read later.
fn path_from_git_line(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    if let Some(quoted) = rest.strip_prefix('"') {
        let end = quoted.find("\" ")?;
        return Some(unquote(quoted[end + 2..].trim()).trim_start_matches("b/").to_owned());
    }
    let (_, b) = rest.split_once(" b/")?;
    Some(b.to_owned())
}

/// Git's C-style quoting (`"a\tb"`, octal escapes) undone for the common cases.
fn unquote(text: &str) -> String {
    let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) else { return text.to_owned() };
    let mut bytes = Vec::new();
    let mut chars = inner.bytes().peekable();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(digit @ b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    if let Some(next @ b'0'..=b'7') = chars.peek().copied() {
                        value = value * 8 + u32::from(next - b'0');
                        chars.next();
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `git` in `cwd` with the repository's own code-running settings switched off.
pub fn git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotepath=false", "-c", "diff.external=", "-c", "color.ui=false"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_EXTERNAL_DIFF")
        .stdin(Stdio::null());
    command
}

/// The repository's top folder, or `None` outside a repository.
pub fn top_level(cwd: &Path) -> Option<std::path::PathBuf> {
    let output = git(cwd).args(["rev-parse", "--show-toplevel"]).stderr(Stdio::null()).output().ok()?;
    output.status.success().then(|| std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

/// What changed in the working tree and has not been staged, file by file; `Err` says why there is nothing to review
/// (not a repository, git missing).
pub fn working_changes(cwd: &Path) -> Result<Vec<FileDiff>, String> {
    let output = git(cwd)
        .args(["diff", "--no-color", "--no-ext-diff", "--no-textconv", "--src-prefix=a/", "--dst-prefix=b/", "-U3", "--"])
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        return Err(if why.to_lowercase().contains("not a git repository") { "not a git repository".to_owned() } else { why.lines().next().unwrap_or("git diff failed").to_owned() });
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.len() > MAX_DIFF_BYTES {
        let cut = text[..MAX_DIFF_BYTES].rfind("\ndiff --git ").unwrap_or(0);
        text.truncate(cut);
    }
    Ok(parse(&text))
}

/// Files the agent (or anyone) created that git does not track yet; they have no diff against the index.
pub fn untracked(cwd: &Path) -> Vec<String> {
    let Ok(output) = git(cwd).args(["ls-files", "-z", "--others", "--exclude-standard"]).stderr(Stdio::null()).output() else { return Vec::new() };
    String::from_utf8_lossy(&output.stdout).split('\0').filter(|name| !name.is_empty()).map(str::to_owned).collect()
}

/// What to do with one hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Stage it: reviewed and kept.
    Keep,
    /// Take it out of the working tree.
    Revert,
}

/// Apply `action` to `file`'s hunk `index`. `cwd` is anywhere in the repository (patch paths are from its top).
pub fn apply(cwd: &Path, file: &FileDiff, index: usize, action: Action) -> Result<(), String> {
    let patch = file.hunk_patch(index).ok_or("no such hunk")?;
    apply_patch(cwd, file, &patch, action)
}

/// Apply `action` to every hunk of `file` at once.
pub fn apply_file(cwd: &Path, file: &FileDiff, action: Action) -> Result<(), String> {
    apply_patch(cwd, file, &file.patch(), action)
}

fn apply_patch(cwd: &Path, file: &FileDiff, patch: &str, action: Action) -> Result<(), String> {
    if file.binary || file.hunks.is_empty() {
        return Err("a binary file (or a mode change) is reviewed outside the app".into());
    }
    let top = top_level(cwd).ok_or("not a git repository")?;
    let flags: &[&str] = match action {
        Action::Keep => &["--cached"],
        Action::Revert => &["-R"],
    };
    // Check first: a hunk that no longer applies (the file moved on since the diff was read) changes nothing.
    run_apply(&top, flags, &["--check"], patch)?;
    run_apply(&top, flags, &[], patch)
}

fn run_apply(top: &Path, flags: &[&str], extra: &[&str], patch: &str) -> Result<(), String> {
    let mut child = git(top)
        .args(["apply", "--whitespace=nowarn", "--unidiff-zero"])
        .args(flags)
        .args(extra)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("git apply: {error}"))?;
    child.stdin.take().ok_or("git apply: no stdin")?.write_all(patch.as_bytes()).map_err(|error| format!("git apply: {error}"))?;
    let output = child.wait_with_output().map_err(|error| format!("git apply: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let why = String::from_utf8_lossy(&output.stderr);
        Err(why.lines().find(|l| l.starts_with("error:")).or_else(|| why.lines().next()).unwrap_or("git apply failed").trim_start_matches("error: ").to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 fn a() {}
-fn b() {}
+fn b() { todo!() }
 fn c() {}
@@ -10,2 +10,3 @@ fn j() {}
 fn k() {}
+fn l() {}
 fn m() {}
diff --git a/old name.txt b/old name.txt
deleted file mode 100644
index 3333333..0000000
--- a/old name.txt
+++ /dev/null
@@ -1 +0,0 @@
-gone
\\ No newline at end of file
diff --git a/logo.png b/logo.png
index 4444444..5555555 100644
Binary files a/logo.png and b/logo.png differ
";

    #[test]
    fn a_diff_splits_into_files_and_hunks() {
        let files = parse(PATCH);
        assert_eq!(files.len(), 3);
        let lib = &files[0];
        assert_eq!(lib.path, "src/lib.rs");
        assert_eq!(lib.hunks.len(), 2);
        assert_eq!((lib.hunks[1].old_start, lib.hunks[1].old_lines, lib.hunks[1].new_start, lib.hunks[1].new_lines), (10, 2, 10, 3));
        assert_eq!((lib.additions(), lib.deletions()), (2, 1));
        assert_eq!(files[1].path, "old name.txt");
        assert_eq!(files[1].status, Status::Deleted);
        assert_eq!(files[1].hunks[0].lines.last().map(String::as_str), Some("\\ No newline at end of file"));
        assert!(files[2].binary && files[2].hunks.is_empty());
    }

    #[test]
    fn one_hunk_is_a_patch_of_its_own() {
        let files = parse(PATCH);
        let second = files[0].hunk_patch(1).unwrap();
        assert!(second.starts_with("diff --git a/src/lib.rs b/src/lib.rs\nindex 1111111..2222222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,2 +10,3 @@"));
        assert!(!second.contains("todo!()"), "the other hunk is not in it");
        assert!(files[0].hunk_patch(2).is_none());
    }

    #[test]
    fn quoted_names_are_unquoted() {
        assert_eq!(unquote("\"a\\tb\""), "a\tb");
        assert_eq!(unquote("\"caf\\303\\251\""), "café");
        assert_eq!(path_from_git_line("diff --git \"a/x y\" \"b/x y\"").as_deref(), Some("x y"));
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| assert!(Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t"]).args(args).current_dir(dir.path()).output().unwrap().status.success(), "{args:?}");
        run(&["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), (1..=20).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "init"]);
        dir
    }

    #[test]
    fn keep_stages_one_hunk_and_revert_takes_one_back() {
        let dir = repo();
        let file = dir.path().join("a.txt");
        let edited = std::fs::read_to_string(&file).unwrap().replace("line 2\n", "line two\n").replace("line 18\n", "line eighteen\n");
        std::fs::write(&file, edited).unwrap();

        let files = working_changes(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks.len(), 2, "two edits far apart are two hunks");

        apply(dir.path(), &files[0], 0, Action::Keep).unwrap();
        let after_keep = working_changes(dir.path()).unwrap();
        assert_eq!(after_keep[0].hunks.len(), 1, "the kept hunk left the unreviewed diff");
        assert!(after_keep[0].hunks[0].lines.iter().any(|l| l == "+line eighteen"));
        assert!(std::fs::read_to_string(&file).unwrap().contains("line two"), "keeping does not touch the file");

        apply(dir.path(), &after_keep[0], 0, Action::Revert).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("line 18\n") && !text.contains("eighteen"), "the reverted hunk is gone from the file");
        assert!(text.contains("line two"), "…and the other one is still there");
        assert!(working_changes(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn a_stale_hunk_changes_nothing() {
        let dir = repo();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, std::fs::read_to_string(&file).unwrap().replace("line 5\n", "line five\n")).unwrap();
        let files = working_changes(dir.path()).unwrap();
        std::fs::write(&file, std::fs::read_to_string(&file).unwrap().replace("line five\n", "line 5 again\n")).unwrap();
        let before = std::fs::read_to_string(&file).unwrap();
        assert!(apply(dir.path(), &files[0], 0, Action::Revert).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    #[test]
    fn untracked_files_are_listed_and_outside_git_says_so() {
        let dir = repo();
        std::fs::write(dir.path().join("new.rs"), "fn main() {}\n").unwrap();
        assert_eq!(untracked(dir.path()), vec!["new.rs".to_owned()]);
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(working_changes(plain.path()).unwrap_err(), "not a git repository");
    }

    #[test]
    fn display_files_groups_contiguous_runs_masks_secrets_and_flags_sensitive_paths() {
        let key = format!("sk-ant-api03-{}", "Q7zR9kMp3XwB".repeat(4));
        let patch = format!(
            "\
diff --git a/Cargo.toml b/Cargo.toml
index 1111111..2222222 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1,4 +1,5 @@
 [package]
 name = \"demo\"
-version = \"0.1.0\"
+version = \"0.2.0\"
+# key = {key}
 edition = \"2024\"
"
        );
        let parsed = parse(&patch);
        let disp = display_files(&parsed);
        assert_eq!(disp.len(), 1);
        assert!(disp[0].sensitive, "Cargo.toml is flagged sensitive");
        assert_eq!((disp[0].additions, disp[0].deletions), (2, 1));
        let runs = &disp[0].hunks[0].runs;
        assert_eq!(
            runs.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![LineKind::Context, LineKind::Del, LineKind::Add, LineKind::Context]
        );
        assert!(runs[0].text.contains(" [package]\n name = \"demo\""));
        assert!(!runs[2].text.contains(&key) && runs[2].text.contains("[REDACTED]"));
    }
}
