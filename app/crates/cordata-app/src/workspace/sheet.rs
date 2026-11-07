//! Proof-First Change Sheet (`⌘D`) and Forensic Tape Scrubber (turn-level fork & rollback).
//!
//! Replaces chat-first post-run reading with a structured change sheet:
//! 1. Verification & Falsification Banner (check state, removed assertions, ignored tests, sensitive files)
//! 2. Clamped Agent Conclusion in reading serif (expandable)
//! 3. Grouped Hunk Cards with inline `keep` / `revert` per hunk and per file
//! 4. Turn-level Fork & Post-Turn Working-Tree Rollback from the Brief's Tape

use std::path::{Component, Path, PathBuf};

use acp_core::{FileDiff as SessionFileDiff, ToolStatus};
use cordata_theme::tokens as t;
use cordata_view::app::Message;
use cordata_view::desk::{self, CheckState};
use cordata_view::review::{Action, LineKind, Status};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AppContext as _, ClickEvent, Context, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window,
    div, px, relative,
};

use super::Workspace;
use crate::theme::{amber, c, ca, line, mono, tk, voice};
use crate::transcript;
use crate::ui::{self, MarkState};

/// Center column mode: Transcript, Proof-First Change Sheet (`⌘D`), or Multi-Agent Triage Rack (`⌘0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeskMode {
    #[default]
    Transcript,
    ChangeSheet,
    Rack,
}

/// Appends a local validation telemetry event to `~/.cordata/ux-events.jsonl` when running outside tests.
pub(super) fn record_ux_event(kind: &str, detail: serde_json::Value) {
    #[cfg(test)]
    {
        let _ = (kind, detail);
    }
    #[cfg(not(test))]
    {
        use std::io::Write as _;
        let home = super::cordata_home();
        if !home.is_dir() {
            return;
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = serde_json::json!({ "ts": ts, "kind": kind, "detail": detail });
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(home.join("ux-events.jsonl")) {
            let _ = writeln!(file, "{entry}");
        }
    }
}

fn safe_project_target(cwd: &Path, raw: &str) -> Option<PathBuf> {
    let p = Path::new(raw);
    for comp in p.components() {
        match comp {
            Component::ParentDir => return None,
            Component::Normal(s) => {
                let lower = s.to_string_lossy().to_ascii_lowercase();
                if lower == ".git" || lower == ".env" || (lower.starts_with(".env.") && lower != ".env.example") {
                    return None;
                }
            }
            _ => {}
        }
    }
    let joined = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    if std::fs::symlink_metadata(&joined).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
    let canon_cwd = cwd.canonicalize().ok()?;
    let parent = joined.parent()?.canonicalize().ok()?;
    if !parent.starts_with(&canon_cwd) {
        return None;
    }
    if joined.exists() && !joined.canonicalize().ok()?.starts_with(&canon_cwd) {
        return None;
    }
    Some(joined)
}

impl Workspace {
    pub(super) fn toggle_change_sheet(&mut self, cx: &mut Context<Self>) {
        self.desk_mode = match self.desk_mode {
            DeskMode::ChangeSheet => DeskMode::Transcript,
            DeskMode::Transcript | DeskMode::Rack => {
                self.load_review(cx);
                record_ux_event("mode_switch", serde_json::json!({ "mode": "change_sheet" }));
                DeskMode::ChangeSheet
            }
        };
        cx.notify();
    }

    pub(super) fn scope_chip(&self, scope: &[String], candidate: &[String], cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        if !scope.is_empty() {
            let label = format!("scope: {} ×", scope.join(", "));
            div()
                .id("scope-chip")
                .role(gpui::Role::Button)
                .cursor_pointer()
                .text_color(c(tk.amber))
                .hover(|s| s.opacity(0.8))
                .tooltip(ui::tip("Pinned auto-mode scope — click to unlock back to project"))
                .on_click(cx.listener(|this, _, _, cx| this.session.update(cx, |s, cx| s.set_scope(Vec::new(), cx))))
                .child(SharedString::from(label))
                .into_any_element()
        } else if !candidate.is_empty() {
            let tip = format!("Pin auto-mode edits to {}", candidate.join(", "));
            let lock_paths = candidate.to_vec();
            div().flex().items_center().gap(px(5.)).child(div().child("scope: project")).child(
                div()
                    .id("scope-lock")
                    .role(gpui::Role::Button)
                    .cursor_pointer()
                    .text_color(c(tk.text_secondary))
                    .hover(|s| s.text_color(c(tk.amber)))
                    .tooltip(ui::tip(SharedString::from(tip)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        record_ux_event("scope_locked", serde_json::json!({ "prefixes": lock_paths }));
                        let next = lock_paths.clone();
                        this.session.update(cx, |s, cx| s.set_scope(next, cx));
                    }))
                    .child("[lock scope]"),
            ).into_any_element()
        } else {
            div().child("scope: project").into_any_element()
        }
    }

    pub(super) fn tape_scrubber(&self, sel: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tk = tk(cx);
        let snap = std::rc::Rc::clone(&self.desk_cache.snap);
        let summary = snap.turn_summaries.get(sel)?.clone();
        let later_edits = snap.turn_summaries.iter().skip(sel + 1).any(|t| t.files > 0);
        let sep = || div().text_color(c(tk.text_faint)).child("·");
        Some(
            div()
                .id("tape-scrubber")
                .flex().flex_wrap().items_center().gap(px(8.)).pt(px(2.))
                .font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted))
                .child(div().text_color(c(tk.amber)).child(SharedString::from(format!("Turn {}", sel + 1))))
                .child(sep())
                .child(div().child(SharedString::from(format!("{} files +{} −{}", summary.files, summary.added, summary.removed))))
                .child(sep())
                .child(div().child(SharedString::from(format!("{} tools", summary.tools))))
                .child(sep())
                .child(
                    div()
                        .id("fork-turn").role(gpui::Role::Button).cursor_pointer().text_color(c(tk.text_secondary))
                        .hover(|s| s.text_color(c(tk.amber)))
                        .tooltip(ui::tip("Branch a clean thread starting right after this turn"))
                        .on_click(cx.listener(move |this, _, window, cx| this.fork_from_turn(sel, window, cx)))
                        .child("[fork from here]"),
                )
                .when(later_edits, |bar| {
                    bar.child(sep()).child(
                        div()
                            .id("revert-after-turn").role(gpui::Role::Button).cursor_pointer().text_color(c(tk.attention))
                            .hover(|s| s.text_color(c(tk.danger)))
                            .tooltip(ui::tip("Roll back working-tree edits made in turns after this stop"))
                            .on_click(cx.listener(move |this, _, window, cx| { let _ = this.revert_edits_after_turn(sel, window, cx); }))
                            .child(SharedString::from(format!("[revert after turn {}]", sel + 1))),
                    )
                })
                .child(div().flex_1())
                .child(
                    div()
                        .id("close-scrubber").role(gpui::Role::Button).aria_label("Close turn scrubber").cursor_pointer().text_color(c(tk.text_faint))
                        .tooltip(ui::tip("Close turn scrubber"))
                        .hover(|s| s.text_color(c(tk.text_primary)))
                        .on_click(cx.listener(|this, _, _, cx| { this.selected_turn = None; cx.notify(); }))
                        .child("✕"),
                )
                .into_any_element(),
        )
    }

    /// Forks a clean conversation truncated right after `turn_ix` (0-based) without mutating the source thread.
    pub(super) fn fork_from_turn(&mut self, turn_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let keep_prompts = turn_ix + 1;
        let (cwd, saved_id, kept) = {
            let ses = self.session.read(cx);
            let turns = desk::turns(&ses.view.messages);
            let end_msg = turns.get(turn_ix + 1).map_or(ses.view.messages.len(), |t| t.message);
            let kept: Vec<Message> = ses.view.messages[..end_msg.min(ses.view.messages.len())].to_vec();
            (ses.cwd.clone(), ses.saved_id().map(str::to_owned), kept)
        };
        record_ux_event("turn_forked", serde_json::json!({ "turn": keep_prompts }));
        self.selected_turn = None;

        if let Some(id) = saved_id
            && (self.session_exists)(&cwd, &id)
            && let Ok(forked_id) = acp_core::history::fork(&cwd, &id, keep_prompts)
        {
            let entry = crate::threads::Open { id: forked_id, cwd };
            let fresh = Self::resumed(&self.connection, &entry, self.prefs.model.clone(), cx);
            self.adopt(fresh, window, cx);
            return;
        }

        let (conn, model) = (self.connection.clone(), self.prefs.model.clone());
        let fresh = cx.new(|cx| {
            let mut s = crate::session::Session::start(&conn, cwd, None, model, cx);
            s.view.messages = kept;
            s.view.turns_done = keep_prompts as u64;
            s.version += 1;
            s
        });
        self.adopt(fresh, window, cx);
    }

    /// Rolls back completed file edits made in turns strictly after `turn_ix` (0-based), returning reverted diff count.
    pub(super) fn revert_edits_after_turn(&mut self, turn_ix: usize, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let (cwd, diffs) = {
            let ses = self.session.read(cx);
            let turns = desk::turns(&ses.view.messages);
            let Some(next_turn) = turns.get(turn_ix + 1) else {
                return 0;
            };
            let mut collected: Vec<SessionFileDiff> = Vec::new();
            for msg in &ses.view.messages[next_turn.message..] {
                if let Message::Tool(card) = msg
                    && card.status == ToolStatus::Completed
                {
                    collected.extend(card.diffs.iter().cloned());
                }
            }
            (ses.cwd.clone(), collected)
        };
        let mut reverted = 0usize;
        for diff in diffs.into_iter().rev() {
            let Some(target) = safe_project_target(&cwd, &diff.path) else {
                continue;
            };
            match diff.old_text {
                None => {
                    if target.is_file()
                        && std::fs::read_to_string(&target).ok().as_deref() == Some(diff.new_text.as_str())
                        && std::fs::remove_file(&target).is_ok()
                    {
                        reverted += 1;
                    }
                }
                Some(old_text) if target.is_file() => {
                    if let Ok(current) = std::fs::read_to_string(&target) {
                        let next = if current == diff.new_text {
                            Some(old_text)
                        } else if !diff.new_text.is_empty() && current.contains(&diff.new_text) {
                            Some(current.replacen(&diff.new_text, &old_text, 1))
                        } else {
                            None
                        };
                        if let Some(restored) = next && std::fs::write(&target, restored).is_ok() {
                            reverted += 1;
                        }
                    }
                }
                Some(_) => {}
            }
        }
        record_ux_event("turn_reverted", serde_json::json!({ "after_turn": turn_ix + 1, "reverted": reverted }));
        self.selected_turn = None;
        if reverted > 0 {
            self.session.update(cx, |s, cx| {
                s.view.messages.push(Message::Info(format!(
                    "Rolled back {reverted} file edit{} made after turn {}.",
                    if reverted == 1 { "" } else { "s" },
                    turn_ix + 1
                )));
                s.version += 1;
                cx.notify();
            });
            self.poll_disk(window, cx);
            self.load_review(cx);
        } else {
            cx.notify();
        }
        reverted
    }

    /// Proof-first Change Sheet (`⌘D`): verification & falsification bar, clamped conclusion, and hunk cards.
    pub(super) fn change_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let check = self.check_command();
        let snap = std::rc::Rc::clone(&self.desk_cache.snap);
        let pending = self.session.read(cx).view.pending.front().map(|p| p.info.clone());
        let receipt = &snap.receipt;
        let full_audit = &snap.session_audit;
        let turn_focus = self.selected_turn.and_then(|sel| snap.turn_summaries.get(sel).cloned().map(|s| (sel, s)));

        let assertions_removed = full_audit.assertions_removed.max(receipt.assertions_removed);
        let tests_ignored = full_audit.tests_ignored.max(receipt.tests_ignored);
        let review_sensitive = self.review.display.iter().filter(|f| f.sensitive).count();
        let sensitive_files = full_audit.sensitive_files.max(receipt.sensitive_files).max(review_sensitive);
        let has_falsification = assertions_removed > 0 || tests_ignored > 0;
        let has_anomaly = has_falsification || sensitive_files > 0;

        let (mark_state, check_label, check_ink) = match (&receipt.check, check.as_deref()) {
            (Some((cmd, CheckState::Passed)), _) if !has_falsification => (MarkState::Done, format!("{cmd} passed"), c(tk.amber)),
            (Some((cmd, CheckState::Passed)), _) => (MarkState::NeedsYou, format!("{cmd} passed · audit flagged warnings"), c(tk.attention)),
            (Some((cmd, CheckState::Failed)), _) => (MarkState::Error, format!("{cmd} failed"), c(tk.danger)),
            (Some((cmd, CheckState::Running)), _) => (MarkState::Working, format!("{cmd} running…"), c(tk.text_muted)),
            (None, Some(cmd)) => (MarkState::NeedsYou, format!("{cmd} not run yet"), c(tk.attention)),
            (None, None) if has_falsification => (MarkState::NeedsYou, "Falsification warnings detected".into(), c(tk.attention)),
            (None, None) => (MarkState::Done, "Working tree diff".into(), c(tk.text_secondary)),
        };

        let mut audit_notes: Vec<String> = Vec::new();
        if assertions_removed > 0 {
            audit_notes.push(format!("{assertions_removed} assertion{} removed", if assertions_removed == 1 { "" } else { "s" }));
        }
        if tests_ignored > 0 {
            audit_notes.push(format!("{tests_ignored} test{} ignored", if tests_ignored == 1 { "" } else { "s" }));
        }
        if sensitive_files > 0 {
            audit_notes.push(format!("{sensitive_files} sensitive file{}", if sensitive_files == 1 { "" } else { "s" }));
        }
        let audit_line = if audit_notes.is_empty() {
            "0 assertions removed · 0 tests ignored · 0 sensitive files".to_owned()
        } else {
            audit_notes.join(" · ")
        };
        let offer_run = check.as_ref().filter(|_| !matches!(receipt.check, Some((_, CheckState::Passed)))).cloned();

        let file_total = self.review.display.len();
        let file_open = |ix: usize, sensitive: bool, path: &str| -> bool {
            let base = self.sheet_all_expanded.unwrap_or(sensitive || ix < 2 || file_total <= 4);
            base ^ self.sheet_toggled_files.contains(path)
        };
        let any_folded = file_total > 4 && self.review.display.iter().enumerate().any(|(i, f)| !file_open(i, f.sensitive, &f.raw_path));

        let sheet_header = snap.brief_shown.is_none().then(|| {
            let count = self.review.file_count();
            let sub = if self.review.loading { "reading git diff…".into() } else { format!("{count} changed file{}", if count == 1 { "" } else { "s" }) };
            div().id("sheet-header").flex().items_baseline().justify_between().pb(px(8.)).border_b_1().border_color(line(cx, t::LINE_SUBTLE))
                .child(div().flex().items_baseline().gap(px(10.)).child(div().font_family(voice(cx)).text_size(px(22.)).text_color(c(tk.text_primary)).child("Change Sheet")).child(ui::meta(SharedString::from(sub), cx)))
                .child(div().id("sheet-close").role(gpui::Role::Button).cursor_pointer().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.amber)).hover(|s| s.text_color(c(tk.text_primary))).on_click(cx.listener(|this, _, _, cx| this.toggle_change_sheet(cx))).child("transcript  ⌘D"))
                .into_any_element()
        });

        let proof_bar = div()
            .id("sheet-proof-bar")
            .flex().flex_col().gap(px(4.)).p(px(12.)).rounded(px(t::RADIUS_CARD)).border_1()
            .border_color(if has_anomaly { ca(tk.attention, 0.45) } else { line(cx, t::LINE_DEFAULT) })
            .bg(line(cx, t::FILL_FAINT))
            .child(
                div().flex().items_center().gap(px(8.))
                    .child(ui::mark("sheet-mark", mark_state, 12., cx))
                    .child(div().flex_1().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).font_weight(FontWeight::MEDIUM).text_color(check_ink).child(SharedString::from(check_label)))
                    .when(file_total > 4, |row| {
                        let lbl = if any_folded { format!("expand all {file_total}") } else { "collapse all".into() };
                        row.child(ui::text_action("sheet-fold-all", SharedString::from(lbl), false, cx).on_click(cx.listener(move |this, _, _, cx| {
                            this.sheet_all_expanded = Some(any_folded);
                            this.sheet_toggled_files.clear();
                            if !any_folded {
                                this.sheet_expanded_hunks.clear();
                            }
                            cx.notify();
                        })))
                    })
                    .when_some(offer_run, |row, cmd| {
                        let prompt = format!("Run the check (`{cmd}`) and fix what fails.");
                        row.child(ui::text_action("sheet-run-check", "run check", false, cx).on_click(cx.listener(move |this, _, window, cx| this.apply_starter(&prompt, false, window, cx))))
                    })
                    .child(ui::text_action("sheet-refresh", "refresh", false, cx).on_click(cx.listener(|this, _, _, cx| this.load_review(cx)))),
            )
            .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(if has_anomaly { tk.attention } else { tk.text_faint })).child(SharedString::from(audit_line)));

        let expanded = self.sheet_expanded;
        let conclusion_el = snap.conclusion_shown.as_ref().map(|text| {
            let long = text.lines().count() > 5 || text.len() > 360;
            div().flex().flex_col().gap(px(4.))
                .child(div().font_family(voice(cx)).text_size(px(t::TEXT_BODY)).line_height(relative(1.5)).text_color(c(tk.text_body)).when(!expanded && long, |d| d.line_clamp(5)).child(SharedString::from(text.clone())))
                .when(long, |col| col.child(div().id("sheet-expand-conclusion").self_start().cursor_pointer().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).hover(|s| s.text_color(c(tk.text_primary))).on_click(cx.listener(|this, _, _, cx| { this.sheet_expanded = !this.sheet_expanded; cx.notify(); })).child(if expanded { "collapse summary" } else { "show full summary" })))
                .into_any_element()
        });

        let focus_el = turn_focus.map(|(sel, s)| {
            div().p(px(10.)).rounded(px(t::RADIUS_CONTROL)).border_1().border_color(line(cx, t::LINE_SUBTLE)).flex().flex_col().gap(px(3.)).font_family(mono(cx)).text_size(px(t::TEXT_META))
                .child(div().text_color(c(tk.amber)).child(SharedString::from(format!("Turn {} — {}", sel + 1, shown(&s.prompt)))))
                .child(div().text_color(c(tk.text_muted)).child(SharedString::from(format!("{} files (+{} −{}) · {} tools", s.files, s.added, s.removed, s.tools))))
                .into_any_element()
        });

        let weak = cx.entity().downgrade();
        let diff_body: AnyElement = if !self.review.display.is_empty() {
            let cards: Vec<AnyElement> = self.review.display.iter().enumerate().map(|(file_idx, file)| {
                let (st_ch, st_ink) = match file.status {
                    Status::Added => ("A", c(tk.amber)),
                    Status::Deleted => ("D", c(tk.danger)),
                    Status::Renamed => ("R", c(tk.amber_soft)),
                    Status::Modified => ("M", c(tk.attention)),
                };
                let (sensitive, is_open) = (file.sensitive, file_open(file_idx, file.sensitive, &file.raw_path));
                let (path, fold_path, click_file) = (file.raw_path.clone(), file.raw_path.clone(), weak.clone());
                let hunks: Vec<AnyElement> = if is_open && !file.binary {
                    file.hunks.iter().enumerate().map(|(hunk_idx, hunk)| {
                        let (click_hunk, p_hunk, p_more) = (weak.clone(), file.raw_path.clone(), file.raw_path.clone());
                        let line_no = hunk.new_start.saturating_sub(1) as u32;
                        let clamped = hunk.hidden_lines > 0 && !self.sheet_expanded_hunks.contains(&(file.raw_path.clone(), hunk_idx));
                        let src_runs = if clamped { &hunk.preview_runs } else { &hunk.runs };
                        let runs: Vec<AnyElement> = src_runs.iter().map(|run| {
                            let (bg, ink) = match run.kind {
                                LineKind::Add => (Some(amber(cx, t::DIFF_ADD_FILL)), tk.text_body),
                                LineKind::Del => (Some(ca(tk.danger, t::DIFF_DEL_FILL)), tk.text_muted),
                                LineKind::Context => (None, tk.text_muted),
                            };
                            div().px(px(8.)).py(px(1.)).whitespace_nowrap().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).line_height(relative(1.45)).text_color(c(ink)).when_some(bg, |d, b| d.bg(b)).child(run.text.clone()).into_any_element()
                        }).collect();
                        let hidden = hunk.hidden_lines;
                        div()
                            .flex().flex_col().border_t_1().border_color(line(cx, t::LINE_SUBTLE))
                            .child(
                                div().flex().items_center().justify_between().px(px(8.)).py(px(3.)).bg(line(cx, t::FILL_FAINT))
                                    .child(div().id(SharedString::from(format!("sheet-hunk-{file_idx}-{hunk_idx}"))).flex_1().cursor_pointer().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).hover(|s| s.text_color(c(tk.text_primary))).child(hunk.header.clone()).on_click(move |_: &ClickEvent, window, cx| {
                                        let _ = click_hunk.update(cx, |this, cx| this.open_file(&p_hunk, Some(line_no), window, cx));
                                    }))
                                    .child(div().flex().items_center().gap(px(8.)).child(ui::text_action(SharedString::from(format!("sheet-keep-{file_idx}-{hunk_idx}")), "keep", false, cx).on_click(cx.listener(move |this, _, w, cx| this.apply_hunk(file_idx, hunk_idx, Action::Keep, w, cx)))).child(ui::text_action(SharedString::from(format!("sheet-rev-{file_idx}-{hunk_idx}")), "revert", true, cx).on_click(cx.listener(move |this, _, w, cx| this.apply_hunk(file_idx, hunk_idx, Action::Revert, w, cx))))),
                            )
                            .children(runs)
                            .when(clamped, |d| {
                                d.child(div().id(SharedString::from(format!("sheet-more-{file_idx}-{hunk_idx}"))).px(px(8.)).py(px(4.)).cursor_pointer().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.amber)).hover(|s| s.text_color(c(tk.text_primary))).on_click(cx.listener(move |this, _, _, cx| {
                                    this.sheet_expanded_hunks.insert((p_more.clone(), hunk_idx));
                                    cx.notify();
                                })).child(SharedString::from(format!("… show {hidden} more lines"))))
                            })
                            .into_any_element()
                    }).collect()
                } else {
                    Vec::new()
                };

                div()
                    .flex().flex_col().rounded(px(t::RADIUS_CARD)).border_1().border_color(if sensitive { ca(tk.attention, 0.45) } else { line(cx, t::LINE_DEFAULT) }).overflow_hidden()
                    .child(
                        div().flex().items_center().justify_between().px(px(10.)).py(px(6.)).bg(c(tk.bg_window))
                            .child(
                                div().flex().items_center().gap(px(6.)).min_w_0().flex_1()
                                    .child(div().id(SharedString::from(format!("sheet-fold-{file_idx}"))).w(px(12.)).cursor_pointer().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).hover(|s| s.text_color(c(tk.text_primary))).on_click(cx.listener(move |this, _, _, cx| {
                                        if !this.sheet_toggled_files.remove(&fold_path) {
                                            this.sheet_toggled_files.insert(fold_path.clone());
                                        }
                                        cx.notify();
                                    })).child(if is_open { "▾" } else { "▸" }))
                                    .child(
                                        div().id(SharedString::from(format!("sheet-file-{file_idx}"))).flex().items_center().gap(px(6.)).min_w_0().cursor_pointer().on_click(move |_: &ClickEvent, w, cx| { let _ = click_file.update(cx, |this, cx| this.open_file(&path, None, w, cx)); })
                                            .child(div().w(px(12.)).font_family(mono(cx)).text_size(px(t::TEXT_META)).font_weight(FontWeight::SEMIBOLD).text_color(st_ink).child(st_ch))
                                            .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_UI)).text_color(c(tk.text_primary)).hover(|s| s.text_color(c(tk.amber))).child(file.path.clone()))
                                            .when(sensitive, |r| r.child(div().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.attention)).child("sensitive")))
                                            .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(format!("+{} −{}", file.additions, file.deletions)))),
                                    ),
                            )
                            .child(div().flex().items_center().gap(px(8.)).child(ui::text_action(SharedString::from(format!("sheet-keep-all-{file_idx}")), "keep all", false, cx).on_click(cx.listener(move |this, _, w, cx| this.apply_file(file_idx, Action::Keep, w, cx)))).child(ui::text_action(SharedString::from(format!("sheet-rev-all-{file_idx}")), "revert all", true, cx).on_click(cx.listener(move |this, _, w, cx| this.apply_file(file_idx, Action::Revert, w, cx))))),
                    )
                    .when(is_open && file.binary, |d| d.child(div().px(px(10.)).py(px(6.)).border_t_1().border_color(line(cx, t::LINE_SUBTLE)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child("Binary file changed (not diffed)")))
                    .children(hunks)
                    .into_any_element()
            }).collect();
            div().flex().flex_col().gap(px(10.)).children(cards).into_any_element()
        } else if !snap.session_diffs.is_empty() {
            transcript::diff_preview(&snap.session_diffs, cx)
        } else {
            let msg = if self.review.loading { "Reading working-tree diff…" } else { "No file diffs in the working tree yet — press ⌘D or esc for the transcript." };
            div().py(px(12.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_faint)).child(msg).into_any_element()
        };

        let gate_el = pending.as_ref().map(|info| self.approval_card(info, cx));
        div()
            .id("change-sheet")
            .size_full()
            .overflow_y_scroll()
            .child(
                div().w_full().flex().justify_center().px(px(24.)).py(px(16.)).child(
                    div()
                        .w_full().max_w(px(t::CONTENT_MAX_W)).flex().flex_col().gap(px(14.))
                        .children(sheet_header)
                        .children(gate_el)
                        .child(proof_bar)
                        .children(conclusion_el)
                        .children(focus_el)
                        .child(diff_body),
                ),
            )
            .into_any_element()
    }
}
