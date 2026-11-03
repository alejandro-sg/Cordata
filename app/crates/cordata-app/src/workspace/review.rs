//! The diff review pane in the dock (docs/app.md §3 D4): per-hunk Keep and Revert buttons.
//!
//! Working tree changes against the index are read through [`cordata_view::review::working_changes`] and
//! [`cordata_view::review::untracked`]. Per-hunk actions stage ("Keep") or unstage/revert ("Revert") individual
//! hunks without disturbing other changes in the same file.

use cordata_theme::tokens as t;
use cordata_view::review::{Action, FileDiff, Status, apply, apply_file, untracked, working_changes};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, Window, div, px,
};

use super::Workspace;
use crate::theme::{amber, c, ca, line, mono, tk};
use crate::ui;

#[derive(Default)]
pub struct Review {
    pub files: Vec<FileDiff>,
    pub untracked: Vec<String>,
    pub loading: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
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
        cx.spawn(async move |this, cx| {
            let (result, untracked_files) = cx.background_executor().spawn(async move {
                let changes = working_changes(&root);
                let untracked = untracked(&root);
                (changes, untracked)
            }).await;
            let _ = this.update(cx, |this, cx| {
                this.review.loading = false;
                match result {
                    Ok(files) => {
                        this.review.files = files;
                        this.review.error = None;
                    }
                    Err(err) => {
                        this.review.files = Vec::new();
                        this.review.error = Some(err);
                    }
                }
                this.review.untracked = untracked_files;
                cx.notify();
            });
        })
        .detach();
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
            .files
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
                let file_path = file.path.clone();
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
                                    .child(SharedString::from(hunk.header.clone()))
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

                        let lines_rendered: Vec<AnyElement> = hunk
                            .lines
                            .iter()
                            .map(|line| {
                                let (prefix, rest) = line.split_at(line.chars().next().map_or(0, char::len_utf8));
                                let (bg, ink) = match prefix {
                                    "+" => (Some(amber(cx, t::DIFF_ADD_FILL)), tk.text_body),
                                    "-" => (Some(ca(tk.danger, t::DIFF_DEL_FILL)), tk.text_muted),
                                    _ => (None, tk.text_muted),
                                };
                                let masked = format!("{prefix}{}", shown(rest));
                                div()
                                    .px(px(8.))
                                    .py(px(1.))
                                    .font_family(mono(cx))
                                    .text_size(px(t::TEXT_SMALL))
                                    .line_height(gpui::relative(1.5))
                                    .text_color(c(ink))
                                    .when_some(bg, |el, bg| el.bg(bg))
                                    .child(SharedString::from(masked))
                                    .into_any_element()
                            })
                            .collect();

                        div()
                            .flex()
                            .flex_col()
                            .mb(px(6.))
                            .rounded(px(t::RADIUS_CONTROL))
                            .border_1()
                            .border_color(line(cx, t::LINE_SUBTLE))
                            .overflow_hidden()
                            .child(hunk_header)
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
                                            .child(SharedString::from(file.path.clone())),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(t::TEXT_META))
                                            .font_family(mono(cx))
                                            .text_color(c(tk.text_faint))
                                            .child(format!("+{} -{}", file.additions(), file.deletions())),
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
