//! Git and GitHub from the Review tab (Claude Code desktop, Codex and Copilot parity): a commit box with a generated
//! message, commit, push, open a pull request, and the PR's checks in the header; a new conversation in its own
//! worktree; a code review of the uncommitted changes or the branch.
//!
//! Every command runs off the UI thread with an argument vector — never a shell — in the project folder, with a
//! timeout. Branch and folder names are checked before they reach git. Pushing and opening a PR happen only when the
//! person presses the button.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use cordata_theme::tokens as t;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, AppContext as _, Context, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px};
use gpui_component::input::{Textarea, TextareaState};
use serde_json::Value;

use super::Workspace;
use crate::theme::{c, line, mono, tk};
use crate::ui::{self, Dot, Kind};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const NET_TIMEOUT: Duration = Duration::from_secs(120);
/// How often the open pull request's checks are read.
pub const PR_EVERY: Duration = Duration::from_secs(45);
/// The most diff text a review prompt carries.
const MAX_REVIEW_DIFF: usize = 120_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pr {
    pub number: u64,
    pub url: String,
    pub state: String,
    /// `pass` · `fail` · `pending` · `none`.
    pub checks: String,
    pub failing: Vec<String>,
}

pub struct Git {
    pub branch: Option<String>,
    pub default_branch: String,
    pub dirty: usize,
    pub staged: usize,
    pub ahead: u32,
    pub has_upstream: bool,
    pub pr: Option<Pr>,
    /// What is running now ("committing…"), one at a time.
    pub busy: Option<&'static str>,
    pub note: Option<(String, bool)>,
    pub message: gpui::Entity<TextareaState>,
    pub branch_input: gpui::Entity<gpui_component::input::InputState>,
}

impl Git {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        Self {
            branch: None,
            default_branch: "main".into(),
            dirty: 0,
            staged: 0,
            ahead: 0,
            has_upstream: false,
            pr: None,
            busy: None,
            note: None,
            message: cx.new(|cx| TextareaState::new(window, cx).rows(3).placeholder("Commit message — or Generate")),
            branch_input: cx.new(|cx| gpui_component::input::InputState::new(window, cx).placeholder("new-branch-name")),
        }
    }
}

/// A branch or worktree name git and the file system both accept: letters, digits, `-`, `_`, `.`, `/`; no `..`, no
/// leading `-` or `.`, at most 80 characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && !name.starts_with(['-', '.', '/'])
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("//")
        && !name.ends_with(".lock")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// `Fix the login bug!` → `fix-the-login-bug`.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(40).collect::<String>().trim_matches('-').to_owned()
}

/// Run a program (no shell) in `cwd`, with stdin and a deadline. `Ok(stdout)` on success, `Err(the last stderr line)`.
pub fn run(program: &str, args: &[&str], cwd: &Path, stdin: Option<&str>, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program} could not start: {e}"))?;
    if let Some(text) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        use std::io::Write as _;
        let _ = pipe.write_all(text.as_bytes());
    }
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                return Err(format!("{program} took longer than {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim().to_owned())
    }
}

/// The repository's state: branch, changes, how far ahead, the default branch.
pub fn status(cwd: &Path) -> Option<(String, usize, usize, u32, bool, String)> {
    let out = run("git", &["status", "--porcelain=v2", "--branch"], cwd, None, GIT_TIMEOUT).ok()?;
    let (mut branch, mut dirty, mut staged, mut ahead, mut upstream) = (String::new(), 0, 0, 0u32, false);
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            branch = rest.to_owned();
        } else if line.starts_with("# branch.upstream ") {
            upstream = true;
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            ahead = rest.split_whitespace().next().and_then(|a| a.trim_start_matches('+').parse().ok()).unwrap_or(0);
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            dirty += 1;
            if line.as_bytes().get(2).is_some_and(|b| *b != b'.') {
                staged += 1;
            }
        } else if line.starts_with("? ") {
            dirty += 1;
        }
    }
    let default = run("git", &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"], cwd, None, GIT_TIMEOUT)
        .ok()
        .map(|s| s.trim().trim_start_matches("origin/").to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".into());
    Some((branch, dirty, staged, ahead, upstream, default))
}

/// The open pull request for the current branch and its checks (`gh`), or None.
pub fn pr_status(cwd: &Path) -> Option<Pr> {
    let out = run("gh", &["pr", "view", "--json", "number,url,state,statusCheckRollup"], cwd, None, NET_TIMEOUT).ok()?;
    let v: Value = serde_json::from_str(&out).ok()?;
    let checks = v.get("statusCheckRollup").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut failing = Vec::new();
    let mut pending = false;
    for check in &checks {
        let conclusion = check.get("conclusion").and_then(Value::as_str).unwrap_or("").to_ascii_uppercase();
        let state = check.get("status").or_else(|| check.get("state")).and_then(Value::as_str).unwrap_or("").to_ascii_uppercase();
        let name = check.get("name").or_else(|| check.get("context")).and_then(Value::as_str).unwrap_or("check").to_owned();
        if matches!(conclusion.as_str(), "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED") || state == "FAILURE" || state == "ERROR" {
            failing.push(name);
        } else if conclusion.is_empty() && !matches!(state.as_str(), "SUCCESS" | "COMPLETED") {
            pending = true;
        }
    }
    let summary = if checks.is_empty() { "none" } else if !failing.is_empty() { "fail" } else if pending { "pending" } else { "pass" };
    Some(Pr {
        number: v.get("number").and_then(Value::as_u64)?,
        url: v.get("url").and_then(Value::as_str).unwrap_or_default().to_owned(),
        state: v.get("state").and_then(Value::as_str).unwrap_or_default().to_owned(),
        checks: summary.into(),
        failing,
    })
}

impl Workspace {
    /// Read the repository's state (and, every `PR_EVERY`, the pull request) off the UI thread.
    pub(super) fn refresh_git(&mut self, with_pr: bool, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        let job = cx.background_executor().spawn(async move { (status(&cwd), if with_pr { Some(pr_status(&cwd)) } else { None }) });
        cx.spawn(async move |this, cx| {
            let (state, pr) = job.await;
            let _ = this.update(cx, |this, cx| {
                match state {
                    Some((branch, dirty, staged, ahead, upstream, default)) => {
                        this.git.branch = Some(branch);
                        this.git.dirty = dirty;
                        this.git.staged = staged;
                        this.git.ahead = ahead;
                        this.git.has_upstream = upstream;
                        this.git.default_branch = default;
                    }
                    None => this.git.branch = None,
                }
                if let Some(pr) = pr {
                    this.git.pr = pr;
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn schedule_git(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut tick = 0u64;
            loop {
                let with_pr = tick.is_multiple_of(PR_EVERY.as_secs() / 15);
                if this.update(cx, |this, cx| this.refresh_git(with_pr, cx)).is_err() {
                    break;
                }
                tick += 1;
                cx.background_executor().timer(Duration::from_secs(15)).await;
            }
        })
        .detach();
    }

    /// Run git/gh steps in order off the UI thread; the first failure stops and is shown. `done` runs on success.
    fn git_steps(&mut self, label: &'static str, steps: Vec<(String, Vec<String>, Option<String>, Duration)>, done: impl FnOnce(&mut Workspace, Vec<String>, &mut Window, &mut Context<Workspace>) + 'static, window: &mut Window, cx: &mut Context<Self>) {
        if self.git.busy.is_some() {
            return;
        }
        self.git.busy = Some(label);
        self.git.note = None;
        let cwd = self.cwd.clone();
        let job = cx.background_executor().spawn(async move {
            let mut outputs = Vec::new();
            for (program, args, stdin, timeout) in steps {
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                outputs.push(run(&program, &refs, &cwd, stdin.as_deref(), timeout)?);
            }
            Ok::<_, String>(outputs)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.git.busy = None;
                match result {
                    Ok(outputs) => done(this, outputs, window, cx),
                    Err(why) => this.git.note = Some((shown(&why), true)),
                }
                this.refresh_git(true, cx);
                this.load_review(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn commit(&mut self, stage_all: bool, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.git.message.read(cx).value().trim().to_owned();
        if message.is_empty() {
            self.git.note = Some(("Write a message first.".into(), true));
            cx.notify();
            return;
        }
        let mut steps = Vec::new();
        if stage_all {
            steps.push(("git".into(), vec!["add".into(), "-A".into()], None, GIT_TIMEOUT));
        }
        steps.push(("git".into(), vec!["commit".into(), "-F".into(), "-".into()], Some(message), GIT_TIMEOUT));
        self.git_steps("committing…", steps, |this, out, window, cx| {
            let first = out.last().and_then(|o| o.lines().next()).unwrap_or("committed").to_owned();
            this.git.note = Some((first, false));
            this.git.message.update(cx, |s, cx| s.set_value("", window, cx));
        }, window, cx);
    }

    fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.git_steps("pushing…", vec![("git".into(), vec!["push".into(), "-u".into(), "origin".into(), "HEAD".into()], None, NET_TIMEOUT)], |this, _, _, _| {
            this.git.note = Some(("Pushed.".into(), false));
        }, window, cx);
    }

    /// Push, then open a pull request titled and described by the latest commit.
    fn create_pr(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.git.branch.clone() else { return };
        if branch == self.git.default_branch || branch == "(detached)" {
            self.git.note = Some((format!("You are on {branch}: make a branch first."), true));
            cx.notify();
            return;
        }
        let base = self.git.default_branch.clone();
        let steps = vec![
            ("git".into(), vec!["push".into(), "-u".into(), "origin".into(), "HEAD".into()], None, NET_TIMEOUT),
            ("gh".into(), vec!["pr".into(), "create".into(), "--fill".into(), "--base".into(), base, "--head".into(), branch], None, NET_TIMEOUT),
        ];
        self.git_steps("opening a pull request…", steps, |this, out, _, cx| {
            let url = out.last().and_then(|o| o.lines().rev().find(|l| l.starts_with("https://"))).unwrap_or("").to_owned();
            this.git.note = Some((if url.is_empty() { "Pull request opened.".into() } else { format!("Opened {url}") }, false));
            if !url.is_empty() {
                cx.open_url(&url);
            }
        }, window, cx);
    }

    fn new_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.git.branch_input.read(cx).value().trim().to_owned();
        if !valid_name(&name) {
            self.git.note = Some(("Use letters, digits, - _ . / (no spaces, no ..).".into(), true));
            cx.notify();
            return;
        }
        self.git_steps("making a branch…", vec![("git".into(), vec!["switch".into(), "-c".into(), name.clone()], None, GIT_TIMEOUT)], move |this, _, window, cx| {
            this.git.note = Some((format!("On {name}."), false));
            this.git.branch_input.update(cx, |s, cx| s.set_value("", window, cx));
        }, window, cx);
    }

    /// A new conversation in its own git worktree beside the repository (`../<repo>-<name>` on branch
    /// `cordata/<name>`): independent work that never shares a working tree.
    pub(super) fn new_worktree_conversation(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let name = slug(name);
        let name = if name.is_empty() { format!("task-{}", crate::prefs::now() % 100_000) } else { name };
        let branch = format!("cordata/{name}");
        let repo = self.cwd.clone();
        let folder_name = format!("{}-{name}", repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into()));
        let Some(parent) = repo.parent().map(Path::to_path_buf) else { return };
        let path: PathBuf = parent.join(folder_name);
        if !valid_name(&branch) || path.exists() {
            self.git.note = Some((format!("{} already exists — pick another name.", super::tilde(&path)), true));
            cx.notify();
            return;
        }
        let target = path.clone();
        let steps = vec![("git".into(), vec!["worktree".into(), "add".into(), "-b".into(), branch, path.display().to_string()], None, GIT_TIMEOUT)];
        self.git_steps("making a worktree…", steps, move |this, _, window, cx| {
            this.git.note = None;
            this.switch_project(target, window, cx);
        }, window, cx);
    }

    /// Ask the agent for a code review of the uncommitted changes, or of the branch against the default branch. The
    /// diff goes as an attachment; the agent is told to report, not edit.
    pub(super) fn request_review(&mut self, branch: bool, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        let base = self.git.default_branch.clone();
        let job = cx.background_executor().spawn(async move {
            let range = if branch { format!("{base}...HEAD") } else { "HEAD".into() };
            run("git", &["diff", "--no-color", "--no-ext-diff", &range], &cwd, None, GIT_TIMEOUT)
        });
        cx.spawn_in(window, async move |this, cx| {
            let diff = job.await;
            let _ = this.update_in(cx, |this, window, cx| match diff {
                Ok(diff) if diff.trim().is_empty() => this.git.note = Some(("No changes to review.".into(), true)),
                Ok(mut diff) => {
                    if diff.len() > MAX_REVIEW_DIFF {
                        diff.truncate(diff.char_indices().take_while(|(i, _)| *i < MAX_REVIEW_DIFF).last().map_or(0, |(i, _)| i));
                        diff.push_str("\n… (diff cut; read the files for the rest)");
                    }
                    let what = if branch { "this branch's changes against the default branch" } else { "the uncommitted changes" };
                    let prompt = format!(
                        "Review {what} (attached as a diff). Report findings only — do not edit files. For each: severity (blocker, major, minor, nit), file:line, the problem, and a concrete fix. Correctness and security first, then tests, then simplicity. Say plainly if nothing needs to change."
                    );
                    let staged = super::attachments::Staged {
                        name: if branch { "branch.diff".into() } else { "uncommitted.diff".into() },
                        size: diff.len(),
                        image: false,
                        kind: super::attachments::Kind::Code,
                        preview: None,
                        attachment: acp_core::Attachment::Text { uri: "file:///review.diff".into(), text: diff },
                    };
                    this.staged.push(staged);
                    this.composer.update(cx, |s, cx| s.set_value(prompt, window, cx));
                    this.send_composer(window, cx);
                }
                Err(why) => this.git.note = Some((shown(&why), true)),
            });
        })
        .detach();
    }

    /// The header's pull-request chip: number and checks; a click opens it.
    pub(super) fn pr_chip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pr = self.git.pr.clone()?;
        let tk = tk(cx);
        let (mark, ink) = match pr.checks.as_str() {
            "pass" => ("✓", tk.teal),
            "fail" => ("✕", tk.danger),
            "pending" => ("●", tk.amber),
            _ => ("", tk.text_muted),
        };
        let tip = if pr.failing.is_empty() { format!("PR #{} · {} · checks {}", pr.number, pr.state.to_lowercase(), pr.checks) } else { format!("PR #{} · failing: {}", pr.number, pr.failing.join(", ")) };
        let url = pr.url.clone();
        Some(
            ui::chip("pr-chip", SharedString::from(format!("PR #{}", pr.number)), cx)
                .child(div().text_color(c(ink)).child(mark))
                .aria_label(SharedString::from(tip.clone()))
                .tooltip(ui::tip(SharedString::from(tip)))
                .on_click(move |_, _, cx| cx.open_url(&url))
                .into_any_element(),
        )
    }

    /// The Review tab's top: branch, commit box, push / PR, review buttons.
    pub(super) fn git_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tk = tk(cx);
        let branch = self.git.branch.clone()?;
        let on_default = branch == self.git.default_branch;
        let busy = self.git.busy;
        let has_message = !self.git.message.read(cx).value().trim().is_empty();
        let stage_all = self.git.staged == 0 && self.git.dirty > 0;
        let commit_label = if stage_all { "Stage all & commit" } else { "Commit" };
        let pr_line = self.git.pr.clone().map(|pr| {
            let dot = match pr.checks.as_str() {
                "pass" => Dot::Ok,
                "fail" => Dot::Danger,
                "pending" => Dot::Live,
                _ => Dot::Muted,
            };
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(ui::dot(dot, cx))
                .child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_secondary)).child(SharedString::from(format!("PR #{} · {} · checks {}", pr.number, pr.state.to_lowercase(), pr.checks))))
                .when(!pr.failing.is_empty(), |d| d.child(ui::meta(SharedString::from(pr.failing.join(", ")), cx)))
        });
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(12.))
                .mb(px(10.))
                .rounded(px(t::RADIUS_CARD))
                .border_1()
                .border_color(line(cx, t::LINE_DEFAULT))
                .bg(line(cx, t::FILL_FAINT))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_primary)).child(SharedString::from(format!("⎇ {branch}"))))
                        .child(ui::meta(SharedString::from(format!("{} changed · {} staged{}", self.git.dirty, self.git.staged, if self.git.ahead > 0 { format!(" · {} to push", self.git.ahead) } else { String::new() })), cx))
                        .child(div().flex_1())
                        .when_some(busy, |r, label| r.child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.amber)).child(label))),
                )
                .children(pr_line)
                .when(self.git.dirty > 0, |col| {
                    col.child(div().px(px(8.)).py(px(6.)).rounded(px(t::RADIUS_CONTROL)).bg(c(tk.bg_input)).border_1().border_color(line(cx, t::LINE_CONTROL_STRONG)).font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).child(Textarea::new(&self.git.message).appearance(false).aria_label("Commit message")))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap(px(6.))
                                .child(ui::button("git-commit", commit_label, Kind::Primary, cx).h(px(26.)).when(!has_message, |b| b.opacity(0.5)).on_click(cx.listener(move |this, _, window, cx| this.commit(stage_all, window, cx)))),
                        )
                })
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .when(self.git.ahead > 0 || (!self.git.has_upstream && !on_default), |r| r.child(ui::button("git-push", "Push", Kind::Outline, cx).h(px(26.)).on_click(cx.listener(|this, _, window, cx| this.push(window, cx)))))
                        .when(!on_default && self.git.pr.is_none(), |r| r.child(ui::button("git-pr", "Create PR", Kind::AmberOutline, cx).h(px(26.)).tooltip(ui::tip("Push, then open a pull request from the latest commit")).on_click(cx.listener(|this, _, window, cx| this.create_pr(window, cx)))))
                        .when(self.git.dirty > 0, |r| r.child(ui::button("git-review", "Review changes", Kind::Ghost, cx).h(px(26.)).tooltip(ui::tip("Ask the agent to review the uncommitted diff")).on_click(cx.listener(|this, _, window, cx| this.request_review(false, window, cx)))))
                        .when(!on_default, |r| r.child(ui::button("git-review-branch", "Review branch", Kind::Ghost, cx).h(px(26.)).on_click(cx.listener(|this, _, window, cx| this.request_review(true, window, cx))))),
                )
                .when(on_default, |col| {
                    col.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().w(px(180.)).h(px(26.)).flex().items_center().px(px(8.)).rounded(px(t::RADIUS_CONTROL)).border_1().border_color(line(cx, t::LINE_CONTROL_STRONG)).font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).child(gpui_component::input::Input::new(&self.git.branch_input).appearance(false)))
                            .child(ui::button("git-branch", "New branch", Kind::Ghost, cx).h(px(26.)).on_click(cx.listener(|this, _, window, cx| this.new_branch(window, cx)))),
                    )
                })
                .when_some(self.git.note.clone(), |col, (note, error)| col.child(div().text_size(px(t::TEXT_SMALL)).text_color(c(if error { tk.danger } else { tk.teal })).child(SharedString::from(note))))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_slugs_are_safe_for_git() {
        assert!(valid_name("cordata/fix-login"));
        assert!(!valid_name("-rf"));
        assert!(!valid_name("a..b"));
        assert!(!valid_name("has space"));
        assert!(!valid_name("x.lock"));
        assert!(!valid_name(""));
        assert_eq!(slug("Fix the login bug!"), "fix-the-login-bug");
        assert_eq!(slug("  ---  "), "");
    }

    #[test]
    fn status_reads_a_real_repository() {
        let dir = std::env::temp_dir().join(format!("cordata-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| run("git", &[&["-c", "user.email=t@t", "-c", "user.name=t"], args].concat(), &dir, None, GIT_TIMEOUT).unwrap();
        g(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("a.txt"), "b").unwrap();
        std::fs::write(dir.join("new.txt"), "n").unwrap();
        let (branch, dirty, staged, _, upstream, _) = status(&dir).unwrap();
        assert_eq!((branch.as_str(), dirty, staged, upstream), ("main", 2, 0, false));
        g(&["add", "a.txt"]);
        assert_eq!(status(&dir).unwrap().2, 1, "staged counted");
        assert!(run("git", &["commit", "-F", "-"], &dir, Some(""), GIT_TIMEOUT).is_err(), "an empty message fails with git's reason");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
