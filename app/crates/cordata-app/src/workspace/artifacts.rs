//! The Living Artifact Canvas in the dock (Canvas tab): persistent plan, specification,
//! and architectural document viewer with an interactive Milestone Scrubber, real-time
//! progress tracking, and one-click agent steering with human guardrails.

use std::path::{Path, PathBuf};

use cordata_theme::tokens as t;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, div, px,
};
use gpui_component::text::{TextView, TextViewStyle};

use super::{DockTab, Workspace};
use crate::theme::{c, ca, line, mono, tk};
use crate::ui::{self, Kind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MilestoneFilter {
    #[default]
    All,
    InProgress,
    Pending,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactTask {
    pub line_idx: usize,
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
    pub filter: MilestoneFilter,
    /// Why the last task toggle was refused (the file changed under it, or could not be written).
    pub notice: Option<String>,
}

impl ArtifactsState {
    pub fn total_tasks(&self) -> usize {
        self.tasks.len()
    }

    pub fn completed_tasks(&self) -> usize {
        self.tasks.iter().filter(|t| t.completed).count()
    }

    pub fn in_progress_tasks(&self) -> usize {
        self.tasks.iter().filter(|t| t.in_progress).count()
    }

    pub fn pending_tasks(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| !t.completed && !t.in_progress)
            .count()
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
        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed
                .strip_prefix("- [x] ")
                .or_else(|| trimmed.strip_prefix("- [X] "))
                .or_else(|| trimmed.strip_prefix("* [x] "))
                .or_else(|| trimmed.strip_prefix("* [X] "))
            {
                tasks.push(ArtifactTask {
                    line_idx,
                    text: rest.trim().to_string(),
                    completed: true,
                    in_progress: false,
                });
            } else if let Some(rest) = trimmed
                .strip_prefix("- [/] ")
                .or_else(|| trimmed.strip_prefix("- [●] "))
                .or_else(|| trimmed.strip_prefix("* [/] "))
                .or_else(|| trimmed.strip_prefix("* [●] "))
            {
                tasks.push(ArtifactTask {
                    line_idx,
                    text: rest.trim().to_string(),
                    completed: false,
                    in_progress: true,
                });
            } else if let Some(rest) = trimmed
                .strip_prefix("- [ ] ")
                .or_else(|| trimmed.strip_prefix("* [ ] "))
            {
                tasks.push(ArtifactTask {
                    line_idx,
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
    ];
    for name in candidates {
        let path = root.join(name);
        if path.is_file() {
            files.push(path);
        }
    }

    let art_dir = root.join(".cordata/artifacts");
    if art_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(&art_dir)
    {
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

    files
}

/// The line `task` names, cycled to its next state, written back atomically; refused if the file changed under it.
pub fn cycle_task_on_disk(path: &Path, task: &ArtifactTask) -> Result<(), String> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("the plan").to_owned();
    let content = std::fs::read_to_string(path).map_err(|e| format!("{name}: {e}"))?;
    let lines: Vec<&str> = content.lines().collect();
    let current = lines.get(task.line_idx).copied().unwrap_or_default();
    let still = ArtifactsState::parse_tasks(current);
    if still.first().is_none_or(|t| t.text != task.text || t.completed != task.completed || t.in_progress != task.in_progress) {
        return Err(format!("{name} changed since it was shown; reloaded it — try again"));
    }
    let indent = &current[..current.len() - current.trim_start().len()];
    let bullet = if current.trim_start().starts_with('*') { '*' } else { '-' };
    let mark = match (task.completed, task.in_progress) {
        (false, false) => "/",
        (false, true) => "x",
        (true, _) => " ",
    };
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    out[task.line_idx] = format!("{indent}{bullet} [{mark}] {}", task.text);
    let mut updated = out.join("\n");
    if content.ends_with('\n') {
        updated.push('\n');
    }
    let tmp = path.with_extension("md.cordata-tmp");
    std::fs::write(&tmp, &updated).map_err(|e| format!("{name}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{name}: {e}")
    })
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

    /// Cycles one task on disk: `[ ]` → `[/]` → `[x]` → `[ ]`. The file is read again first, and the line must still
    /// be that task: an agent may have edited the plan since it was shown, and a stale copy must never be written back
    /// over its work. The write goes through a temporary file and a rename, so a crash leaves the old plan whole.
    pub fn toggle_artifact_task(&mut self, task_idx: usize, cx: &mut Context<Self>) {
        let Some(path) = self.artifacts.active_path.clone() else {
            return;
        };
        let Some(task) = self.artifacts.tasks.get(task_idx).cloned() else {
            return;
        };
        self.artifacts.notice = cycle_task_on_disk(&path, &task).err();
        self.load_artifacts(cx);
    }

    pub(super) fn artifacts_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let artifacts = &self.artifacts;

        if artifacts.detected.is_empty() {
            return self.artifacts_empty_state(cx);
        }

        let total = artifacts.total_tasks();
        let completed = artifacts.completed_tasks();
        let in_progress = artifacts.in_progress_tasks();
        let pending = artifacts.pending_tasks();
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
                    .font_weight(if selected {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .bg(if selected {
                        ca(tk.amber, 0.15)
                    } else {
                        ca(tk.bg_panel, 0.5)
                    })
                    .text_color(c(if selected {
                        tk.amber_soft
                    } else {
                        tk.text_muted
                    }))
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
            .pb(px(8.))
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
                            .child(
                                div()
                                    .text_size(px(t::TEXT_UI))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(c(tk.text_primary))
                                    .child("Plans"),
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
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load_artifacts(cx)
                                    })),
                            )
                            .child(
                                ui::button("edit-artifact", "✎ Editor", Kind::Ghost, cx)
                                    .tooltip(ui::tip("Open in Editor tab (⌘⌥3)"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.switch_dock(DockTab::Editor, cx);
                                    })),
                            )
                            .child(
                                ui::button("ask-artifact", "Steer plan", Kind::Ghost, cx)
                                    .tooltip(ui::tip("Discuss or update this plan with the agent"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.apply_starter(
                                            &format!("Update {active_name_for_composer}: "),
                                            false,
                                            window,
                                            cx,
                                        );
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
            )
            .children(artifacts.notice.clone().map(|why| div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.attention)).child(SharedString::from(why))));

        // Milestone Scrubber pipeline nodes (connected DAG strip)
        let scrubber_strip = (total > 0).then(|| {
            let mut flow_items: Vec<AnyElement> = Vec::new();
            for (t_idx, task) in artifacts.tasks.iter().enumerate() {
                if t_idx > 0 {
                    flow_items.push(
                        div()
                            .font_family(mono(cx))
                            .text_size(px(t::TEXT_META))
                            .text_color(c(tk.text_faint))
                            .child("➔")
                            .into_any_element(),
                    );
                }

                let is_done = task.completed;
                let is_running = task.in_progress;
                let status_icon = if is_done {
                    "✓"
                } else if is_running {
                    "●"
                } else {
                    "○"
                };

                let badge_bg = if is_done {
                    ca(tk.amber, 0.15)
                } else if is_running {
                    ca(tk.amber, 0.28)
                } else {
                    ca(tk.bg_panel, 0.4)
                };

                let badge_fg = if is_done || is_running {
                    c(tk.amber_soft)
                } else {
                    c(tk.text_muted)
                };

                let border_c = if is_running {
                    ca(tk.amber, 0.5)
                } else {
                    line(cx, t::LINE_SUBTLE)
                };

                let char_count = task.text.chars().count();
                let short_label = if char_count > 18 {
                    let mut s: String = task.text.chars().take(16).collect();
                    s.push('…');
                    s
                } else {
                    task.text.clone()
                };

                flow_items.push(
                    div()
                        .id(("scrubber-node", t_idx))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .px(px(6.))
                        .py(px(2.))
                        .rounded(px(t::RADIUS_ROW))
                        .bg(badge_bg)
                        .border_1()
                        .border_color(border_c)
                        .cursor_pointer()
                        .tooltip(ui::tip(format!(
                            "Milestone {}: {} (click to cycle state)",
                            t_idx + 1,
                            task.text
                        )))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_artifact_task(t_idx, cx);
                        }))
                        .child(
                            div()
                                .font_family(mono(cx))
                                .text_size(px(t::TEXT_META))
                                .font_weight(FontWeight::BOLD)
                                .text_color(badge_fg)
                                .child(status_icon),
                        )
                        .child(
                            div()
                                .text_size(px(t::TEXT_META))
                                .font_weight(if is_running {
                                    FontWeight::MEDIUM
                                } else {
                                    FontWeight::NORMAL
                                })
                                .text_color(if is_done {
                                    c(tk.text_muted)
                                } else {
                                    c(tk.text_primary)
                                })
                                .child(SharedString::from(format!(
                                    "{}. {}",
                                    t_idx + 1,
                                    short_label
                                ))),
                        )
                        .into_any_element(),
                );
            }

            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .py(px(6.))
                .px(px(8.))
                .bg(ca(tk.bg_panel, 0.3))
                .rounded(px(t::RADIUS_ROW))
                .border_1()
                .border_color(line(cx, t::LINE_SUBTLE))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(t::TEXT_META))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(c(tk.text_secondary))
                                .child("PIPELINE SCRUBBER"),
                        )
                        .child(
                            div()
                                .font_family(mono(cx))
                                .text_size(px(t::TEXT_META))
                                .text_color(c(tk.amber_soft))
                                .child(format!("{pct}% complete")),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(6.))
                        .children(flow_items),
                )
        });

        // Filter pills for the task scrubber
        let current_filter = artifacts.filter;
        let filters = [
            (MilestoneFilter::All, format!("All ({total})")),
            (MilestoneFilter::InProgress, format!("Active ({in_progress})")),
            (MilestoneFilter::Pending, format!("Pending ({pending})")),
            (MilestoneFilter::Completed, format!("Done ({completed})")),
        ];

        let filter_pills: Vec<AnyElement> = filters
            .into_iter()
            .enumerate()
            .map(|(ix, (filt, label))| {
                let is_sel = current_filter == filt;
                div()
                    .id(("milestone-filter-pill", ix))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(t::RADIUS_ROW))
                    .cursor_pointer()
                    .text_size(px(t::TEXT_META))
                    .font_weight(if is_sel {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .bg(if is_sel {
                        ca(tk.amber, 0.2)
                    } else {
                        ca(tk.bg_panel, 0.3)
                    })
                    .text_color(c(if is_sel {
                        tk.amber_soft
                    } else {
                        tk.text_muted
                    }))
                    .hover(move |s| s.bg(ca(tk.amber, 0.12)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.artifacts.filter = filt;
                        cx.notify();
                    }))
                    .child(SharedString::from(label))
                    .into_any_element()
            })
            .collect();

        // Interactive Milestone cards
        let task_cards: Vec<AnyElement> = artifacts
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| match artifacts.filter {
                MilestoneFilter::All => true,
                MilestoneFilter::InProgress => t.in_progress,
                MilestoneFilter::Pending => !t.completed && !t.in_progress,
                MilestoneFilter::Completed => t.completed,
            })
            .map(|(t_idx, task)| {
                let task_text = task.text.clone();
                let is_done = task.completed;
                let is_running = task.in_progress;

                let check_icon = if is_done {
                    "✓"
                } else if is_running {
                    "●"
                } else {
                    " "
                };

                div()
                    .id(("milestone-card", t_idx))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(px(t::RADIUS_ROW))
                    .bg(if is_running {
                        ca(tk.amber, 0.08)
                    } else {
                        ca(tk.bg_panel, 0.25)
                    })
                    .border_1()
                    .border_color(if is_running {
                        ca(tk.amber, 0.3)
                    } else {
                        line(cx, t::LINE_SUBTLE)
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .flex_1()
                            .min_w_0()
                            .child(
                                // Checkbox toggle button
                                div()
                                    .id(("task-check", t_idx))
                                    .w(px(18.))
                                    .h(px(18.))
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(if is_done || is_running {
                                        c(tk.amber)
                                    } else {
                                        line(cx, t::LINE_DEFAULT)
                                    })
                                    .bg(if is_done {
                                        ca(tk.amber, 0.2)
                                    } else if is_running {
                                        ca(tk.amber, 0.35)
                                    } else {
                                        ca(tk.bg_panel, 0.5)
                                    })
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .tooltip(ui::tip("Click to cycle: Pending → In Progress → Done"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_artifact_task(t_idx, cx);
                                    }))
                                    .child(
                                        div()
                                            .font_family(mono(cx))
                                            .text_size(px(t::TEXT_META))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(c(tk.amber_soft))
                                            .child(check_icon),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_size(px(t::TEXT_SMALL))
                                    .text_color(if is_done {
                                        c(tk.text_muted)
                                    } else {
                                        c(tk.text_primary)
                                    })
                                    .child(SharedString::from(format!("{}. {}", t_idx + 1, task.text))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .child(
                                ui::button(("focus-task", t_idx), "▶ Focus", Kind::Ghost, cx)
                                    .h(px(20.))
                                    .px(px(6.))
                                    .text_size(px(t::TEXT_META))
                                    .tooltip(ui::tip(
                                        "Prompt agent to focus on and implement this milestone",
                                    ))
                                    .on_click(cx.listener({
                                        let text = task_text.clone();
                                        move |this, _, window, cx| {
                                            this.apply_starter(
                                                &format!(
                                                    "Focus on milestone: \"{}\". Implement and verify.\n",
                                                    text
                                                ),
                                                false,
                                                window,
                                                cx,
                                            );
                                        }
                                    })),
                            )
                            .child(
                                ui::button(("guard-task", t_idx), "Guardrail", Kind::Ghost, cx)
                                    .h(px(20.))
                                    .px(px(6.))
                                    .text_size(px(t::TEXT_META))
                                    .tooltip(ui::tip(
                                        "Inject a human invariant/guardrail constraint for this milestone",
                                    ))
                                    .on_click(cx.listener({
                                        let text = task_text.clone();
                                        move |this, _, window, cx| {
                                            this.apply_starter(
                                                &format!(
                                                    "Guardrail for milestone \"{}\": ",
                                                    text
                                                ),
                                                false,
                                                window,
                                                cx,
                                            );
                                        }
                                    })),
                            ),
                    )
                    .into_any_element()
            })
            .collect();

        // Tasks interactive drawer section
        let tasks_section = (total > 0).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .py(px(6.))
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
                                .child(
                                    div()
                                        .text_size(px(t::TEXT_META))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(c(tk.text_secondary))
                                        .child("MILESTONE CONTROLS"),
                                )
                                .children(filter_pills),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .children(task_cards),
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
            .gap(px(8.))
            .child(header)
            .children(scrubber_strip)
            .children(tasks_section)
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
        assert!(tasks[0].completed);
        assert!(tasks[1].completed);
        assert!(tasks[2].in_progress);
        assert!(!tasks[3].completed);
        assert!(tasks[4].completed);
        assert!(!tasks[5].completed);

        let state = ArtifactsState { tasks, ..Default::default() };
        assert_eq!(state.total_tasks(), 6);
        assert_eq!(state.completed_tasks(), 3);
        assert_eq!(state.in_progress_tasks(), 1);
        assert_eq!(state.pending_tasks(), 2);
        assert_eq!(state.progress_pct(), 50);
    }

    #[test]
    fn task_toggling_preserves_indentation_and_bullets() {
        let text = "  - [ ] Indented task\n* [/] Star bullet in progress\n* [●] Star bullet in progress dot\n- [x] Done task";
        let tasks = ArtifactsState::parse_tasks(text);
        assert_eq!(tasks.len(), 4);
        assert_eq!(tasks[0].line_idx, 0);
        assert_eq!(tasks[1].line_idx, 1);
        assert_eq!(tasks[2].line_idx, 2);
        assert!(tasks[2].in_progress);
        assert_eq!(tasks[3].line_idx, 3);
    }

    #[test]
    fn unicode_tasks_do_not_panic_on_short_label() {
        let text = "- [ ] Implement \u{1F6E1}\u{1F6E1} invariant checks for multi-agent floorplan";
        let tasks = ArtifactsState::parse_tasks(text);
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        let char_count = task.text.chars().count();
        let short_label = if char_count > 18 {
            let mut s: String = task.text.chars().take(16).collect();
            s.push('…');
            s
        } else {
            task.text.clone()
        };
        assert!(short_label.ends_with('…'));
        assert_eq!(short_label.chars().count(), 17);
    }

    #[test]
    fn a_task_cycles_on_disk_and_a_stale_one_is_refused() {
        let dir = crate::test_dir("plan-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("PLAN.md");
        std::fs::write(&path, "# Plan\n  - [ ] first\n- [x] second\n").unwrap();
        let tasks = ArtifactsState::parse_tasks(&std::fs::read_to_string(&path).unwrap());
        cycle_task_on_disk(&path, &tasks[0]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Plan\n  - [/] first\n- [x] second\n");
        // The agent rewrote the line since it was shown: the old copy is not written back.
        std::fs::write(&path, "# Plan\n  - [ ] renamed\n- [x] second\n").unwrap();
        assert!(cycle_task_on_disk(&path, &tasks[0]).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Plan\n  - [ ] renamed\n- [x] second\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
