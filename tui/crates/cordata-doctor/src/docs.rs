//! Doc staleness: which sections of `ARCHITECTURE.md` and `docs/*.md` describe files that changed since the section was
//! last reviewed. A section opts in with a marker anywhere under its heading:
//!
//! ```text
//! <!-- covers: tui/crates/cordata-orchestrator, docs/settings.md @ 3ca8154 -->
//! ```
//!
//! The paths are files or directories (relative to the repo root); the revision is the commit the section was last
//! checked against. A section is out of date when any covered path differs from that revision in the working tree, or
//! has a new untracked file. A marker without a revision is out of date until someone reviews the section and stamps it
//! (`cordata doctor --docs --stamp`). Deterministic: git decides, no model reads the docs.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Section {
    pub file: String,
    pub heading: String,
    /// 1-based line of the marker.
    pub line: usize,
    pub paths: Vec<String>,
    pub rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stale {
    pub file: String,
    pub heading: String,
    pub line: usize,
    pub why: String,
    pub changed: Vec<String>,
}

/// `(paths, rev)` of a marker line, or `None` when the line is not one.
fn marker(line: &str) -> Option<(Vec<String>, Option<String>)> {
    let inner = line.trim().strip_prefix("<!--")?.strip_suffix("-->")?.trim().strip_prefix("covers:")?;
    let (paths, rev) = match inner.rsplit_once('@') {
        Some((paths, rev)) => (paths, Some(rev.trim().to_owned()).filter(|r| !r.is_empty())),
        None => (inner, None),
    };
    let paths: Vec<String> = paths.split(',').map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()).collect();
    (!paths.is_empty()).then_some((paths, rev))
}

pub fn parse(file: &str, text: &str) -> Vec<Section> {
    let mut heading = String::from("(top of file)");
    let mut in_fence = false;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if in_fence {
            continue;
        }
        if line.starts_with('#') {
            heading = line.trim_start_matches('#').trim().to_owned();
        } else if let Some((paths, rev)) = marker(line) {
            out.push(Section { file: file.to_owned(), heading: heading.clone(), line: i + 1, paths, rev });
        }
    }
    out
}

/// `ARCHITECTURE.md` and the top-level `docs/*.md`, in a fixed order.
pub fn doc_files(repo: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = vec![repo.join("ARCHITECTURE.md")];
    let mut docs: Vec<PathBuf> = std::fs::read_dir(repo.join("docs")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "md")).collect();
    docs.sort();
    files.extend(docs);
    files.retain(|f| f.is_file());
    files
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn safe_path(p: &str) -> bool {
    !p.starts_with('/') && !p.starts_with('-') && !p.split('/').any(|c| c == "..")
}

fn stale_one(repo: &Path, s: &Section) -> Option<Stale> {
    let stale = |why: &str, changed: Vec<String>| Some(Stale { file: s.file.clone(), heading: s.heading.clone(), line: s.line, why: why.to_owned(), changed });
    let Some(rev) = s.rev.as_deref() else { return stale("no revision stamped: review the section, then run `cordata doctor --docs --stamp`", Vec::new()) };
    if let Some(bad) = s.paths.iter().find(|p| !safe_path(p)) {
        return stale(&format!("path {bad:?} must be relative to the repo root"), Vec::new());
    }
    if rev.starts_with('-') || git(repo, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")]).is_none() {
        return stale(&format!("revision {rev} is not in this repository"), Vec::new());
    }
    let mut changed: Vec<String> = Vec::new();
    let mut args = vec!["diff", "--name-only", rev, "--"];
    args.extend(s.paths.iter().map(String::as_str));
    changed.extend(git(repo, &args)?.lines().map(str::to_owned));
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "--"];
    args.extend(s.paths.iter().map(String::as_str));
    changed.extend(git(repo, &args).unwrap_or_default().lines().map(str::to_owned));
    changed.sort();
    changed.dedup();
    (!changed.is_empty()).then(|| Stale { why: format!("{} covered file(s) changed since {rev}", changed.len()), changed, ..stale("", Vec::new()).expect("built") })
}

/// How many sections of the repo's docs carry a marker.
pub fn marked(repo: &Path) -> usize {
    doc_files(repo).iter().filter_map(|f| std::fs::read_to_string(f).ok()).map(|t| parse("", &t).len()).sum()
}

/// Every marked section of the repo's docs that is out of date. `None` when `repo` is not a git work tree.
pub fn check(repo: &Path) -> Option<Vec<Stale>> {
    git(repo, &["rev-parse", "--is-inside-work-tree"])?;
    let mut out = Vec::new();
    for file in doc_files(repo) {
        let name = file.strip_prefix(repo).unwrap_or(&file).to_string_lossy().into_owned();
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        out.extend(parse(&name, &text).iter().filter_map(|s| stale_one(repo, s)));
    }
    Some(out)
}

/// Sets every marker's revision to the current HEAD: the person's statement that the docs match the code as it is.
/// Returns how many markers were rewritten.
pub fn stamp(repo: &Path) -> Result<usize, String> {
    let head = git(repo, &["rev-parse", "--short", "HEAD"]).map(|h| h.trim().to_owned()).ok_or("not a git repository with a commit")?;
    let mut count = 0;
    for file in doc_files(repo) {
        let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let mut in_fence = false;
        let rewritten: Vec<String> = text
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("```") {
                    in_fence = !in_fence;
                }
                match (in_fence, marker(line)) {
                    (false, Some((paths, _))) => {
                        count += 1;
                        format!("<!-- covers: {} @ {head} -->", paths.join(", "))
                    }
                    _ => line.to_owned(),
                }
            })
            .collect();
        let trailing = if text.ends_with('\n') { "\n" } else { "" };
        std::fs::write(&file, rewritten.join("\n") + trailing).map_err(|e| format!("{}: {e}", file.display()))?;
    }
    Ok(count)
}

pub fn render(stale: &[Stale]) -> String {
    if stale.is_empty() {
        return "docs: every marked section is current".into();
    }
    let mut lines = vec![format!("docs: {} section(s) out of date", stale.len())];
    for s in stale {
        lines.push(format!("  {}:{}  {} — {}", s.file, s.line, s.heading, s.why));
        lines.extend(s.changed.iter().take(5).map(|c| format!("      {c}")));
        if s.changed.len() > 5 {
            lines.push(format!("      … and {} more", s.changed.len() - 5));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_in(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn a_marker_names_its_section_its_paths_and_its_revision() {
        let text = "# Title\n<!-- covers: a/b, c.rs @ abc123 -->\n```\n<!-- covers: ignored -->\n```\n## Next\n<!-- covers: d -->\n";
        let found = parse("docs/x.md", text);
        assert_eq!(found.len(), 2, "a marker inside a code fence is an example, not a marker");
        assert_eq!((found[0].heading.as_str(), found[0].paths.clone(), found[0].rev.as_deref()), ("Title", vec!["a/b".into(), "c.rs".into()], Some("abc123")));
        assert_eq!((found[1].heading.as_str(), found[1].rev.clone()), ("Next", None));
    }

    #[test]
    fn a_section_goes_out_of_date_when_a_covered_file_changes_and_current_again_once_stamped() {
        if Command::new("git").arg("--version").output().is_err() {
            return eprintln!("skipped: git is not installed");
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git_in(repo, &["init", "-q"]);
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/a.rs"), "one\n").unwrap();
        std::fs::write(repo.join("docs/a.md"), "# A\n<!-- covers: src -->\ntext\n").unwrap();
        git_in(repo, &["add", "."]);
        git_in(repo, &["commit", "-qm", "first"]);

        let unstamped = check(repo).unwrap();
        assert_eq!(unstamped.len(), 1);
        assert!(unstamped[0].why.starts_with("no revision stamped"), "{unstamped:?}");

        assert_eq!(stamp(repo), Ok(1));
        assert_eq!(check(repo), Some(Vec::new()), "stamped at HEAD with nothing changed");
        assert!(std::fs::read_to_string(repo.join("docs/a.md")).unwrap().ends_with("text\n"), "the rest of the doc is untouched");

        std::fs::write(repo.join("src/a.rs"), "two\n").unwrap();
        std::fs::write(repo.join("src/new.rs"), "x\n").unwrap();
        let changed = check(repo).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].changed, vec!["src/a.rs".to_owned(), "src/new.rs".to_owned()]);
        assert!(render(&changed).contains("docs/a.md:2  A — 2 covered file(s) changed since"), "{}", render(&changed));
    }

    #[test]
    fn a_path_that_leaves_the_repo_is_refused() {
        let s = Section { file: "d.md".into(), heading: "H".into(), line: 1, paths: vec!["../x".into()], rev: Some("HEAD".into()) };
        let dir = tempfile::tempdir().unwrap();
        assert!(stale_one(dir.path(), &s).unwrap().why.contains("relative to the repo root"));
    }
}
