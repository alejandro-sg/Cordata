//! The native tool registry: `read_file`, `edit_file`, `run_command`, `code_search`, `code_outline`.
//! Pure functions of (arguments, cwd) so they test without a model.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};

use super::sandbox;
use crate::events::{FileDiff, ToolKind};

/// Files over this many lines come back as an outline unless a range is asked for.
pub(crate) const OUTLINE_LINES: usize = 500;
/// Tool output over this many characters is written to disk and replaced by a head + pointer.
pub(crate) const OFFLOAD_CHARS: usize = 6_000;
const HEAD_CHARS: usize = 2_000;
const MAX_MATCHES: usize = 200;
const DEFAULT_TIMEOUT_SECS: u64 = 120;

pub(crate) struct ToolSpec {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) schema: Value,
}

impl cordata_economizer::toolprune::Named for ToolSpec {
    fn tool_name(&self) -> &str {
        self.name
    }
}

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_file",
            description: "Read a text file. Files over 500 lines return an outline unless offset/limit are given.",
            schema: json!({"type":"object","properties":{
                "path":{"type":"string"},
                "offset":{"type":"integer","description":"1-based first line"},
                "limit":{"type":"integer","description":"number of lines"}},
                "required":["path"]}),
        },
        ToolSpec {
            name: "edit_file",
            description: "Replace `target` with `replacement` in a file. `target` must occur exactly once. \
                          For a new file, use an empty `target`.",
            schema: json!({"type":"object","properties":{
                "path":{"type":"string"},"target":{"type":"string"},"replacement":{"type":"string"}},
                "required":["path","target","replacement"]}),
        },
        ToolSpec {
            name: "run_command",
            description: "Run a shell command in the project directory, sandboxed. Returns combined stdout and stderr.",
            schema: json!({"type":"object","properties":{
                "cmd":{"type":"string"},"timeout":{"type":"integer","description":"seconds, default 120"}},
                "required":["cmd"]}),
        },
        ToolSpec {
            name: "code_search",
            description: "Regex search over files (gitignore-aware). Returns path:line:text.",
            schema: json!({"type":"object","properties":{
                "pattern":{"type":"string"},"path":{"type":"string","description":"directory or file, default ."}},
                "required":["pattern"]}),
        },
        ToolSpec {
            name: "code_outline",
            description: "List the definitions (functions, types, headings) in a file with line numbers.",
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        },
    ]
}

pub(crate) fn kind_of(name: &str) -> ToolKind {
    match name {
        "read_file" | "code_outline" => ToolKind::Read,
        "edit_file" => ToolKind::Edit,
        "run_command" => ToolKind::Execute,
        "code_search" => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

/// Mutating tools ask first; reads and searches do not.
pub(crate) fn needs_approval(name: &str) -> bool {
    matches!(kind_of(name), ToolKind::Edit | ToolKind::Execute | ToolKind::Other)
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing string argument `{key}`"))
}

/// A path the tools may touch: relative to `cwd`, never a secret file. Edits must also stay inside `cwd`.
fn resolve(cwd: &Path, raw: &str, must_be_inside: bool) -> Result<PathBuf, String> {
    let joined = if Path::new(raw).is_absolute() { PathBuf::from(raw) } else { cwd.join(raw) };
    if sandbox::is_secret_path(&joined) {
        return Err(format!("{raw} is a secret file; the sandbox keeps it from tools"));
    }
    if must_be_inside {
        // Resolve `..` lexically: the file may not exist yet, so canonicalize cannot be used.
        let mut clean = PathBuf::new();
        for part in joined.components() {
            match part {
                std::path::Component::ParentDir => {
                    clean.pop();
                }
                other => clean.push(other),
            }
        }
        let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let inside = clean.starts_with(cwd) || clean.starts_with(&root);
        if !inside {
            return Err(format!("{raw} is outside the project directory"));
        }
        return Ok(clean);
    }
    Ok(joined)
}

/// What an edit will change, for the approval card. `None` when the arguments cannot produce one.
pub(crate) fn preview(name: &str, args: &Value, cwd: &Path) -> Option<Vec<FileDiff>> {
    if name != "edit_file" {
        return None;
    }
    let path = resolve(cwd, arg(args, "path").ok()?, true).ok()?;
    let (old, new) = apply_edit(&path, args).ok()?;
    Some(vec![FileDiff { path: path.display().to_string(), old_text: old, new_text: new }])
}

/// Returns (old contents or `None` for a new file, new contents).
fn apply_edit(path: &Path, args: &Value) -> Result<(Option<String>, String), String> {
    let target = arg(args, "target")?;
    let replacement = arg(args, "replacement")?;
    match std::fs::read_to_string(path) {
        Ok(old) => {
            if target.is_empty() {
                return Err("`target` is empty but the file exists; give the text to replace".into());
            }
            match old.matches(target).count() {
                0 => Err("`target` was not found in the file".into()),
                1 => {
                    let new = old.replacen(target, replacement, 1);
                    Ok((Some(old), new))
                }
                n => Err(format!("`target` occurs {n} times; include more context so it is unique")),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && target.is_empty() => Ok((None, replacement.to_owned())),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Runs a tool. Output is already length-limited by [`limit_output`] at the call site.
pub(crate) async fn execute(name: &str, args: &Value, cwd: &Path) -> Result<String, String> {
    match name {
        "read_file" => read_file(args, cwd),
        "edit_file" => {
            let path = resolve(cwd, arg(args, "path")?, true)?;
            let (old, new) = apply_edit(&path, args)?;
            if old.is_none() {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
            }
            std::fs::write(&path, new).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            Ok(format!("{} {}", if old.is_some() { "edited" } else { "created" }, path.display()))
        }
        "run_command" => {
            let secs = args.get("timeout").and_then(Value::as_u64).unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, 600);
            sandbox::run(arg(args, "cmd")?, cwd, Duration::from_secs(secs)).await
        }
        "code_search" => code_search(args, cwd),
        "code_outline" => {
            let path = resolve(cwd, arg(args, "path")?, false)?;
            let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            Ok(outline(&text))
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

fn read_file(args: &Value, cwd: &Path) -> Result<String, String> {
    let path = resolve(cwd, arg(args, "path")?, false)?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let offset = args.get("offset").and_then(Value::as_u64);
    let limit = args.get("limit").and_then(Value::as_u64);
    let total = text.lines().count();
    if offset.is_none() && limit.is_none() && total > OUTLINE_LINES {
        return Ok(format!(
            "{} has {total} lines; showing its outline. Re-read with offset/limit for a range.\n{}",
            path.display(),
            outline(&text)
        ));
    }
    let start = offset.unwrap_or(1).max(1) as usize - 1;
    let take = limit.map_or(usize::MAX, |l| l as usize);
    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(start).take(take) {
        let _ = writeln!(out, "{:>5}  {line}", i + 1);
    }
    Ok(out)
}

fn outline(text: &str) -> String {
    let def = Regex::new(
        r"^\s*(pub(\([^)]*\))?\s+)?(async\s+)?(unsafe\s+)?(fn|struct|enum|trait|impl|mod|class|def|function|interface|type|export)\b|^#{1,4}\s",
    )
    .expect("static regex");
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if def.is_match(line) {
            let _ = writeln!(out, "{:>5}  {}", i + 1, line.trim_end());
        }
    }
    if out.is_empty() { "(no definitions found)".into() } else { out }
}

fn code_search(args: &Value, cwd: &Path) -> Result<String, String> {
    let re = Regex::new(arg(args, "pattern")?).map_err(|e| format!("bad pattern: {e}"))?;
    let root = resolve(cwd, args.get("path").and_then(Value::as_str).unwrap_or("."), false)?;
    let mut out = String::new();
    let mut hits = 0;
    'files: for entry in ignore::WalkBuilder::new(&root).build().flatten() {
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) || sandbox::is_secret_path(path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else { continue }; // binary or unreadable
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let shown = path.strip_prefix(cwd).unwrap_or(path);
                let _ = writeln!(out, "{}:{}:{}", shown.display(), i + 1, line.trim_end());
                hits += 1;
                if hits >= MAX_MATCHES {
                    out.push_str("(stopped at 200 matches)\n");
                    break 'files;
                }
            }
        }
    }
    if hits == 0 { Ok("(no matches)".into()) } else { Ok(out) }
}

/// Output over [`OFFLOAD_CHARS`] goes to `dir`; the model gets the head and the path.
pub(crate) fn limit_output(text: String, dir: &Path) -> String {
    let total = text.chars().count();
    if total <= OFFLOAD_CHARS {
        return text;
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let file = dir.join(format!("native-{stamp}-{}-{n}.txt", std::process::id()));
    let head: String = text.chars().take(HEAD_CHARS).collect();
    match std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&file, &text)) {
        Ok(()) => format!("{head}\n…[{total} chars; full output offloaded to {} — read it with read_file offset/limit]", file.display()),
        Err(_) => format!("{head}\n…[{total} chars; truncated, offload failed]"),
    }
}

pub(crate) fn offload_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cordata/offloaded")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn edit_requires_a_unique_target_and_previews_the_diff() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.txt"), "one two one").unwrap();
        let cwd = dir.path();
        let dup = json!({"path":"a.txt","target":"one","replacement":"1"});
        assert!(execute("edit_file", &dup, cwd).await.unwrap_err().contains("2 times"));
        let miss = json!({"path":"a.txt","target":"zzz","replacement":"1"});
        assert!(execute("edit_file", &miss, cwd).await.unwrap_err().contains("not found"));
        let ok = json!({"path":"a.txt","target":"two","replacement":"2"});
        let diffs = preview("edit_file", &ok, cwd).unwrap();
        assert_eq!(diffs[0].old_text.as_deref(), Some("one two one"));
        assert_eq!(diffs[0].new_text, "one 2 one");
        assert!(execute("edit_file", &ok, cwd).await.unwrap().starts_with("edited"));
        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "one 2 one");
    }

    #[tokio::test]
    async fn edit_creates_files_but_not_outside_the_project_or_secrets() {
        let dir = tmp();
        let cwd = dir.path();
        let new = json!({"path":"sub/n.txt","target":"","replacement":"hi"});
        assert!(execute("edit_file", &new, cwd).await.unwrap().starts_with("created"));
        assert_eq!(std::fs::read_to_string(cwd.join("sub/n.txt")).unwrap(), "hi");
        let out = json!({"path":"../escape.txt","target":"","replacement":"x"});
        assert!(execute("edit_file", &out, cwd).await.unwrap_err().contains("outside"));
        let env = json!({"path":".env","target":"","replacement":"K=1"});
        assert!(execute("edit_file", &env, cwd).await.unwrap_err().contains("secret"));
        assert!(execute("read_file", &json!({"path":".env.local"}), cwd).await.unwrap_err().contains("secret"));
    }

    #[tokio::test]
    async fn long_files_come_back_as_an_outline_until_a_range_is_given() {
        let dir = tmp();
        let mut body = String::new();
        for i in 0..600 {
            body.push_str(&format!("fn f{i}() {{}}\n// filler\n"));
        }
        std::fs::write(dir.path().join("big.rs"), &body).unwrap();
        let out = execute("read_file", &json!({"path":"big.rs"}), dir.path()).await.unwrap();
        assert!(out.contains("1200 lines") && out.contains("fn f599"));
        assert!(!out.contains("filler"));
        let ranged = execute("read_file", &json!({"path":"big.rs","offset":2,"limit":1}), dir.path()).await.unwrap();
        assert_eq!(ranged, "    2  // filler\n");
    }

    #[tokio::test]
    async fn search_skips_secrets_and_reports_locations() {
        let dir = tmp();
        std::fs::write(dir.path().join("a.rs"), "let needle = 1;\n").unwrap();
        std::fs::write(dir.path().join(".env"), "needle=secret\n").unwrap();
        let out = execute("code_search", &json!({"pattern":"needle"}), dir.path()).await.unwrap();
        assert!(out.contains("a.rs:1:let needle"));
        assert!(!out.contains("secret"));
        let none = execute("code_search", &json!({"pattern":"zzz"}), dir.path()).await.unwrap();
        assert_eq!(none, "(no matches)");
    }

    #[test]
    fn big_output_is_offloaded() {
        let dir = tmp();
        let small = limit_output("x".repeat(100), dir.path());
        assert_eq!(small.len(), 100);
        let big = limit_output("y".repeat(7000), dir.path());
        assert!(big.contains("offloaded to"));
        assert!(big.chars().count() < 2400);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn approval_policy() {
        assert!(!needs_approval("read_file") && !needs_approval("code_search") && !needs_approval("code_outline"));
        assert!(needs_approval("edit_file") && needs_approval("run_command") && needs_approval("mystery"));
    }
}
