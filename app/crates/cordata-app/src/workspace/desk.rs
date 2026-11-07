//! The desk: the shown thread drawn as a piece of work, not a chat (`cordata_view::desk` derives the facts).
//!
//! - **Brief** — at the top of the column, what you asked for in the reading serif, and under it the check that says
//!   it is done, how many turns and files so far, and the session's opening notes folded into one word.
//! - **Tape** — one stop per prompt; a click walks the transcript to that turn. A gate waiting on you is the last stop.
//! - **Places** — the right margin: the files this thread read and changed, grouped by component, each marked with
//!   what happened to it. A click opens the file. Replaces the old floorplan strip, whose agent roles were guessed.
//! - **Receipt** — after a turn: what changed, what ran, whether the check passed, and what to do next.

use std::path::{Path, PathBuf};

use cordata_theme::tokens as t;
use cordata_view::app::Message;
use cordata_view::desk::{self, CheckState, Touch};
use cordata_view::{fold, shown};

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, div, px, relative};

use super::{DockTab, Workspace};
use crate::theme::{c, line, mono, tk, voice};
use crate::ui::{self, MarkState};

/// The margin is drawn only when the column is wide enough to keep a full reading width beside it.
pub const PLACES_W: f32 = 248.;
pub const PLACES_MIN_COLUMN: f32 = t::CONTENT_MAX_W + PLACES_W + 48.;
/// The tape shows this many stops; older turns fold into a count.
const TAPE_STOPS: usize = 24;

fn cut(text: &str, n: usize) -> String {
    if text.chars().count() <= n { text.to_owned() } else { format!("{}…", text.chars().take(n).collect::<String>().trim_end()) }
}

impl Workspace {
    /// Read the project's check command again when the window moved to another folder.
    pub(super) fn refresh_check(&mut self) {
        if self.check.0 != self.cwd {
            let found = desk::check_command(&self.cwd);
            self.check = (self.cwd.clone(), found);
        }
    }

    pub(super) fn check_command(&self) -> Option<String> {
        self.check.1.clone()
    }

    /// The brief: your first prompt as the page's title, a meta line with scope contract, and the tape. `None` before you have asked.
    pub(super) fn brief_block(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tk = tk(cx);
        let snap = std::rc::Rc::clone(&self.desk_cache.snap);
        let brief = snap.brief_shown.as_ref()?;
        let (pending, turn_active, scope) = {
            let view = &self.session.read(cx).view;
            (!view.pending.is_empty(), view.turn_active, view.scope.clone())
        };
        let notes = snap.note_texts.len();
        let failing = snap.failing_notes;
        let files = snap.edited_files;
        let check = self.check_command();
        let in_sheet = self.desk_mode == super::DeskMode::ChangeSheet;

        let sep = || div().text_color(c(tk.text_faint)).child("·");
        let scope_el = self.scope_chip(&scope, &snap.candidate_scope, cx);

        let meta = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .font_family(mono(cx))
            .text_size(px(t::TEXT_META))
            .text_color(c(tk.text_faint))
            .when_some(check, |row, check| {
                row.child(div().child("done when"))
                    .child(div().text_color(c(tk.text_secondary)).child(SharedString::from(check)))
                    .child(sep())
            })
            .child(scope_el)
            .child(sep())
            .child(div().child(SharedString::from(format!("{} turn{}", snap.turns.len(), if snap.turns.len() == 1 { "" } else { "s" }))))
            .when(files > 0, |row| row.child(sep()).child(div().child(SharedString::from(format!("{files} file{} changed", if files == 1 { "" } else { "s" })))))
            .when(notes > 0, |row| {
                row.child(sep()).child(
                    div()
                        .id("opening-notes")
                        .cursor_default()
                        .tooltip(ui::tip(SharedString::from(snap.note_texts.join("\n"))))
                        .when(failing > 0, |d| d.text_color(c(tk.attention)))
                        .child(SharedString::from(match failing {
                            0 => format!("{notes} startup note{}", if notes == 1 { "" } else { "s" }),
                            f => format!("{notes} startup notes · {f} need a look"),
                        })),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .id("brief-mode-toggle")
                    .role(gpui::Role::Button)
                    .cursor_pointer()
                    .text_color(c(if in_sheet { tk.amber } else { tk.text_muted }))
                    .hover(|s| s.text_color(c(tk.text_primary)))
                    .tooltip(ui::tip("Switch between proof-first change sheet and transcript  ⌘D"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_change_sheet(cx)))
                    .child(if in_sheet { "transcript  ⌘D" } else { "change sheet  ⌘D" }),
            );

        Some(
            div()
                .id("brief")
                .w_full()
                .flex()
                .justify_center()
                .px(px(24.))
                .pt(px(14.))
                .pb(px(10.))
                .border_b_1()
                .border_color(line(cx, t::LINE_SUBTLE))
                .child(
                    div()
                        .w_full()
                        .max_w(px(t::CONTENT_MAX_W))
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(
                            div()
                                .id("brief-text")
                                .tooltip(ui::tip(SharedString::from(cut(brief, 600))))
                                .font_family(voice(cx))
                                .text_size(px(21.))
                                .line_height(relative(1.3))
                                .text_color(c(tk.text_primary))
                                .line_clamp(2)
                                .child(SharedString::from(brief.clone())),
                        )
                        .child(meta)
                        .when(!snap.turns.is_empty() || pending || turn_active, |col| col.child(self.tape(&snap.turns, cx))),
                )
                .into_any_element(),
        )
    }

    /// One stop per prompt, joined by a hairline; clicking a stop opens its forensic scrubber (fork / rollback) and
    /// scrolls the transcript when in Transcript mode.
    fn tape(&self, turns: &[desk::Turn], cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        let (running, waiting) = (view.turn_active, !view.pending.is_empty());
        let skipped = turns.len().saturating_sub(TAPE_STOPS);
        let joint = || div().w(px(18.)).h(px(1.)).flex_shrink_0().bg(line(cx, t::LINE_CONTROL_STRONG));
        let mut row = div().flex().items_center().h(px(18.)).font_family(mono(cx)).text_size(px(10.));
        if skipped > 0 {
            row = row.child(div().pr(px(6.)).text_color(c(tk.text_faint)).child(SharedString::from(format!("+{skipped}"))));
        }
        let last = turns.len().saturating_sub(1);
        for (i, turn) in turns.iter().enumerate().skip(skipped) {
            if i > skipped {
                row = row.child(joint());
            }
            let selected = self.selected_turn == Some(i);
            let live = i == last && running && !waiting;
            let ink = if selected || (i == last && (running || waiting)) { c(tk.amber) } else { c(tk.text_muted) };
            let message = turn.message;
            let stop = div()
                .id(("tape", i))
                .flex_shrink_0()
                .px(px(2.))
                .cursor_pointer()
                .text_color(ink)
                .hover(|s| s.text_color(c(tk.text_primary)))
                .tooltip(ui::tip(SharedString::from(format!("Turn {} — {} (click to inspect, fork, or roll back)", i + 1, cut(&shown(&turn.prompt), 140)))))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected_turn = if this.selected_turn == Some(i) { None } else { Some(i) };
                    if this.desk_mode == super::DeskMode::Transcript {
                        let (rows, _) = super::display_rows(this.session.read(cx));
                        let at = |r: &fold::Row| match r {
                            fold::Row::Message(j) => *j,
                            fold::Row::Activity(a) => a.first,
                        };
                        if let Some(ix) = rows.iter().position(|r| at(r) >= message) {
                            this.transcript.scroll_to_reveal_item(ix);
                        }
                    }
                    cx.notify();
                }))
                .child(if selected { "◉" } else { "●" });
            row = row.child(if live && ui::motion(cx) {
                gpui::AnimationExt::with_animation(stop, ("tape-live", i), gpui::Animation::new(std::time::Duration::from_millis(1600)).repeat().with_easing(gpui::ease_in_out), |el, t| {
                    el.opacity(0.35 + 0.65 * (1.0 - (2.0 * t - 1.0).powi(2)))
                })
                .into_any_element()
            } else {
                stop.into_any_element()
            });
        }
        if waiting {
            row = row.child(joint()).child(div().flex_shrink_0().px(px(2.)).text_color(c(tk.attention)).child("◆"));
        }
        let scrubber = self.selected_turn.filter(|&i| i < turns.len()).and_then(|sel| self.tape_scrubber(sel, cx));
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(row)
            .children(scrubber)
            .into_any_element()
    }

    /// The right margin: what this thread touched, by place, with sensitive-file and cross-thread overlap radar.
    pub(super) fn places_margin(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let snap = std::rc::Rc::clone(&self.desk_cache.snap);
        let areas = &snap.places;
        let hover = line(cx, t::HOVER);
        let has_blueprint = floorplan_bin().is_some();

        let mut col = div().id("places").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().px(px(14.)).pb(px(14.));
        if areas.is_empty() {
            col = col.child(div().pt(px(4.)).text_size(px(t::TEXT_SMALL)).line_height(relative(1.5)).text_color(c(tk.text_faint)).child("The files this thread reads and changes collect here, by place."));
        }
        for (a, area) in areas.iter().enumerate() {
            col = col.child(
                div()
                    .pt(px(if a == 0 { 2. } else { 12. }))
                    .pb(px(3.))
                    .font_family(mono(cx))
                    .text_size(px(t::TEXT_TAG))
                    .text_color(c(if area.touch() <= Touch::Working { tk.text_secondary } else { tk.text_faint }))
                    .child(SharedString::from(area.name.clone())),
            );
            for (p, place) in area.places.iter().enumerate() {
                let (mark, ink) = match place.touch {
                    Touch::Waiting => ("◆", c(tk.attention)),
                    Touch::Failed => ("✕", c(tk.danger)),
                    Touch::Working => ("●", c(tk.amber)),
                    Touch::Edited => ("+", c(tk.amber)),
                    Touch::Read => ("·", c(tk.text_faint)),
                };
                let name = Path::new(&place.path).file_name().and_then(|n| n.to_str()).unwrap_or(&place.path).to_owned();
                let rel = place.path.clone();
                let counts = (place.added + place.removed > 0).then_some((place.added, place.removed));
                let mut tip = format!("{}\nOpen in the editor", shown(&place.path));
                if place.sensitive {
                    tip.push_str("\n△ Sensitive build / lock / policy file");
                }
                if !place.shared_threads.is_empty() {
                    let list: Vec<String> = place.shared_threads.iter().map(|t| format!("#{t}")).collect();
                    tip.push_str(&format!("\n⎇ Also edited in thread {}", list.join(", ")));
                }
                let shared_badge = place.shared_threads.first().copied();
                col = col.child(
                    div()
                        .id(SharedString::from(format!("place-{a}-{p}")))
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .h(px(24.))
                        .px(px(4.))
                        .ml(px(-4.))
                        .rounded(px(t::RADIUS_ROW))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .tooltip(ui::tip(SharedString::from(tip)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_panel(true, cx);
                            this.open_file(&rel, None, window, cx);
                        }))
                        .child(div().w(px(10.)).flex_shrink_0().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(ink).child(mark))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(mono(cx))
                                .text_size(px(t::TEXT_META))
                                .text_color(c(if place.touch == Touch::Read { tk.text_muted } else { tk.text_primary }))
                                .child(SharedString::from(name)),
                        )
                        .when(place.sensitive, |row| {
                            row.child(div().flex_shrink_0().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.attention)).child("△"))
                        })
                        .when_some(shared_badge, |row, peer_num| {
                            row.child(div().flex_shrink_0().font_family(mono(cx)).text_size(px(t::TEXT_TAG)).text_color(c(tk.teal)).child(SharedString::from(format!("⎇#{peer_num}"))))
                        })
                        .when_some(counts, |row, (add, del)| {
                            row.child(
                                div()
                                    .flex()
                                    .flex_shrink_0()
                                    .gap(px(4.))
                                    .font_family(mono(cx))
                                    .text_size(px(t::TEXT_TAG))
                                    .when(add > 0, |r| r.child(div().text_color(c(tk.amber)).child(SharedString::from(format!("+{add}")))))
                                    .when(del > 0, |r| r.child(div().text_color(c(tk.danger)).child(SharedString::from(format!("−{del}"))))),
                            )
                        }),
                );
            }
        }

        let root = self.cwd.clone();
        div()
            .id("places-margin")
            .w(px(PLACES_W))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(line(cx, t::LINE_SUBTLE))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(36.))
                    .flex_shrink_0()
                    .px(px(14.))
                    .text_size(px(t::TEXT_TAG))
                    .text_color(c(tk.text_faint))
                    .child(div().flex_1().child("Places"))
                    .when(has_blueprint, |row| {
                        row.child(
                            div()
                                .id("open-blueprint")
                                .cursor_pointer()
                                .hover(|s| s.text_color(c(tk.text_primary)))
                                .tooltip(ui::tip("The whole project's map, in your browser"))
                                .on_click(move |_, _, _| open_blueprint(&root))
                                .child("whole map ↗"),
                        )
                    }),
            )
            .child(col)
            .into_any_element()
    }

    /// After a turn: what it changed, what ran, whether the check passed — and the next step. A turn that only
    /// answered (no files, no commands) gets the plain links.
    pub(super) fn receipt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tk = tk(cx);
        let view = &self.session.read(cx).view;
        if view.turn_active || !view.pending.is_empty() || !matches!(view.messages.last(), Some(Message::Agent(_))) {
            return None;
        }
        let check = self.check_command();
        let snap = std::rc::Rc::clone(&self.desk_cache.snap);
        let r = &snap.receipt;
        let link = |id: &'static str, text: &'static str| {
            let hover = c(tk.text_primary);
            div().id(id).role(gpui::Role::Button).cursor_pointer().text_color(c(tk.text_muted)).hover(move |s| s.text_color(hover)).child(text)
        };
        let copy = link("copy-reply", "copy").tooltip(ui::tip("Copy the reply as Markdown")).on_click(cx.listener(|this, _, _, cx| this.copy_last_reply(cx)));
        let quiet = r.files == 0 && r.commands == 0 && r.failed == 0;
        let state = match r.check.as_ref().map(|(_, s)| *s) {
            _ if r.failed > 0 && r.check.as_ref().is_none_or(|(_, s)| *s != CheckState::Passed) => MarkState::Error,
            Some(CheckState::Failed) => MarkState::Error,
            _ if r.has_falsification_warning() => MarkState::NeedsYou,
            _ => MarkState::Done,
        };
        let sep = || div().text_color(c(tk.text_faint)).child("·");
        let mut words = div().flex().flex_1().min_w_0().flex_wrap().items_center().gap(px(6.)).text_color(c(tk.text_secondary));
        if r.files > 0 {
            words = words
                .child(div().child(SharedString::from(format!("Changed {} file{}", r.files, if r.files == 1 { "" } else { "s" }))))
                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.amber)).child(SharedString::from(format!("+{}", r.added))))
                .when(r.removed > 0, |w| w.child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.danger)).child(SharedString::from(format!("−{}", r.removed)))));
        } else if !quiet {
            words = words.child(div().child("No files changed"));
        }
        if r.commands > 0 {
            words = words.child(sep()).child(div().child(SharedString::from(format!("ran {} command{}", r.commands, if r.commands == 1 { "" } else { "s" }))));
        }
        if r.failed > 0 {
            words = words.child(sep()).child(div().text_color(c(tk.danger)).child(SharedString::from(format!("{} failed", r.failed))));
        }
        if r.assertions_removed > 0 {
            words = words.child(sep()).child(
                div()
                    .text_color(c(tk.attention))
                    .child(SharedString::from(format!("{} assertion{} removed", r.assertions_removed, if r.assertions_removed == 1 { "" } else { "s" }))),
            );
        }
        if r.tests_ignored > 0 {
            words = words.child(sep()).child(
                div()
                    .text_color(c(tk.attention))
                    .child(SharedString::from(format!("{} test{} ignored", r.tests_ignored, if r.tests_ignored == 1 { "" } else { "s" }))),
            );
        }
        if r.sensitive_files > 0 {
            words = words.child(sep()).child(
                div()
                    .id("receipt-sensitive")
                    .text_color(c(tk.attention))
                    .tooltip(ui::tip("Build, lockfile, or policy files were modified"))
                    .child(SharedString::from(format!("{} sensitive", r.sensitive_files))),
            );
        }
        let check_said: Option<(String, gpui::Hsla)> = match (&r.check, r.files > 0) {
            (Some((_, CheckState::Passed)), _) => Some(("check passed".into(), c(tk.amber))),
            (Some((_, CheckState::Failed)), _) => Some(("check failed".into(), c(tk.danger))),
            (Some((_, CheckState::Running)), _) => Some(("check running".into(), c(tk.text_muted))),
            (None, true) if check.is_some() => Some(("not checked yet".into(), c(tk.attention))),
            _ => None,
        };
        if let Some((said, ink)) = check_said {
            words = words.child(sep()).child(div().text_color(ink).child(SharedString::from(said)));
        }
        let offer_check = r.files > 0 && check.is_some() && !matches!(r.check, Some((_, CheckState::Passed)));
        let check_prompt = check.map(|c| format!("Run the check (`{c}`) and fix what fails."));

        Some(
            div()
                .id("receipt")
                .flex()
                .items_center()
                .gap(px(10.))
                .pb(px(10.))
                .text_size(px(t::TEXT_SMALL))
                .when(!quiet, |row| row.child(ui::mark("receipt-mark", state, 12., cx)).child(words))
                .when(quiet, |row| row.child(div().flex_1()))
                .child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .gap(px(14.))
                        .when_some(offer_check.then_some(check_prompt).flatten(), |row, prompt| {
                            row.child(link("run-check", "run check").tooltip(ui::tip("Ask the agent to run the check and fix what fails")).on_click(cx.listener(move |this, _, window, cx| this.apply_starter(&prompt, false, window, cx))))
                        })
                        .when(r.files > 0, |row| {
                            row.child(link("open-change-sheet", "change sheet").tooltip(ui::tip("Proof-first change sheet  ⌘D")).on_click(cx.listener(|this, _, _, cx| this.toggle_change_sheet(cx))))
                            .child(link("review-files", "review").tooltip(ui::tip("Changes, hunk by hunk  ⌘⌥4")).on_click(cx.listener(|this, _, _, cx| {
                                this.set_panel(true, cx);
                                this.switch_dock(DockTab::Review, cx);
                            })))
                        })
                        .child(copy),
                )
                .into_any_element(),
        )
    }
}

/// The `cordata-floorplan` binary (or the `cordata` launcher, which forwards `floorplan`), cached once per process.
fn floorplan_bin() -> Option<PathBuf> {
    static FLOORPLAN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FLOORPLAN
        .get_or_init(|| {
            if let Some(path) = std::env::var_os("CORDATA_FLOORPLAN_BIN").map(PathBuf::from).filter(|p| p.is_file()) {
                return Some(path);
            }
            let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            if let Some(p) = [home.join(".local/bin/cordata"), PathBuf::from("/usr/local/bin/cordata")].into_iter().find(|p| p.is_file()) {
                return Some(p);
            }
            let exe = std::env::current_exe().ok()?;
            exe.ancestors().find_map(|dir| [dir.join("tui/target/release/cordata-floorplan"), dir.join("bin/cordata")].into_iter().find(|p| p.is_file()))
        })
        .clone()
}

/// Open the whole project's map (the floorplan's HTML view) in the browser.
fn open_blueprint(cwd: &Path) {
    let Some(bin) = floorplan_bin() else { return };
    let mut cmd = std::process::Command::new(&bin);
    if bin.file_name().is_some_and(|n| n == "cordata") {
        cmd.arg("floorplan");
    }
    cmd.arg("--cwd").arg(cwd).arg("--open").stdin(std::process::Stdio::null());
    let _ = cmd.spawn();
}
