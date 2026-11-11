//! Recursive Self-Improvement (RSI) Engine: Iron Gates, Immutable Evaluator, and APFS Sandboxed Evolution.
//!
//! **Invariants:**
//! 1. Immutable Evaluator: Mutating agents must NEVER modify evaluation tests, benchmarks, scripts, or AGENTS.md.
//!    Any detected alteration triggers an immediate `TamperingViolation` and aborts evolution.
//! 2. Zero-Loss CoW Sandboxing: Candidate mutations run exclusively in APFS clonefile sandboxes (`synthesizer::create`).
//! 3. The 4 Iron Gates:
//!    - Gate 1: Type & Compilation Check (`cargo check --release`).
//!    - Gate 2: Hermetic Test Suite (`cargo test`).
//!    - Gate 3: Anti-Tamper Evaluator Verification (`ImmutableGuard`).
//!    - Gate 4: Deterministic Fitness Delta Verification ($\Delta \text{Fitness} \ge \text{threshold}$).
//! 4. Atomic Harvest & Rollback: Only candidates that pass all 4 gates are harvested into the primary tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::synthesizer;

/// Subsystems capable of undergoing recursive self-improvement.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EvolutionTarget {
    Repomap,
    Economizer,
    Guard,
    Custom(String),
}

impl std::fmt::Display for EvolutionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Repomap => write!(f, "repomap"),
            Self::Economizer => write!(f, "economizer"),
            Self::Guard => write!(f, "guard"),
            Self::Custom(name) => write!(f, "custom:{name}"),
        }
    }
}

impl std::str::FromStr for EvolutionTarget {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "repomap" => Self::Repomap,
            "economizer" => Self::Economizer,
            "guard" => Self::Guard,
            other => Self::Custom(other.to_string()),
        })
    }
}

/// Quantitative fitness score for an evolved subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitnessScore {
    /// Primary optimization metric (e.g., MRR for repomap, compression ratio for economizer, throughput for guard).
    pub primary: f64,
    /// Secondary performance metric (e.g. median query/step latency in milliseconds).
    pub latency_ms: f64,
    /// Descriptive details / breakdown.
    pub details: String,
}

/// A tamper detection violation when an evolving candidate modifies evaluation harnesses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TamperingViolation {
    Modified { path: String, expected_sha1: String, observed_sha1: String },
    Deleted { path: String },
    Planted { path: String },
}

impl std::fmt::Display for TamperingViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Modified { path, expected_sha1, observed_sha1 } => {
                write!(f, "Evaluation file {path} was tampered with (expected {expected_sha1}, got {observed_sha1})")
            }
            Self::Deleted { path } => write!(f, "Protected evaluation file {path} was deleted"),
            Self::Planted { path } => write!(f, "Unauthorized file {path} was planted in protected evaluation paths"),
        }
    }
}

/// Physical guarantee against Goodhart's law: snapshots and verifies protected evaluation paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImmutableGuard {
    pub protected_hashes: HashMap<String, String>,
}

impl ImmutableGuard {
    /// Hashes a single file.
    pub fn hash_file(path: &Path) -> Option<String> {
        let bytes = std::fs::read(path).ok()?;
        Some(Sha1::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Snapshots all evaluation and governance files in `root`.
    pub fn snapshot(root: &Path) -> Self {
        let mut hashes = HashMap::new();

        // 1. Core governance and verification files
        for fixed in ["AGENTS.md", "scripts/verify.sh", "scripts/bench.sh", "Cargo.lock"] {
            let p = root.join(fixed);
            if let Some(h) = Self::hash_file(&p) {
                hashes.insert(fixed.to_string(), h);
            }
        }

        // 2. All files under tests/ directories
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else { continue };
                let p = entry.path();
                if kind.is_dir() {
                    let name = entry.file_name();
                    let s = name.to_string_lossy();
                    if s == "target" || s == ".git" || s == ".cordata" {
                        continue;
                    }
                    stack.push(p);
                } else if kind.is_file() {
                    let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned();
                    if (rel.contains("/tests/") || rel.starts_with("tests/") || rel.ends_with("_test.rs") || rel.ends_with("_tests.rs"))
                        && let Some(h) = Self::hash_file(&p)
                    {
                        hashes.insert(rel, h);
                    }
                }
            }
        }

        Self { protected_hashes: hashes }
    }

    /// Verifies that no evaluation files were modified, removed, or unauthorized tests planted in `candidate_root`.
    pub fn verify(&self, candidate_root: &Path) -> Result<(), TamperingViolation> {
        // 1. Verify all known protected files exist and match expected hashes
        for (rel, expected_hash) in &self.protected_hashes {
            let p = candidate_root.join(rel);
            if !p.exists() {
                return Err(TamperingViolation::Deleted { path: rel.clone() });
            }
            let observed = Self::hash_file(&p).unwrap_or_default();
            if observed != *expected_hash {
                return Err(TamperingViolation::Modified {
                    path: rel.clone(),
                    expected_sha1: expected_hash.clone(),
                    observed_sha1: observed,
                });
            }
        }

        // 2. Anti-gaming check: ensure no unauthorized evaluation/test files were planted in candidate_root
        let candidate_guard = Self::snapshot(candidate_root);
        for candidate_rel in candidate_guard.protected_hashes.keys() {
            if !self.protected_hashes.contains_key(candidate_rel) {
                return Err(TamperingViolation::Planted { path: candidate_rel.clone() });
            }
        }

        Ok(())
    }
}

/// Result of evaluating an Iron Gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    pub gate_number: u8,
    pub name: String,
    pub passed: bool,
    pub output: String,
}

/// Overall report of an evolution cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionReport {
    pub target: EvolutionTarget,
    pub promoted: bool,
    pub baseline_fitness: FitnessScore,
    pub candidate_fitness: Option<FitnessScore>,
    pub delta_fitness: f64,
    pub gates: Vec<GateResult>,
    pub changed_files: Vec<String>,
    pub duration_ms: u64,
}

/// Configuration for an evolution execution.
#[derive(Debug, Clone)]
pub struct EvolutionConfig {
    pub target: EvolutionTarget,
    pub threshold: f64,
    pub dry_run: bool,
    pub project_root: PathBuf,
    /// Shell command that proposes a change by editing files in its working directory, which is the CoW sandbox
    /// (never the project). `CORDATA_EVOLVE_TARGET` and `CORDATA_EVOLVE_BASELINE` are set. Without one, a cycle has
    /// nothing to judge and is refused.
    pub mutate_cmd: Option<String>,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            target: EvolutionTarget::Repomap,
            threshold: 0.005,
            dry_run: false,
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            mutate_cmd: None,
        }
    }
}

/// Runs target-specific benchmark evaluation and returns parsed `FitnessScore`.
pub fn evaluate_fitness(target: &EvolutionTarget, root: &Path) -> Result<FitnessScore, String> {
    match target {
        EvolutionTarget::Repomap => {
            let start = Instant::now();
            let mut cmd = Command::new("cargo");
            cmd.args([
                "test",
                "--manifest-path",
                "tui/Cargo.toml",
                "--release",
                "-p",
                "cordata-repomap",
                "bench_ranking_words_versus_meaning",
                "--",
                "--ignored",
                "--nocapture",
            ]);
            cmd.current_dir(root);

            let output = cmd.output().map_err(|e| format!("failed to run cargo test for repomap bench: {e}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = format!("{stdout}\n{stderr}");

            // Look for: "BENCH ranking graph+BM25       code words: MRR (\d+\.\d+) ranks ... | paraphrased: MRR (\d+\.\d+) ... | (\d+\.\d+)ms/query"
            let mut code_mrr = None;
            let mut para_mrr = None;
            let mut latency = None;

            for line in combined.lines() {
                if line.contains("BENCH ranking graph+BM25") {
                    if let Some(pos) = line.find("code words: MRR ") {
                        let rem = &line[pos + 16..];
                        if let Some(val_str) = rem.split_whitespace().next() {
                            code_mrr = val_str.parse::<f64>().ok();
                        }
                    }
                    if let Some(pos) = line.find("paraphrased: MRR ") {
                        let rem = &line[pos + 17..];
                        if let Some(val_str) = rem.split_whitespace().next() {
                            para_mrr = val_str.parse::<f64>().ok();
                        }
                    }
                    if let Some(pos) = line.rfind('|') {
                        let rem = line[pos + 1..].trim();
                        if let Some(lat_str) = rem.strip_suffix("ms/query") {
                            latency = lat_str.trim().parse::<f64>().ok();
                        }
                    }
                }
            }

            let mrr = code_mrr.ok_or_else(|| format!("could not extract code words MRR from benchmark output:\n{combined}"))?;
            let para = para_mrr.unwrap_or(0.0);
            let lat = latency.unwrap_or_else(|| start.elapsed().as_millis() as f64);

            // Composite fitness: 70% direct code query MRR + 30% paraphrased query MRR
            let composite = 0.70 * mrr + 0.30 * para;

            Ok(FitnessScore {
                primary: composite,
                latency_ms: lat,
                details: format!("code_mrr={mrr:.4}, paraphrased_mrr={para:.4}, composite={composite:.4}, latency={lat:.1}ms"),
            })
        }
        EvolutionTarget::Economizer => {
            let start = Instant::now();
            let mut cmd = Command::new("cargo");
            cmd.args([
                "test",
                "--manifest-path",
                "tui/Cargo.toml",
                "--release",
                "-p",
                "cordata-economizer",
            ]);
            cmd.current_dir(root);

            let output = cmd.output().map_err(|e| format!("failed to run economizer tests: {e}"))?;
            let lat = start.elapsed().as_millis() as f64;
            if !output.status.success() {
                return Err(format!("economizer tests failed: {}", String::from_utf8_lossy(&output.stderr)));
            }
            Ok(FitnessScore {
                primary: 1.0,
                latency_ms: lat,
                details: format!("economizer hermetic suite passed in {lat:.1}ms"),
            })
        }
        EvolutionTarget::Guard => {
            let start = Instant::now();
            let mut cmd = Command::new("cargo");
            cmd.args([
                "test",
                "--manifest-path",
                "tui/Cargo.toml",
                "--release",
                "-p",
                "cordata-guard",
                "bench_",
                "--",
                "--ignored",
                "--nocapture",
            ]);
            cmd.current_dir(root);

            let output = cmd.output().map_err(|e| format!("failed to run guard bench: {e}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let lat = start.elapsed().as_millis() as f64;
            let mut throughput = 0.0;
            for line in stdout.lines() {
                if line.contains("BENCH")
                    && let Some(pos) = line.find("MB/s")
                    && let Some(num_str) = line[..pos].split_whitespace().last()
                {
                    throughput = num_str.parse::<f64>().unwrap_or(0.0);
                }
            }
            Ok(FitnessScore {
                primary: throughput.max(1.0),
                latency_ms: lat,
                details: format!("guard bench throughput: {throughput:.1} MB/s"),
            })
        }
        EvolutionTarget::Custom(name) => {
            let start = Instant::now();
            let mut cmd = Command::new("cargo");
            cmd.args(["test", "--manifest-path", "tui/Cargo.toml", "-p", name]);
            cmd.current_dir(root);
            let output = cmd.output().map_err(|e| format!("failed to run tests for {name}: {e}"))?;
            let lat = start.elapsed().as_millis() as f64;
            if !output.status.success() {
                return Err(format!("custom target {name} tests failed"));
            }
            Ok(FitnessScore {
                primary: 1.0,
                latency_ms: lat,
                details: format!("target {name} tests passed in {lat:.1}ms"),
            })
        }
    }
}

/// Evaluates a candidate sandbox across the 4 Iron Gates in strict zero-trust order:
/// Gate 1: Immutable Evaluator Anti-Tamper Check (physically prevents running modified/tampered tests)
/// Gate 2: Type & Compilation Check
/// Gate 3: Hermetic Unit Tests
/// Gate 4: Deterministic Fitness Delta Verification
pub fn evaluate_candidate(
    sandbox_root: &Path,
    target: &EvolutionTarget,
    baseline: &FitnessScore,
    guard: &ImmutableGuard,
    threshold: f64,
) -> (bool, Option<FitnessScore>, Vec<GateResult>) {
    let mut gates = Vec::new();

    // Gate 1: Immutable Evaluator Anti-Tamper Check (MUST run before any test or build execution)
    let g1_res = guard.verify(sandbox_root);
    let g1_pass = g1_res.is_ok();
    let g1_details = match g1_res {
        Ok(()) => format!("Verified {} evaluation files intact and zero planted tests", guard.protected_hashes.len()),
        Err(err) => format!("TAMPERING DETECTED: {err}"),
    };
    gates.push(GateResult {
        gate_number: 1,
        name: "Immutable Evaluator Anti-Tamper".into(),
        passed: g1_pass,
        output: g1_details,
    });
    if !g1_pass {
        return (false, None, gates);
    }

    // Gate 2: Type & Compilation Check
    let mut check_cmd = Command::new("cargo");
    check_cmd.args(["check", "--manifest-path", "tui/Cargo.toml", "--release"]);
    check_cmd.current_dir(sandbox_root);
    let check_out = check_cmd.output();
    let g2_pass = check_out.as_ref().is_ok_and(|o| o.status.success());
    let g2_details = match &check_out {
        Ok(o) if o.status.success() => "Compilation successful with zero errors".to_string(),
        Ok(o) => format!("Compilation failed: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("Failed to spawn cargo check: {e}"),
    };
    gates.push(GateResult {
        gate_number: 2,
        name: "Type & Release Compilation".into(),
        passed: g2_pass,
        output: g2_details,
    });
    if !g2_pass {
        return (false, None, gates);
    }

    // Gate 3: Hermetic Unit Tests
    let crate_name = match target {
        EvolutionTarget::Repomap => "cordata-repomap",
        EvolutionTarget::Economizer => "cordata-economizer",
        EvolutionTarget::Guard => "cordata-guard",
        EvolutionTarget::Custom(c) => c.as_str(),
    };
    let mut test_cmd = Command::new("cargo");
    test_cmd.args(["test", "--manifest-path", "tui/Cargo.toml", "-p", crate_name]);
    test_cmd.current_dir(sandbox_root);
    let test_out = test_cmd.output();
    let g3_pass = test_out.as_ref().is_ok_and(|o| o.status.success());
    let g3_details = match &test_out {
        Ok(o) if o.status.success() => "100% of crate unit tests passed hermetically".to_string(),
        Ok(o) => format!("Test failures detected: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("Failed to run cargo test: {e}"),
    };
    gates.push(GateResult {
        gate_number: 3,
        name: "Hermetic Unit Tests".into(),
        passed: g3_pass,
        output: g3_details,
    });
    if !g3_pass {
        return (false, None, gates);
    }

    // Gate 4: Deterministic Fitness Delta Verification
    match evaluate_fitness(target, sandbox_root) {
        Ok(cand) => {
            let delta = cand.primary - baseline.primary;
            let g4_pass = delta >= threshold;
            let g4_details = format!(
                "Candidate fitness: {:.4} vs Baseline: {:.4} (delta: {:+.4}, required: {:+.4}) — {}",
                cand.primary, baseline.primary, delta, threshold, cand.details
            );
            gates.push(GateResult {
                gate_number: 4,
                name: "Fitness Delta Threshold".into(),
                passed: g4_pass,
                output: g4_details,
            });
            (g4_pass, Some(cand), gates)
        }
        Err(e) => {
            gates.push(GateResult {
                gate_number: 4,
                name: "Fitness Delta Threshold".into(),
                passed: false,
                output: format!("Failed to evaluate candidate fitness: {e}"),
            });
            (false, None, gates)
        }
    }
}

/// The files under `sandbox` whose bytes differ from the same path under `root` (or that `root` lacks), relative.
/// Build output, git metadata and Cordata's own state are not candidates.
fn changed_between(sandbox: &Path, root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![sandbox.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let p = entry.path();
            if kind.is_dir() {
                let name = entry.file_name();
                if matches!(name.to_str(), Some("target" | ".git" | ".cordata")) {
                    continue;
                }
                stack.push(p);
            } else if kind.is_file()
                && let Ok(rel) = p.strip_prefix(sandbox)
                && ImmutableGuard::hash_file(&p) != ImmutableGuard::hash_file(&root.join(rel))
            {
                out.push(rel.to_path_buf());
            }
        }
    }
    out.sort();
    out
}

/// Runs the mutator in `sandbox` and returns the files it changed relative to `root`. The mutator's own exit status
/// is not trusted as a verdict: a failing mutator that left edits behind is still judged by the gates.
pub fn propose_changes(sandbox: &Path, root: &Path, cmd: &str, target: &EvolutionTarget, baseline: &FitnessScore) -> Result<Vec<PathBuf>, String> {
    let out = Command::new("sh")
        .args(["-c", cmd])
        .current_dir(sandbox)
        .env("CORDATA_EVOLVE_TARGET", target.to_string())
        .env("CORDATA_EVOLVE_BASELINE", format!("{:.6}", baseline.primary))
        .output()
        .map_err(|e| format!("failed to run mutator: {e}"))?;
    if !out.status.success() {
        let tail: String = String::from_utf8_lossy(&out.stderr).chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
        eprintln!("mutator exited {}: {tail}", out.status);
    }
    Ok(changed_between(sandbox, root))
}

/// Whether `root`'s git working tree has uncommitted changes (or is not a git checkout at all).
fn tree_is_dirty(root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
        .map_or(true, |o| !o.status.success() || !o.stdout.is_empty())
}

/// Executes a full, sandboxed evolution cycle for `config.target`.
///
/// Refuses, rather than reporting gates, when there is nothing to judge: with no `mutate_cmd`, or one that changes
/// nothing, a fresh sandbox is identical to the tree and a "cycle" would only re-measure the baseline. A promotion is copied
/// over the working tree only when that tree is clean, so `git checkout .` always undoes it.
pub fn run_evolution_cycle(config: EvolutionConfig) -> Result<EvolutionReport, String> {
    let start_time = Instant::now();
    let root = std::fs::canonicalize(&config.project_root).map_err(|e| format!("invalid root: {e}"))?;
    if !config.dry_run && tree_is_dirty(&root) {
        return Err("the working tree has uncommitted changes (or is not a git checkout): commit or stash first, or pass --dry-run".into());
    }

    let baseline = evaluate_fitness(&config.target, &root)?;
    let guard = ImmutableGuard::snapshot(&root);

    let label = format!("rsi-{}", config.target);
    let sandbox = synthesizer::create(&root, None, &label, &[], synthesizer::MAX_FILES)
        .map_err(|e| format!("failed to provision APFS CoW sandbox: {e}"))?;

    let candidates = match &config.mutate_cmd {
        Some(cmd) => match propose_changes(&sandbox.root, &root, cmd, &config.target, &baseline) {
            Ok(c) => c,
            Err(e) => {
                sandbox.discard();
                return Err(e);
            }
        },
        None => changed_between(&sandbox.root, &root),
    };
    if candidates.is_empty() {
        sandbox.discard();
        return Err("nothing to evaluate: the mutator proposed no change (pass --mutate \"<command>\" to propose one)".into());
    }

    let (promoted_all_gates, candidate_opt, gates) = evaluate_candidate(&sandbox.root, &config.target, &baseline, &guard, config.threshold);
    let delta = candidate_opt.as_ref().map_or(0.0, |c| c.primary - baseline.primary);

    let mut changed_files = Vec::new();
    if promoted_all_gates && !config.dry_run {
        for rel in &candidates {
            let (from, to) = (sandbox.root.join(rel), root.join(rel));
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::copy(&from, &to).is_ok() {
                changed_files.push(rel.to_string_lossy().into_owned());
            }
        }
    }

    sandbox.discard();

    Ok(EvolutionReport {
        target: config.target,
        promoted: promoted_all_gates && !config.dry_run,
        baseline_fitness: baseline,
        candidate_fitness: candidate_opt,
        delta_fitness: delta,
        gates,
        changed_files,
        duration_ms: start_time.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_guard_snapshots_and_detects_tampering() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let agents_md = root.join("AGENTS.md");
        std::fs::write(&agents_md, "test invariant").unwrap();

        let tests_dir = root.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        let test_file = tests_dir.join("eval.rs");
        std::fs::write(&test_file, "fn assert_something() {}").unwrap();

        let guard = ImmutableGuard::snapshot(root);
        assert_eq!(guard.protected_hashes.len(), 2);
        assert!(guard.verify(root).is_ok());

        // Tampering: edit eval.rs
        std::fs::write(&test_file, "fn assert_something() { /* tampered */ }").unwrap();
        let err = guard.verify(root).unwrap_err();
        match err {
            TamperingViolation::Modified { path, .. } => assert_eq!(path, "tests/eval.rs"),
            other => panic!("expected Modified, got {other:?}"),
        }

        // Tampering: delete AGENTS.md
        std::fs::remove_file(&agents_md).unwrap();
        let err2 = guard.verify(root).unwrap_err();
        assert!(matches!(err2, TamperingViolation::Deleted { .. } | TamperingViolation::Modified { .. }));

        // Restore original state and test planted file detection
        std::fs::write(&agents_md, "test invariant").unwrap();
        std::fs::write(&test_file, "fn assert_something() {}").unwrap();
        assert!(guard.verify(root).is_ok());

        let planted = tests_dir.join("planted_eval.rs");
        std::fs::write(&planted, "fn planted_test() {}").unwrap();
        let err3 = guard.verify(root).unwrap_err();
        match err3 {
            TamperingViolation::Planted { path } => assert_eq!(path, "tests/planted_eval.rs"),
            other => panic!("expected Planted, got {other:?}"),
        }
    }

    #[test]
    fn evolution_target_parsing() {
        assert_eq!("repomap".parse::<EvolutionTarget>().unwrap(), EvolutionTarget::Repomap);
        assert_eq!("economizer".parse::<EvolutionTarget>().unwrap(), EvolutionTarget::Economizer);
        assert_eq!("guard".parse::<EvolutionTarget>().unwrap(), EvolutionTarget::Guard);
        assert_eq!("custom_subsystem".parse::<EvolutionTarget>().unwrap(), EvolutionTarget::Custom("custom_subsystem".into()));
    }

    #[test]
    fn mutator_edits_land_in_sandbox_only_and_tampering_is_caught() {
        let (root, sb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for dir in [root.path(), sb.path()] {
            std::fs::create_dir_all(dir.join("tests")).unwrap();
            std::fs::write(dir.join("tests/eval.rs"), "eval").unwrap();
            std::fs::write(dir.join("lib.rs"), "v1").unwrap();
        }
        let guard = ImmutableGuard::snapshot(root.path());
        let base = FitnessScore { primary: 1.0, latency_ms: 0.0, details: String::new() };
        let t = EvolutionTarget::Guard;

        let changed = propose_changes(sb.path(), root.path(), "echo v2 > lib.rs", &t, &base).unwrap();
        assert_eq!(changed, vec![PathBuf::from("lib.rs")]);
        assert_eq!(std::fs::read_to_string(root.path().join("lib.rs")).unwrap(), "v1", "project untouched");
        assert!(guard.verify(sb.path()).is_ok(), "benign edit passes gate 1");

        propose_changes(sb.path(), root.path(), "echo cheat > tests/eval.rs", &t, &base).unwrap();
        assert!(matches!(guard.verify(sb.path()), Err(TamperingViolation::Modified { .. })), "editing the evaluator is caught");
        let (ok, _, gates) = evaluate_candidate(sb.path(), &t, &base, &guard, 0.0);
        assert!(!ok && gates.len() == 1 && gates[0].gate_number == 1, "gate 1 stops it before any build or test runs");
    }

    #[test]
    fn only_differing_files_are_candidates() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for dir in [a.path(), b.path()] {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("src/same.rs"), "same").unwrap();
            std::fs::create_dir_all(dir.join("target")).unwrap();
        }
        assert!(changed_between(a.path(), b.path()).is_empty(), "an untouched copy proposes nothing");
        std::fs::write(a.path().join("src/new.rs"), "new").unwrap();
        std::fs::write(a.path().join("target/out"), "build output").unwrap();
        assert_eq!(changed_between(a.path(), b.path()), vec![PathBuf::from("src/new.rs")]);
    }
}
