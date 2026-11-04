//! The Living Artifact Canvas in the dock (Canvas tab): persistent plan, specification,
//! and architectural document viewer with real-time milestone progress tracking and
//! one-click agent steering.

use std::path::{Path, PathBuf};

use cordata_theme::tokens as t;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, div, px, relative,
};
use gpui_component::text::{TextView, TextViewStyle};

use super::{DockTab, Workspace};
use crate::theme::{c, ca, line, mono, tk};
use crate::ui::{self, Kind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactTask {
    pub text: String,
    pub completed: bool,
    pub in_progress: bool,
}

#[derive(Default)]
pub struct ArtifactsState {
    pub detected: Vec<PathBuf>,
    pub selected_idx: usize,
    pub content: String,
    pub tasks: Vec<ArtifactTask>,
    pub active_path: Option<PathBuf>,
}

impl ArtifactsState {
    pub fn total_tasks(&self) -> usize {
        self.tasks.len()
    }

    pub fn completed_tasks(&self) -> usize {
        self.tasks.iter().filter(|t| t.completed).count()
    }

    pub fn progress_pct(&self) -> u32 {
        if self.tasks.is_empty() {
            0
        } else {
            ((self.completed_tasks() as f32 / self.tasks.len() as f32) * 100.) as u32
        }
    }

    pub fn parse_tasks(text: &str) -> Vec<ArtifactTask> {
        let mut tasks = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("- [x] ").or_else(|| trimmed.strip_prefix("- [X] ")).or_else(|| trimmed.strip_prefix("* [x] ")).or_else(|| trimmed.strip_prefix("* [X] ")) {
                tasks.push(ArtifactTask {
                    text: rest.trim().to_string(),
                    completed: true,
                    in_progress: false,
                });
            } else if let Some(rest) = trimmed.strip_prefix("- [/] ").or_else(|| trimmed.strip_prefix("- [●] ")).or_else(|| trimmed.strip_prefix("* [/] ")) {
                tasks.push(ArtifactTask {
                    text: rest.trim().to_string(),
                    completed: false,
                    in_progress: true,
                });
            } else if let Some(rest) = trimmed.strip_prefix("- [ ] ").or_else(|| trimmed.strip_prefix("* [ ] ")) {
                tasks.push(ArtifactTask {
                    text: rest.trim().to_string(),
                    completed: false,
                    in_progress: false,
                });
            }
        }
        tasks
    }
}

pub fn scan_artifacts(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let candidates = [
        "PLAN.md",
        "ROADMAP.md",
        "ARCHITECTURE.md",
        "SPEC.md",
        "TASKS.md",
        "AGENTS.md",
    ];
    for name in candidates {
        let path = root.join(name);
        if path.is_file() {
            files.push(path);
        }
    }

    let art_dir = root.join(".cordata/artifacts");
    if art_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&art_dir) {
            let mut sub_files = Vec::new();
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() && p.extension().is_some_and(|e| e == "md") {
                    sub_files.push(p);
                }
            }
            sub_files.sort();
            files.extend(sub_files);
        }
    }

    files
}

impl Workspace {
    pub(super) fn load_artifacts(&mut self, cx: &mut Context<Self>) {
        let root = self.root();
        let detected = scan_artifacts(&root);
        self.artifacts.detected = detected;

        if !self.artifacts.detected.is_empty() {
            if self.artifacts.selected_idx >= self.artifacts.detected.len() {
                self.artifacts.selected_idx = 0;
            }
            let active = self.artifacts.detected[self.artifacts.selected_idx].clone();
            if let Ok(content) = std::fs::read_to_string(&active) {
                self.artifacts.tasks = ArtifactsState::parse_tasks(&content);
                self.artifacts.content = content;
                self.artifacts.active_path = Some(active);
            }
        } else {
            self.artifacts.content.clear();
            self.artifacts.tasks.clear();
            self.artifacts.active_path = None;
        }
        cx.notify();
    }

    pub(super) fn artifacts_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let artifacts = &self.artifacts;

        if artifacts.detected.is_empty() {
            return self.artifacts_empty_state(cx);
        }

        let total = artifacts.total_tasks();
        let completed = artifacts.completed_tasks();
        let pct = artifacts.progress_pct();

        // Artifact selector pills
        let selector_pills: Vec<AnyElement> = artifacts
            .detected
            .iter()
            .enumerate()
            .map(|(idx, path)| {
                let selected = idx == artifacts.selected_idx;
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("document");
                let label = if selected {
                    format!("● {}", file_name)
                } else {
                    file_name.to_string()
                };
                div()
                    .id(("artifact-tab", idx))
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .text_size(px(t::TEXT_SMALL))
                    .font_weight(if selected { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                    .bg(if selected { ca(tk.amber, 0.15) } else { ca(tk.bg_panel, 0.5) })
                    .text_color(c(if selected { tk.amber_soft } else { tk.text_muted }))
                    .hover(move |s| s.bg(ca(tk.amber, 0.12)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.artifacts.selected_idx = idx;
                        this.load_artifacts(cx);
                    }))
                    .child(SharedString::from(label))
                    .into_any_element()
            })
            .collect();

        // Header controls
        let active_name = artifacts
            .active_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("PLAN.md");

        let active_name_for_composer = active_name.to_string();

        let header = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .pb(px(10.))
            .border_b_1()
            .border_color(line(cx, t::LINE_DEFAULT))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().text_color(c(tk.amber)).font_weight(FontWeight::BOLD).child("✦"))
                            .child(
                                div()
                                    .text_size(px(t::TEXT_BODY))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(c(tk.text_primary))
                                    .child("Living Artifact Canvas"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                ui::button("refresh-artifact", "⟳", Kind::Ghost, cx)
                                    .tooltip(ui::tip("Reload artifact from disk"))
                                    .on_click(cx.listener(|this, _, _, cx| this.load_artifacts(cx))),
                            )
                            .child(
                                ui::button("edit-artifact", "✎ Editor", Kind::Ghost, cx)
                                    .tooltip(ui::tip("Open in Editor tab (⌘⌥3)"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.switch_dock(DockTab::Editor, cx);
                                    })),
                            )
                            .child(
                                ui::button("ask-artifact", "💬 Steer Agent", Kind::Ghost, cx)
                                    .tooltip(ui::tip("Discuss or update this plan with the agent"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.apply_starter(&format!("Update {active_name_for_composer}: "), false, window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .children(selector_pills),
            );

        // Progress bar component
        let progress_section = (total > 0).then(|| {
            let pct_f32 = (completed as f32 / total as f32).clamp(0., 1.);
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .py(px(8.))
                .px(px(10.))
                .bg(ca(tk.bg_panel, 0.4))
                .rounded(px(t::RADIUS_ROW))
                .border_1()
                .border_color(line(cx, t::LINE_DEFAULT))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(t::TEXT_SMALL))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(c(tk.text_primary))
                                .child(format!("Milestones & Tasks: {completed} of {total} Complete")),
                        )
                        .child(
                            div()
                                .font_family(mono(cx))
                                .text_size(px(t::TEXT_META))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(c(tk.amber_soft))
                                .child(format!("{pct}%")),
                        ),
                )
                .child(
                    div()
                        .relative()
                        .w_full()
                        .h(px(4.))
                        .rounded_full()
                        .bg(ca(tk.ink, 0.15))
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .h_full()
                                .w(relative(pct_f32))
                                .rounded_full()
                                .bg(c(tk.amber)),
                        ),
                )
        });

        // Content reader
        let content_view = div()
            .id("artifact-reader")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                TextView::markdown("artifact-doc", SharedString::from(artifacts.content.clone()))
                    .selectable(true)
                    .style(TextViewStyle::default().inline_code(gpui::HighlightStyle {
                        color: Some(c(tk.amber_soft)),
                        background_color: Some(ca(tk.amber, if tk.is_dark { 0.16 } else { 0.12 })),
                        font_weight: Some(FontWeight::MEDIUM),
                        ..Default::default()
                    })),
            );

        div()
            .flex()
            .flex_col()
            .size_full()
            .gap(px(10.))
            .child(header)
            .children(progress_section)
            .child(content_view)
            .into_any_element()
    }

    fn artifacts_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .gap(px(12.))
            .p(px(24.))
            .child(div().text_size(px(28.)).text_color(c(tk.amber)).child("✦"))
            .child(
                div()
                    .text_size(px(t::TEXT_BODY))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(c(tk.text_primary))
                    .child("Living Artifact Canvas"),
            )
            .child(
                div()
                    .max_w(px(320.))
                    .text_size(px(t::TEXT_SMALL))
                    .text_color(c(tk.text_muted))
                    .text_center()
                    .child("Plans, architectural specs, and milestone checklists stay persistent and interactive here without scrolling away in chat."),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .mt(px(8.))
                    .child(
                        ui::button("create-plan", "+ Create PLAN.md", Kind::Primary, cx)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let plan_path = this.root().join("PLAN.md");
                                if !plan_path.exists() {
                                    let template = "# Implementation Plan\n\n## Tasks\n- [ ] 1. Architecture Alignment & Schema\n- [ ] 2. Core Implementation\n- [ ] 3. Hermetic Tests & Verification\n";
                                    let _ = std::fs::write(&plan_path, template);
                                }
                                this.load_artifacts(cx);
                            })),
                    )
                    .child(
                        ui::button("create-spec", "+ Create ARCHITECTURE.md", Kind::Ghost, cx)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let spec_path = this.root().join("ARCHITECTURE.md");
                                if !spec_path.exists() {
                                    let template = "# Architecture Specification\n\n## Overview\nSystem components and boundaries.\n\n## Data Flow\n- Pipeline Stages\n- Invariants\n";
                                    let _ = std::fs::write(&spec_path, template);
                                }
                                this.load_artifacts(cx);
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
    fn parse_tasks_counts_completed_in_progress_and_pending() {
        let md = r#"
# Test Plan
- [x] Task 1 done
- [X] Task 2 also done
- [/] Task 3 in progress
- [ ] Task 4 pending
* [x] Task 5 bullet done
* [ ] Task 6 bullet pending
        "#;
        let tasks = ArtifactsState::parse_tasks(md);
        assert_eq!(tasks.len(), 6);
        assert_eq!(tasks[0].completed, true);
        assert_eq!(tasks[1].completed, true);
        assert_eq!(tasks[2].in_progress, true);
        assert_eq!(tasks[3].completed, false);
        assert_eq!(tasks[4].completed, true);
        assert_eq!(tasks[5].completed, false);

        let mut state = ArtifactsState::default();
        state.tasks = tasks;
        assert_eq!(state.total_tasks(), 6);
        assert_eq!(state.completed_tasks(), 3);
        assert_eq!(state.progress_pct(), 50);
    }
}
