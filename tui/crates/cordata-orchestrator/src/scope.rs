//! A step's file scope, ported from `backend/workers/scope.py`: which paths it may change, checked after the fact by
//! [`out_of_scope`] and before by the Seatbelt rules from [`seatbelt_rules`].
//!
//! Scope entries, relative to the project: `src/app.py` (that file and same-name siblings `src/app.py.*`),
//! `src/` or `src/**` (everything under it), `src/*.py` / `**/x.py` (a glob).

use std::path::Path;

use regex::Regex;

const GLOB: [char; 3] = ['*', '?', '['];
const ERE_SPECIAL: &str = ".^$*+?()[]{}|\\";
const TEMPLATES: [&str; 4] = ["example", "sample", "template", "dist"];

fn has_glob(text: &str) -> bool {
    text.contains(GLOB)
}

/// `.env`, `.env.local` … but not the committed templates. Matches the final path component.
pub fn is_secret_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == ".env" || name.strip_prefix(".env.").is_some_and(|suffix| !TEMPLATES.contains(&suffix))
}

/// `./src//a.py` → `src/a.py`; a directory keeps one trailing `/`.
pub fn normalize(entry: &str) -> String {
    let mut raw = entry.trim().to_owned();
    let is_dir = raw.ends_with('/') || raw.ends_with("/**");
    if let Some(stripped) = raw.strip_suffix("/**") {
        raw = stripped.to_owned();
    }
    let clean = raw.split('/').filter(|p| !p.is_empty() && *p != ".").collect::<Vec<_>>().join("/");
    if is_dir && !clean.is_empty() { clean + "/" } else { clean }
}

/// Why `entry` cannot be a scope entry, or `None`. Absolute paths, `..` and secrets files are refused.
pub fn valid(entry: &str) -> Option<String> {
    let raw = entry.trim();
    if raw.is_empty() {
        return Some("empty scope entry".into());
    }
    if raw.starts_with('/') {
        return Some(format!("{} is absolute; scope entries are project-relative", py_repr(raw)));
    }
    if raw.split('/').any(|p| p == "..") {
        return Some(format!("{} leaves the project", py_repr(raw)));
    }
    if is_secret_path(raw) {
        return Some(format!("{} is a secrets file", py_repr(raw)));
    }
    None
}

/// Python's `repr` for the strings that appear in messages (quotes only; enough for paths).
pub(crate) fn py_repr(text: &str) -> String {
    if text.contains('\'') && !text.contains('"') { format!("\"{text}\"") } else { format!("'{}'", text.replace('\'', "\\'")) }
}

fn is_dir_entry(entry: &str, root: &str) -> bool {
    entry.ends_with('/') || (!has_glob(entry) && Path::new(root).join(entry).is_dir())
}

/// Python's `fnmatch.fnmatchcase`: `*` matches anything including `/`, `?` one char, `[seq]` / `[!seq]`.
fn fnmatch(name: &str, pattern: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let mut re = String::from("(?s)^");
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => {
                let mut j = i;
                if j < chars.len() && chars[j] == '!' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    re.push_str("\\[");
                } else {
                    let mut body: String = chars[i..j].iter().collect::<String>().replace('\\', "\\\\");
                    i = j + 1;
                    if let Some(rest) = body.strip_prefix('!') {
                        body = format!("^{rest}");
                    } else if body.starts_with('^') {
                        body = format!("\\{body}");
                    }
                    re.push('[');
                    re.push_str(&body);
                    re.push(']');
                }
            }
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).is_ok_and(|r| r.is_match(name))
}

/// Is project-relative `path` inside `scope`?
pub fn matches<S: AsRef<str>>(path: &str, scope: &[S], root: &str) -> bool {
    let normalized = normalize(path);
    let rel = normalized.trim_end_matches('/');
    for raw in scope {
        let entry = normalize(raw.as_ref());
        if entry.is_empty() {
            continue;
        }
        if has_glob(&entry) {
            if fnmatch(rel, entry.trim_end_matches('/')) {
                return true;
            }
            continue;
        }
        if is_dir_entry(&entry, root) {
            let base = entry.trim_end_matches('/');
            if rel == base || rel.starts_with(&format!("{base}/")) {
                return true;
            }
            continue;
        }
        if rel == entry || rel.strip_prefix(&format!("{entry}.")).is_some_and(|rest| !rest.contains('/')) {
            return true;
        }
    }
    false
}

/// The changed paths no scope entry covers, sorted.
pub fn out_of_scope<P: AsRef<str>, S: AsRef<str>>(changed: &[P], scope: &[S], root: &str) -> Vec<String> {
    let mut out: Vec<String> = changed.iter().map(|p| p.as_ref().to_owned()).filter(|p| !matches(p, scope, root)).collect();
    out.sort();
    out
}

fn literal_prefix(entry: &str) -> String {
    entry.split('/').take_while(|part| !has_glob(part)).collect::<Vec<_>>().join("/")
}

fn ere(text: &str) -> String {
    text.chars().map(|c| if ERE_SPECIAL.contains(c) { format!("\\{c}") } else { c.to_string() }).collect()
}

fn sb_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The profile lines that confine writes inside `root` to `scope`. An empty scope means read-only.
pub fn seatbelt_rules<S: AsRef<str>>(root: &str, scope: &[S]) -> Vec<String> {
    let real = std::fs::canonicalize(root).map_or_else(|_| root.to_owned(), |p| p.to_string_lossy().into_owned());
    let mut lines = vec![format!("(deny file-write* (subpath {}))", sb_string(&real))];
    let mut allows: Vec<String> = Vec::new();
    let mut ancestors = std::collections::BTreeSet::new();
    for raw in scope {
        let raw = raw.as_ref();
        if valid(raw).is_some() {
            continue;
        }
        let entry = normalize(raw);
        if entry.is_empty() {
            continue;
        }
        let rel_dir: String;
        if has_glob(&entry) {
            let prefix = literal_prefix(&entry);
            if prefix.is_empty() {
                continue; // a glob at the root cannot be widened without opening the whole project
            }
            allows.push(format!("(subpath {})", sb_string(&format!("{real}/{prefix}"))));
            rel_dir = prefix;
        } else if is_dir_entry(&entry, &real) {
            let dir = entry.trim_end_matches('/').to_owned();
            allows.push(format!("(subpath {})", sb_string(&format!("{real}/{dir}"))));
            rel_dir = dir;
        } else {
            let target = format!("{real}/{entry}");
            allows.push(format!("(literal {})", sb_string(&target)));
            allows.push(format!("(regex #\"^{}\\.[^/]*$\")", ere(&target)));
            rel_dir = entry.rsplit_once('/').map_or(String::new(), |(d, _)| d.to_owned());
        }
        let parts: Vec<&str> = rel_dir.split('/').filter(|p| !p.is_empty()).collect();
        for i in 1..=parts.len() {
            ancestors.insert(format!("{real}/{}", parts[..i].join("/")));
        }
    }
    allows.extend(ancestors.iter().map(|a| format!("(literal {})", sb_string(a))));
    if !allows.is_empty() {
        lines.push(format!("(allow file-write* {})", allows.join(" ")));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_cleans_and_keeps_directories() {
        assert_eq!(normalize("./src//a.py"), "src/a.py");
        assert_eq!(normalize("src/**"), "src/");
        assert_eq!(normalize(" src/ "), "src/");
        assert_eq!(normalize("./"), "");
    }

    #[test]
    fn valid_refuses_absolute_parent_and_secrets() {
        assert!(valid("src/a.py").is_none());
        assert!(valid("").unwrap().contains("empty"));
        assert!(valid("/etc/passwd").unwrap().contains("absolute"));
        assert!(valid("a/../b").unwrap().contains("leaves the project"));
        assert!(valid("cfg/.env.local").unwrap().contains("secrets file"));
        assert!(valid(".env.example").is_none());
    }

    #[test]
    fn matching_rules() {
        let root = "/nonexistent";
        assert!(matches("src/a.py", &["src/a.py"], root));
        assert!(matches("src/a.py.tmp", &["src/a.py"], root), "same-name sibling");
        assert!(!matches("src/a.py.d/x", &["src/a.py"], root));
        assert!(matches("src/deep/x.rs", &["src/"], root));
        assert!(!matches("srcx/a.rs", &["src/"], root));
        assert!(matches("src/a.py", &["src/*.py"], root));
        assert!(matches("src/sub/a.py", &["src/*.py"], root), "fnmatch's * crosses /");
        assert!(matches("deep/x.py", &["**/x.py"], root));
        assert!(!matches("a.rs", &["*.py"], root));
        assert!(matches("a1.py", &["a[0-9].py"], root));
        assert!(!matches("ab.py", &["a[!a-z].py"], root));
        assert_eq!(out_of_scope(&["b.py", "a.py", "src/z"], &["src/"], root), ["a.py", "b.py"]);
    }

    #[test]
    fn a_plain_entry_naming_a_directory_on_disk_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("lib")).unwrap();
        let root = dir.path().to_str().unwrap();
        assert!(matches("lib/x.rs", &["lib"], root));
        assert!(!matches("lib/x.rs", &["lib"], "/nonexistent"));
    }

    #[test]
    fn seatbelt_rules_allow_only_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let real = std::fs::canonicalize(root).unwrap().to_string_lossy().into_owned();
        assert_eq!(seatbelt_rules(root, &[] as &[&str]), vec![format!("(deny file-write* (subpath \"{real}\"))")]);
        let rules = seatbelt_rules(root, &["calc/add.py", "docs/", "*.md", "/abs"]);
        assert_eq!(rules.len(), 2, "root glob and invalid entries add nothing: {rules:?}");
        let allow = &rules[1];
        assert!(allow.contains(&format!("(literal \"{real}/calc/add.py\")")));
        assert!(allow.contains(&format!("(subpath \"{real}/docs\")")));
        assert!(allow.contains(&format!("(literal \"{real}/calc\")")), "ancestors are writable for renames");
        assert!(allow.contains(r"add\.py\.[^/]*$"), "editor temp siblings: {allow}");
    }
}
