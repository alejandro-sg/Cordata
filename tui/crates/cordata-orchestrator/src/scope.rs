//! A step's file scope: which paths it may change, checked after the fact by
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

/// Python's `repr` for a `str`: single quotes unless the text has a `'` and no `"`; backslash and control characters
/// escaped; other printable characters (including non-ASCII) kept.
pub(crate) fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
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

/// A step that writes Rust source runs `cargo`, which rewrites the root `Cargo.lock` as a side effect. Treating that as
/// an out-of-scope write failed every worker on the step (a `worker_failed` handoff with no one left to hand to). The
/// lockfile is in scope when the scope holds a `.rs` file, a `Cargo.toml` or a directory (which may hold either).
fn lockfile_follows_scope<S: AsRef<str>>(rel: &str, scope: &[S]) -> bool {
    rel == "Cargo.lock"
        && scope.iter().any(|raw| {
            let e = normalize(raw.as_ref());
            e.ends_with(".rs") || e.ends_with("Cargo.toml") || e.ends_with('/')
        })
}

/// Is project-relative `path` inside `scope`?
pub fn matches<S: AsRef<str>>(path: &str, scope: &[S], root: &str) -> bool {
    lockfile_follows_scope(normalize(path).trim_end_matches('/'), scope) || names(path, scope, root)
}

/// Does `scope` itself name `path`? Unlike [`matches`] this leaves out the lockfile a Rust step is merely tolerated to
/// rewrite, so a plan may still promise that `Cargo.lock` does not change.
pub fn names<S: AsRef<str>>(path: &str, scope: &[S], root: &str) -> bool {
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

pub fn literal_prefix(entry: &str) -> String {
    entry.split('/').take_while(|part| !has_glob(part)).collect::<Vec<_>>().join("/")
}

pub(crate) fn ere(text: &str) -> String {
    text.chars().map(|c| if ERE_SPECIAL.contains(c) { format!("\\{c}") } else { c.to_string() }).collect()
}

pub(crate) fn sb_string(value: &str) -> String {
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
        let rel_dir: String = if has_glob(&entry) {
            let prefix = literal_prefix(&entry);
            if prefix.is_empty() {
                continue; // a glob at the root cannot be widened without opening the whole project
            }
            allows.push(format!("(subpath {})", sb_string(&format!("{real}/{prefix}"))));
            prefix
        } else if is_dir_entry(&entry, &real) {
            let dir = entry.trim_end_matches('/').to_owned();
            allows.push(format!("(subpath {})", sb_string(&format!("{real}/{dir}"))));
            dir
        } else {
            let target = format!("{real}/{entry}");
            allows.push(format!("(literal {})", sb_string(&target)));
            allows.push(format!("(regex #\"^{}\\.[^/]*$\")", ere(&target)));
            entry.rsplit_once('/').map_or(String::new(), |(d, _)| d.to_owned())
        };
        let parts: Vec<&str> = rel_dir.split('/').filter(|p| !p.is_empty()).collect();
        for i in 1..=parts.len() {
            ancestors.insert(format!("{real}/{}", parts[..i].join("/")));
        }
    }
    allows.extend(ancestors.iter().map(|a| format!("(literal {})", sb_string(a))));
    if lockfile_follows_scope("Cargo.lock", scope) {
        allows.push(format!("(literal {})", sb_string(&format!("{real}/Cargo.lock"))));
        allows.push(format!("(regex #\"^{}\\.[^/]*$\")", ere(&format!("{real}/Cargo.lock"))));
    }
    if !allows.is_empty() {
        lines.push(format!("(allow file-write* {})", allows.join(" ")));
    }
    lines
}

const DENY_HOME: &[&str] = &[
    ".cordata",
    ".ssh",
    ".gnupg",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".gemini",
    ".anthropic",
    ".config/op",
    ".aws",
    ".netrc",
    ".config/gh",
    ".docker/config.json",
    ".kube/config",
];

/// `scrubbed_sandbox.seatbelt_profile`: `(allow default)` and then deny — the denials are the secrets.
pub fn base_profile(home: &str) -> Vec<String> {
    let mut lines = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        r#"(deny file-read* file-write* (regex #"(^|/)\.env($|\.[^/]*$)"))"#.to_owned(),
        r#"(allow file-read* (regex #"(^|/)\.env\.(example|sample|template|dist)$"))"#.to_owned(),
        // Hooks and config that git later runs outside any sandbox (`core.fsmonitor`, `core.hooksPath`, filters).
        r#"(deny file-write* (regex #"/\.git/(config|hooks(/|$)|info/attributes$|modules/.+/(config|hooks(/|$)))"))"#.to_owned(),
    ];
    lines.extend(DENY_HOME.iter().map(|rel| format!("(deny file-read* file-write* (subpath {}))", sb_string(&format!("{home}/{rel}")))));
    lines
}

/// What the Antigravity CLI itself needs under the denied `~/.gemini`, appended to *its* profile only (the last
/// matching rule wins): its state folder read-write, its config read-only. What it later runs outside any sandbox stays
/// shut — its binaries, updater, MCP and settings files — so a confined worker cannot plant code or a server there.
pub fn agy_home_rules(home: &str) -> Vec<String> {
    let cli = format!("{home}/.gemini/antigravity-cli");
    let mut lines = vec![
        format!("(allow file-read* file-write* (subpath {}))", sb_string(&cli)),
        format!("(allow file-read* (subpath {}))", sb_string(&format!("{home}/.gemini/config"))),
    ];
    let shut = ["bin", "builtin", "updater", "mcp"].iter().map(|d| format!("(subpath {})", sb_string(&format!("{cli}/{d}"))));
    let files = ["settings.json"].iter().map(|f| format!("(literal {})", sb_string(&format!("{cli}/{f}"))));
    lines.push(format!("(deny file-write* {})", shut.chain(files).collect::<Vec<_>>().join(" ")));
    lines
}

/// The base profile with the scope rules spliced in after `(allow default)` and before the secret denials, so the
/// denials still win (Seatbelt takes the last matching rule).
pub fn scoped_profile<S: AsRef<str>>(root: &str, scope: &[S], home: &str) -> String {
    scoped_profile_with_bus(root, scope, home, None)
}

/// The scoped profile, plus a connect allowance for this run's bus socket. It lives under `~/.cordata`, which stays
/// unreadable and unwritable: the worker can talk to its own bus but not replace it or reach another run's.
pub fn scoped_profile_with_bus<S: AsRef<str>>(root: &str, scope: &[S], home: &str, bus_socket: Option<&str>) -> String {
    let base = base_profile(home);
    let (head, tail) = base.split_at(2);
    let rules = seatbelt_rules(root, scope);
    let mut sockets = socket_rules(root);
    if let Some(socket) = bus_socket {
        sockets.push(format!("(allow network-outbound (remote unix-socket (path-literal {})))", sb_string(socket)));
    }
    head.iter().chain(rules.iter()).chain(tail.iter()).chain(sockets.iter()).cloned().collect::<Vec<_>>().join("\n") + "\n"
}

/// Unix sockets lead to local daemons that act outside the sandbox (herdr and other multiplexers start programs,
/// Docker runs containers, ssh-agent signs). A worker reaches only sockets in its project and temporary folders, plus
/// mDNSResponder's for name resolution; launchd's per-login sockets under `/private/tmp` stay shut.
pub fn socket_rules(root: &str) -> Vec<String> {
    let real = std::fs::canonicalize(root).map_or_else(|_| root.to_owned(), |p| p.to_string_lossy().into_owned());
    // One rule per place: Seatbelt does not OR several filters inside one `(remote unix-socket …)`; a combined rule
    // matched nothing, which shut DNS and left every confined worker unable to reach its provider.
    let mut rules = vec!["(deny network-outbound (remote unix-socket))".to_owned()];
    rules.extend([format!("(subpath {})", sb_string(&real)), "(subpath \"/private/tmp\")".into(), "(subpath \"/private/var/folders\")".into(), "(path-literal \"/private/var/run/mDNSResponder\")".into()].iter().map(|f| format!("(allow network-outbound (remote unix-socket {f}))")));
    rules.push(r#"(deny network-outbound (remote unix-socket (path-regex #"^/private/tmp/com\.apple\.launchd\.[^/]+/")))"#.to_owned());
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under real Seatbelt: no worker but agy reaches `~/.gemini`, and agy cannot write what it later runs unsandboxed.
    #[test]
    fn only_agy_reaches_its_state_and_never_its_binaries() {
        if !std::path::Path::new("/usr/bin/sandbox-exec").exists() || !crate::workers::cli::seatbelt_works() {
            eprintln!("skipped: no Seatbelt here");
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        let h = home.to_string_lossy().into_owned();
        for d in [".gemini/antigravity-cli/bin", ".gemini/antigravity-cli/conversations", ".gemini/config"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        std::fs::write(home.join(".gemini/config/config.json"), "{}").unwrap();
        let touch = |profile: &str, rel: &str| {
            std::process::Command::new("/usr/bin/sandbox-exec").args(["-p", profile, "/usr/bin/touch", &format!("{h}/{rel}")]).status().unwrap().success()
        };
        let cat = |profile: &str, rel: &str| {
            std::process::Command::new("/usr/bin/sandbox-exec").args(["-p", profile, "/bin/cat", &format!("{h}/{rel}")]).output().unwrap().status.success()
        };
        let other = base_profile(&h).join("\n");
        let agy = format!("{other}\n{}", agy_home_rules(&h).join("\n"));
        assert!(!touch(&other, ".gemini/antigravity-cli/conversations/x"), "another worker writes agy's state");
        assert!(!cat(&other, ".gemini/config/config.json"), "another worker reads agy's config");
        assert!(touch(&agy, ".gemini/antigravity-cli/conversations/x"), "agy keeps its own state");
        assert!(cat(&agy, ".gemini/config/config.json"), "agy reads its config");
        assert!(!touch(&agy, ".gemini/config/mcp_config.json"), "config is read-only");
        assert!(!touch(&agy, ".gemini/antigravity-cli/bin/agy"), "agy cannot replace its own binary");
        assert!(!touch(&agy, ".gemini/antigravity-cli/settings.json"), "nor its settings");
        std::fs::write(home.join(".gemini/antigravity-cli/bin/agy"), "orig").unwrap();
        let link = std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &agy, "/bin/ln", &format!("{h}/.gemini/antigravity-cli/bin/agy"), &format!("{h}/.gemini/antigravity-cli/x")])
            .output()
            .unwrap();
        assert!(!link.status.success(), "a hard link into the writable state folder would let agy write its own binary");
    }

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
        // cargo rewrites the lockfile for a Rust step; a Python step has no business with it.
        assert!(out_of_scope(&["Cargo.lock"], &["crates/x/src/lib.rs"], root).is_empty());
        assert_eq!(out_of_scope(&["Cargo.lock"], &["calc/add.py"], root), ["Cargo.lock"]);
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

    /// The profile as Seatbelt reads it, not as text: a worker reaches a socket in its project and the DNS resolver,
    /// and not one elsewhere. (A combined allow rule once matched nothing and passed every text check.)
    #[test]
    fn under_sandbox_exec_the_project_socket_and_dns_are_reachable_and_others_are_not() {
        if !crate::workers::cli::seatbelt_works() || !std::path::Path::new("/usr/bin/nc").exists() {
            return;
        }
        let project = tempfile::Builder::new().prefix("sb").tempdir_in("/tmp").unwrap();
        let outside = tempfile::Builder::new().prefix("so").tempdir_in(std::env::var("HOME").unwrap()).unwrap();
        let inside_sock = project.path().join("s.sock");
        let outside_sock = outside.path().join("s.sock");
        let _a = std::os::unix::net::UnixListener::bind(&inside_sock).unwrap();
        let _b = std::os::unix::net::UnixListener::bind(&outside_sock).unwrap();
        let root = project.path().to_str().unwrap();
        let profile = scoped_profile(root, &["a.txt"], "/Users/nobody");
        let reach = |path: &std::path::Path| {
            std::process::Command::new("/usr/bin/sandbox-exec")
                .args(["-p", &profile, "/usr/bin/nc", "-U", "-w", "1", &path.to_string_lossy()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(reach(&inside_sock), "a socket in the project");
        assert!(reach(std::path::Path::new("/private/var/run/mDNSResponder")), "DNS, or no provider is reachable");
        assert!(!reach(&outside_sock), "a socket elsewhere in the home folder");
    }

    #[test]
    fn workers_reach_only_their_own_unix_sockets_and_git_stays_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let real = std::fs::canonicalize(root).unwrap().to_string_lossy().into_owned();
        let profile = scoped_profile(root, &["src/"], "/Users/x");
        let deny = profile.find("(deny network-outbound (remote unix-socket))").expect("sockets are refused first");
        let allow = profile.find(&format!("(allow network-outbound (remote unix-socket (subpath \"{real}\")))")).unwrap();
        assert!(profile.contains("(allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))"), "DNS has its own rule");
        assert!(deny < allow && profile.contains("mDNSResponder") && profile.contains("com\\.apple\\.launchd"));
        assert!(profile.trim_end().ends_with("[^/]+/\")))"), "the socket rules are last, so they win");
        assert!(profile.contains(r"/\.git/(config|hooks"));
        let with_bus = scoped_profile_with_bus(root, &["src/"], "/Users/x", Some("/Users/x/.cordata/bus/ab/bus.sock"));
        let bus = with_bus.find("(allow network-outbound (remote unix-socket (path-literal \"/Users/x/.cordata/bus/ab/bus.sock\")))").expect("the bus is allowed");
        assert!(bus > with_bus.find("(deny network-outbound (remote unix-socket))").unwrap(), "after the blanket refusal, so it wins");
        assert!(with_bus.contains("(deny file-read* file-write* (subpath \"/Users/x/.cordata\"))"), "the folder stays shut");
    }
}
