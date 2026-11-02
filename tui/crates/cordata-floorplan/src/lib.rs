use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

pub mod activity;
pub mod discovery;
pub mod graph;
pub mod model;
pub mod render_html;
pub mod render_term;

pub use model::{Component, ComponentRole, ComponentStatus, Floorplan, ProjectMetrics};
pub use render_html::write_html_report;
pub use render_term::render_terminal;

/// Generates a complete Floorplan for the workspace at `root`, overlaying any `active_scope` files.
pub fn generate(root: &Path, active_scope: &[String]) -> Result<Floorplan> {
    let canonical_root = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf());

    let project_name = canonical_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Cordata")
        .to_string();

    // 1. Discover architectural components
    let raw_components = discovery::discover_components(&canonical_root)?;

    // 2. Query git for uncommitted and changed files
    let git_activity = activity::GitActivity::detect(&canonical_root);

    let active_set: HashSet<String> = active_scope.iter().cloned().collect();

    // 3. Scan files for each component
    let mut files_map = HashMap::new();
    let mut status_map = HashMap::new();

    let mut total_files_count = 0;
    let mut total_lines_count = 0;
    let mut clean_count = 0;
    let mut modified_count = 0;
    let mut active_count = 0;

    for comp in &raw_components {
        let (files, lines) = discovery::scan_component_files(
            &canonical_root,
            &comp.abs_path,
            &git_activity.modified_files,
            &active_set,
        );

        total_files_count += files.len();
        total_lines_count += lines;

        let status = activity::determine_status(
            &comp.rel_path,
            &files,
            active_scope,
            &git_activity.modified_files,
        );

        match &status {
            ComponentStatus::Active { .. } => active_count += 1,
            ComponentStatus::Modified { .. } => modified_count += 1,
            ComponentStatus::Clean => clean_count += 1,
        }

        files_map.insert(comp.id.clone(), (files, lines));
        status_map.insert(comp.id.clone(), status);
    }

    // 4. Build graph & compute layer depths
    let graph::GraphResult {
        components,
        connections,
    } = graph::build_graph(raw_components, files_map, status_map);

    let metrics = ProjectMetrics {
        total_components: components.len(),
        clean_components: clean_count,
        modified_components: modified_count,
        active_components: active_count,
        total_files: total_files_count,
        total_lines: total_lines_count,
    };

    Ok(Floorplan {
        project_name,
        root: canonical_root.to_string_lossy().to_string(),
        components,
        connections,
        active_scope: active_scope.to_vec(),
        metrics,
    })
}

/// Opens the specified file in the default system browser or viewer.
pub fn open_in_browser(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(path)
            .spawn()
            .with_context(|| format!("failed launching 'open {}'", path.display()))?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(path)
            .spawn()
            .with_context(|| format!("failed launching 'xdg-open {}'", path.display()))?;
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(["/C", "start", &path.to_string_lossy()])
            .spawn()
            .with_context(|| format!("failed launching 'start {}'", path.display()))?;
    }
    Ok(())
}
