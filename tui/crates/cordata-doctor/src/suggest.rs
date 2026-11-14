//! `covers:` suggestions: for each doc section with no marker, the code the section most likely describes, ranked by
//! the repo map (`cordata-repomap`: definitions, a reference graph and BM25 over names and comments). A suggestion is a
//! marker line to paste under the heading, never written for you: whether the section really covers that code is the
//! reviewer's call. Paths the section names itself (in backticks) and that exist come first.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    pub file: String,
    pub heading: String,
    pub line: usize,
    pub paths: Vec<String>,
}

struct Part {
    heading: String,
    line: usize,
    body: String,
    marked: bool,
}

fn parts(text: &str) -> Vec<Part> {
    let mut out: Vec<Part> = Vec::new();
    let mut in_fence = false;
    for (i, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with('#') {
            out.push(Part { heading: line.trim_start_matches('#').trim().to_owned(), line: i + 1, body: String::new(), marked: false });
        } else if let Some(part) = out.last_mut() {
            part.marked |= !in_fence && line.trim_start().starts_with("<!-- covers:");
            part.body.push_str(line);
            part.body.push('\n');
        }
    }
    out
}

fn stems(text: &str) -> std::collections::HashSet<String> {
    cordata_repomap::words(text).iter().map(|w| cordata_repomap::stem(w)).collect()
}

/// `tui/crates/<name>/…` and `app/crates/<name>/…` count as their crate; any other file stands for itself.
fn group(path: &str) -> String {
    let mut parts = path.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(top @ ("tui" | "app")), Some("crates"), Some(name)) => format!("{top}/crates/{name}"),
        _ => path.to_owned(),
    }
}

/// Backticked tokens of `body` that look like paths and exist under `root`.
fn named(root: &Path, body: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for token in body.split('`').skip(1).step_by(2) {
        let token = token.trim().trim_end_matches('/');
        if token.contains('/') && !token.contains(' ') && !token.starts_with('/') && !token.contains("..") && root.join(token).exists() && !found.iter().any(|f| f == token) {
            found.push(token.to_owned());
        }
    }
    found
}

/// At most `per_section` paths for every unmarked, non-trivial section of the repo's docs.
pub fn suggest(repo: &Path, per_section: usize) -> Vec<Suggestion> {
    let mut map = cordata_repomap::RepoMap::new(repo);
    map.refresh();
    let mut out = Vec::new();
    for file in crate::docs::doc_files(repo) {
        let name = file.strip_prefix(repo).unwrap_or(&file).to_string_lossy().into_owned();
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        for part in parts(&text).into_iter().filter(|p| !p.marked && p.body.split_whitespace().count() >= 12) {
            let mut paths = named(repo, &part.body);
            let query: String = format!("{} {}", part.heading, part.body).chars().take(2000).collect();
            let mentioned: Vec<&str> = paths.iter().map(String::as_str).collect();
            let wanted = stems(&query);
            let mut score: BTreeMap<String, f64> = BTreeMap::new();
            // The map ranks by graph importance too, so a hit counts only when its own names or comment share a word with
            // the section: otherwise the busiest code in the repo would be suggested for every section.
            for hit in map.search(&query, &mentioned, 40).into_iter().filter(|h| stems(&format!("{} {}", h.def.name, h.def.doc)).iter().any(|w| wanted.contains(w))) {
                *score.entry(group(&hit.path.to_string_lossy())).or_default() += hit.score;
            }
            let mut ranked: Vec<(String, f64)> = score.into_iter().filter(|(g, s)| *s > 0.0 && !name.ends_with(g.as_str())).collect();
            ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            for (g, _) in ranked {
                if paths.len() >= per_section {
                    break;
                }
                if !paths.contains(&g) {
                    paths.push(g);
                }
            }
            paths.truncate(per_section);
            if !paths.is_empty() {
                out.push(Suggestion { file: name.clone(), heading: part.heading, line: part.line, paths });
            }
        }
    }
    out
}

pub fn render(found: &[Suggestion]) -> String {
    if found.is_empty() {
        return "docs: nothing to suggest — every section has a marker or too little text".into();
    }
    let mut lines = vec![format!("docs: {} section(s) without a marker; paste the line under the heading after checking it", found.len())];
    for s in found {
        lines.push(format!("  {}:{}  {}", s.file, s.line, s.heading));
        lines.push(format!("      <!-- covers: {} -->", s.paths.join(", ")));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_section_that_names_a_path_gets_it_first_and_the_repo_map_adds_the_code_it_describes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (rel, body) in [
            ("tui/crates/ledger/src/lib.rs", "/// Charges the ledger for one metered request.\npub fn charge_ledger_request() {}\n"),
            ("tui/crates/socket/src/lib.rs", "/// Opens a listening socket.\npub fn open_socket_listener() {}\n"),
        ] {
            std::fs::create_dir_all(root.join(rel).parent().unwrap()).unwrap();
            std::fs::write(root.join(rel), body).unwrap();
        }
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(
            root.join("docs/ledger.md"),
            "# Ledger\n\n## Charging\n\nEvery metered request is charged to the ledger before the next one starts, and the charge is kept per run in `tui/crates/ledger`.\n\n## Done\n<!-- covers: tui/crates/socket -->\nThis section already has a marker so it is not suggested again, whatever it says about sockets listening.\n",
        )
        .unwrap();
        let found = suggest(root, 2);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].heading.as_str(), found[0].paths[0].as_str()), ("Charging", "tui/crates/ledger"));
        assert!(!found[0].paths.iter().any(|p| p.contains("socket")), "{found:?}");
        assert!(render(&found).contains("<!-- covers: tui/crates/ledger"), "{}", render(&found));
    }

    #[test]
    fn crates_group_by_crate_and_other_files_stand_alone() {
        assert_eq!(group("tui/crates/cordata-run/src/cli.rs"), "tui/crates/cordata-run");
        assert_eq!(group("app/crates/cordata-view/src/runs.rs"), "app/crates/cordata-view");
        assert_eq!(group("scripts/verify.sh"), "scripts/verify.sh");
    }
}
