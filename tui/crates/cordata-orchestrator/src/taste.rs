//! Design Engineering Taste, Verification, and Constraint Evaluator.
//!
//! Evaluates code changes against the Design Engineering checks the audit runs (`audit.rs`): colocated pods over
//! shared `utils.rs`, and fallback views when lists or collections are empty.

use std::path::Path;
use serde::{Deserialize, Serialize};

/// Severity of an aesthetic or constraint finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TasteSeverity {
    Info,
    Warning,
    Error,
}

impl std::fmt::Display for TasteSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Info => write!(f, "INFO"),
            Self::Warning => write!(f, "WARN"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

/// Category of taste or constraint evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TasteCategory {
    ViewportOverflow,
    AnsiHygiene,
    CrudeCrash,
    VisualBalance,
    AntiDryViolation,
    EmptyStateMissing,
    Ergonomics,
}

/// A specific aesthetic or ergonomic finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasteFinding {
    pub category: TasteCategory,
    pub severity: TasteSeverity,
    pub message: String,
    pub location: Option<String>,
}

/// Metrics gathered during aesthetic inspection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TasteMetrics {
    pub max_line_width: usize,
    pub line_count: usize,
    pub trailing_whitespace_lines: usize,
    pub ansi_escape_count: usize,
    pub unhandled_crash_markers: usize,
}

/// Overall result of a taste and constraint evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TasteResult {
    pub passed: bool,
    pub score: f64,
    pub findings: Vec<TasteFinding>,
    pub metrics: TasteMetrics,
}

impl TasteResult {
    pub fn success(metrics: TasteMetrics) -> Self {
        Self {
            passed: true,
            score: 1.0,
            findings: Vec::new(),
            metrics,
        }
    }
}

/// Tier 1: Evaluates source code for Design Engineering ergonomics and anti-patterns.
///
/// Discourages centralized `utils.rs` sinks (the DRY trap) and encourages explicit
/// empty-state handling in UI/rendering code.
pub fn lint_source_ergonomics(source: &str, file_path: &Path) -> TasteResult {
    let mut findings = Vec::new();
    let metrics = TasteMetrics {
        line_count: source.lines().count(),
        max_line_width: source.lines().map(|l| l.chars().count()).max().unwrap_or(0),
        trailing_whitespace_lines: 0,
        ansi_escape_count: 0,
        unhandled_crash_markers: 0,
    };

    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // 1. The DRY Trap Warning: Flag creation of centralized junk-drawer utility sinks
    if filename == "utils.rs" || filename == "helpers.rs" || filename == "common.rs" {
        findings.push(TasteFinding {
            category: TasteCategory::AntiDryViolation,
            severity: TasteSeverity::Warning,
            message: format!(
                "Centralized utility file `{filename}` detected. Prefer self-contained colocated modules: duplicate local helpers rather than coupling disparate features to a shared commons.",
            ),
            location: Some(file_path.to_string_lossy().into_owned()),
        });
    }

    // 2. Empty-State Handling Check: In UI/rendering modules, check if empty states are handled
    let is_ui_file = file_path.to_string_lossy().contains("view")
        || file_path.to_string_lossy().contains("ui")
        || filename.contains("render")
        || filename.contains("palette");

    if is_ui_file && !source.contains("is_empty()") && !source.contains("empty") && !source.contains("None") {
        findings.push(TasteFinding {
            category: TasteCategory::EmptyStateMissing,
            severity: TasteSeverity::Warning,
            message: "UI component does not appear to handle empty or zero-item states (missing `.is_empty()` or empty fallback view)".into(),
            location: Some(file_path.to_string_lossy().into_owned()),
        });
    }

    let has_errors = findings.iter().any(|f| f.severity == TasteSeverity::Error);
    let penalty = (findings.len() as f64 * 0.15).min(1.0);
    let score = if has_errors { 0.0 } else { (1.0 - penalty).max(0.4) };

    TasteResult {
        passed: !has_errors && score >= 0.7,
        score,
        findings,
        metrics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn lint_source_ergonomics_warns_against_dry_utils_sinks() {
        let dummy_source = "pub fn helper() -> usize { 42 }\n";
        let utils_path = PathBuf::from("src/utils.rs");
        let res = lint_source_ergonomics(dummy_source, &utils_path);

        assert!(res.findings.iter().any(|f| f.category == TasteCategory::AntiDryViolation));
        assert!(res.findings[0].message.contains("Centralized utility file"));

        let pod_path = PathBuf::from("src/session/mod.rs");
        let res2 = lint_source_ergonomics(dummy_source, &pod_path);
        assert!(!res2.findings.iter().any(|f| f.category == TasteCategory::AntiDryViolation));
    }

}
