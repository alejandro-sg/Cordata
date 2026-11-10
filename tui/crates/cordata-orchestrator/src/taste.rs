//! Design Engineering Taste, Verification, and Constraint Evaluator.
//!
//! Evaluates code changes, terminal buffers, and user-facing surfaces against the
//! Design Engineering AI paradigm:
//! - **Visual & Terminal Polish**: Viewport bounds, ANSI escape hygiene, absence of crude crashes.
//! - **Ergonomic Quality & Anti-DRY**: Prefers self-contained colocated pods over bloated shared `utils.rs`.
//! - **Empty-State & Graceful Degradation**: Enforces fallback views when lists/collections are empty.
//! - **Taste Review Schema**: Structured Tier 2 model critique for visual polish and empathy.

use std::path::Path;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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

/// Tier 1: Deterministic evaluation of terminal output and text buffers.
///
/// Ensures terminal rendering adheres to standard column bounds, leaves no raw
/// unescaped ANSI sequences, and contains no crude unhandled panic stack dumps.
pub fn lint_terminal_output(text: &str, max_width: usize) -> TasteResult {
    let mut findings = Vec::new();
    let mut metrics = TasteMetrics {
        line_count: text.lines().count(),
        ..Default::default()
    };

    for (idx, line) in text.lines().enumerate() {
        let line_num = idx + 1;
        let char_len = line.chars().count();
        if char_len > metrics.max_line_width {
            metrics.max_line_width = char_len;
        }

        // 1. Viewport width check (soft ceiling for terminal readouts)
        if char_len > max_width {
            findings.push(TasteFinding {
                category: TasteCategory::ViewportOverflow,
                severity: TasteSeverity::Warning,
                message: format!(
                    "Line {line_num} exceeds standard terminal width ({char_len} > {max_width} chars): risk of ugly terminal auto-wrap",
                ),
                location: Some(format!("line:{line_num}")),
            });
        }

        // 2. Trailing whitespace check
        if line.ends_with(' ') || line.ends_with('\t') {
            metrics.trailing_whitespace_lines += 1;
        }

        // 3. ANSI escape counts & hygiene
        if line.contains("\x1b[") {
            metrics.ansi_escape_count += line.matches("\x1b[").count();
            // Check for unclosed escape sequences or broken resets
            if line.contains("\x1b[") && !line.contains('m') {
                findings.push(TasteFinding {
                    category: TasteCategory::AnsiHygiene,
                    severity: TasteSeverity::Error,
                    message: format!("Malformed or unclosed ANSI escape sequence on line {line_num}"),
                    location: Some(format!("line:{line_num}")),
                });
            }
        }

        // 4. Crude crash and panic markers
        let lower = line.to_ascii_lowercase();
        if lower.contains("called `option::unwrap()` on a `none` value")
            || lower.contains("called `result::unwrap()` on an `err` value")
            || lower.contains("thread 'main' panicked at")
            || lower.contains("fatal error: runtime: out of memory")
        {
            metrics.unhandled_crash_markers += 1;
            findings.push(TasteFinding {
                category: TasteCategory::CrudeCrash,
                severity: TasteSeverity::Error,
                message: format!("Crude unhandled panic or crash string emitted on line {line_num}: \"{line}\""),
                location: Some(format!("line:{line_num}")),
            });
        }
    }

    // Trailing whitespace warning if excessive (>10% of lines)
    if metrics.line_count > 0 && metrics.trailing_whitespace_lines * 10 > metrics.line_count {
        findings.push(TasteFinding {
            category: TasteCategory::VisualBalance,
            severity: TasteSeverity::Info,
            message: format!("Excessive trailing whitespace: {} lines have trailing spaces", metrics.trailing_whitespace_lines),
            location: None,
        });
    }

    let has_errors = findings.iter().any(|f| f.severity == TasteSeverity::Error);
    let penalty = (findings.len() as f64 * 0.1).min(1.0);
    let score = if has_errors { (0.5 - penalty * 0.5).max(0.0) } else { (1.0 - penalty).max(0.2) };

    TasteResult {
        passed: !has_errors && score >= 0.7,
        score,
        findings,
        metrics,
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

/// JSON Schema for Tier 2 Reviewer Model Taste Critique.
pub fn taste_review_schema() -> Value {
    json!({
        "type": "object",
        "required": ["taste_score", "verdict", "visual_polish", "ergonomics", "recommendations"],
        "properties": {
            "taste_score": {
                "type": "number",
                "minimum": 0.0,
                "maximum": 1.0,
                "description": "Composite aesthetic and ergonomic quality score (1.0 = exceptional, <0.7 = substandard)"
            },
            "verdict": {
                "type": "string",
                "enum": ["delightful", "acceptable", "needs_polish", "unacceptable"]
            },
            "visual_polish": {
                "type": "object",
                "required": ["hierarchy", "balance", "spacing"],
                "properties": {
                    "hierarchy": {"type": "string"},
                    "balance": {"type": "string"},
                    "spacing": {"type": "string"}
                }
            },
            "ergonomics": {
                "type": "object",
                "required": ["clarity", "cognitive_load", "empty_state_handling"],
                "properties": {
                    "clarity": {"type": "string"},
                    "cognitive_load": {"type": "string"},
                    "empty_state_handling": {"type": "string"}
                }
            },
            "recommendations": {
                "type": "array",
                "items": {"type": "string"}
            }
        }
    })
}

/// Formats the Tier 2 Taste Review prompt for a diff.
pub fn taste_review_prompt(diff: &str, goal: &str) -> String {
    format!(
        "You are the DESIGN & TASTE AUDITOR of a high-craft engineering system.\n\
        Review this code change through the lens of Design Engineering: aesthetic balance, visual hierarchy,\n\
        delightful CLI/UI ergonomics, thoughtful empty states, and clarity of intent.\n\n\
        Goal: {goal}\n\n\
        Diff to critique:\n\
        ```\n\
        {diff}\n\
        ```\n\n\
        Answer ONLY with a single JSON object matching the required schema."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn lint_terminal_output_detects_overflow_and_crashes() {
        let clean = "Line 1: Normal output\nLine 2: Ready.\n";
        let r_clean = lint_terminal_output(clean, 80);
        assert!(r_clean.passed);
        assert_eq!(r_clean.findings.len(), 0);
        assert!((r_clean.score - 1.0).abs() < 1e-6);

        // Viewport overflow (>80 chars)
        let wide = format!("Short line\n{}\nEnd line\n", "A".repeat(95));
        let r_wide = lint_terminal_output(&wide, 80);
        assert!(r_wide.passed); // Warning only, still passes threshold
        assert_eq!(r_wide.findings.len(), 1);
        assert_eq!(r_wide.findings[0].category, TasteCategory::ViewportOverflow);

        // Crude panic marker
        let panic_str = "Error: called `Option::unwrap()` on a `None` value\n";
        let r_panic = lint_terminal_output(panic_str, 80);
        assert!(!r_panic.passed);
        assert!(r_panic.findings.iter().any(|f| f.category == TasteCategory::CrudeCrash));
    }

    #[test]
    fn lint_terminal_output_checks_ansi_hygiene() {
        let broken_ansi = "Hello \x1b[31mRed text without closing escape";
        let res = lint_terminal_output(broken_ansi, 120);
        assert!(res.passed); // Has ansi, but valid 'm' terminator

        let unclosed = "Broken \x1b[42 unclosed seq";
        let r_bad = lint_terminal_output(unclosed, 120);
        assert!(!r_bad.passed);
        assert!(r_bad.findings.iter().any(|f| f.category == TasteCategory::AnsiHygiene));
    }

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

    #[test]
    fn taste_review_schema_is_valid() {
        let schema = taste_review_schema();
        assert!(schema.is_object());
        let required = schema.get("required").unwrap().as_array().unwrap();
        assert!(required.iter().any(|v| v == "taste_score"));
        assert!(required.iter().any(|v| v == "visual_polish"));
    }
}
