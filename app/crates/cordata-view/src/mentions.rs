//! `@file` mentions: find the project's files, rank them against what was typed, and — when the prompt is sent —
//! put the mentioned files' contents in front of the model so it does not have to spend a tool call (and a turn) on
//! reading them.
//!
//! Nothing here talks to the backend: the TUI sends one ordinary text prompt. Files that look like secrets are never
//! attached, whatever the person typed (`THREAT_MODEL.md`: the provider only ever sees what the harness lets through).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Files in the index; a monorepo beyond this is searched by its first files only.
const MAX_INDEXED: usize = 60_000;
/// One attached file, and all of them together.
const MAX_FILE_BYTES: usize = 48 * 1024;
const MAX_TOTAL_BYTES: usize = 160 * 1024;
const MAX_DIR_ENTRIES: usize = 80;

/// The project's files, relative to `cwd`, plus every directory that holds one (with a trailing `/`).
/// Git's own view (tracked + untracked, minus ignored) when the folder is a repository, a bounded walk otherwise.
pub fn index(cwd: &Path) -> Vec<String> {
    let mut files = git_files(cwd).unwrap_or_else(|| walk(cwd));
    files.truncate(MAX_INDEXED);
    let mut dirs = std::collections::BTreeSet::new();
    for file in &files {
        let mut path = Path::new(file).parent();
        while let Some(dir) = path.filter(|p| !p.as_os_str().is_empty()) {
            dirs.insert(format!("{}/", dir.display()));
            path = dir.parent();
        }
    }
    files.extend(dirs);
    files
}

fn git_files(cwd: &Path) -> Option<Vec<String>> {
    // The repository's fsmonitor hook is its own code: listing files must not run it (`review::git`).
    let output = crate::review::git(cwd)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(text.split('\0').filter(|name| !name.is_empty()).map(str::to_owned).collect())
}

fn walk(cwd: &Path) -> Vec<String> {
    const SKIP: [&str; 8] = [".git", "node_modules", "target", ".venv", "venv", "__pycache__", ".next", "dist"];
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        let Ok(entries) = fs::read_dir(cwd.join(&relative)) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = relative.join(&name);
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if !SKIP.contains(&name.as_str()) && !name.starts_with('.') {
                    stack.push(path);
                }
            } else if kind.is_file() {
                out.push(path.to_string_lossy().into_owned());
            }
            if out.len() >= MAX_INDEXED {
                return out;
            }
        }
    }
    out.sort();
    out
}

/// How well `query` matches `path` (higher is better), or `None` when its letters are not all there in order.
/// Case-insensitive; a match in the file's own name beats one in its folders, a run of consecutive letters or a match
/// at a word start beats scattered ones, and shorter paths win ties.
pub fn score(query: &str, path: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let path_chars: Vec<char> = path.chars().collect();
    let name_start = path.trim_end_matches('/').rfind('/').map_or(0, |i| path[..=i].chars().count());
    let mut total = 0;
    let mut at = 0;
    let mut previous: Option<usize> = None;
    for wanted in query.chars().flat_map(char::to_lowercase) {
        let found = (at..path_chars.len()).find(|&i| path_chars[i].to_lowercase().next() == Some(wanted))?;
        total += 1;
        if previous.is_some_and(|p| p + 1 == found) {
            total += 6;
        }
        if found == 0 || matches!(path_chars[found - 1], '/' | '_' | '-' | '.' | ' ') {
            total += 8;
        }
        if found >= name_start {
            total += 3;
        }
        previous = Some(found);
        at = found + 1;
    }
    Some(total * 4 - path_chars.len() as i32 / 8)
}

/// The best `limit` entries for `query`. With nothing typed yet: the shallowest paths, which is where a person looks.
pub fn rank<'a>(query: &str, entries: &'a [String], limit: usize) -> Vec<&'a str> {
    if query.is_empty() {
        let mut shallow: Vec<&str> = entries.iter().map(String::as_str).collect();
        shallow.sort_by_key(|path| (path.trim_end_matches('/').matches('/').count(), path.len(), *path));
        shallow.truncate(limit);
        return shallow;
    }
    let mut scored: Vec<(i32, &str)> = entries
        .iter()
        .filter_map(|entry| score(query, entry).map(|s| (s, entry.as_str())))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, entry)| entry).collect()
}

/// A path whose contents must never be sent to a model on a keystroke: keys, tokens, environment files.
pub fn looks_secret(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    name == ".env"
        || name.starts_with(".env.") && !name.ends_with(".example") && !name.ends_with(".sample") && !name.ends_with(".template")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.ends_with(".keystore")
        || matches!(name, "id_rsa" | "id_ed25519" | "id_ecdsa" | "id_dsa" | ".netrc" | ".npmrc" | ".pypirc" | "credentials" | "credentials.json" | "secrets.json")
        || lower.contains("/.ssh/")
        || lower.contains("/.aws/")
        || lower.contains("/.gnupg/")
}

/// The `@mentions` in a prompt, in order, with the byte range of each (`@` included). A mention starts at the
/// beginning of the text or after whitespace, so `me@example.com` is not one; trailing sentence punctuation is not
/// part of it.
pub fn find(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut found = Vec::new();
    let mut start_of_word = true;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = text[i..].chars().next().unwrap_or(' ');
        if c == '@' && start_of_word {
            let end = text[i + 1..].find(char::is_whitespace).map_or(text.len(), |n| i + 1 + n);
            let raw = &text[i + 1..end];
            let trimmed = raw.trim_end_matches([',', ';', ':', ')', ']', '}', '"', '\'', '?', '!']);
            let trimmed = if trimmed.ends_with('.') && !trimmed.contains("/.") && trimmed.len() > 1 { trimmed.trim_end_matches('.') } else { trimmed };
            if !trimmed.is_empty() {
                found.push((i..i + 1 + trimmed.len(), trimmed.to_owned()));
            }
            i = end.max(i + 1);
            start_of_word = false;
            continue;
        }
        start_of_word = c.is_whitespace();
        i += c.len_utf8();
    }
    found
}

/// What sending `text` attaches.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Expanded {
    /// The prompt as the model gets it: the person's words, then the attached contents.
    pub prompt: String,
    /// Paths attached (for the one-line receipt in the transcript).
    pub attached: Vec<String>,
    /// Mentions left as plain text, each with why — printed so a typo is noticed, not silently ignored.
    pub skipped: Vec<(String, &'static str)>,
}

pub fn expand(text: &str, cwd: &Path) -> Expanded {
    let mut out = Expanded { prompt: text.to_owned(), ..Default::default() };
    let mut blocks = Vec::new();
    let mut total = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    for (_, mention) in find(text) {
        if !seen.insert(mention.clone()) {
            continue;
        }
        let path = resolve(&mention, cwd);
        let Ok(meta) = fs::metadata(&path) else {
            out.skipped.push((mention, "no such file"));
            continue;
        };
        if looks_secret(&path.to_string_lossy()) {
            out.skipped.push((mention, "looks like a secret — not attached"));
            continue;
        }
        if meta.is_dir() {
            let listing = list_dir(&path);
            total += listing.len();
            blocks.push(format!("--- {mention} (directory) ---\n{listing}\n--- end ---"));
            out.attached.push(mention);
            continue;
        }
        if total >= MAX_TOTAL_BYTES {
            out.skipped.push((mention, "attachment budget used up — the agent can read it"));
            continue;
        }
        match fs::read(&path) {
            Ok(bytes) if bytes.contains(&0) => out.skipped.push((mention, "binary file")),
            Ok(bytes) => {
                let limit = MAX_FILE_BYTES.min(MAX_TOTAL_BYTES - total);
                let cut = bytes.len() > limit;
                let mut end = bytes.len().min(limit);
                // Never cut a character in half: back up to the start of the one the limit lands in.
                while cut && end > 0 && (bytes[end] & 0xC0) == 0x80 {
                    end -= 1;
                }
                let body = String::from_utf8_lossy(&bytes[..end]).into_owned();
                total += body.len();
                let note = if cut { format!(" — first {} of {} bytes", body.len(), bytes.len()) } else { String::new() };
                blocks.push(format!("--- {mention}{note} ---\n{body}\n--- end ---"));
                out.attached.push(mention);
            }
            Err(_) => out.skipped.push((mention, "unreadable")),
        }
    }
    if !blocks.is_empty() {
        out.prompt = format!("{text}\n\n[Attached by the user with @ — these files are already in front of you, read them before using a tool]\n{}", blocks.join("\n"));
    }
    out
}

fn resolve(mention: &str, cwd: &Path) -> PathBuf {
    let mention = mention.trim_end_matches('/');
    if let Some(rest) = mention.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let path = Path::new(mention);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}

fn list_dir(path: &Path) -> String {
    let mut names: Vec<String> = fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if e.file_type().is_ok_and(|t| t.is_dir()) { format!("{name}/") } else { name }
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let more = names.len().saturating_sub(MAX_DIR_ENTRIES);
    names.truncate(MAX_DIR_ENTRIES);
    if more > 0 {
        names.push(format!("… and {more} more"));
    }
    names.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn letters_must_all_be_there_in_order() {
        assert!(score("app", "src/app.rs").is_some());
        assert!(score("apr", "src/app.rs").is_some());
        assert!(score("zzz", "src/app.rs").is_none());
        assert!(score("sa", "src/app.rs").is_some());
        assert!(score("pra", "src/app.rs").is_none(), "order matters");
    }

    #[test]
    fn the_name_beats_the_folders_and_a_run_beats_a_scatter() {
        let all = entries(&["src/ui/footer.rs", "src/app.rs", "tests/application/readme.md", "docs/a/p/p.txt"]);
        assert_eq!(rank("app", &all, 3)[0], "src/app.rs");
        let ranked = rank("foot", &all, 3);
        assert_eq!(ranked[0], "src/ui/footer.rs");
    }

    #[test]
    fn nothing_typed_shows_the_shallowest_paths_first() {
        let all = entries(&["a/b/c/deep.rs", "README.md", "src/", "Makefile"]);
        let shown = rank("", &all, 2);
        assert!(shown.iter().all(|p| !p.contains("deep")), "{shown:?}");
    }

    #[test]
    fn mentions_start_at_a_word_and_drop_trailing_punctuation() {
        let text = "look at @src/app.rs, then @README.md. mail me@example.com or @Makefile?";
        let found: Vec<String> = find(text).into_iter().map(|(_, m)| m).collect();
        assert_eq!(found, ["src/app.rs", "README.md", "Makefile"]);
        let (range, _) = &find("see @a/b.rs now")[0];
        assert_eq!(&"see @a/b.rs now"[range.clone()], "@a/b.rs");
        assert!(find("just an @ sign").is_empty());
        assert_eq!(find("@.gitignore")[0].1, ".gitignore");
    }

    #[test]
    fn secrets_are_recognised_by_name() {
        for secret in [".env", "app/.env.production", "keys/server.pem", "~/.ssh/id_ed25519", "id_rsa", "a/b/credentials.json", "x/.aws/config"] {
            assert!(looks_secret(secret), "{secret}");
        }
        for fine in [".env.example", "src/environment.rs", "README.md", "notes/keyboard.md"] {
            assert!(!looks_secret(fine), "{fine}");
        }
    }

    #[test]
    fn sending_attaches_what_exists_and_says_what_it_did_not() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join(".env"), "TOKEN=abc\n").unwrap();
        fs::write(dir.path().join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        let text = "fix @src/lib.rs and @.env and @nope.rs and @blob.bin and @src";
        let expanded = expand(text, dir.path());
        assert_eq!(expanded.attached, ["src/lib.rs", "src"]);
        assert!(expanded.prompt.starts_with(text), "the person's words come first, unchanged");
        assert!(expanded.prompt.contains("--- src/lib.rs ---\nfn main() {}\n\n--- end ---") || expanded.prompt.contains("fn main() {}"));
        assert!(expanded.prompt.contains("--- src (directory) ---\nlib.rs"));
        assert!(!expanded.prompt.contains("TOKEN=abc"), "a secret is never attached");
        let why: std::collections::BTreeMap<_, _> = expanded.skipped.iter().cloned().collect();
        assert_eq!(why["nope.rs"], "no such file");
        assert!(why[".env"].contains("secret"));
        assert_eq!(why["blob.bin"], "binary file");
    }

    #[test]
    fn a_big_file_is_cut_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big.txt"), "x".repeat(MAX_FILE_BYTES * 2)).unwrap();
        let expanded = expand("read @big.txt", dir.path());
        assert!(expanded.prompt.contains(&format!("first {MAX_FILE_BYTES} of {} bytes", MAX_FILE_BYTES * 2)), "{}", &expanded.prompt[..200]);
    }

    #[test]
    fn a_prompt_without_mentions_is_unchanged() {
        let expanded = expand("no mentions here", Path::new("."));
        assert_eq!(expanded, Expanded { prompt: "no mentions here".into(), ..Default::default() });
    }

    #[test]
    fn the_index_lists_files_and_their_folders() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/c.txt"), "").unwrap();
        fs::write(dir.path().join("top.txt"), "").unwrap();
        let listed = index(dir.path());
        for expected in ["a/b/c.txt", "top.txt", "a/", "a/b/"] {
            assert!(listed.iter().any(|e| e == expected), "{expected} in {listed:?}");
        }
    }
}
