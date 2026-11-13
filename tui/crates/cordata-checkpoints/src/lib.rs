//! Checkpoints: what makes `auto` mode and the orchestrator's rollbacks
//! reversible.
//!
//! Before any step that may change files, the project is snapshotted into a **shadow git repository** under
//! `~/.cordata/checkpoints/<key>/git` — never inside the project, so the project's own `.git` (commits, branches,
//! index, hooks) is untouched, and it works in a folder that is not a git repository at all. Each snapshot is one commit
//! whose message carries who asked for it. The layout, exclude list and commit-message format are the Python module's,
//! so either implementation reads and restores the other's snapshots (`tests/python_compat.rs`).
//!
//! Covered: every file that is not git-ignored, not a dependency/build directory, not a secrets file and not over
//! `max_file_mb`. Created files are removed on restore, deleted ones recreated. Not covered: ignored paths, `.env*` /
//! keys / `.npmrc` (never copied), files over the size limit, the insides of a nested repository, anything outside the
//! project, and side effects that are not files. A project over 50,000 files, your home directory and `/` are not
//! checkpointed, and say why.
//!
//! Never panics into a run: a failed snapshot is a [`Snapshot`] with `ok == false` and a reason, which callers report as
//! *unprotected* — never silently as protected.
//!
//! Not ported: `writes_files` / `shell_writes` (the heuristic that decides whether a tool call may change files), which
//! the Python agent loop uses and which depends on its verifier recognition.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};

pub const SCHEMA: i64 = 1;
pub const DEFAULT_MAX_FILE_MB: f64 = 8.0;
pub const MAX_PROJECT_FILES: usize = 50_000;
pub const MAX_LIST: usize = 200;
const FIRST_TIMEOUT: Duration = Duration::from_secs(90);
const TIMEOUT: Duration = Duration::from_secs(25);
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EXCLUDE_VERSION: &str = "# cordata checkpoint excludes v2";

/// Directories that are rebuilt or vendored.
pub const DEPENDENCY_DIRS: [&str; 18] = [
    ".git", "node_modules", "target", ".venv", "venv", "__pycache__", ".next", "dist", "build", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox", ".cache", "coverage", ".turbo", ".gradle", ".idea",
];

const EXCLUDES: &str = "# cordata checkpoint excludes v2
.git/
.cordata/
.DS_Store
node_modules/
target/
.venv/
venv/
__pycache__/
*.pyc
.next/
dist/
build/
.mypy_cache/
.pytest_cache/
.ruff_cache/
.tox/
.cache/
.turbo/
.gradle/
.idea/
*.lance/
# Secrets are never copied into a snapshot.
.env
.env.*
!.env.example
!.env.sample
!.env.template
!.env.dist
id_rsa*
id_ed25519*
*.pem
*.p12
*.pfx
*.key
*.keychain
.npmrc
.netrc
.pypirc
";

const NEVER: [&str; 17] = ["/", "/Users", "/home", "/tmp", "/private", "/private/tmp", "/var", "/private/var", "/etc", "/usr", "/opt", "/bin", "/sbin", "/System", "/Library", "/Applications", "/Volumes"];

static LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = LazyLock::new(Mutex::default);
static VIABLE: LazyLock<Mutex<HashMap<String, Option<String>>>> = LazyLock::new(Mutex::default);

// ---------------------------------------------------------------------------------------------- results

/// The outcome of [`Store::take`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub ok: bool,
    pub id: Option<String>,
    pub reason: Option<String>,
    /// Paths the call is about to write that are outside the project: a checkpoint cannot cover them.
    pub outside: Vec<String>,
    pub ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: String,
    pub change: Change,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileAction {
    pub path: String,
    /// `delete` (a file the snapshot did not have), `recreate`, or `revert`.
    pub action: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Restore {
    pub ok: bool,
    pub reason: Option<String>,
    pub target: Option<String>,
    pub dry_run: bool,
    pub files: Vec<FileAction>,
    pub count: usize,
    pub more: usize,
    /// Files someone changed since `edited_after`, which the restore would overwrite.
    pub edited_since: Vec<String>,
    /// The checkpoint of the present, taken first, so a restore can itself be undone.
    pub undo_with: Option<String>,
    pub failed: Vec<String>,
    /// Set by [`Store::undo_turn`].
    pub turn: Option<String>,
    pub calls: Option<usize>,
}

impl Restore {
    fn refused(reason: impl Into<String>) -> Self {
        Self { ok: false, reason: Some(reason.into()), ..Self::default() }
    }
}

/// One row of history, with the commit-message metadata flattened out.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub id: String,
    pub ts: f64,
    pub meta: Value,
}

impl Entry {
    fn str(&self, key: &str) -> Option<&str> {
        self.meta.get(key).and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Turn {
    pub turn: String,
    pub session: Option<String>,
    /// The project as it was before the turn changed anything.
    pub first: Option<String>,
    pub end: Option<String>,
    pub calls: usize,
    pub tools: Vec<String>,
    pub agents: Vec<String>,
    pub ts: f64,
    pub started: Option<f64>,
    pub last_call: Option<String>,
    pub undone: bool,
    pub files: Option<Vec<FileChange>>,
    pub file_count: Option<usize>,
}

/// What [`Store::take`] needs to know about the request.
#[derive(Debug, Clone, Default)]
pub struct TakeOptions {
    /// `pre` (before a mutating call), `end` (after a turn), `restore` / `restore-from`. Empty means `pre`.
    pub kind: String,
    pub tool: String,
    pub agent: String,
    pub session_id: String,
    pub turn: String,
    pub call: String,
    pub paths: Vec<String>,
    pub max_file_mb: Option<f64>,
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------------------------- plumbing

fn real(project: &Path) -> String {
    std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf()).to_string_lossy().into_owned()
}

fn lock_for(project: &str) -> Arc<Mutex<()>> {
    LOCKS.lock().unwrap().entry(project.to_owned()).or_default().clone()
}

fn inside(project: &str, path: &str) -> bool {
    let joined = if Path::new(path).is_absolute() { PathBuf::from(path) } else { Path::new(project).join(path) };
    let mut clean = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    let full = clean.to_string_lossy().into_owned();
    full == project || full.starts_with(&format!("{}/", project.trim_end_matches('/')))
}

fn ascii_json(value: &Value) -> String {
    let text = serde_json::to_string(value).expect("a Value serializes");
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

fn escape_glob(path: &str) -> String {
    path.chars().flat_map(|c| if "\\*?[]!#".contains(c) { vec!['\\', c] } else { vec![c] }).collect()
}

#[allow(clippy::too_many_arguments)]
fn meta_message(kind: &str, tool: &str, agent: &str, session_id: &str, turn: &str, call: &str, paths: &[String], extra: &Map<String, Value>) -> String {
    let mut data = Map::new();
    data.insert("v".into(), json!(SCHEMA));
    data.insert("kind".into(), json!(kind));
    data.insert("tool".into(), json!(tool));
    data.insert("agent".into(), json!(agent));
    data.insert("session".into(), json!(session_id));
    data.insert("turn".into(), json!(turn));
    data.insert("call".into(), json!(call.chars().take(240).collect::<String>()));
    data.insert("paths".into(), json!(paths.iter().take(20).collect::<Vec<_>>()));
    data.extend(extra.clone());
    format!("{kind} {}\n\n{}\n", if tool.is_empty() { "-" } else { tool }, ascii_json(&Value::Object(data)))
}

fn parse_meta(body: &str) -> Value {
    body.trim().lines().rev().find(|l| l.starts_with('{')).and_then(|l| serde_json::from_str::<Value>(l).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}))
}

struct Output {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `cmd` with a deadline; the child is killed when it passes. `Err` is a timeout or a spawn failure.
fn run_with_timeout(mut cmd: Command, input: Option<Vec<u8>>, timeout: Duration) -> Result<Output, String> {
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let writer = child.stdin.take().zip(input).map(|(mut pipe, bytes)| std::thread::spawn(move || {
        let _ = pipe.write_all(&bytes);
    }));
    let drain = |mut r: Box<dyn Read + Send>| std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    });
    let out_t = drain(Box::new(child.stdout.take().expect("piped")));
    let err_t = drain(Box::new(child.stderr.take().expect("piped")));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status.code().unwrap_or(-1)),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e.to_string()),
        }
    };
    if let Some(w) = writer {
        let _ = w.join();
    }
    Ok(Output { status, stdout: out_t.join().unwrap_or_default(), stderr: err_t.join().unwrap_or_default() })
}

/// `git` bound to one project's shadow repository.
struct Git {
    project: String,
    dir: PathBuf,
    env: Vec<(String, String)>,
}

impl Git {
    fn new(project: &str, dir: PathBuf) -> Self {
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_owned());
        let env = vec![
            ("PATH".into(), var("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")),
            ("HOME".into(), var("HOME", "")),
            ("LC_ALL".into(), "C".into()),
            ("GIT_DIR".into(), dir.to_string_lossy().into_owned()),
            ("GIT_WORK_TREE".into(), project.to_owned()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
            ("GIT_AUTHOR_NAME".into(), "cordata".into()),
            ("GIT_AUTHOR_EMAIL".into(), "checkpoints@cordata.local".into()),
            ("GIT_COMMITTER_NAME".into(), "cordata".into()),
            ("GIT_COMMITTER_EMAIL".into(), "checkpoints@cordata.local".into()),
        ];
        Self { project: project.to_owned(), dir, env }
    }

    /// Literal pathspecs: a file called `*.txt` or `[a]` is that file, not a glob.
    fn run(&self, args: &[&str], input: Option<Vec<u8>>, timeout: Duration, check: bool) -> Result<Output, String> {
        let mut cmd = Command::new("git");
        cmd.arg("--literal-pathspecs").args(args).env_clear().envs(self.env.iter().cloned()).current_dir(&self.project);
        let out = run_with_timeout(cmd, input, timeout)?;
        if check && out.status != Some(0) {
            let msg = String::from_utf8_lossy(&out.stderr);
            let msg = msg.trim();
            return Err(format!("git {}: {}", args[0], if msg.is_empty() { out.status.map_or("?".into(), |s| s.to_string()) } else { msg.chars().take(300).collect() }));
        }
        Ok(out)
    }

    fn out(&self, args: &[&str]) -> Result<String, String> {
        self.out_with(args, None, TIMEOUT)
    }

    fn out_with(&self, args: &[&str], input: Option<Vec<u8>>, timeout: Duration) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.run(args, input, timeout, true)?.stdout).into_owned())
    }

    fn ok(&self, args: &[&str]) -> bool {
        self.run(args, None, TIMEOUT, false).is_ok_and(|o| o.status == Some(0))
    }

    fn has_head(&self) -> bool {
        self.ok(&["rev-parse", "--verify", "-q", "HEAD"])
    }
}

// ---------------------------------------------------------------------------------------------- the store

/// A checkpoint store rooted at `home` (`$CORDATA_HOME`, else `~/.cordata`).
#[derive(Debug, Clone)]
pub struct Store {
    home: PathBuf,
}

impl Store {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn from_env() -> Self {
        Self::new(std::env::var_os("CORDATA_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cordata")))
    }

    /// Where this project's snapshots live (the shadow repository's git directory is inside it).
    pub fn shadow_dir(&self, project: &Path) -> PathBuf {
        let digest = Sha1::digest(real(project).as_bytes());
        let key: String = digest.iter().map(|b| format!("{b:02x}")).collect::<String>().chars().take(16).collect();
        self.home.join("checkpoints").join(key)
    }

    fn git(&self, root: &str) -> Git {
        Git::new(root, self.shadow_dir(Path::new(root)).join("git"))
    }

    fn exists(&self, root: &str) -> bool {
        self.shadow_dir(Path::new(root)).join("git/HEAD").exists()
    }

    fn ensure(&self, git: &Git) -> Result<(), String> {
        let root = git.dir.parent().expect("git dir has a parent").to_path_buf();
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
            if let Some(parent) = root.parent() {
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        if !git.dir.join("HEAD").exists() {
            git.run(&["init", "-q"], None, TIMEOUT, true)?;
            git.run(&["symbolic-ref", "HEAD", "refs/heads/main"], None, TIMEOUT, true)?;
            let big_files = git.dir.join("big-files").to_string_lossy().into_owned();
            for (key, value) in [
                ("core.autocrlf", "false"),
                ("core.safecrlf", "false"),
                ("core.quotepath", "false"),
                ("core.fsmonitor", "false"),
                ("core.untrackedCache", "true"),
                ("core.preloadindex", "true"),
                ("core.hooksPath", "/dev/null"),
                ("commit.gpgsign", "false"),
                ("gc.auto", "0"),
                ("advice.addEmbeddedRepo", "false"),
                ("core.excludesFile", big_files.as_str()),
            ] {
                git.run(&["config", key, value], None, TIMEOUT, true)?;
            }
            std::fs::write(root.join("project.txt"), format!("{}\n", git.project)).map_err(|e| e.to_string())?;
        }
        let info = git.dir.join("info");
        std::fs::create_dir_all(&info).map_err(|e| e.to_string())?;
        let exclude = info.join("exclude");
        if std::fs::read_to_string(&exclude).map_or(true, |t| !t.starts_with(EXCLUDE_VERSION)) {
            std::fs::write(&exclude, EXCLUDES).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Why this directory should not be checkpointed, or `None`. Cached per process: a walk is not free.
    fn unviable(&self, project: &str) -> Option<String> {
        if let Some(cached) = VIABLE.lock().unwrap().get(project) {
            return cached.clone();
        }
        let home = real(Path::new(&std::env::var("HOME").unwrap_or_default()));
        let reason = if !Path::new(project).is_dir() {
            Some("the project directory does not exist".to_owned())
        } else if NEVER.contains(&project) || project == home {
            Some("refusing to checkpoint your home directory or a system directory".into())
        } else if !on_path("git") {
            Some("git is not installed".into())
        } else {
            let mut count = 0usize;
            let mut stack = vec![PathBuf::from(project)];
            let mut found = None;
            'walk: while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let Ok(kind) = entry.file_type() else { continue };
                    if kind.is_dir() {
                        if !DEPENDENCY_DIRS.contains(&entry.file_name().to_string_lossy().as_ref()) {
                            stack.push(entry.path());
                        }
                    } else {
                        count += 1;
                        if count > MAX_PROJECT_FILES {
                            found = Some(format!("the project has more than {} files", group_thousands(MAX_PROJECT_FILES)));
                            break 'walk;
                        }
                    }
                }
            }
            found
        };
        VIABLE.lock().unwrap().insert(project.to_owned(), reason.clone());
        reason
    }

    /// Stage the working tree, then drop any *new* file over the size limit from the snapshot (and remember it so it is
    /// skipped next time) — a dataset someone forgot to ignore must not be copied on every call.
    fn add_all(&self, git: &Git, max_file_bytes: u64, first: bool) -> Result<(), String> {
        let _ = git.run(&["add", "-A", "--ignore-errors"], None, if first { FIRST_TIMEOUT } else { TIMEOUT }, false)?;
        let base = if git.has_head() { "HEAD" } else { EMPTY_TREE };
        let raw = git.out(&["diff", "--cached", "--raw", "-z", "--no-renames", "--abbrev=40", "--diff-filter=A", base])?;
        let parts: Vec<&str> = raw.split('\0').collect();
        let mut added: Vec<(String, String)> = Vec::new();
        let mut i = 0;
        while i + 1 < parts.len() {
            let fields: Vec<&str> = parts[i].trim_start_matches(':').split_whitespace().collect();
            if fields.len() >= 4 && !fields[3].chars().all(|c| c == '0') {
                added.push((fields[3].to_owned(), parts[i + 1].to_owned()));
            }
            i += 2;
        }
        if added.is_empty() {
            return Ok(());
        }
        let wanted = added.iter().map(|(sha, _)| sha.as_str()).collect::<Vec<_>>().join("\n") + "\n";
        let sizes = git.out_with(&["cat-file", "--batch-check=%(objectname) %(objectsize)"], Some(wanted.into_bytes()), TIMEOUT)?;
        let big: Vec<&str> = added
            .iter()
            .zip(sizes.lines())
            .filter(|(_, line)| line.split_whitespace().last().and_then(|n| n.parse::<u64>().ok()).is_some_and(|n| n > max_file_bytes))
            .map(|((_, path), _)| path.as_str())
            .collect();
        if big.is_empty() {
            return Ok(());
        }
        let input = big.iter().map(|p| format!("{p}\0")).collect::<String>().into_bytes();
        let _ = git.run(&["rm", "-q", "--cached", "--ignore-unmatch", "--pathspec-from-file=-", "--pathspec-file-nul"], Some(input), TIMEOUT, false);
        let mut handle = std::fs::OpenOptions::new().append(true).create(true).open(git.dir.join("big-files")).map_err(|e| e.to_string())?;
        for path in big {
            writeln!(handle, "/{}", escape_glob(path)).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn clear_stale_lock(&self, project: &str) {
        let lock = self.git(project).dir.join("index.lock");
        if let Ok(meta) = std::fs::metadata(&lock)
            && meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(5)) {
                let _ = std::fs::remove_file(lock);
            }
    }

    // ------------------------------------------------------------------------------ taking one

    /// Snapshot the project. Always returns; `ok == false` carries the reason and means *not protected*.
    pub fn take(&self, project: &Path, opts: &TakeOptions) -> Snapshot {
        let started = Instant::now();
        let root = real(project);
        let outside: Vec<String> = opts.paths.iter().filter(|p| !inside(&root, p)).cloned().collect();
        let elapsed = || started.elapsed().as_millis() as u64;
        let fail = |reason: String| Snapshot { ok: false, id: None, reason: Some(reason), outside: outside.clone(), ms: elapsed() };
        if let Some(reason) = self.unviable(&root) {
            return Snapshot { ok: false, id: None, reason: Some(reason), outside, ms: 0 };
        }
        let lock = lock_for(&root);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let git = self.git(&root);
        let first = !git.dir.join("HEAD").exists();
        let attempt = || -> Result<String, String> {
            self.ensure(&git)?;
            self.add_all(&git, (opts.max_file_mb.unwrap_or(DEFAULT_MAX_FILE_MB) * 1024.0 * 1024.0) as u64, first)?;
            let relative: Vec<String> = opts
                .paths
                .iter()
                .filter(|p| !outside.contains(p))
                .map(|p| {
                    let full = if Path::new(p).is_absolute() { PathBuf::from(p) } else { Path::new(&root).join(p) };
                    full.strip_prefix(&root).map_or_else(|_| p.clone(), |r| r.to_string_lossy().into_owned())
                })
                .collect();
            let kind = if opts.kind.is_empty() { "pre" } else { &opts.kind };
            let message = meta_message(kind, &opts.tool, &opts.agent, &opts.session_id, &opts.turn, &opts.call, &relative, &opts.extra);
            git.run(&["commit", "-q", "--allow-empty", "--no-verify", "--no-gpg-sign", "-m", &message], None, TIMEOUT, true)?;
            Ok(git.out(&["rev-parse", "HEAD"])?.trim().to_owned())
        };
        match attempt() {
            Ok(sha) => Snapshot { ok: true, id: Some(sha), reason: None, outside, ms: elapsed() },
            Err(e) if e == "timed out" => {
                self.clear_stale_lock(&root);
                fail("the snapshot timed out (a very large project?)".into())
            }
            Err(e) => fail(e.chars().take(300).collect()),
        }
    }

    // ------------------------------------------------------------------------------ reading history

    fn log(git: &Git, limit: usize) -> Result<Vec<Entry>, String> {
        if !git.has_head() {
            return Ok(Vec::new());
        }
        let raw = git.out(&["log", &format!("-n{limit}"), "--format=%H%x1f%ct%x1f%B%x1e"])?;
        Ok(raw
            .split('\x1e')
            .map(|c| c.trim_matches('\n'))
            .filter(|c| !c.trim().is_empty())
            .map(|chunk| {
                let (sha, rest) = chunk.split_once('\x1f').unwrap_or((chunk, ""));
                let (stamp, body) = rest.split_once('\x1f').unwrap_or((rest, ""));
                Entry { id: sha.trim().to_owned(), ts: stamp.parse().unwrap_or(0.0), meta: parse_meta(body) }
            })
            .collect())
    }

    fn parse_status(raw: &str) -> Vec<FileChange> {
        let parts: Vec<&str> = raw.split('\0').collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i + 1 < parts.len() {
            let (status, path) = (parts[i], parts[i + 1]);
            if !status.is_empty() && !path.is_empty() {
                let change = match status.chars().next() {
                    Some('A') => Change::Added,
                    Some('D') => Change::Deleted,
                    _ => Change::Modified,
                };
                out.push(FileChange { path: path.to_owned(), change });
            }
            i += 2;
        }
        out
    }

    fn name_status(git: &Git, old: &str, new: Option<&str>, cached: bool) -> Result<Vec<FileChange>, String> {
        let mut args = vec!["diff", "--name-status", "-z", "--no-renames"];
        if cached {
            args.push("--cached");
        }
        args.push(old);
        args.extend(new);
        Ok(Self::parse_status(&git.out(&args)?))
    }

    fn scoped_changes(git: &Git, sha: &str, paths: &[String]) -> Result<Vec<FileChange>, String> {
        let mut args = vec!["diff", "--name-status", "-z", "--no-renames", "--cached", sha, "--"];
        args.extend(paths.iter().map(String::as_str));
        Ok(Self::parse_status(&git.out(&args)?))
    }

    /// The turns that have checkpoints, newest first: the first checkpoint (the project before the turn changed
    /// anything), the end snapshot if the turn finished, the files it changed, and whether it has since been undone.
    pub fn turns(&self, project: &Path, limit: usize) -> Vec<Turn> {
        let root = real(project);
        if !self.exists(&root) {
            return Vec::new();
        }
        let lock = lock_for(&root);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let git = self.git(&root);
        let Ok(rows) = Self::log(&git, 600) else { return Vec::new() };
        let mut order: Vec<String> = Vec::new();
        let mut by_turn: HashMap<String, Turn> = HashMap::new();
        let mut undone: HashSet<String> = HashSet::new();
        for row in &rows {
            // newest first
            if row.str("kind") == Some("restore")
                && let Some(u) = row.meta.get("undid").and_then(Value::as_str).filter(|u| !u.is_empty()) {
                    undone.insert(u.to_owned());
                }
            let turn = row.str("turn").unwrap_or("");
            if turn.is_empty() || !matches!(row.str("kind"), Some("pre" | "end")) {
                continue;
            }
            if !by_turn.contains_key(turn) {
                order.push(turn.to_owned());
            }
            let entry = by_turn.entry(turn.to_owned()).or_insert_with(|| Turn {
                turn: turn.to_owned(),
                session: row.str("session").map(str::to_owned),
                first: None,
                end: None,
                calls: 0,
                tools: Vec::new(),
                agents: Vec::new(),
                ts: row.ts,
                started: None,
                last_call: None,
                undone: false,
                files: None,
                file_count: None,
            });
            if row.str("kind") == Some("pre") {
                entry.first = Some(row.id.clone()); // overwritten as we walk back: ends at the oldest
                entry.calls += 1;
                entry.started = Some(row.ts);
                for (list, key) in [(&mut entry.tools, "tool"), (&mut entry.agents, "agent")] {
                    if let Some(v) = row.str(key).filter(|v| !v.is_empty())
                        && !list.iter().any(|x| x == v) {
                            list.push(v.to_owned());
                        }
                }
                if entry.last_call.as_deref().is_none_or(str::is_empty) {
                    entry.last_call = row.str("call").map(str::to_owned);
                }
            } else if entry.end.is_none() {
                entry.end = Some(row.id.clone());
            }
        }
        let mut result = Vec::new();
        for name in order.into_iter().take(limit.max(1)) {
            let mut entry = by_turn.remove(&name).expect("ordered");
            entry.undone = undone.contains(&entry.turn);
            if let (Some(first), Some(end)) = (&entry.first, &entry.end)
                && let Ok(changes) = Self::name_status(&git, first, Some(end), false) {
                    entry.file_count = Some(changes.len());
                    entry.files = Some(changes.into_iter().take(MAX_LIST).collect());
                }
            result.push(entry);
        }
        result
    }

    pub fn list(&self, project: &Path, limit: usize) -> Vec<Entry> {
        let root = real(project);
        if !self.exists(&root) {
            return Vec::new();
        }
        let lock = lock_for(&root);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        Self::log(&self.git(&root), limit.clamp(1, MAX_LIST)).unwrap_or_default()
    }

    // ------------------------------------------------------------------------------ restoring

    fn resolve(git: &Git, reference: &str) -> Result<String, String> {
        let reference = reference.trim().to_lowercase();
        if !(7..=40).contains(&reference.len()) || !reference.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("a checkpoint id is 7-40 hex characters".into());
        }
        let out = git.run(&["rev-parse", "--verify", "-q", &format!("{reference}^{{commit}}")], None, TIMEOUT, false)?;
        if out.status != Some(0) {
            return Err(format!("no checkpoint {reference}"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    fn safe_paths(project: &str, paths: Option<&[String]>) -> Result<Option<Vec<String>>, String> {
        let Some(paths) = paths.filter(|p| !p.is_empty()) else { return Ok(None) };
        let mut out = Vec::new();
        for path in paths {
            let normalized = lexical_join(project, path);
            if !inside(project, &normalized) || normalized == project {
                return Err(format!("'{path}' is outside the project"));
            }
            out.push(normalized.strip_prefix(&format!("{}/", project.trim_end_matches('/'))).unwrap_or(&normalized).to_owned());
        }
        Ok(Some(out))
    }

    /// Put the project (or just `paths`) back as it was at checkpoint `target`. With `dry_run` only reports. Otherwise
    /// the present is snapshotted first (`undo_with`), so a restore can itself be undone. `edited_after` is a
    /// checkpoint id: files that differ now from *that* snapshot — somebody changed them since — are listed in
    /// `edited_since` so the caller can warn before they are overwritten.
    pub fn restore(&self, project: &Path, target: &str, opts: &RestoreOptions) -> Restore {
        let root = real(project);
        if let Some(reason) = self.unviable(&root) {
            return Restore::refused(reason);
        }
        let lock = lock_for(&root);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let git = self.git(&root);
        let run = || -> Result<Restore, String> {
            self.ensure(&git)?;
            let sha = Self::resolve(&git, target)?;
            let limit = Self::safe_paths(&root, opts.paths.as_deref())?;
            let max_bytes = (opts.max_file_mb.unwrap_or(DEFAULT_MAX_FILE_MB) * 1024.0 * 1024.0) as u64;
            self.add_all(&git, max_bytes, false)?;
            let changes = match &limit {
                None => Self::name_status(&git, &sha, None, true)?,
                Some(paths) => Self::scoped_changes(&git, &sha, paths)?,
            };
            let mut edited = Vec::new();
            if let Some(after) = opts.edited_after.as_deref().filter(|a| !a.is_empty()) {
                let after = Self::resolve(&git, after)?;
                let touched: HashSet<&str> = changes.iter().map(|c| c.path.as_str()).collect();
                edited = Self::name_status(&git, &after, None, true)?.into_iter().map(|c| c.path).filter(|p| touched.contains(p.as_str())).collect();
                edited.sort();
            }
            let files: Vec<FileAction> = changes
                .iter()
                .map(|c| FileAction { path: c.path.clone(), action: match c.change { Change::Added => "delete", Change::Deleted => "recreate", Change::Modified => "revert" } })
                .collect();
            let mut report = Restore {
                ok: true,
                target: Some(sha.clone()),
                dry_run: opts.dry_run,
                count: files.len(),
                more: files.len().saturating_sub(MAX_LIST),
                files: files.into_iter().take(MAX_LIST).collect(),
                edited_since: edited.into_iter().take(MAX_LIST).collect(),
                ..Restore::default()
            };
            if opts.dry_run || changes.is_empty() {
                return Ok(report);
            }
            let mut extra = Map::new();
            extra.insert("target".into(), json!(sha));
            let probe = git.run(
                &["commit", "-q", "--allow-empty", "--no-verify", "--no-gpg-sign", "-m", &meta_message("restore-from", "restore", "", &opts.session_id, "", &format!("before restoring {}", &sha[..12]), &[], &extra)],
                None,
                TIMEOUT,
                false,
            )?;
            report.undo_with = (probe.status == Some(0)).then(|| git.out(&["rev-parse", "HEAD"]).map(|s| s.trim().to_owned()).ok()).flatten();
            let mut failed: Vec<String> = Vec::new();
            let write: Vec<&str> = changes.iter().filter(|c| c.change != Change::Added).map(|c| c.path.as_str()).collect();
            for batch in write.chunks(100) {
                let mut args = vec!["checkout", sha.as_str(), "--"];
                args.extend(batch);
                if git.run(&args, None, TIMEOUT, false)?.status != Some(0) {
                    failed.extend(batch.iter().map(|p| (*p).to_owned()));
                }
            }
            for change in changes.iter().filter(|c| c.change == Change::Added) {
                let full = Path::new(&root).join(&change.path);
                if inside(&root, &full.to_string_lossy()) && std::fs::symlink_metadata(&full).is_ok() {
                    match std::fs::remove_file(&full) {
                        Ok(()) => prune_empty_dirs(&root, full.parent()),
                        Err(_) => failed.push(change.path.clone()),
                    }
                }
            }
            self.add_all(&git, max_bytes, false)?;
            let mut extra = Map::new();
            extra.insert("target".into(), json!(sha));
            if !opts.undid_turn.is_empty() {
                extra.insert("undid".into(), json!(opts.undid_turn));
            }
            let paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
            let _ = git.run(&["commit", "-q", "--allow-empty", "--no-verify", "--no-gpg-sign", "-m", &meta_message("restore", "restore", "", &opts.session_id, "", &format!("restored {}", &sha[..12]), &paths, &extra)], None, TIMEOUT, false)?;
            report.dry_run = false;
            report.ok = failed.is_empty();
            report.failed = failed.into_iter().take(MAX_LIST).collect();
            Ok(report)
        };
        match run() {
            Ok(report) => report,
            Err(e) if e == "timed out" => {
                self.clear_stale_lock(&root);
                Restore::refused("the restore timed out")
            }
            Err(e) => Restore::refused(e.chars().take(300).collect::<String>()),
        }
    }

    /// What differs now from checkpoint `target`, optionally only under `paths`. Stages the present into the shadow
    /// index only — no commit. `None` when it cannot tell (no shadow repository, an unknown checkpoint, a timeout);
    /// never an empty list in that case.
    pub fn changes_since(&self, project: &Path, target: &str, paths: Option<&[String]>) -> Option<Vec<FileChange>> {
        let root = real(project);
        if self.unviable(&root).is_some() || !self.exists(&root) {
            return None;
        }
        let lock = lock_for(&root);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let git = self.git(&root);
        let run = || -> Result<Vec<FileChange>, String> {
            let sha = Self::resolve(&git, target)?;
            let limit = Self::safe_paths(&root, paths)?;
            self.add_all(&git, (DEFAULT_MAX_FILE_MB * 1024.0 * 1024.0) as u64, false)?;
            match &limit {
                None => Self::name_status(&git, &sha, None, true),
                Some(p) => Self::scoped_changes(&git, &sha, p),
            }
        };
        match run() {
            Ok(changes) => Some(changes),
            Err(e) => {
                if e == "timed out" {
                    self.clear_stale_lock(&root);
                }
                None
            }
        }
    }

    /// `(unified diff, diffstat line)` of the project now against checkpoint `target`, optionally only `paths`.
    /// `("", "")` when there is nothing to show or it cannot be computed. Secrets files are never in a checkpoint, so
    /// never in this diff.
    pub fn diff_since(&self, project: &Path, target: &str, paths: Option<&[String]>) -> (String, String) {
        const MAX_CHARS: usize = 200_000;
        let root = real(project);
        if self.unviable(&root).is_some() || !self.exists(&root) {
            return (String::new(), String::new());
        }
        let lock = lock_for(&root);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let git = self.git(&root);
        let run = || -> Result<(String, String), String> {
            let sha = Self::resolve(&git, target)?;
            let limit = Self::safe_paths(&root, paths)?.unwrap_or_default();
            self.add_all(&git, (DEFAULT_MAX_FILE_MB * 1024.0 * 1024.0) as u64, false)?;
            let with = |head: &[&str]| -> Vec<String> { head.iter().map(|s| (*s).to_owned()).chain([sha.clone(), "--".to_owned()]).chain(limit.iter().cloned()).collect() };
            let call = |args: Vec<String>, timeout| git.out_with(&args.iter().map(String::as_str).collect::<Vec<_>>(), None, timeout);
            let patch = call(with(&["diff", "--cached", "--no-color", "--no-renames"]), Duration::from_secs(60))?;
            let stat = call(with(&["diff", "--cached", "--shortstat"]), TIMEOUT)?;
            Ok((patch, stat.trim().to_owned()))
        };
        match run() {
            Ok((mut patch, stat)) => {
                if patch.chars().count() > MAX_CHARS {
                    patch = patch.chars().take(MAX_CHARS).collect::<String>() + "\n(diff truncated)\n";
                }
                (patch, stat)
            }
            Err(e) => {
                if e == "timed out" {
                    self.clear_stale_lock(&root);
                }
                (String::new(), String::new())
            }
        }
    }

    /// Undo a turn (the latest one that has not been undone, unless `turn` names one): the project goes back to how it
    /// was before the turn's first change.
    pub fn undo_turn(&self, project: &Path, turn: Option<&str>, dry_run: bool, paths: Option<Vec<String>>, session_id: &str) -> Restore {
        let rows: Vec<Turn> = self.turns(project, 30).into_iter().filter(|t| t.first.is_some()).collect();
        let chosen = match turn {
            Some(name) => rows.iter().find(|t| t.turn == name),
            None => rows.iter().find(|t| !t.undone),
        };
        let Some(chosen) = chosen else {
            return Restore::refused(match turn {
                None => "no turn to undo".to_owned(),
                Some(name) => format!("no checkpoints for turn {name}"),
            });
        };
        let scoped = paths.as_ref().is_some_and(|p| !p.is_empty());
        let opts = RestoreOptions { paths, dry_run, session_id: session_id.into(), undid_turn: if scoped { String::new() } else { chosen.turn.clone() }, edited_after: chosen.end.clone(), max_file_mb: None };
        let mut result = self.restore(project, chosen.first.as_deref().expect("filtered"), &opts);
        result.turn = Some(chosen.turn.clone());
        result.calls = Some(chosen.calls);
        result
    }

    /// Bound the history: past `2 * keep` commits, rebuild the chain from the newest `keep` (same trees and messages, the
    /// oldest becoming the root), then collect garbage. Returns whether it rewrote anything.
    pub fn prune(&self, project: &Path, keep: usize) -> bool {
        let root = real(project);
        if !self.exists(&root) {
            return false;
        }
        let lock = lock_for(&root);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let git = self.git(&root);
        let run = || -> Result<bool, String> {
            if !git.has_head() {
                return Ok(false);
            }
            let total: usize = git.out(&["rev-list", "--count", "HEAD"])?.trim().parse().unwrap_or(0);
            let mut rewrote = false;
            if total > keep * 2 {
                let commits: Vec<String> = git.out(&["rev-list", &format!("-n{keep}"), "--reverse", "HEAD"])?.split_whitespace().map(str::to_owned).collect();
                let mut parent: Option<String> = None;
                for commit in commits {
                    let tree = git.out(&["rev-parse", &format!("{commit}^{{tree}}")])?.trim().to_owned();
                    let message = git.out(&["log", "-1", "--format=%B", &commit])?;
                    let date = git.out(&["log", "-1", "--format=%cI", &commit])?.trim().to_owned();
                    let mut cmd = Command::new("git");
                    cmd.args(["commit-tree", &tree]);
                    if let Some(p) = &parent {
                        cmd.args(["-p", p]);
                    }
                    cmd.args(["-m", message.trim_end_matches('\n')]).env_clear().envs(git.env.iter().cloned()).env("GIT_AUTHOR_DATE", &date).env("GIT_COMMITTER_DATE", &date).current_dir(&root);
                    let out = run_with_timeout(cmd, None, TIMEOUT)?;
                    if out.status != Some(0) {
                        return Ok(false);
                    }
                    parent = Some(String::from_utf8_lossy(&out.stdout).trim().to_owned());
                }
                git.run(&["update-ref", "refs/heads/main", parent.as_deref().unwrap_or("HEAD")], None, TIMEOUT, true)?;
                git.run(&["reflog", "expire", "--expire=now", "--all"], None, TIMEOUT, false)?;
                rewrote = true;
            }
            let loose = git.out(&["count-objects"])?;
            if rewrote || loose.split_whitespace().next().and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| n > 3000) {
                let args: Vec<&str> = if rewrote { vec!["gc", "-q", "--prune=now"] } else { vec!["gc", "-q"] };
                git.run(&args, None, Duration::from_secs(120), false)?;
            }
            Ok(rewrote)
        };
        run().unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    pub paths: Option<Vec<String>>,
    pub dry_run: bool,
    pub session_id: String,
    pub undid_turn: String,
    pub edited_after: Option<String>,
    pub max_file_mb: Option<f64>,
}

fn lexical_join(project: &str, path: &str) -> String {
    let joined = if Path::new(path).is_absolute() { PathBuf::from(path) } else { Path::new(project).join(path) };
    let mut clean = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    clean.to_string_lossy().into_owned()
}

fn prune_empty_dirs(root: &str, mut dir: Option<&Path>) {
    while let Some(d) = dir {
        let s = d.to_string_lossy();
        if s == root || !inside(root, &s) || std::fs::remove_dir(d).is_err() {
            return;
        }
        dir = d.parent();
    }
}

fn on_path(name: &str) -> bool {
    std::env::var("PATH").is_ok_and(|p| p.split(':').any(|d| Path::new(d).join(name).is_file()))
}

fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Seconds since the epoch, for callers that stamp their own records.
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// A convenience for tests and tools: `{turn: Turn}` ordered by name.
pub fn by_turn(turns: &[Turn]) -> BTreeMap<&str, &Turn> {
    turns.iter().map(|t| (t.turn.as_str(), t)).collect()
}
