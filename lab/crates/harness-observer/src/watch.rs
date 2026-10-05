//! Watching a workspace: saves in, [`Snippet`]s out.
//!
//! `notify` reports what the OS file-event API sees — often several events per save, or a rename
//! when an editor writes through a temp file. This module turns that into "this file settled":
//! each path is debounced (no event for `quiet`, or `max_wait` since the first), then read, parsed
//! and diffed. The diff — by the hash of each symbol's own text — is the real filter, so a stray
//! event for a file that did not change reports nothing, and the event kinds are not trusted to
//! be precise (they are not, across platforms).
//!
//! What it will not look at, whatever the OS says: build output and dependency directories, `.git`,
//! anything not in a language it can parse, files over `max_file_bytes`, and **secrets files**
//! (`.env`, `.env.*`; templates like `.env.example` are code-adjacent and also skipped — nothing
//! in them is a function). Nothing is read until a save settles, and nothing is sent that is not a
//! symbol's own text.
//!
//! The first time a file is seen it is compared with `git show HEAD:<path>` (when the workspace is
//! a repository and the file is tracked), so saving an existing file reports the functions you
//! changed since the last commit — not all of them. After that, each save is compared with the one
//! before. A file git does not know is entirely new: all its symbols are `Added`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::extract::{self, Change, Key, Lang, Symbol};

const IGNORED_DIRS: &[&str] = &[
    ".git", "target", "node_modules", ".venv", "venv", "__pycache__", ".next", "dist", "build", ".sierralab", ".idea", ".vscode", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox", ".cache",
];

#[derive(Debug, thiserror::Error)]
pub enum ObserverError {
    #[error("cannot watch {path}: {source}")]
    Watch { path: PathBuf, source: notify::Error },
    #[error("{0} is not a directory")]
    NotADirectory(PathBuf),
}

#[derive(Debug, Clone)]
pub struct ObserverConfig {
    pub root: PathBuf,
    /// A file is processed once it has been quiet this long.
    pub quiet: Duration,
    /// …or this long after its first event, however busy it stays.
    pub max_wait: Duration,
    pub max_file_bytes: u64,
    pub use_git_baseline: bool,
}

impl ObserverConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), quiet: Duration::from_millis(150), max_wait: Duration::from_millis(1500), max_file_bytes: 1024 * 1024, use_git_baseline: true }
    }
}

/// One changed function, class or type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub workspace: PathBuf,
    /// Relative to `workspace`, `/`-separated.
    pub path: String,
    pub language: &'static str,
    pub node_kind: String,
    pub name: String,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
    pub content_hash: String,
    pub change: Change,
    pub saved_ms: i64,
}

#[derive(Debug, Default)]
struct Counters {
    saves: AtomicU64,
    snippets: AtomicU64,
    skipped: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Settled saves that were parsed.
    pub saves: u64,
    pub snippets: u64,
    /// Paths that settled but were not parsed (too large, unreadable, unsupported).
    pub skipped: u64,
}

pub struct Observer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _watcher: RecommendedWatcher,
    counters: Arc<Counters>,
    pub root: PathBuf,
}

/// Should this path even be considered? (Pure; the first gate, before any I/O.)
pub fn wanted(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else { return false };
    if relative.components().any(|c| IGNORED_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref())) {
        return false;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if name == ".env" || name.starts_with(".env.") {
        return false;
    }
    // Editors' scratch files.
    if name.starts_with(".#") || name.ends_with('~') || name.ends_with(".swp") || name.ends_with(".tmp") {
        return false;
    }
    Lang::from_path(path).is_some()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `git show HEAD:<path>`, parsed. `None` when this is not a repository, the file is not tracked,
/// or git is not installed — "no baseline", not an error.
fn git_baseline(root: &Path, relative: &str, lang: Lang) -> Option<Vec<(Key, Symbol)>> {
    let output = Command::new("git").arg("-C").arg(root).arg("show").arg(format!("HEAD:{relative}")).output().ok()?;
    if !output.status.success() || output.stdout.len() > 4 * 1024 * 1024 {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(extract::keyed(extract::symbols(lang, &text)))
}

struct State {
    config: ObserverConfig,
    cache: HashMap<PathBuf, Vec<(Key, Symbol)>>,
}

impl State {
    fn process(&mut self, path: &Path, counters: &Counters) -> Vec<Snippet> {
        let Some(lang) = Lang::from_path(path) else { return Vec::new() };
        let Ok(meta) = std::fs::metadata(path) else { return Vec::new() }; // deleted since the event
        if !meta.is_file() || meta.len() > self.config.max_file_bytes {
            counters.skipped.fetch_add(1, Ordering::Relaxed);
            return Vec::new();
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            counters.skipped.fetch_add(1, Ordering::Relaxed);
            return Vec::new();
        };
        counters.saves.fetch_add(1, Ordering::Relaxed);
        let relative = path.strip_prefix(&self.config.root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        let fresh = extract::keyed(extract::symbols(lang, &source));
        let previous = match self.cache.get(path) {
            Some(known) => Some(known.clone()),
            None if self.config.use_git_baseline => git_baseline(&self.config.root, &relative, lang),
            None => None,
        };
        let diff = extract::changed(previous.as_deref(), &fresh);
        self.cache.insert(path.to_owned(), fresh);
        let saved_ms = now_ms();
        diff.into_iter()
            .map(|(change, symbol)| Snippet {
                workspace: self.config.root.clone(),
                path: relative.clone(),
                language: lang.name(),
                node_kind: symbol.kind,
                name: symbol.name,
                start_line: symbol.start_line,
                end_line: symbol.end_line,
                content_hash: symbol.hash,
                text: symbol.own_text,
                change,
                saved_ms,
            })
            .collect()
    }
}

impl Observer {
    /// Start watching. `sink` is called on the observer's thread with each settled save's snippets
    /// (never an empty batch); it must not block — the GUI passes a closure that `try_send`s.
    pub fn start(config: ObserverConfig, sink: impl Fn(Vec<Snippet>) + Send + 'static) -> Result<Self, ObserverError> {
        let root = std::fs::canonicalize(&config.root).map_err(|_| ObserverError::NotADirectory(config.root.clone()))?;
        if !root.is_dir() {
            return Err(ObserverError::NotADirectory(root));
        }
        let config = ObserverConfig { root: root.clone(), ..config };
        let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .map_err(|source| ObserverError::Watch { path: root.clone(), source })?;
        watcher.watch(&root, RecursiveMode::Recursive).map_err(|source| ObserverError::Watch { path: root.clone(), source })?;

        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let thread = {
            let (stop, counters, config) = (stop.clone(), counters.clone(), config.clone());
            std::thread::Builder::new()
                .name("harness-observer".into())
                .spawn(move || run(config, rx, sink, stop, counters))
                .expect("spawn the observer thread")
        };
        Ok(Self { stop, thread: Some(thread), _watcher: watcher, counters, root })
    }

    pub fn stats(&self) -> Stats {
        Stats { saves: self.counters.saves.load(Ordering::Relaxed), snippets: self.counters.snippets.load(Ordering::Relaxed), skipped: self.counters.skipped.load(Ordering::Relaxed) }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn relevant(kind: &EventKind) -> bool {
    // The hash diff decides whether anything changed; here we only drop events that cannot be a save.
    !matches!(kind, EventKind::Access(_) | EventKind::Remove(_))
}

fn run(config: ObserverConfig, events: mpsc::Receiver<notify::Result<notify::Event>>, sink: impl Fn(Vec<Snippet>), stop: Arc<AtomicBool>, counters: Arc<Counters>) {
    let mut state = State { config: config.clone(), cache: HashMap::new() };
    // path -> (first event, latest event)
    let mut pending: HashMap<PathBuf, (Instant, Instant)> = HashMap::new();
    let tick = Duration::from_millis(25);
    while !stop.load(Ordering::Relaxed) {
        match events.recv_timeout(tick) {
            Ok(Ok(event)) if relevant(&event.kind) => {
                let now = Instant::now();
                for path in event.paths {
                    if wanted(&config.root, &path) {
                        pending.entry(path).and_modify(|(_, last)| *last = now).or_insert((now, now));
                    }
                }
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let now = Instant::now();
        let due: Vec<PathBuf> = pending
            .iter()
            .filter(|(_, (first, last))| now.duration_since(*last) >= config.quiet || now.duration_since(*first) >= config.max_wait)
            .map(|(path, _)| path.clone())
            .collect();
        for path in due {
            pending.remove(&path);
            let snippets = state.process(&path, &counters);
            if !snippets.is_empty() {
                counters.snippets.fetch_add(snippets.len() as u64, Ordering::Relaxed);
                sink(snippets);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_and_is_not_watched() {
        let root = Path::new("/w");
        for (path, expected) in [
            ("/w/src/lib.rs", true),
            ("/w/a.py", true),
            ("/w/web/app.tsx", true),
            ("/w/target/debug/x.rs", false),
            ("/w/node_modules/p/index.js", false),
            ("/w/.git/hooks/x.py", false),
            ("/w/.env", false),
            ("/w/.env.local", false),
            ("/w/src/.env.py", false),
            ("/w/README.md", false),
            ("/w/src/lib.rs~", false),
            ("/w/src/.#lib.rs", false),
            ("/w/.venv/lib/x.py", false),
            ("/elsewhere/a.rs", false),
        ] {
            assert_eq!(wanted(root, Path::new(path)), expected, "{path}");
        }
    }
}
