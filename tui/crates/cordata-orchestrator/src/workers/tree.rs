//! What a worker changed: a cheap before/after snapshot of the project, and git's view of
//! it. A snapshot is `{relative path: (size, mtime_ns)}` minus dependency/build directories and secrets files — it
//! sees a change in any directory, git repository or not, tracked or not, and a second edit to an already-dirty file.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

use crate::scope;

const SKIP_DIRS: [&str; 13] = [".git", "node_modules", "target", ".venv", "venv", "__pycache__", ".next", "dist", "build", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox"];
const MAX_FILES: usize = 50_000;

pub type Snapshot = BTreeMap<String, (u64, u128)>;

/// `None` when the tree is too large to walk cheaply; callers then report "unknown", never "no change".
pub fn snapshot(root: &Path) -> Option<Snapshot> {
    let mut out = Snapshot::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(entry.path());
                }
            } else if kind.is_file() && !scope::is_secret_path(&name) {
                let Ok(meta) = entry.metadata() else { continue };
                let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
                if let Ok(rel) = entry.path().strip_prefix(root) {
                    out.insert(rel.to_string_lossy().into_owned(), (meta.len(), mtime));
                }
                if out.len() > MAX_FILES {
                    return None;
                }
            }
        }
    }
    Some(out)
}

pub fn changed(before: Option<&Snapshot>, after: Option<&Snapshot>) -> Option<Vec<String>> {
    let (before, after) = (before?, after?);
    let mut paths: Vec<String> = before.keys().chain(after.keys()).filter(|p| before.get(*p) != after.get(*p)).cloned().collect();
    paths.sort();
    paths.dedup();
    Some(paths)
}

fn git(root: &Path, args: &[&str]) -> Option<(bool, String)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()))
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    Some((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

pub fn is_git_repo(root: &Path) -> bool {
    git(root, &["rev-parse", "--is-inside-work-tree"]).is_some_and(|(ok, out)| ok && out.trim() == "true")
}

/// `git diff --stat` for the tracked ones among `paths` plus a count of new files. `""` outside git.
pub fn diff_stat(root: &Path, paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    if !is_git_repo(root) {
        return format!("{} file(s) changed", paths.len());
    }
    let limited: Vec<&str> = paths.iter().take(500).map(String::as_str).collect();
    let args = |head: &[&'static str]| -> Vec<String> { head.iter().map(|s| (*s).to_owned()).chain(limited.iter().map(|s| (*s).to_owned())).collect() };
    let run = |a: Vec<String>| git(root, &a.iter().map(String::as_str).collect::<Vec<_>>());
    let mut stat = run(args(&["diff", "--stat", "HEAD", "--"])).filter(|(ok, out)| *ok && !out.trim().is_empty()).and_then(|(_, out)| out.trim().lines().last().map(|l| l.trim().to_owned())).unwrap_or_default();
    let known: std::collections::HashSet<String> = run(args(&["ls-files", "--"])).filter(|(ok, _)| *ok).map(|(_, out)| out.split_whitespace().map(str::to_owned).collect()).unwrap_or_default();
    let new = paths.iter().filter(|p| !known.contains(*p)).count();
    if new > 0 {
        stat = format!("{}{new} new file(s)", if stat.is_empty() { String::new() } else { format!("{stat}, ") });
    }
    stat
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_edits_creations_and_deletions_but_not_secrets_or_build_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/a.rs"), "one").unwrap();
        std::fs::write(root.join("gone.txt"), "x").unwrap();
        let before = snapshot(root);
        std::fs::write(root.join("src/a.rs"), "three").unwrap();
        std::fs::write(root.join("new.txt"), "n").unwrap();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        std::fs::write(root.join("target/out"), "o").unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        let after = snapshot(root);
        assert_eq!(changed(before.as_ref(), after.as_ref()).unwrap(), ["gone.txt", "new.txt", "src/a.rs"]);
        assert_eq!(changed(None, after.as_ref()), None);
    }

    #[test]
    fn diff_stat_outside_git_counts_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(diff_stat(dir.path(), &[]), "");
        assert_eq!(diff_stat(dir.path(), &["a".into(), "b".into()]), "2 file(s) changed");
    }

    #[test]
    fn diff_stat_in_git_reports_tracked_changes_and_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(root).args(args).output().unwrap().status.success(), "git {args:?}");
        run(&["init", "-q"]);
        std::fs::write(root.join("a.txt"), "1\n").unwrap();
        run(&["add", "."]);
        run(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
        std::fs::write(root.join("a.txt"), "1\n2\n").unwrap();
        std::fs::write(root.join("b.txt"), "n").unwrap();
        let stat = diff_stat(root, &["a.txt".into(), "b.txt".into()]);
        assert!(stat.contains("1 file changed") && stat.ends_with("1 new file(s)"), "{stat}");
    }
}
