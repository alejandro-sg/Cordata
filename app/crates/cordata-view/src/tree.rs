//! The project's files as a tree the person can open and close (the native app's Files pane), or, while a filter is
//! typed, as one ranked list (the same fuzzy match as `@` mentions).

use std::collections::BTreeSet;

use crate::mentions;

/// Rows a filter shows at most.
const FILTERED: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Relative to the project; a folder ends in `/`.
    pub path: String,
    /// What is drawn: the last component, or the whole path in a filtered list.
    pub name: String,
    pub depth: usize,
    pub dir: bool,
    pub open: bool,
}

#[derive(Debug, Default)]
pub struct Tree {
    /// [`mentions::index`]'s entries: files and folders (`dir/`), sorted.
    entries: Vec<String>,
    open: BTreeSet<String>,
    filter: String,
    rows: Vec<Row>,
}

impl Tree {
    pub fn new(mut entries: Vec<String>) -> Self {
        entries.sort_by_cached_key(|entry| sort_key(entry));
        entries.dedup();
        let mut tree = Self { entries, ..Default::default() };
        tree.rebuild();
        tree
    }

    /// The project's files again (a refresh), keeping what is open and the filter.
    pub fn set_entries(&mut self, entries: Vec<String>) {
        let (open, filter) = (std::mem::take(&mut self.open), std::mem::take(&mut self.filter));
        *self = Self::new(entries);
        self.open = open;
        self.filter = filter;
        self.rebuild();
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn set_filter(&mut self, filter: &str) {
        if self.filter != filter.trim() {
            self.filter = filter.trim().to_owned();
            self.rebuild();
        }
    }

    /// Open a closed folder, close an open one.
    pub fn toggle(&mut self, dir: &str) {
        if !self.open.remove(dir) {
            self.open.insert(dir.to_owned());
        }
        self.rebuild();
    }

    /// Open every folder above `path`, so it shows.
    pub fn reveal(&mut self, path: &str) {
        let mut at = 0;
        while let Some(slash) = path[at..].find('/') {
            at += slash + 1;
            self.open.insert(path[..at].to_owned());
        }
        self.rebuild();
    }

    fn rebuild(&mut self) {
        if !self.filter.is_empty() {
            let files: Vec<String> = self.entries.iter().filter(|e| !e.ends_with('/')).cloned().collect();
            self.rows = mentions::rank(&self.filter, &files, FILTERED)
                .into_iter()
                .map(|path| Row { path: path.to_owned(), name: path.to_owned(), depth: 0, dir: false, open: false })
                .collect();
            return;
        }
        self.rows.clear();
        for entry in &self.entries {
            let trimmed = entry.trim_end_matches('/');
            let depth = trimmed.matches('/').count();
            // Shown only when every folder above it is open.
            let parent_open = match trimmed.rfind('/') {
                None => true,
                Some(slash) => {
                    let mut ok = true;
                    let mut at = 0;
                    while let Some(next) = trimmed[at..=slash].find('/') {
                        at += next + 1;
                        if !self.open.contains(&trimmed[..at]) {
                            ok = false;
                            break;
                        }
                    }
                    ok
                }
            };
            if !parent_open {
                continue;
            }
            let dir = entry.ends_with('/');
            self.rows.push(Row {
                path: entry.clone(),
                name: trimmed.rsplit('/').next().unwrap_or(trimmed).to_owned() + if dir { "/" } else { "" },
                depth,
                dir,
                open: dir && self.open.contains(entry),
            });
        }
    }
}

/// Folders before files at each level, each group alphabetical: compare component by component, a folder component
/// (one with more path after it) before a file.
fn sort_key(entry: &str) -> Vec<(bool, String)> {
    let trimmed = entry.trim_end_matches('/');
    let parts: Vec<&str> = trimmed.split('/').collect();
    let dir = entry.ends_with('/');
    parts
        .iter()
        .enumerate()
        .map(|(ix, part)| {
            let is_file = ix + 1 == parts.len() && !dir;
            (is_file, part.to_lowercase())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Tree {
        Tree::new(["README.md", "src/main.rs", "src/ui/view.rs", "Cargo.toml", "src/", "src/ui/", "build.rs"].map(String::from).to_vec())
    }

    fn names(tree: &Tree) -> Vec<String> {
        tree.rows().iter().map(|r| format!("{}{}", "  ".repeat(r.depth), r.name)).collect()
    }

    #[test]
    fn folders_first_and_closed_until_opened() {
        let mut tree = tree();
        assert_eq!(names(&tree), ["src/", "build.rs", "Cargo.toml", "README.md"]);
        tree.toggle("src/");
        assert_eq!(names(&tree), ["src/", "  ui/", "  main.rs", "build.rs", "Cargo.toml", "README.md"]);
        tree.toggle("src/ui/");
        assert!(names(&tree).contains(&"    view.rs".to_owned()));
        tree.toggle("src/");
        assert_eq!(names(&tree).len(), 4, "closing a folder hides everything under it");
    }

    #[test]
    fn reveal_opens_the_folders_above_a_file() {
        let mut tree = tree();
        tree.reveal("src/ui/view.rs");
        assert!(tree.rows().iter().any(|r| r.path == "src/ui/view.rs"));
    }

    #[test]
    fn a_filter_is_a_ranked_flat_list_of_files() {
        let mut tree = tree();
        tree.set_filter("view");
        assert_eq!(tree.rows().first().map(|r| r.path.as_str()), Some("src/ui/view.rs"));
        assert!(tree.rows().iter().all(|r| !r.dir));
        tree.set_filter("");
        assert_eq!(tree.rows().len(), 4);
    }
}
