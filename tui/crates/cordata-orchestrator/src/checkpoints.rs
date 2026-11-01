//! The seam to the checkpoint store (`backend/checkpoints.py`): snapshot the project before a step, restore it when
//! the step must be undone, diff against it to carry partial work in a handoff.
//!
//! The production store (a shadow git repository) is not ported yet; the executor depends only on this trait, so it
//! can be driven by [`crate::testkit::CopyCheckpoints`] now and by the git-backed store later.

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
}
