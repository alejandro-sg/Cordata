use crate::model::{Component, ComponentStatus, Floorplan};

pub fn render_terminal(fp: &Floorplan, color: bool) -> String {
    let mut out = String::new();

    let (c_reset, c_bold, c_cyan, c_yellow, c_green, c_dim, c_blue, _c_magenta) = if color {
        (
            "\x1b[0m",
            "\x1b[1m",
            "\x1b[36m",
            "\x1b[33m",
            "\x1b[32m",
            "\x1b[2m",
            "\x1b[34m",
            "\x1b[35m",
        )
    } else {
        ("", "", "", "", "", "", "", "")
    };

    // 1. Header Banner
    out.push_str(&format!(
        "\n{c_bold}{c_cyan}╔══════════════════════════════════════════════════════════════════════════════════════╗{c_reset}\n"
    ));
    out.push_str(&format!(
        "{c_bold}{c_cyan}║   FLOORPLAN ARCHITECTURAL BLUEPRINT — {:<46} ║{c_reset}\n",
        fp.project_name
    ));
    out.push_str(&format!(
        "{c_bold}{c_cyan}╚══════════════════════════════════════════════════════════════════════════════════════╝{c_reset}\n"
    ));

    // 2. Metrics summary bar
    let status_bar = format!(
        " Components: {c_bold}{}{c_reset} │ Active: {c_cyan}{c_bold}{}{c_reset} │ Modified: {c_yellow}{c_bold}{}{c_reset} │ Clean: {c_green}{}{c_reset} │ Total LOC: {c_bold}{}{c_reset} ",
        fp.metrics.total_components,
        fp.metrics.active_components,
        fp.metrics.modified_components,
        fp.metrics.clean_components,
        fp.metrics.total_lines
    );
    out.push_str(&format!("  {status_bar}\n"));

    if !fp.active_scope.is_empty() {
        out.push_str(&format!(
            "  {c_dim}Active Wave Scope:{c_reset} {c_cyan}{}{c_reset}\n",
            fp.active_scope.join(", ")
        ));
    }
    out.push_str("\n");

    // Group components into layers / sections
    let mut layer_groups: Vec<(usize, Vec<&Component>)> = Vec::new();
    for comp in &fp.components {
        if let Some(group) = layer_groups.iter_mut().find(|(l, _)| *l == comp.layer) {
            group.1.push(comp);
        } else {
            layer_groups.push((comp.layer, vec![comp]));
        }
    }
    layer_groups.sort_by_key(|(l, _)| *l);
    // Reverse layer display so UI/Top layers are on top and Core is at bottom, like a real architectural building/stack!
    layer_groups.reverse();

    for (layer_idx, comps) in layer_groups {
        let layer_title = match layer_idx {
            0 => "FOUNDATION & CORE LAYER",
            1 => "SERVICES & EXECUTION ENGINES",
            2 => "USER INTERFACES & CLIENTS",
            3 => "SYSTEM TOOLS & DIAGNOSTICS",
            _ => "SUBSYSTEMS",
        };

        out.push_str(&format!(
            "  {c_dim}─── {c_bold}{c_blue}{layer_title}{c_reset}{c_dim} (Depth {layer_idx}) ────────────────────────────────────────────────────────{c_reset}\n\n"
        ));

        for comp in comps {
            let room_box = render_room_box(comp, color);
            out.push_str(&room_box);
            out.push('\n');
        }

        // Connection arrow flow down to next layer
        if layer_idx > 0 {
            out.push_str(&format!(
                "               {c_cyan}▼   ▼   ▼   (depends on lower layers)   ▼   ▼   ▼{c_reset}\n\n"
            ));
        }
    }

    // Footer tip
    out.push_str(&format!(
        "  {c_dim}Tip: Run {c_bold}'cordata floorplan --html'{c_reset}{c_dim} to open the interactive SVG blueprint canvas.{c_reset}\n\n"
    ));

    out
}

fn render_room_box(comp: &Component, color: bool) -> String {
    let (c_reset, c_bold, c_cyan, c_yellow, c_green, c_dim, _c_blue, c_magenta) = if color {
        (
            "\x1b[0m",
            "\x1b[1m",
            "\x1b[36m",
            "\x1b[33m",
            "\x1b[32m",
            "\x1b[2m",
            "\x1b[34m",
            "\x1b[35m",
        )
    } else {
        ("", "", "", "", "", "", "", "")
    };

    let border_color = match &comp.status {
        ComponentStatus::Active { .. } => c_cyan,
        ComponentStatus::Modified { .. } => c_yellow,
        ComponentStatus::Clean => c_dim,
    };

    let status_badge = match &comp.status {
        ComponentStatus::Active { active_files, .. } => {
            format!("{c_cyan}{c_bold}★ ACTIVE ({}) {c_reset}", active_files.len())
        }
        ComponentStatus::Modified { changed_files } => {
            format!("{c_yellow}{c_bold}● MODIFIED ({}) {c_reset}", changed_files.len())
        }
        ComponentStatus::Clean => format!("{c_green}✓ Clean {c_reset}"),
    };

    let role_badge = format!("{c_magenta}[{}]{c_reset}", comp.role.badge());

    let mut buf = String::new();
    let width = 76;

    // Top border
    buf.push_str(&format!("  {border_color}┌{}┐{c_reset}\n", "─".repeat(width)));

    // Line 1: Name, Role, Status
    let name_str = format!("{c_bold}{}{c_reset} {role_badge}", comp.name);
    let right_status = status_badge;
    buf.push_str(&format!(
        "  {border_color}│{c_reset}  {:<35} {:>35}  {border_color}│{c_reset}\n",
        name_str, right_status
    ));

    // Line 2: Path & Summary
    let path_str = format!("{c_dim}{}{c_reset}", comp.path);
    let summary_max = 50;
    let summary_clean = if comp.summary.len() > summary_max {
        format!("{}...", &comp.summary[..summary_max - 3])
    } else {
        comp.summary.clone()
    };
    buf.push_str(&format!(
        "  {border_color}│{c_reset}  {path_str} — {summary_clean:<45} {border_color}│{c_reset}\n"
    ));

    // Line 3: Metrics (files, lines)
    let files_info = format!(
        "{c_dim}{} files · {} lines{c_reset}",
        comp.files.len(),
        comp.total_lines
    );
    buf.push_str(&format!(
        "  {border_color}│{c_reset}  {files_info:<65} {border_color}│{c_reset}\n"
    ));

    // Line 4: Connections
    let deps_str = if comp.dependencies.is_empty() {
        "none".to_string()
    } else {
        comp.dependencies.join(", ")
    };
    let dep_line = format!("{c_dim}Depends on:{c_reset} {deps_str}");
    buf.push_str(&format!(
        "  {border_color}│{c_reset}  {dep_line:<65} {border_color}│{c_reset}\n"
    ));

    // If modified or active, show up to 3 changed files
    match &comp.status {
        ComponentStatus::Active { active_files, .. } => {
            for f in active_files.iter().take(3) {
                let f_clean = f.strip_prefix(&comp.path).unwrap_or(f).trim_start_matches('/');
                buf.push_str(&format!(
                    "  {border_color}│{c_reset}    {c_cyan}★ active: {f_clean}{c_reset}\n"
                ));
            }
        }
        ComponentStatus::Modified { changed_files } => {
            for f in changed_files.iter().take(3) {
                let f_clean = f.strip_prefix(&comp.path).unwrap_or(f).trim_start_matches('/');
                buf.push_str(&format!(
                    "  {border_color}│{c_reset}    {c_yellow}● modified: {f_clean}{c_reset}\n"
                ));
            }
        }
        ComponentStatus::Clean => {}
    }

    // Bottom border
    buf.push_str(&format!("  {border_color}└{}┘{c_reset}\n", "─".repeat(width)));

    buf
}
