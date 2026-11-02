use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use crate::model::ComponentStatus;

#[derive(Debug, Default)]
pub struct GitActivity {
    pub modified_files: HashSet<String>,
}

impl GitActivity {
    pub fn detect(root: &Path) -> Self {
        let mut modified = HashSet::new();

        // 1. git status --porcelain=v1
        if let Ok(output) = Command::new("git")
            .arg("status")
            .arg("--porcelain=v1")
            .current_dir(root)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if line.len() > 3 {
                        let file = line[3..].trim();
                        // Handle renames "old -> new"
                        let actual = if let Some((_, new_name)) = file.split_once("->") {
                            new_name.trim()
                        } else {
                            file
                        };
                        modified.insert(actual.to_string());
                    }
                }
            }
        }

        // 2. git diff --name-only HEAD
        if let Ok(output) = Command::new("git")
            .arg("diff")
            .arg("--name-only")
            .arg("HEAD")
            .current_dir(root)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        modified.insert(trimmed.to_string());
                    }
                }
            }
        }

        Self {
            modified_files: modified,
        }
    }
}

pub fn determine_status(
    comp_rel_path: &str,
    files: &[crate::model::FileInfo],
    active_scope: &[String],
    git_modified: &HashSet<String>,
) -> ComponentStatus {
    let mut comp_active_files = Vec::new();
    let mut comp_modified_files = Vec::new();

    for f in files {
        if f.is_active {
            comp_active_files.push(f.relative_path.clone());
        }
        if f.is_modified {
            comp_modified_files.push(f.relative_path.clone());
        }
    }

    // Also check if any active_scope item matches component path prefix
    for scope in active_scope {
        if scope.starts_with(comp_rel_path) || comp_rel_path.starts_with(scope) {
            if !comp_active_files.contains(scope) {
                comp_active_files.push(scope.clone());
            }
        }
    }

    // Also check if any git modified file matches component path prefix
    for git_f in git_modified {
        if git_f.starts_with(comp_rel_path) && !comp_modified_files.contains(git_f) {
            comp_modified_files.push(git_f.clone());
        }
    }

    if !comp_active_files.is_empty() {
        ComponentStatus::Active {
            active_files: comp_active_files,
            note: None,
        }
    } else if !comp_modified_files.is_empty() {
        ComponentStatus::Modified {
            changed_files: comp_modified_files,
        }
    } else {
        ComponentStatus::Clean
    }
}
