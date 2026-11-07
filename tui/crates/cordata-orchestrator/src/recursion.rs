//! Recursive Agent Harnessing (Propose–Probe–Compose waves).
//!
//! Inspired by the Harness² dual-loop recursion model (`harness2/core/recursion.py`),
//! complex tasks are tackled by running orthogonal candidate probes in isolated
//! APFS clonefile sandboxes, observing their contrastive execution evidence, and
//! composing the winning changes into a single verified solution.
//!
//! Dual axes of candidate probes:
//! - **$INV$ (Invariant)**: Strengthens property checks, test suites, and safety invariants.
//! - **$DOM$ (Domain)**: Applies specialized domain heuristics, helper scripts, and algorithms.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::synthesizer::{copy_file, digest, Sandbox, SKIP};

/// Kind of candidate probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbeKind {
    /// Invariant: focuses on verification, test hardening, defensive assertions.
    Invariant,
    /// Domain: focuses on domain-specific logic, workflow scripts, heuristics.
    Domain,
}

impl std::fmt::Display for ProbeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invariant => write!(f, "INV"),
            Self::Domain => write!(f, "DOM"),
        }
    }
}

/// The outcome of running one probe in its sandbox.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub kind: ProbeKind,
    pub label: String,
    pub sandbox_root: PathBuf,
    pub changed_files: Vec<String>,
    pub success: bool,
    pub notes: String,
}

/// The result of composing multiple candidate probes into the target tree.
#[derive(Debug, Clone, Default)]
pub struct ComposeOutcome {
    /// Files adopted from candidate probes into the target tree.
    pub adopted_files: Vec<String>,
    /// Files with overlapping edits that required resolution.
    pub conflicts: Vec<String>,
    /// Summary of which probe contributed which file.
    pub provenance: HashMap<String, ProbeKind>,
}

/// Engine for orchestrating Propose–Probe–Compose recursion.
pub struct ProposeProbeCompose {
    project_root: PathBuf,
    sandbox_base: PathBuf,
}

impl ProposeProbeCompose {
    pub fn new(project_root: PathBuf, sandbox_base: Option<PathBuf>) -> Self {
        let base = sandbox_base.unwrap_or_else(std::env::temp_dir);
        Self {
            project_root,
            sandbox_base: base,
        }
    }

    /// Creates an isolated APFS clonefile sandbox for a candidate probe.
    pub fn spawn_probe_sandbox(&self, kind: ProbeKind, label: &str) -> std::io::Result<Sandbox> {
        let run_id = format!("cordata-probe-{}-{}-{}", kind, label, std::process::id());
        let root = self.sandbox_base.join(run_id);
        if root.exists() {
            let _ = std::fs::remove_dir_all(&root);
        }
        std::fs::create_dir_all(&root)?;

        let mut cloned = 0;
        let mut copied = 0;
        let mut base = HashMap::new();

        self.clone_recursive(&self.project_root, &root, &mut cloned, &mut copied, &mut base)?;

        Ok(Sandbox {
            root,
            source: self.project_root.clone(),
            cloned,
            copied,
            linked: Vec::new(),
            base,
        })
    }

    fn clone_recursive(
        &self,
        src: &Path,
        dst: &Path,
        cloned: &mut usize,
        copied: &mut usize,
        base: &mut HashMap<String, Option<String>>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(src)?.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if SKIP.iter().any(|s| *s == name_str) {
                continue;
            }
            let src_path = entry.path();
            let dst_path = dst.join(&name);
            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                std::fs::create_dir_all(&dst_path)?;
                self.clone_recursive(&src_path, &dst_path, cloned, copied, base)?;
            } else if file_type.is_file() {
                let rel = src_path
                    .strip_prefix(&self.project_root)
                    .unwrap_or(&src_path)
                    .to_string_lossy()
                    .into_owned();
                base.insert(rel, digest(&src_path));
                if copy_file(&src_path, &dst_path)? {
                    *cloned += 1;
                } else {
                    *copied += 1;
                }
            }
        }
        Ok(())
    }

    /// Harvests the modifications made inside a candidate probe sandbox.
    pub fn harvest_probe(
        &self,
        kind: ProbeKind,
        label: &str,
        sandbox: &Sandbox,
        success: bool,
        notes: String,
    ) -> std::io::Result<ProbeResult> {
        let mut changed = Vec::new();
        self.collect_changes(&sandbox.root, &sandbox.base, &mut changed)?;

        Ok(ProbeResult {
            kind,
            label: label.to_string(),
            sandbox_root: sandbox.root.clone(),
            changed_files: changed,
            success,
            notes,
        })
    }

    fn collect_changes(
        &self,
        current_dir: &Path,
        base_digests: &HashMap<String, Option<String>>,
        changed: &mut Vec<String>,
    ) -> std::io::Result<()> {
        let Ok(entries) = std::fs::read_dir(current_dir) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if SKIP.iter().any(|s| *s == name_str) {
                continue;
            }
            if path.is_dir() {
                self.collect_changes(&path, base_digests, changed)?;
            } else if path.is_file() {
                let current_digest = digest(&path);
                let rel = path
                    .strip_prefix(current_dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                let initial = base_digests.get(&rel).cloned().flatten();
                if current_digest != initial {
                    changed.push(rel);
                }
            }
        }
        Ok(())
    }

    /// Composes changes from candidate probes into the target working tree.
    pub fn compose(&self, probes: &[ProbeResult], target: &Path) -> std::io::Result<ComposeOutcome> {
        let mut outcome = ComposeOutcome::default();
        let mut file_sources: HashMap<String, Vec<&ProbeResult>> = HashMap::new();

        for probe in probes {
            if !probe.success {
                continue;
            }
            for rel in &probe.changed_files {
                file_sources.entry(rel.clone()).or_default().push(probe);
            }
        }

        for (rel, contributing) in file_sources {
            if contributing.len() == 1 {
                let winner = contributing[0];
                let src = winner.sandbox_root.join(&rel);
                let dst = target.join(&rel);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if src.is_file() {
                    std::fs::copy(&src, &dst)?;
                    outcome.adopted_files.push(rel.clone());
                    outcome.provenance.insert(rel, winner.kind);
                }
            } else {
                // If multiple probes modified the same file, pick Invariant if identical or flag conflict
                let first_src = contributing[0].sandbox_root.join(&rel);
                let all_identical = contributing.iter().all(|p| {
                    let other_src = p.sandbox_root.join(&rel);
                    digest(&first_src) == digest(&other_src)
                });

                if all_identical {
                    let dst = target.join(&rel);
                    if let Some(parent) = dst.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(&first_src, &dst)?;
                    outcome.adopted_files.push(rel.clone());
                    outcome.provenance.insert(rel, contributing[0].kind);
                } else {
                    outcome.conflicts.push(rel);
                }
            }
        }

        outcome.adopted_files.sort();
        outcome.conflicts.sort();
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn propose_probe_compose_merges_disjoint_changes_cleanly() {
        let project_dir = tempdir().unwrap();
        let sandbox_tmp = tempdir().unwrap();
        let target_dir = tempdir().unwrap();

        std::fs::write(project_dir.path().join("base.txt"), "original content").unwrap();
        std::fs::write(project_dir.path().join("test_suite.rs"), "// tests").unwrap();

        let ppc = ProposeProbeCompose::new(
            project_dir.path().to_path_buf(),
            Some(sandbox_tmp.path().to_path_buf()),
        );

        // 1. Invariant probe strengthens tests
        let inv_sandbox = ppc.spawn_probe_sandbox(ProbeKind::Invariant, "inv1").unwrap();
        std::fs::write(
            inv_sandbox.root.join("test_suite.rs"),
            "// tests + added assertions",
        )
        .unwrap();
        let inv_probe = ppc
            .harvest_probe(
                ProbeKind::Invariant,
                "inv1",
                &inv_sandbox,
                true,
                "added property test".into(),
            )
            .unwrap();

        // 2. Domain probe adds domain feature
        let dom_sandbox = ppc.spawn_probe_sandbox(ProbeKind::Domain, "dom1").unwrap();
        std::fs::write(
            dom_sandbox.root.join("feature.rs"),
            "pub fn execute_domain() -> bool { true }",
        )
        .unwrap();
        let dom_probe = ppc
            .harvest_probe(
                ProbeKind::Domain,
                "dom1",
                &dom_sandbox,
                true,
                "implemented feature".into(),
            )
            .unwrap();

        // 3. Compose both into target tree
        let outcome = ppc
            .compose(&[inv_probe, dom_probe], target_dir.path())
            .unwrap();

        assert_eq!(outcome.conflicts.len(), 0);
        assert_eq!(outcome.adopted_files, vec!["feature.rs", "test_suite.rs"]);
        assert_eq!(outcome.provenance.get("feature.rs"), Some(&ProbeKind::Domain));
        assert_eq!(
            outcome.provenance.get("test_suite.rs"),
            Some(&ProbeKind::Invariant)
        );

        assert!(target_dir.path().join("feature.rs").is_file());
        assert!(target_dir.path().join("test_suite.rs").is_file());
        assert_eq!(
            std::fs::read_to_string(target_dir.path().join("test_suite.rs")).unwrap(),
            "// tests + added assertions"
        );
    }
}
