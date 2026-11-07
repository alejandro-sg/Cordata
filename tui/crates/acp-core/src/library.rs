//! Domain Experience Library ($\mathcal{R}$): persistent cross-task lessons and adopted harnesses.
//!
//! Inspired by Harness² harnessing-level recursion (`harness2/core/library.py`),
//! earlier verified changes and lessons learned in a given domain (e.g. `rust-gpui`, `macos-seatbelt`,
//! `database`, `api-design`) are indexed and carried forward to guide future tasks in that domain.
//!
//! Storage layout:
//! ```text
//! ~/.cordata/library/<domain>/INDEX.md
//! ~/.cordata/library/<domain>/harnesses/<task_slug>/EXPERIENCE.md
//! ```

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

/// Record of an adopted solution and its verified lessons.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExperienceRecord {
    pub task_id: String,
    pub domain: String,
    pub cue: String,
    pub lesson: String,
    pub evidence: String,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default)]
    pub timestamp: u64,
}

/// Persistent per-domain experience library.
#[derive(Debug, Clone)]
pub struct DomainLibrary {
    root: PathBuf,
}

impl DomainLibrary {
    /// Opens or initializes the experience library at the default `~/.cordata/library` location.
    pub fn default_location() -> Self {
        let home = std::env::var("CORDATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(".cordata")
            });
        Self {
            root: home.join("library"),
        }
    }

    /// Opens the library at a specific root folder.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Returns the directory for a given domain name.
    pub fn domain_dir(&self, domain: &str) -> PathBuf {
        let slug = domain
            .to_ascii_lowercase()
            .replace(['/', '\\', ' '], "-");
        self.root.join(slug)
    }

    /// Records an experience into the appropriate domain folder and updates `INDEX.md`.
    pub fn record(&self, exp: &ExperienceRecord) -> std::io::Result<PathBuf> {
        let domain_dir = self.domain_dir(&exp.domain);
        let task_slug = exp.task_id.replace(['/', '\\', ' '], "-");
        let task_dir = domain_dir.join("harnesses").join(&task_slug);
        std::fs::create_dir_all(&task_dir)?;

        let exp_path = task_dir.join("EXPERIENCE.md");
        let mut content = format!("# {}\n\n", exp.task_id);
        content.push_str(&format!("- domain: `{}`\n", exp.domain));
        if !exp.components.is_empty() {
            content.push_str(&format!("- components: {}\n", exp.components.join(", ")));
        }
        content.push_str(&format!("- timestamp: {}\n\n", exp.timestamp));
        content.push_str(&format!("## Cue\n{}\n\n", exp.cue));
        content.push_str(&format!("## Lesson\n{}\n\n", exp.lesson));
        content.push_str(&format!("## Evidence\n{}\n", exp.evidence));

        std::fs::write(&exp_path, content)?;
        self.rebuild_domain_index(&exp.domain)?;

        Ok(exp_path)
    }

    /// Rebuilds the `INDEX.md` file for a domain by scanning its `harnesses/` subdirectories.
    pub fn rebuild_domain_index(&self, domain: &str) -> std::io::Result<()> {
        let domain_dir = self.domain_dir(domain);
        let harnesses_dir = domain_dir.join("harnesses");
        if !harnesses_dir.is_dir() {
            return Ok(());
        }

        let mut lines = Vec::new();
        lines.push(format!("# Experience Index: {domain}\n"));
        lines.push("Lessons and verified patterns from completed tasks in this domain.\n".to_string());

        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&harnesses_dir)?.flatten() {
            if entry.path().is_dir() {
                let exp_file = entry.path().join("EXPERIENCE.md");
                if exp_file.is_file() {
                    let text = std::fs::read_to_string(&exp_file).unwrap_or_default();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let cue = extract_section(&text, "## Cue").unwrap_or_default();
                    let lesson = extract_section(&text, "## Lesson").unwrap_or_default();
                    entries.push((name, cue, lesson));
                }
            }
        }

        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, cue, lesson) in entries {
            lines.push(format!("### {name}"));
            if !cue.is_empty() {
                lines.push(format!("**When:** {cue}"));
            }
            if !lesson.is_empty() {
                lines.push(format!("**Lesson:** {lesson}"));
            }
            lines.push(String::new());
        }

        std::fs::write(domain_dir.join("INDEX.md"), lines.join("\n"))?;
        Ok(())
    }

    /// Queries all lessons for a domain.
    pub fn find_domain_experiences(&self, domain: &str) -> Vec<ExperienceRecord> {
        let domain_dir = self.domain_dir(domain);
        let harnesses_dir = domain_dir.join("harnesses");
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&harnesses_dir) else {
            return out;
        };

        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let exp_file = entry.path().join("EXPERIENCE.md");
                if let Ok(text) = std::fs::read_to_string(&exp_file) {
                    let task_id = text
                        .lines()
                        .next()
                        .and_then(|l| l.strip_prefix("# "))
                        .unwrap_or_default()
                        .to_string();
                    let cue = extract_section(&text, "## Cue").unwrap_or_default();
                    let lesson = extract_section(&text, "## Lesson").unwrap_or_default();
                    let evidence = extract_section(&text, "## Evidence").unwrap_or_default();

                    out.push(ExperienceRecord {
                        task_id,
                        domain: domain.to_string(),
                        cue,
                        lesson,
                        evidence,
                        components: Vec::new(),
                        timestamp: 0,
                    });
                }
            }
        }

        out.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        out
    }

    /// Formats a compact context priming block for a domain to enhance agent instructions.
    pub fn context_prompt(&self, domain: &str) -> Option<String> {
        let experiences = self.find_domain_experiences(domain);
        if experiences.is_empty() {
            return None;
        }

        let mut block = format!("## Domain Experience ({domain})\n");
        block.push_str("Proven patterns and lessons learned from past tasks in this domain:\n\n");
        for exp in experiences.iter().take(4) {
            block.push_str(&format!("- **{}**: {}\n", exp.cue.trim(), exp.lesson.trim()));
        }
        Some(block)
    }
}

fn extract_section(markdown: &str, header: &str) -> Option<String> {
    let pos = markdown.find(header)?;
    let after = &markdown[pos + header.len()..];
    let content = if let Some(next_pos) = after.find("\n## ") {
        &after[..next_pos]
    } else {
        after
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn records_experience_and_builds_domain_index() {
        let dir = tempdir().unwrap();
        let lib = DomainLibrary::new(dir.path().to_path_buf());
        let record = ExperienceRecord {
            task_id: "task-01".into(),
            domain: "rust-gpui".into(),
            cue: "Layout scroll container fails to scroll in GPUI".into(),
            lesson: "Do not put flex row directly on overflow_y_scroll".into(),
            evidence: "Verified with 120Hz frame bench and manual scroll test".into(),
            components: vec!["c2:guardrails".into(), "c8:hooks".into()],
            timestamp: 1234567890,
        };

        let path = lib.record(&record).unwrap();
        assert!(path.is_file());

        let index_path = lib.domain_dir("rust-gpui").join("INDEX.md");
        assert!(index_path.is_file());
        let index_content = std::fs::read_to_string(index_path).unwrap();
        assert!(index_content.contains("task-01"));
        assert!(index_content.contains("Layout scroll container fails to scroll"));

        let found = lib.find_domain_experiences("rust-gpui");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].task_id, "task-01");

        let prompt = lib.context_prompt("rust-gpui").unwrap();
        assert!(prompt.contains("Do not put flex row directly on overflow_y_scroll"));
    }
}
