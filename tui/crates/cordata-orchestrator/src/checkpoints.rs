//! The seam to the checkpoint store (`backend/checkpoints.py`): snapshot the project before a step, restore it when
//! the step must be undone, diff against it to carry partial work in a handoff.
//!
//! [`GitCheckpoints`] is the production store (the shadow git repository of `cordata-checkpoints`, shared with the
//! Python backend); [`crate::testkit::CopyCheckpoints`] is a copy-based double for tests that do not need git.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub ok: bool,
    pub id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestoreReport {
    pub ok: bool,
    pub reason: String,
    pub files: Vec<String>,
}

pub trait Checkpoints: Send + Sync {
    /// Snapshot `project`. `turn` is the step id, `call` a label like `step 2`.
    fn take(&self, project: &Path, session_id: &str, turn: &str, call: &str) -> Snapshot;

    /// Put the project (or only `paths`, as project-relative pathspecs) back as it was at `id`.
    fn restore(&self, project: &Path, id: &str, paths: Option<&[String]>, session_id: &str) -> RestoreReport;

    /// `(patch, stat)` of what changed since `id`, limited to `paths` when given.
    fn diff_since(&self, project: &Path, id: &str, paths: Option<&[String]>) -> (String, String);

    /// The project-relative paths that differ now from checkpoint `id`; `None` when it cannot tell (never an empty
    /// list in that case). Stores that cannot answer keep the default.
    fn changes_since(&self, _project: &Path, _id: &str) -> Option<Vec<String>> {
        None
    }
}

/// The shadow-git store behind the executor's [`Checkpoints`] contract.
pub struct GitCheckpoints {
    store: cordata_checkpoints::Store,
}

impl GitCheckpoints {
    pub fn new(store: cordata_checkpoints::Store) -> Self {
        Self { store }
    }

    /// `$CORDATA_HOME` or `~/.cordata`, like the Python backend.
    pub fn from_env() -> Self {
        Self::new(cordata_checkpoints::Store::from_env())
    }
}

impl Checkpoints for GitCheckpoints {
    fn take(&self, project: &Path, session_id: &str, turn: &str, call: &str) -> Snapshot {
        let snap = self.store.take(
            project,
            &cordata_checkpoints::TakeOptions {
                kind: "pre".into(),
                tool: "orchestrator".into(),
                agent: "executor".into(),
                session_id: session_id.into(),
                turn: turn.into(),
                call: call.into(),
                ..Default::default()
            },
        );
        Snapshot { ok: snap.ok, id: snap.id, reason: snap.reason.unwrap_or_default() }
    }

    fn restore(&self, project: &Path, id: &str, paths: Option<&[String]>, session_id: &str) -> RestoreReport {
        let report = self.store.restore(project, id, &cordata_checkpoints::RestoreOptions { paths: paths.map(<[String]>::to_vec), session_id: session_id.into(), ..Default::default() });
        RestoreReport { ok: report.ok, reason: report.reason.unwrap_or_default(), files: report.files.into_iter().map(|f| f.path).collect() }
    }

    fn diff_since(&self, project: &Path, id: &str, paths: Option<&[String]>) -> (String, String) {
        self.store.diff_since(project, id, paths)
    }

    fn changes_since(&self, project: &Path, id: &str) -> Option<Vec<String>> {
        Some(self.store.changes_since(project, id, None)?.into_iter().map(|c| c.path).collect())
    }
}
