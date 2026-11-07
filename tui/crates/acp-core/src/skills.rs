//! Skill autotriggering and discovery ($c_6$): preventing orphaned skills.
//!
//! Inspired by the Harness² skill autotrigger invariant (`harness2/core/tree.py`),
//! skills defined on disk (`skills/<name>/SKILL.md` or `.agents/skills/<name>/SKILL.md`)
//! must be discoverable and referenced with an explicit trigger condition in `AGENTS.md`.
//!
//! Without a trigger condition in the always-on context, models rarely discover or load
//! on-demand workflows.

use std::path::{Path, PathBuf};

/// Metadata extracted from a `SKILL.md` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub rel_path: String,
}

/// Audit report comparing discovered skills against trigger lines in `AGENTS.md`.
#[derive(Debug, Clone, Default)]
pub struct SkillAudit {
    pub triggered: Vec<SkillInfo>,
    pub orphaned: Vec<SkillInfo>,
}

/// Extracts YAML frontmatter `name:` and `description:` from a markdown string.
pub fn parse_skill_frontmatter(content: &str) -> (Option<String>, Option<String>) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, None);
    }
    let after_start = &trimmed[3..];
    let Some(end_idx) = after_start.find("---") else {
        return (None, None);
    };
    let fm = &after_start[..end_idx];

    let mut name = None;
    let mut desc = None;

    for line in fm.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            let v = val.trim().trim_matches('"').trim_matches('\'').to_string();
            if !v.is_empty() {
                name = Some(v);
            }
        } else if let Some(val) = line.strip_prefix("description:") {
            let v = val.trim().trim_matches('"').trim_matches('\'').to_string();
            if !v.is_empty() {
                desc = Some(v);
            }
        }
    }

    (name, desc)
}

/// Discovers all skills in `.agents/skills/`, `skills/`, and `.cordata/skills/`.
pub fn discover_skills(cwd: &Path) -> Vec<SkillInfo> {
    let mut out = Vec::new();
    let candidates = [
        cwd.join(".agents").join("skills"),
        cwd.join("skills"),
        cwd.join(".cordata").join("skills"),
    ];

    for base in candidates {
        if !base.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_file = path.join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let dir_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let content = std::fs::read_to_string(&skill_file).unwrap_or_default();
            let (fm_name, fm_desc) = parse_skill_frontmatter(&content);
            let name = fm_name.unwrap_or_else(|| dir_name.clone());
            let description = fm_desc.unwrap_or_else(|| format!("procedure for {name}"));
            let rel_path = skill_file
                .strip_prefix(cwd)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| skill_file.display().to_string());

            out.push(SkillInfo {
                name,
                description,
                path: skill_file,
                rel_path,
            });
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Audits whether discovered skills have an active trigger line in `AGENTS.md` or `CORDATA.md`.
pub fn audit_skills(cwd: &Path) -> SkillAudit {
    let skills = discover_skills(cwd);
    let mut instructions = String::new();

    for name in ["AGENTS.md", "CORDATA.md", ".cordata/CORDATA.md", "CLAUDE.md"] {
        let p = cwd.join(name);
        if p.is_file()
            && let Ok(text) = std::fs::read_to_string(&p)
        {
            instructions.push('\n');
            instructions.push_str(&text);
        }
    }

    let mut audit = SkillAudit::default();

    for skill in skills {
        let triggered = instructions.contains(&skill.name) || instructions.contains(&skill.rel_path);
        if triggered {
            audit.triggered.push(skill);
        } else {
            audit.orphaned.push(skill);
        }
    }

    audit
}

/// Synchronizes missing skill triggers into `AGENTS.md`.
/// Returns the number of newly added trigger lines.
pub fn sync_skill_triggers(cwd: &Path) -> std::io::Result<usize> {
    let audit = audit_skills(cwd);
    if audit.orphaned.is_empty() {
        return Ok(0);
    }

    let agents_md = cwd.join("AGENTS.md");
    let mut current = if agents_md.is_file() {
        std::fs::read_to_string(&agents_md)?
    } else {
        "# AGENTS.md — Workspace Index\n\n".to_string()
    };

    let count = audit.orphaned.len();
    if !current.contains("## Available Skills & Triggers") {
        current.push_str("\n## Available Skills & Triggers\n\n");
    } else if !current.ends_with('\n') {
        current.push('\n');
    }

    for skill in audit.orphaned {
        let line = format!(
            "- When {}, read `{}` and follow it.\n",
            skill.description.trim_end_matches('.'),
            skill.rel_path
        );
        current.push_str(&line);
    }

    std::fs::write(&agents_md, current)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_frontmatter_extracts_name_and_description() {
        let doc = r#"---
name: verify-deploy
description: Runs smoke tests and sanity checks before production release.
---
# Instructions
"#;
        let (name, desc) = parse_skill_frontmatter(doc);
        assert_eq!(name.as_deref(), Some("verify-deploy"));
        assert_eq!(
            desc.as_deref(),
            Some("Runs smoke tests and sanity checks before production release.")
        );
    }

    #[test]
    fn discovers_and_audits_orphaned_skills() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("skills").join("deploy");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deploy\ndescription: deployment workflow\n---\n",
        )
        .unwrap();

        let audit = audit_skills(dir.path());
        assert_eq!(audit.orphaned.len(), 1);
        assert_eq!(audit.orphaned[0].name, "deploy");
        assert_eq!(audit.triggered.len(), 0);

        let added = sync_skill_triggers(dir.path()).unwrap();
        assert_eq!(added, 1);

        let audit_after = audit_skills(dir.path());
        assert_eq!(audit_after.orphaned.len(), 0);
        assert_eq!(audit_after.triggered.len(), 1);

        let agents_content = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(agents_content.contains("- When deployment workflow, read `skills/deploy/SKILL.md` and follow it."));
    }
}
