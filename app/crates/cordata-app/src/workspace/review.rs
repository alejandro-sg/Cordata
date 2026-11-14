//! The diff review pane in the dock (docs/app.md §3 D4): per-hunk Keep and Revert buttons.
//!
//! Working tree changes against the index are read through [`cordata_view::review::working_changes`] and
//! [`cordata_view::review::untracked`]. Per-hunk actions stage ("Keep") or unstage/revert ("Revert") individual
//! hunks without disturbing other changes in the same file.

use cordata_theme::tokens as t;
use cordata_view::annotate::{self, Annotations, DIRECTION_LIMIT};
use cordata_view::review::{Action, FileDiff, LineKind, Status, apply, apply_file, display_files, untracked, working_changes};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, Window, div, px,
};

use super::Workspace;
use crate::theme::{amber, c, ca, line, mono, tk};
use crate::ui;

#[derive(Clone)]
pub struct SharedRun {
    pub kind: LineKind,
    pub text: SharedString,
}

#[derive(Clone)]
pub struct SharedHunk {
    pub header: SharedString,
    pub new_start: usize,
    pub runs: Vec<SharedRun>,
    pub preview_runs: Vec<SharedRun>,
    pub hidden_lines: usize,
}

#[derive(Clone)]
pub struct SharedFile {
    pub raw_path: String,
    pub path: SharedString,
    pub status: Status,
    pub binary: bool,
    pub additions: usize,
    pub deletions: usize,
    pub sensitive: bool,
    pub hunks: Vec<SharedHunk>,
}

impl SharedFile {
    pub fn from_diffs(files: &[FileDiff]) -> Vec<Self> {
        let to_shared = |runs: Vec<cordata_view::review::DisplayRun>| -> Vec<SharedRun> {
            runs.into_iter().map(|r| SharedRun { kind: r.kind, text: SharedString::from(r.text) }).collect()
        };
        files
            .iter()
            .zip(display_files(files))
            .map(|(raw, disp)| Self {
                raw_path: raw.path.clone(),
                path: SharedString::from(disp.path),
                status: disp.status,
                binary: disp.binary,
                additions: disp.additions,
                deletions: disp.deletions,
                sensitive: disp.sensitive,
                hunks: disp
                    .hunks
                    .into_iter()
                    .map(|h| SharedHunk {
                        header: SharedString::from(h.header),
                        new_start: h.new_start,
                        runs: to_shared(h.runs),
                        preview_runs: to_shared(h.preview_runs),
                        hidden_lines: h.hidden_lines,
                    })
                    .collect(),
            })
            .collect()
    }
}

#[derive(Default)]
pub struct Review {
    pub files: Vec<FileDiff>,
    pub display: Vec<SharedFile>,
    pub untracked: Vec<String>,
    pub loading: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// Line comments waiting to be sent to the run that wrote the diff (`cordata_view::annotate`).
    pub annotations: Annotations,
    /// The hunk (file, hunk) whose lines can be picked for a comment, and the picked body lines (anchor, head).
    pub annotating: Option<(usize, usize)>,
    pub picked: Option<(usize, usize)>,
}

impl Review {
    pub fn hunks(&self) -> usize {
        self.files.iter().map(|f| f.hunks.len()).sum()
    }

    /// Changed files waiting for review (tracked and new).
    pub fn file_count(&self) -> usize {
        self.files.len() + self.untracked.len()
    }
}

impl Workspace {
    pub(super) fn load_review(&mut self, cx: &mut Context<Self>) {
        if self.review.loading {
            return;
        }
        self.review.loading = true;
        let root = self.root();
        let owner = self.cwd.clone();
        cx.spawn(async move |this, cx| {
            let (result, untracked_files) = cx.background_executor().spawn(async move {
                let changes = working_changes(&root).map(|files| {
                    let display = SharedFile::from_diffs(&files);
                    (files, display)
                });
                let untracked = untracked(&root);
                (changes, untracked)
            }).await;
            let _ = this.update(cx, |this, cx| {
                // Started for another folder: the panes have moved on, and its loading flag was reset with them.
                if this.cwd != owner {
                    return;
                }
                this.review.loading = false;
                match result {
                    Ok((files, display)) => {
                        this.review.files = files;
                        this.review.display = display;
                        this.review.error = None;
                    }
                    Err(err) => {
                        this.review.files = Vec::new();
                        this.review.display = Vec::new();
                        this.review.error = Some(err);
                    }
                }
                this.review.untracked = untracked_files;
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens or closes line picking on one hunk.
    pub(super) fn toggle_annotating(&mut self, file_idx: usize, hunk_idx: usize, cx: &mut Context<Self>) {
        self.review.annotating = if self.review.annotating == Some((file_idx, hunk_idx)) { None } else { Some((file_idx, hunk_idx)) };
        self.review.picked = None;
        cx.notify();
    }

    /// A click on body line `line`: starts a selection, or with shift stretches the current one.
    pub(super) fn pick_line(&mut self, line: usize, extend: bool, cx: &mut Context<Self>) {
        self.review.picked = Some(match self.review.picked {
            Some((anchor, _)) if extend => (anchor, line),
            _ => (line, line),
        });
        cx.notify();
    }

    /// Stores the bar's text as a comment on the picked lines.
    pub(super) fn add_annotation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some((file_idx, hunk_idx)), Some((a, b))) = (self.review.annotating, self.review.picked) else {
            self.review.notice = Some("pick the lines first (click a line, shift-click to stretch)".into());
            cx.notify();
            return;
        };
        let Some((file, hunk)) = self.review.files.get(file_idx).and_then(|f| Some((f.path.clone(), f.hunks.get(hunk_idx)?))) else { return };
        let Some((start, end)) = annotate::selection_range(hunk, a, b) else { return };
        let words = self.composer.read(cx).value().to_string();
        if self.review.annotations.add(&file, start, end, &words) {
            self.composer.update(cx, |st, cx| st.set_value("", window, cx));
            self.review.picked = None;
            self.review.notice = Some(format!("comment saved on {file}:{start}-{end} · {} waiting", self.review.annotations.len()));
        } else {
            self.review.notice = Some("write the comment in the bar first".into());
        }
        cx.notify();
    }

    /// The run and step the waiting comments belong to: the newest run with a step whose scope names the commented files.
    pub(super) fn annotation_target(&self) -> Option<(String, String)> {
        let files: Vec<&str> = self.review.annotations.ordered().iter().map(|a| a.file.as_str()).collect();
        self.field_runs.iter().find_map(|run| annotate::target_step(&run.steps, &files).map(|s| (run.run_id.clone(), s.id.clone())))
    }

    /// Sends the waiting comments as one direction and runs the step again; what did not fit goes in the next batch.
    pub(super) fn send_annotations(&mut self, cx: &mut Context<Self>) {
        let Some((run_id, step_id)) = self.annotation_target() else {
            self.review.notice = Some("no run of this project has a step that wrote these files".into());
            cx.notify();
            return;
        };
        let Some(batch) = self.review.annotations.batch(DIRECTION_LIMIT) else { return };
        self.resume_run_with(&run_id, None, Some(batch.text), Some(step_id.clone()), cx);
        self.review.annotations.drop_sent(batch.sent);
        let more = if batch.left > 0 { format!(" · {} more waiting, send again after it runs", batch.left) } else { String::new() };
        self.review.notice = Some(format!("sent {} comment(s) to {run_id} step {step_id}{more}", batch.sent));
        cx.notify();
    }

    pub(super) fn apply_hunk(
        &mut self,
        file_idx: usize,
        hunk_idx: usize,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.review.files.get(file_idx).cloned() else { return };
        let root = self.root();
        match apply(&root, &file, hunk_idx, action) {
            Ok(()) => {
                let verb = match action {
                    Action::Keep => "kept (staged)",
                    Action::Revert => "reverted",
                };
                self.review.notice = Some(format!("{verb} hunk {} of {}", hunk_idx + 1, file.path));
                self.poll_disk(window, cx);
                self.load_review(cx);
            }
            Err(err) => {
                self.review.notice = Some(format!("git apply failed: {err}"));
                cx.notify();
            }
        }
    }

    pub(super) fn apply_file(
        &mut self,
        file_idx: usize,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.review.files.get(file_idx).cloned() else { return };
        let root = self.root();
        match apply_file(&root, &file, action) {
            Ok(()) => {
                let verb = match action {
                    Action::Keep => "kept (staged) all hunks of",
                    Action::Revert => "reverted all hunks of",
                };
                self.review.notice = Some(format!("{verb} {}", file.path));
                self.poll_disk(window, cx);
                self.load_review(cx);
            }
            Err(err) => {
                self.review.notice = Some(format!("git apply failed: {err}"));
                cx.notify();
            }
        }
    }

    pub(super) fn review_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let file_count = self.review.files.len();
        let hunk_count = self.review.hunks();
        let this = cx.entity().downgrade();

        let header_row = div()
            .flex()
            .items_center()
            .justify_between()
            .pb(px(4.))
            .child(
                div()
                    .font_family(mono(cx))
                    .text_size(px(t::TEXT_META))
                    .text_color(c(tk.text_muted))
                    .child(if self.review.loading {
                        "Reading git diff…".to_string()
                    } else if let Some(err) = &self.review.error {
                        err.clone()
                    } else {
                        format!(
                            "{} changed file{} · {} hunk{}",
                            file_count,
                            if file_count == 1 { "" } else { "s" },
                            hunk_count,
                            if hunk_count == 1 { "" } else { "s" }
                        )
                    }),
            )
            .child(
                ui::icon_action("review-refresh", gpui_kit_assets::IconName::RefreshCw, cx)
                    .aria_label("Refresh diff")
                    .tooltip(ui::tip("Refresh diff"))
                    .on_click(cx.listener(|this, _, _, cx| this.load_review(cx))),
            );

        let files_list: Vec<AnyElement> = self
            .review
            .display
            .iter()
            .enumerate()
            .map(|(file_idx, file)| {
                let status_str = match file.status {
                    Status::Added => "A",
                    Status::Deleted => "D",
                    Status::Renamed => "R",
                    Status::Modified => "M",
                };
                let status_color = match file.status {
                    Status::Added => c(tk.amber),
                    Status::Deleted => c(tk.danger),
                    Status::Renamed => c(tk.amber_soft),
                    Status::Modified => c(tk.attention),
                };
                let file_path = file.raw_path.clone();
                let this_click = this.clone();
                let path_for_click = file_path.clone();

                let hunks_rendered: Vec<AnyElement> = file
                    .hunks
                    .iter()
                    .enumerate()
                    .map(|(hunk_idx, hunk)| {
                        let this_hunk_click = this.clone();
                        let path_for_hunk = file_path.clone();
                        let line_no = hunk.new_start.saturating_sub(1) as u32;

                        let hunk_header = div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px(px(8.))
                            .py(px(3.))
                            .bg(line(cx, t::FILL_FAINT))
                            .border_b_1()
                            .border_color(line(cx, t::LINE_SUBTLE))
                            .child(
                                div()
                                    .id(SharedString::from(format!("hunk-hdr-{file_idx}-{hunk_idx}")))
                                    .flex_1()
                                    .cursor_pointer()
                                    .font_family(mono(cx))
                                    .text_size(px(t::TEXT_META))
                                    .text_color(c(tk.text_muted))
                                    .hover(|el| el.text_color(c(tk.text_primary)))
                                    .child(hunk.header.clone())
                                    .on_click(move |_: &ClickEvent, window, cx| {
                                        let _ = this_hunk_click.update(cx, |this, cx| {
                                            this.open_file(&path_for_hunk, Some(line_no), window, cx);
                                        });
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        ui::text_action(SharedString::from(format!("hunk-comment-{file_idx}-{hunk_idx}")), "comment", self.review.annotating == Some((file_idx, hunk_idx)), cx)
                                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_annotating(file_idx, hunk_idx, cx))),
                                    )
                                    .child(
                                        ui::text_action(SharedString::from(format!("hunk-keep-{file_idx}-{hunk_idx}")), "keep", false, cx)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.apply_hunk(file_idx, hunk_idx, Action::Keep, window, cx);
                                            })),
                                    )
                                    .child(
                                        ui::text_action(SharedString::from(format!("hunk-revert-{file_idx}-{hunk_idx}")), "revert", true, cx)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.apply_hunk(file_idx, hunk_idx, Action::Revert, window, cx);
                                            })),
                                    ),
                            );

                        let picking = self.review.annotating == Some((file_idx, hunk_idx));
                        let numbers = self.review.files.get(file_idx).and_then(|f| f.hunks.get(hunk_idx)).map(annotate::line_numbers).unwrap_or_default();
                        let lines_rendered: Vec<AnyElement> = if picking {
                            let raw = self.review.files.get(file_idx).and_then(|f| f.hunks.get(hunk_idx)).map(|h| h.lines.clone()).unwrap_or_default();
                            let (lo, hi) = self.review.picked.map_or((usize::MAX, 0), |(a, b)| (a.min(b), a.max(b)));
                            let saved = self.review.annotations.for_file(&file_path);
                            raw.iter()
                                .enumerate()
                                .map(|(li, text)| {
                                    let (pfx, rest) = text.split_at(text.chars().next().map_or(0, char::len_utf8));
                                    let cleaned = format!("{pfx}{}", cordata_view::redact::scrub(&cordata_view::app::sanitize(rest)).0.replace('\n', " "));
                                    let ink = match pfx { "+" => tk.text_body, _ => tk.text_muted };
                                    let number = numbers.get(li).copied().flatten();
                                    let marked = number.is_some_and(|n| pfx != "-" && saved.iter().any(|a| a.start <= n && n <= a.end));
                                    div()
                                        .id(SharedString::from(format!("diff-line-{file_idx}-{hunk_idx}-{li}")))
                                        .flex()
                                        .gap(px(8.))
                                        .px(px(8.))
                                        .cursor_pointer()
                                        .whitespace_nowrap()
                                        .font_family(mono(cx))
                                        .text_size(px(t::TEXT_SMALL))
                                        .line_height(gpui::relative(1.5))
                                        .text_color(c(ink))
                                        .when(pfx == "+", |el| el.bg(amber(cx, t::DIFF_ADD_FILL)))
                                        .when(pfx == "-", |el| el.bg(ca(tk.danger, t::DIFF_DEL_FILL)))
                                        .when(li >= lo && li <= hi, |el| el.bg(amber(cx, t::AMBER_FILL_FAINT)).border_l_2().border_color(c(tk.amber)))
                                        .child(div().w(px(28.)).flex_shrink_0().text_color(c(if marked { tk.amber } else { tk.text_faint })).child(SharedString::from(number.map_or(String::new(), |n| if marked { format!("●{n}") } else { n.to_string() }))))
                                        .child(SharedString::from(cleaned))
                                        .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| this.pick_line(li, e.modifiers().shift, cx)))
                                        .into_any_element()
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let runs_rendered: Vec<AnyElement> = if picking { Vec::new() } else { hunk
                            .runs
                            .iter()
                            .map(|run| {
                                let (bg, ink) = match run.kind {
                                    LineKind::Add => (Some(amber(cx, t::DIFF_ADD_FILL)), tk.text_body),
                                    LineKind::Del => (Some(ca(tk.danger, t::DIFF_DEL_FILL)), tk.text_muted),
                                    LineKind::Context => (None, tk.text_muted),
                                };
                                div()
                                    .px(px(8.))
                                    .py(px(1.))
                                    .whitespace_nowrap()
                                    .font_family(mono(cx))
                                    .text_size(px(t::TEXT_SMALL))
                                    .line_height(gpui::relative(1.5))
                                    .text_color(c(ink))
                                    .when_some(bg, |el, bg| el.bg(bg))
                                    .child(run.text.clone())
                                    .into_any_element()
                            })
                            .collect() };

                        div()
                            .flex()
                            .flex_col()
                            .mb(px(6.))
                            .rounded(px(t::RADIUS_CONTROL))
                            .border_1()
                            .border_color(line(cx, t::LINE_SUBTLE))
                            .overflow_hidden()
                            .child(hunk_header)
                            .children(runs_rendered)
                            .children(lines_rendered)
                            .into_any_element()
                    })
                    .collect();

                div()
                    .flex()
                    .flex_col()
                    .mb(px(8.))
                    .rounded(px(t::RADIUS_CARD))
                    .border_1()
                    .border_color(line(cx, t::LINE_DEFAULT))
                    .bg(c(tk.bg_window))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px(px(8.))
                            .py(px(5.))
                            .border_b_1()
                            .border_color(line(cx, t::LINE_SUBTLE))
                            .child(
                                div()
                                    .id(SharedString::from(format!("file-row-{file_idx}")))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .cursor_pointer()
                                    .on_click(move |_: &ClickEvent, window, cx| {
                                        let _ = this_click.update(cx, |this, cx| {
                                            this.open_file(&path_for_click, None, window, cx);
                                        });
                                    })
                                    .child(
                                        div()
                                            .w(px(12.))
                                            .font_family(mono(cx))
                                            .text_size(px(t::TEXT_META))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(status_color)
                                            .child(status_str),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(t::TEXT_UI))
                                            .font_family(mono(cx))
                                            .text_color(c(tk.text_primary))
                                            .hover(|el| el.text_color(c(tk.amber)))
                                            .child(file.path.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(t::TEXT_META))
                                            .font_family(mono(cx))
                                            .text_color(c(tk.text_faint))
                                            .child(format!("+{} -{}", file.additions, file.deletions)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        ui::text_action(SharedString::from(format!("file-keep-all-{file_idx}")), "keep all", false, cx)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.apply_file(file_idx, Action::Keep, window, cx);
                                            })),
                                    )
                                    .child(
                                        ui::text_action(SharedString::from(format!("file-revert-all-{file_idx}")), "revert all", true, cx)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.apply_file(file_idx, Action::Revert, window, cx);
                                            })),
                                    ),
                            ),
                    )
                    .when(file.binary, |el| {
                        el.child(
                            div()
                                .p(px(8.))
                                .text_size(px(t::TEXT_META))
                                .text_color(c(tk.text_muted))
                                .child("Binary file changed (not diffed)"),
                        )
                    })
                    .when(!file.binary, |el| el.p(px(6.)).children(hunks_rendered))
                    .into_any_element()
            })
            .collect();

        let untracked_section = (!self.review.untracked.is_empty()).then(|| {
            let items: Vec<AnyElement> = self
                .review
                .untracked
                .iter()
                .enumerate()
                .map(|(u_idx, path)| {
                    let path_clone = path.clone();
                    let this_untracked = this.clone();
                    div()
                        .id(SharedString::from(format!("untracked-row-{u_idx}")))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .py(px(2.))
                        .cursor_pointer()
                        .on_click(move |_: &ClickEvent, window, cx| {
                            let _ = this_untracked.update(cx, |this, cx| {
                                this.open_file(&path_clone, None, window, cx);
                            });
                        })
                        .child(
                            div()
                                .w(px(12.))
                                .font_family(mono(cx))
                                .text_size(px(t::TEXT_META))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(c(tk.amber))
                                .child("?"),
                        )
                        .child(
                            div()
                                .text_size(px(t::TEXT_SMALL))
                                .font_family(mono(cx))
                                .text_color(c(tk.text_secondary))
                                .hover(|el| el.text_color(c(tk.text_primary)))
                                .child(SharedString::from(path.clone())),
                        )
                        .into_any_element()
                })
                .collect();

            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .mt(px(8.))
                .child(ui::quiet_label(format!("untracked ({})", self.review.untracked.len()), cx))
                .children(items)
        });

        let annotation_bar = (self.review.annotating.is_some() || !self.review.annotations.is_empty()).then(|| {
            let waiting = self.review.annotations.ordered();
            let target = self.annotation_target();
            let rows: Vec<AnyElement> = waiting
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(div().flex_1().min_w_0().truncate().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_secondary))
                            .child(SharedString::from(shown(&format!("{}: {}", a.place(), a.comment)))))
                        .child(ui::text_action(SharedString::from(format!("annotation-drop-{i}")), "drop", true, cx)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.review.annotations.remove(i);
                                cx.notify();
                            })))
                        .into_any_element()
                })
                .collect();
            let hint = match (&target, self.review.annotating, self.review.picked) {
                (_, Some(_), None) => "click a line (shift-click to stretch), write the comment in the bar, then add".to_owned(),
                (Some((run, step)), _, _) => format!("goes to {run} · step {step}, which runs again with its checks"),
                (None, _, _) if self.review.annotations.is_empty() => String::new(),
                (None, _, _) => "no run of this project has a step that wrote these files".to_owned(),
            };
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .p(px(6.))
                .rounded(px(t::RADIUS_CONTROL))
                .border_1()
                .border_color(line(cx, t::LINE_SUBTLE))
                .child(ui::quiet_label(format!("comments ({})", waiting.len()), cx))
                .children(rows)
                .child(div().text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child(SharedString::from(hint)))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .when(self.review.picked.is_some(), |el| {
                            el.child(ui::text_action("annotation-add", "add comment from bar", false, cx).on_click(cx.listener(|this, _, window, cx| this.add_annotation(window, cx))))
                        })
                        .when(target.is_some() && !self.review.annotations.is_empty(), |el| {
                            el.child(ui::text_action("annotation-send", "send all and run the step again", false, cx).on_click(cx.listener(|this, _, _, cx| this.send_annotations(cx))))
                        }),
                )
                .into_any_element()
        });

        let empty = if file_count == 0 && self.review.untracked.is_empty() && !self.review.loading && self.review.error.is_none() {
            Some(ui::empty("working tree clean", "No unstaged changes in this project.", cx))
        } else {
            None
        };

        div()
            .id("review-pane")
            .key_context("CordataReview")
            .flex()
            .flex_col()
            .size_full()
            .gap(px(6.))
            .child(header_row)
            .when_some(self.review.notice.clone(), |el, notice| {
                el.child(
                    div()
                        .px(px(6.))
                        .py(px(4.))
                        .rounded(px(t::RADIUS_CONTROL))
                        .bg(amber(cx, t::AMBER_FILL_FAINT))
                        .text_size(px(t::TEXT_SMALL))
                        .text_color(c(tk.amber))
                        .child(SharedString::from(shown(&notice))),
                )
            })
            .when_some(self.review.error.clone(), |el, err| {
                el.child(
                    div()
                        .px(px(6.))
                        .py(px(4.))
                        .rounded(px(t::RADIUS_CONTROL))
                        .bg(ca(tk.danger, 0.08))
                        .text_size(px(t::TEXT_SMALL))
                        .text_color(c(tk.danger))
                        .child(SharedString::from(shown(&err))),
                )
            })
            .when_some(empty, |el, msg| el.child(msg))
            .when_some(annotation_bar, |el, bar| el.child(bar))
            .child(
                div()
                    .id("review-files-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(files_list)
                    .when_some(untracked_section, |el, untracked| el.child(untracked)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_state_counts_hunks_and_defaults_clean() {
        let mut review = Review::default();
        assert_eq!(review.hunks(), 0);
        assert!(review.files.is_empty());
        assert!(review.untracked.is_empty());

        let patch = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn a() {}
+fn b() {}
@@ -10,2 +11,3 @@
 fn c() {}
+fn d() {}
";
        review.files = cordata_view::review::parse(patch);
        assert_eq!(review.files.len(), 1);
        assert_eq!(review.hunks(), 2);
    }
}
