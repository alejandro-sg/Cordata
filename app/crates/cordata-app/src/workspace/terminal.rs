//! The Terminal pane in the dock (docs/app.md §3 D5, phase P4):
//! Interactive terminal shell and streaming execution for developer commands.
//! Runs in the project folder with ANSI sanitization and exit status tracking.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use cordata_theme::Role;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};

use crate::theme::color;
use super::Workspace;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Command,
    Output,
    Success,
    Error,
}

#[derive(Clone, Debug)]
pub struct TerminalLine {
    pub text: String,
    pub kind: LineKind,
}

pub struct Terminal {
    pub lines: Vec<TerminalLine>,
    pub input: Entity<InputState>,
    pub running: bool,
    pub running_cancel: Option<Arc<AtomicBool>>,
    pub last_exit: Option<i32>,
    pub history: Vec<String>,
    pub history_ix: Option<usize>,
}

impl Terminal {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Run command in project… (e.g. git status, cargo test)"));
        Self {
            lines: vec![TerminalLine {
                text: "Cordata Terminal — commands run in the active project directory.".into(),
                kind: LineKind::Output,
            }],
            input,
            running: false,
            running_cancel: None,
            last_exit: None,
            history: Vec::new(),
            history_ix: None,
        }
    }

    pub fn append(&mut self, text: String, kind: LineKind) {
        self.lines.push(TerminalLine { text, kind });
        if self.lines.len() > 1000 {
            self.lines.drain(0..100);
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.last_exit = None;
    }
}

impl Workspace {
    pub(super) fn terminal_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let term = &self.terminal;

        let status_strip = div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(10.))
            .py(px(6.))
            .rounded(px(cordata_theme::tokens::RADIUS_CARD))
            .bg(crate::theme::line(cx, cordata_theme::tokens::FILL_FAINT))
            .border_1()
            .border_color(crate::theme::line(cx, cordata_theme::tokens::LINE_DEFAULT))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_xs()
                    .child(
                        div()
                            .font_family(crate::theme::mono(cx))
                            .text_color(color(cx, Role::TextDim))
                            .child(SharedString::from(format!("cwd: {}", shown(&self.cwd.display().to_string())))),
                    )
                    .when(term.running, |el| {
                        el.child(
                            div()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(color(cx, Role::Accent))
                                .child("● running"),
                        )
                    })
                    .when_some(term.last_exit, |el, code| {
                        let role = if code == 0 { Role::Ok } else { Role::Danger };
                        el.child(
                            div()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(color(cx, role))
                                .child(SharedString::from(format!("exit {code}"))),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .when(term.running, |el| {
                        el.child(
                            Button::new("term-stop-btn")
                                .label("Stop")
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.stop_terminal_command(cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("term-clear-btn")
                            .label("Clear")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.terminal.clear();
                                cx.notify();
                            })),
                    ),
            );

        let quick_commands = ["git status", "git diff", "cargo check", "cargo test"];
        let quick_bar = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .flex_wrap()
            .children(quick_commands.into_iter().map(|cmd| {
                let cmd_owned = cmd.to_owned();
                div()
                    .id(SharedString::from(format!("quick-cmd-{cmd}")))
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(cordata_theme::tokens::RADIUS_CONTROL))
                    .bg(crate::theme::line(cx, cordata_theme::tokens::FILL_FAINT))
                    .border_1()
                    .border_color(crate::theme::line(cx, cordata_theme::tokens::LINE_CONTROL))
                    .cursor_pointer()
                    .text_xs()
                    .font_family(crate::theme::mono(cx))
                    .text_color(color(cx, Role::TextSecondary))
                    .hover(|el| el.text_color(color(cx, Role::Accent)).border_color(color(cx, Role::Accent)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.run_terminal_command(&cmd_owned, cx);
                    }))
                    .child(cmd)
            }));

        let scrollback = {
            let line_elements = term.lines.iter().enumerate().map(|(ix, line)| {
                let (role, prefix) = match line.kind {
                    LineKind::Command => (Role::Accent, "$ "),
                    LineKind::Success => (Role::Ok, "✓ "),
                    LineKind::Error => (Role::Danger, "✗ "),
                    LineKind::Output => (Role::TextSecondary, ""),
                };
                div()
                    .id(("term-line", ix))
                    .flex()
                    .gap(px(4.))
                    .text_xs()
                    .font_family(crate::theme::mono(cx))
                    .text_color(color(cx, role))
                    .child(prefix)
                    .child(SharedString::from(line.text.clone()))
            });

            div()
                .id("term-scrollback")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(px(10.))
                .bg(color(cx, Role::Ground))
                .rounded(px(cordata_theme::tokens::RADIUS_CARD))
                .border_1()
                .border_color(crate::theme::line(cx, cordata_theme::tokens::LINE_DEFAULT))
                .flex()
                .flex_col()
                .gap(px(2.))
                .children(line_elements)
        };

        let input_bar = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(div().flex_1().child(Input::new(&term.input)))
            .child(
                Button::new("term-run-btn")
                    .label("Run")
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let text = this.terminal.input.read(cx).value().to_string();
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            this.run_terminal_command(trimmed, cx);
                        }
                    })),
            );

        div()
            .id("terminal-dock-pane")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(status_strip)
            .child(quick_bar)
            .child(scrollback)
            .child(input_bar)
            .into_any_element()
    }

    pub(super) fn run_terminal_command(&mut self, command: &str, cx: &mut Context<Self>) {
        if self.terminal.running {
            return;
        }
        let command_str = command.to_owned();
        self.terminal.append(command_str.clone(), LineKind::Command);
        self.terminal.history.push(command_str.clone());
        self.terminal.history_ix = None;
        self.terminal.running = true;
        self.terminal.last_exit = None;

        let cancel = Arc::new(AtomicBool::new(false));
        self.terminal.running_cancel = Some(cancel.clone());

        let cwd = self.cwd.clone();
        let background = cx.background_executor().clone();

        cx.spawn(async move |this, cx| {
            let start = Instant::now();
            let outcome = background
                .spawn(async move {
                    cordata_view::shell::run(&command_str, &cwd, std::time::Duration::from_secs(120))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.terminal.running = false;
                this.terminal.running_cancel = None;
                this.terminal.last_exit = outcome.code;

                for line in outcome.output.lines() {
                    let sanitized = cordata_view::shown(line);
                    this.terminal.append(sanitized, LineKind::Output);
                }

                if outcome.timed_out {
                    this.terminal.append("Command timed out after 120s".into(), LineKind::Error);
                } else if outcome.code == Some(0) {
                    this.terminal.append(format!("Process completed in {:.2}s", start.elapsed().as_secs_f32()), LineKind::Success);
                } else {
                    let code_str = outcome.code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
                    this.terminal.append(format!("Process exited with code {code_str}"), LineKind::Error);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn stop_terminal_command(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = &self.terminal.running_cancel {
            cancel.store(true, Ordering::Relaxed);
            self.terminal.append("Interrupt signal sent.".into(), LineKind::Error);
            self.terminal.running = false;
            self.terminal.running_cancel = None;
            cx.notify();
        }
    }
}
