use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;
use toml::Value;

use crate::model::{ComponentRole, FileInfo};

#[derive(Debug)]
pub struct RawComponent {
    pub id: String,
    pub name: String,
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub role: ComponentRole,
    pub summary: String,
    pub internal_deps: Vec<String>,
}

pub fn discover_components(root: &Path) -> Result<Vec<RawComponent>> {
    let mut raw_components = Vec::new();
    let mut visited_paths = HashSet::new();

    // 1. Check Cargo workspaces (root, and sub-workspaces like tui/ and app/)
    let cargo_roots = vec![root.to_path_buf(), root.join("tui"), root.join("app")];
    for base in cargo_roots {
        let manifest_path = base.join("Cargo.toml");
        if manifest_path.exists() {
            if let Ok(content) = fs::read_to_string(&manifest_path) {
                if let Ok(value) = content.parse::<Value>() {
                    if let Some(workspace) = value.get("workspace") {
                        if let Some(members) = workspace.get("members").and_then(Value::as_array) {
                            for member in members {
                                if let Some(m_str) = member.as_str() {
                                    let member_dir = base.join(m_str);
                                    if member_dir.exists() && member_dir.is_dir() {
                                        let canonical = member_dir.canonicalize().unwrap_or(member_dir.clone());
                                        if visited_paths.insert(canonical) {
                                            if let Some(comp) = inspect_cargo_crate(root, &member_dir) {
                                                raw_components.push(comp);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if value.get("package").is_some() {
                        let canonical = base.canonicalize().unwrap_or(base.clone());
                        if visited_paths.insert(canonical) {
                            if let Some(comp) = inspect_cargo_crate(root, &base) {
                                raw_components.push(comp);
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. If no components found, fallback to top-level directories
    if raw_components.is_empty() {
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let file_name = entry.file_name().to_string_lossy().to_string();
                    if !file_name.starts_with('.') && file_name != "target" && file_name != "node_modules" {
                        let role = infer_role(&file_name, &file_name);
                        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
                        raw_components.push(RawComponent {
                            id: file_name.clone(),
                            name: file_name,
                            rel_path: rel,
                            abs_path: path,
                            role,
                            summary: "Directory subsystem".into(),
                            internal_deps: Vec::new(),
                        });
                    }
                }
            }
        }
    }

    // Filter dependency names so they only refer to existing component IDs
    let all_ids: HashSet<String> = raw_components.iter().map(|c| c.id.clone()).collect();
    for comp in &mut raw_components {
        comp.internal_deps.retain(|dep| all_ids.contains(dep));
    }

    Ok(raw_components)
}

fn inspect_cargo_crate(root: &Path, crate_dir: &Path) -> Option<RawComponent> {
    let manifest_path = crate_dir.join("Cargo.toml");
    let content = fs::read_to_string(&manifest_path).ok()?;
    let value: Value = content.parse().ok()?;

    let pkg = value.get("package")?;
    let name = pkg.get("name")?.as_str()?.to_string();
    let summary = pkg
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| extract_doc_comment(crate_dir).unwrap_or_else(|| format!("{name} module")));

    let rel_path = crate_dir
        .strip_prefix(root)
        .unwrap_or(crate_dir)
        .to_string_lossy()
        .to_string();

    let role = infer_role(&name, &rel_path);

    let mut internal_deps = Vec::new();
    if let Some(deps) = value.get("dependencies").and_then(Value::as_table) {
        for (dep_name, dep_val) in deps {
            // Check if it's a path dependency or matches one of our crates
            if let Some(table) = dep_val.as_table() {
                if table.contains_key("path") {
                    internal_deps.push(dep_name.clone());
                }
            }
        }
    }

    Some(RawComponent {
        id: name.clone(),
        name,
        rel_path,
        abs_path: crate_dir.to_path_buf(),
        role,
        summary,
        internal_deps,
    })
}

fn infer_role(name: &str, path: &str) -> ComponentRole {
    let combined = format!("{}/{}", path.to_lowercase(), name.to_lowercase());
    if combined.contains("core")
        || combined.contains("ledger")
        || combined.contains("checkpoint")
        || combined.contains("store")
        || combined.contains("db")
    {
        ComponentRole::Core
    } else if combined.contains("orchestrator")
        || combined.contains("engine")
        || combined.contains("synthesizer")
        || combined.contains("worker")
        || combined.contains("economizer")
    {
        ComponentRole::Engine
    } else if combined.contains("tui")
        || combined.contains("app")
        || combined.contains("cli")
        || combined.contains("view")
        || combined.contains("ui")
        || combined.contains("run")
    {
        ComponentRole::Interface
    } else if combined.contains("doctor")
        || combined.contains("guard")
        || combined.contains("secrets")
        || combined.contains("floorplan")
        || combined.contains("tool")
    {
        ComponentRole::Tool
    } else {
        ComponentRole::Subsystem
    }
}

fn extract_doc_comment(crate_dir: &Path) -> Option<String> {
    for entry in ["src/lib.rs", "src/main.rs"] {
        let file = crate_dir.join(entry);
        if let Ok(content) = fs::read_to_string(file) {
            for line in content.lines() {
                let trimmed = line.trim();
                if let Some(doc) = trimmed.strip_prefix("//!") {
                    let cleaned = doc.trim().trim_start_matches('*').trim();
                    if !cleaned.is_empty() {
                        return Some(cleaned.to_string());
                    }
                }
            }
        }
    }
    None
}

pub fn scan_component_files(
    root: &Path,
    comp_path: &Path,
    modified_paths: &HashSet<String>,
    active_paths: &HashSet<String>,
) -> (Vec<FileInfo>, usize) {
    let mut files = Vec::new();
    let mut total_lines = 0;

    let walker = WalkBuilder::new(comp_path)
        .hidden(true)
        .git_ignore(true)
        .build();

    for result in walker.flatten() {
        let p = result.path();
        if p.is_file() {
            // Only examine relevant source/config files
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(
                ext,
                "rs" | "toml" | "json" | "ts" | "tsx" | "js" | "py" | "go" | "sh" | "html" | "css" | "md"
            ) && !name.ends_with(".lock")
            {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .to_string();

                let line_count = count_lines(p);
                total_lines += line_count;

                let is_mod = modified_paths.contains(&rel)
                    || modified_paths.iter().any(|m| rel.ends_with(m) || m.ends_with(&rel));
                let is_act = active_paths.contains(&rel)
                    || active_paths.iter().any(|a| rel.ends_with(a) || a.ends_with(&rel));

                files.push(FileInfo {
                    relative_path: rel,
                    lines: line_count,
                    is_modified: is_mod,
                    is_active: is_act,
                });
            }
        }
    }

    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    (files, total_lines)
}

fn count_lines(path: &Path) -> usize {
    if let Ok(content) = fs::read_to_string(path) {
        content.lines().count()
    } else {
        0
    }
}
