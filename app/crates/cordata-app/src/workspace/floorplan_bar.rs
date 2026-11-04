//! The Spatial Agent Floorplan Bar: a quiet, high-signal multi-agent status strip
//! situated directly beneath the conversation header.
//!
//! Displays:
//! - Coordinated agent roles: Researcher, Architect, Implementer, Auditor
//! - Active focus & APFS scope lock
//! - Workspace architectural metrics (components, changed files, hermetic boundaries)
//! - Quick navigation to the interactive HTML blueprint and Milestone Scrubber canvas

use std::path::{Path, PathBuf};

use cordata_theme::tokens as t;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, div, px,
};
use serde_json::Value;

use super::{DockTab, Workspace};
use crate::theme::{c, ca, line, mono, tk};
use crate::ui::{self, Kind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Researcher,
    Architect,
    Implementer,
    Auditor,
}

impl AgentRole {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Researcher => "Researcher",
            Self::Architect => "Architect",
            Self::Implementer => "Implementer",
            Self::Auditor => "Auditor",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Researcher => "Codebase discovery, AST queries & contract research",
            Self::Architect => "Plan synthesis, DAG milestones & conformance verification",
            Self::Implementer => "Parallel wave execution in APFS clonefile sandboxes",
            Self::Auditor => "Stateless Goldfish audit, compiler checks & 4 Iron Gates",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorplanSummary {
    pub project_name: String,
    pub total_components: usize,
    pub clean_components: usize,
    pub modified_components: usize,
    pub total_files: usize,
    pub active_scope: Vec<String>,
    pub loading: bool,
    pub last_status: Option<String>,
}

impl Default for FloorplanSummary {
    fn default() -> Self {
        Self {
            project_name: "Cordata".into(),
            total_components: 12,
            clean_components: 12,
            modified_components: 0,
            total_files: 0,
            active_scope: Vec::new(),
            loading: false,
            last_status: None,
        }
    }
}

/// Helper to locate cordata or cordata-floorplan binary
fn floorplan_bin() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CORDATA_FLOORPLAN_BIN")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
    {
        return Some(path);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let search_paths = [
        home.join(".local/bin/cordata"),
        PathBuf::from("/usr/local/bin/cordata"),
    ];
    for p in search_paths {
        if p.is_file() {
            return Some(p);
        }
    }

    let exe = std::env::current_exe().ok()?;
    exe.ancestors().find_map(|dir| {
        let p1 = dir.join("tui/target/release/cordata-floorplan");
        if p1.is_file() {
            return Some(p1);
        }
        let p2 = dir.join("bin/cordata");
        if p2.is_file() {
            return Some(p2);
        }
        None
    })
}

/// Reads the floorplan JSON model off the UI thread
pub fn scan_floorplan(cwd: &Path, active_scope: &[String]) -> FloorplanSummary {
    let mut summary = FloorplanSummary {
        project_name: cwd
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Cordata")
            .to_string(),
        active_scope: active_scope.to_vec(),
        ..Default::default()
    };

    if let Some(bin) = floorplan_bin() {
        let mut cmd = std::process::Command::new(bin);
        if cmd.get_program().to_string_lossy().ends_with("cordata") {
            cmd.arg("floorplan");
        }
        cmd.args(["--cwd", &cwd.to_string_lossy(), "--json"]);
        if !active_scope.is_empty() {
            cmd.args(["--scope", &active_scope.join(",")]);
        }
        cmd.stdin(std::process::Stdio::null());

        if let Ok(out) = cmd.output() {
            if out.status.success() {
                if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
                    if let Some(proj) = v.get("project_name").and_then(Value::as_str) {
                        summary.project_name = proj.to_string();
                    }
                    if let Some(m) = v.get("metrics") {
                        summary.total_components =
                            m.get("total_components").and_then(Value::as_u64).unwrap_or(12) as usize;
                        summary.clean_components =
                            m.get("clean_components").and_then(Value::as_u64).unwrap_or(12) as usize;
                        summary.modified_components =
                            m.get("modified_components").and_then(Value::as_u64).unwrap_or(0) as usize;
                        summary.total_files =
                            m.get("total_files").and_then(Value::as_u64).unwrap_or(0) as usize;
                    }
                    return summary;
                }
            }
        }
    }

    // Fallback: estimate from git and workspace folders
    let git_status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output();

    if let Ok(out) = git_status {
        let text = String::from_utf8_lossy(&out.stdout);
        let modified: Vec<String> = text
            .lines()
            .filter(|l| l.len() > 3)
            .map(|l| l.get(3..).unwrap_or("").trim().to_string())
            .collect();
        summary.modified_components = if modified.is_empty() { 0 } else { 1 };
        summary.total_components = 12;
        summary.total_files = modified.len();
    }

    summary
}

/// Spawns the standalone HTML interactive blueprint viewer
pub fn open_blueprint(cwd: &Path) {
    if let Some(bin) = floorplan_bin() {
        let mut cmd = std::process::Command::new(bin);
        if cmd.get_program().to_string_lossy().ends_with("cordata") {
            cmd.arg("floorplan");
        }
        cmd.args(["--cwd", &cwd.to_string_lossy(), "--open"]);
        let _ = cmd.spawn();
    } else {
        let html_path = cwd.join(".cordata/floorplan.html");
        if html_path.exists() {
            let _ = std::process::Command::new("open").arg(html_path).spawn();
        }
    }
}

impl Workspace {
    pub(super) fn load_floorplan(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = cx.try_global::<crate::session::Runtime>().map(|r| r.0.clone()) else {
            return;
        };
        if self.floorplan.loading {
            return;
        }
        self.floorplan.loading = true;
        let cwd = self.cwd.clone();
        let scope = self.floorplan.active_scope.clone();

        cx.spawn(async move |this, cx| {
            let res = runtime
                .spawn_blocking(move || scan_floorplan(&cwd, &scope))
                .await
                .unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.floorplan = res;
                this.floorplan.loading = false;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Determines the currently active agent role from live session view
    fn active_agent_role(&self, cx: &Context<Self>) -> (Option<AgentRole>, &'static str) {
        let view = &self.session.read(cx).view;
        if !view.turn_active {
            return (None, "Standby · Ready");
        }

        let hint = view.hint.as_deref().unwrap_or("");
        let lower = hint.to_ascii_lowercase();

        if lower.contains("research") || lower.contains("search") || lower.contains("ast") {
            (Some(AgentRole::Researcher), "Researching Codebase")
        } else if lower.contains("plan") || lower.contains("conformance") || lower.contains("dag") {
            (Some(AgentRole::Architect), "Synthesizing Plan & DAG")
        } else if lower.contains("audit") || lower.contains("gate") || lower.contains("test") {
            (Some(AgentRole::Auditor), "Running Stateless Audit")
        } else if lower.contains("edit") || lower.contains("write") || lower.contains("wave") {
            (Some(AgentRole::Implementer), "Executing Scoped Wave")
        } else if view.turn_tools > 0 {
            (Some(AgentRole::Implementer), "Applying Tool Turn")
        } else {
            (Some(AgentRole::Implementer), "Orchestrating Turn")
        }
    }

    pub(super) fn floorplan_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let (active_role, status_label) = self.active_agent_role(cx);
        let fp = &self.floorplan;
        let cwd = self.cwd.clone();

        let roles = [
            AgentRole::Researcher,
            AgentRole::Architect,
            AgentRole::Implementer,
            AgentRole::Auditor,
        ];

        let role_chips: Vec<AnyElement> = roles
            .into_iter()
            .enumerate()
            .map(|(ix, role)| {
                let is_active = active_role == Some(role);
                let dot_color = if is_active {
                    c(tk.amber)
                } else {
                    c(tk.text_faint)
                };
                let text_color = if is_active {
                    c(tk.text_primary)
                } else {
                    c(tk.text_muted)
                };
                let bg_color = if is_active {
                    ca(tk.amber, 0.12)
                } else {
                    ca(tk.bg_panel, 0.2)
                };

                div()
                    .id(("agent-role-chip", ix))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .px(px(7.))
                    .py(px(2.))
                    .rounded(px(t::RADIUS_ROW))
                    .bg(bg_color)
                    .border_1()
                    .border_color(if is_active {
                        ca(tk.amber, 0.4)
                    } else {
                        line(cx, t::LINE_SUBTLE)
                    })
                    .tooltip(ui::tip(role.description()))
                    .child(
                        div()
                            .w(px(6.))
                            .h(px(6.))
                            .rounded_full()
                            .bg(dot_color),
                    )
                    .child(
                        div()
                            .text_size(px(t::TEXT_META))
                            .font_weight(if is_active {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(text_color)
                            .child(role.name()),
                    )
                    .into_any_element()
            })
            .collect();

        // Scope lock badge
        let scope_locked = !fp.active_scope.is_empty();
        let scope_text = if scope_locked {
            format!("⚿ Scope: {} locked", fp.active_scope.len())
        } else {
            "⚿ Scope: Clean".to_string()
        };

        // Metrics badge
        let metrics_text = format!(
            "✦ {} crates · {} mod",
            fp.total_components, fp.modified_components
        );

        div()
            .id("floorplan-bar")
            .flex()
            .items_center()
            .justify_between()
            .h(px(32.))
            .px(px(14.))
            .bg(ca(tk.bg_panel, 0.4))
            .border_b_1()
            .border_color(line(cx, t::LINE_SUBTLE))
            .child(
                // Left: Agent pods
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(t::TEXT_META))
                            .font_weight(FontWeight::BOLD)
                            .text_color(c(tk.amber_soft))
                            .child("PODS"),
                    )
                    .children(role_chips)
                    .child(
                        div()
                            .text_size(px(t::TEXT_META))
                            .font_family(mono(cx))
                            .text_color(c(if active_role.is_some() {
                                tk.amber
                            } else {
                                tk.text_faint
                            }))
                            .child(format!("· {status_label}")),
                    ),
            )
            .child(
                // Right: Scope lock, Blueprint button & Canvas jump
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .id("floorplan-scope-chip")
                            .font_family(mono(cx))
                            .text_size(px(t::TEXT_META))
                            .px(px(6.))
                            .py(px(1.5))
                            .rounded(px(t::RADIUS_ROW))
                            .bg(ca(tk.bg_panel, 0.3))
                            .border_1()
                            .border_color(line(cx, t::LINE_SUBTLE))
                            .text_color(c(if scope_locked {
                                tk.attention
                            } else {
                                tk.text_muted
                            }))
                            .tooltip(ui::tip("APFS copy-on-write scope lock & sandboxing"))
                            .child(SharedString::from(scope_text)),
                    )
                    .child(
                        div()
                            .id("floorplan-metrics-chip")
                            .font_family(mono(cx))
                            .text_size(px(t::TEXT_META))
                            .text_color(c(tk.text_faint))
                            .tooltip(ui::tip("Workspace architectural components and drift"))
                            .child(SharedString::from(metrics_text)),
                    )
                    .child(
                        ui::button("open-blueprint-btn", "⊞ Blueprint", Kind::Ghost, cx)
                            .h(px(22.))
                            .px(px(7.))
                            .text_size(px(t::TEXT_META))
                            .tooltip(ui::tip("Open interactive architectural blueprint in browser"))
                            .on_click(cx.listener(move |_, _, _, _| {
                                open_blueprint(&cwd);
                            })),
                    )
                    .child(
                        ui::button("open-canvas-btn", "✦ Milestones", Kind::Ghost, cx)
                            .h(px(22.))
                            .px(px(7.))
                            .text_size(px(t::TEXT_META))
                            .tooltip(ui::tip("Open interactive Milestone Scrubber in Canvas (⌘⌥6)"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.switch_dock(DockTab::Artifacts, cx);
                            })),
                    )
                    .child(
                        ui::button("refresh-fp-btn", "⟳", Kind::Ghost, cx)
                            .h(px(22.))
                            .px(px(6.))
                            .text_size(px(t::TEXT_META))
                            .tooltip(ui::tip("Refresh floorplan & active scope"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load_floorplan(cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_roles_have_correct_names_and_descriptions() {
        assert_eq!(AgentRole::Researcher.name(), "Researcher");
        assert_eq!(AgentRole::Architect.name(), "Architect");
        assert_eq!(AgentRole::Implementer.name(), "Implementer");
        assert_eq!(AgentRole::Auditor.name(), "Auditor");

        assert!(AgentRole::Researcher.description().contains("AST"));
        assert!(AgentRole::Architect.description().contains("DAG"));
        assert!(AgentRole::Implementer.description().contains("APFS"));
        assert!(AgentRole::Auditor.description().contains("Iron Gates"));
    }

    #[test]
    fn floorplan_summary_default() {
        let fp = FloorplanSummary::default();
        assert_eq!(fp.total_components, 12);
        assert_eq!(fp.modified_components, 0);
        assert!(!fp.loading);
    }
}
