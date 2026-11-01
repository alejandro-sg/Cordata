//! Test doubles for the executor: a scripted worker and a copy-based checkpoint store. `pub` so the integration
//! tests (and later the state machine's) can use them; nothing here is used in production.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::checkpoints::{Checkpoints, RestoreReport, Snapshot};
use crate::dispatch::Candidate;
use crate::executor::Runnable;
use crate::worker::{EventSink, TaskPacket, Worker, WorkerResult};

type Handler = Box<dyn Fn(&TaskPacket, usize) -> WorkerResult + Send + Sync>;

/// A worker whose behaviour is a closure of (task, call number): it may write files under `task.cwd`.
pub struct ScriptedWorker {
    id: String,
    handler: Handler,
    pub calls: Mutex<Vec<TaskPacket>>,
}

impl ScriptedWorker {
    pub fn new(id: &str, handler: impl Fn(&TaskPacket, usize) -> WorkerResult + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { id: id.into(), handler: Box::new(handler), calls: Mutex::new(Vec::new()) })
    }
}

impl Worker for ScriptedWorker {
    fn worker_id(&self) -> &str {
        &self.id
    }
    fn run<'a>(&'a self, task: TaskPacket, _events: EventSink) -> BoxFuture<'a, WorkerResult> {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(task.clone());
            calls.len()
        };
        Box::pin(async move { (self.handler)(&task, n) })
    }
    fn cancel<'a>(&'a self, _task_id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
}

pub fn runnable(worker: &Arc<ScriptedWorker>, model: &str, family: &str, vendor: &str) -> Runnable {
    Runnable {
        candidate: Candidate { worker_id: worker.worker_id().into(), family: family.into(), model: Some(model.into()), effort: None, model_family: vendor.into() },
        worker: worker.clone(),
    }
}

/// Snapshots by copying the whole project into a side directory; restores by putting the copy back (deleting files
/// that did not exist). Honest about being a double: no history, no dedup, small trees only.
pub struct CopyCheckpoints {
    store: PathBuf,
    next: Mutex<u64>,
}

impl CopyCheckpoints {
    pub fn new(store: &Path) -> Self {
        Self { store: store.to_owned(), next: Mutex::new(0) }
    }
}

fn read_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if let (Ok(rel), Ok(bytes)) = (path.strip_prefix(root), std::fs::read(&path)) {
                out.insert(rel.to_string_lossy().into_owned(), bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn in_paths(path: &str, paths: Option<&[String]>) -> bool {
    paths.is_none_or(|ps| ps.iter().any(|p| path == p || path.starts_with(&format!("{p}/"))))
}

impl Checkpoints for CopyCheckpoints {
    fn take(&self, project: &Path, _session_id: &str, _turn: &str, _call: &str) -> Snapshot {
        let id = {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            format!("cp-{n}")
        };
        let dest = self.store.join(&id);
        for (rel, bytes) in read_tree(project) {
            let target = dest.join(&rel);
            if std::fs::create_dir_all(target.parent().expect("has parent")).and_then(|()| std::fs::write(&target, bytes)).is_err() {
                return Snapshot { ok: false, id: None, reason: "copy failed".into() };
            }
        }
        std::fs::create_dir_all(&dest).ok();
        Snapshot { ok: true, id: Some(id), reason: String::new() }
    }

    fn restore(&self, project: &Path, id: &str, paths: Option<&[String]>, _session_id: &str) -> RestoreReport {
        let saved = read_tree(&self.store.join(id));
        let now = read_tree(project);
        let mut files = Vec::new();
        for (rel, bytes) in &saved {
            if in_paths(rel, paths) && now.get(rel) != Some(bytes) {
                let target = project.join(rel);
                std::fs::create_dir_all(target.parent().expect("has parent")).ok();
                std::fs::write(&target, bytes).ok();
                files.push(rel.clone());
            }
        }
        for rel in now.keys() {
            if in_paths(rel, paths) && !saved.contains_key(rel) {
                std::fs::remove_file(project.join(rel)).ok();
                files.push(rel.clone());
            }
        }
        RestoreReport { ok: true, reason: String::new(), files }
    }

    fn diff_since(&self, project: &Path, id: &str, paths: Option<&[String]>) -> (String, String) {
        let saved = read_tree(&self.store.join(id));
        let now = read_tree(project);
        let (mut patch, mut stat) = (String::new(), Vec::new());
        for rel in saved.keys().chain(now.keys()).collect::<std::collections::BTreeSet<_>>() {
            if !in_paths(rel, paths) || saved.get(rel) == now.get(rel) {
                continue;
            }
            let text = |m: &BTreeMap<String, Vec<u8>>| m.get(rel).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
            patch.push_str(&format!("--- a/{rel}\n+++ b/{rel}\n"));
            patch.extend(text(&saved).lines().map(|l| format!("-{l}\n")));
            patch.extend(text(&now).lines().map(|l| format!("+{l}\n")));
            stat.push(format!(" {rel}"));
        }
        (patch, stat.join("\n"))
    }
}
