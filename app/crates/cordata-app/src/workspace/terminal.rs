//! The Terminal pane in the dock: a real terminal (`crate::vt`) running your login shell in the project. Full-screen
//! programs work; the agent's sandbox does not apply (this is your shell, like Terminal.app) and the agent never types
//! into it. Keys go to the shell except ⌘-shortcuts, which stay the window's; ⌘V pastes, the wheel scrolls the history.


use std::time::{Duration, Instant};

use alacritty_terminal::event::Event;
use alacritty_terminal::term::TermMode;
use cordata_theme::Role;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, FocusHandle, FontStyle, FontWeight, HighlightStyle, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Pixels, ScrollWheelEvent, SharedString, StatefulInteractiveElement as _, StrikethroughStyle, Styled, StyledText, UnderlineStyle,
    Window, canvas, div, px, rgb,
};
use gpui_component::button::{Button, ButtonVariants as _};

use super::Workspace;
use crate::detect;
use crate::theme::color;
use crate::vt::{self, Mods, Palette, Session};

pub struct Terminal {
    session: Option<Session>,
    pub focus: FocusHandle,
    /// What the program set as its title (OSC 0/2), shown in the strip.
    title: Option<String>,
    /// The shell exited: its code (`None`: a signal), until Restart.
    exited: Option<Option<i32>>,
    error: Option<String>,
    /// Wheel movement not yet a whole line.
    scroll_rest: f32,
    /// Bumped by each new shell, so a finished shell's last events cannot touch the next one.
    generation: u64,
    rules: detect::Rules,
    /// Your detect.toml did not parse (the built-in rules are used); shown in the strip.
    pub rules_warning: Option<String>,
    /// The agent in the foreground and what it is doing.
    pub detection: Option<detect::Detection>,
    last_output: Option<Instant>,
    last_input: Option<Instant>,
    ticking: bool,
}

/// Output this recent counts as an agent working, unless it is the echo of a key.
const ACTIVE_FOR: Duration = Duration::from_millis(1500);
const ECHO_WITHIN: Duration = Duration::from_millis(300);
const DETECT_EVERY: Duration = Duration::from_millis(600);

/// What `cordata-app read` reports.
pub(super) struct TerminalText {
    pub title: Option<String>,
    pub lines: Vec<String>,
    /// The shell's exit code once it ended (`Some(None)`: a signal).
    pub exited: Option<Option<i32>>,
}

/// Grid metrics for the current font: one cell's width and height.
#[derive(Clone, Copy)]
struct Metrics {
    cell_w: Pixels,
    line_h: Pixels,
    size: Pixels,
}

impl Terminal {
    pub fn new(_window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let (rules, rules_warning) = detect::Rules::with_user(detect::user_file().as_deref());
        Self {
            session: None,
            focus: cx.focus_handle(),
            title: None,
            exited: None,
            error: None,
            scroll_rest: 0.0,
            generation: 0,
            rules,
            rules_warning,
            detection: None,
            last_output: None,
            last_input: None,
            ticking: false,
        }
    }

    /// Output that is not the echo of a key, just now.
    fn active(&self, now: Instant) -> bool {
        let Some(output) = self.last_output else { return false };
        let echo = self.last_input.is_some_and(|input| output >= input && output.duration_since(input) < ECHO_WITHIN);
        now.duration_since(output) < ACTIVE_FOR && !echo
    }

    /// The terminal's agent is waiting for you.
    pub fn blocked(&self) -> bool {
        self.detection.as_ref().is_some_and(|d| d.state == detect::State::Blocked)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            session.shutdown();
        }
    }
}

fn hsla(value: u32) -> gpui::Hsla {
    rgb(value).into()
}

fn palette(cx: &gpui::App) -> Palette {
    let p = crate::theme::palette(cx);
    let palette = Palette::new(p.get(Role::Text).to_u32(), p.get(Role::Ground).to_u32(), p.get(Role::Accent).to_u32());
    if crate::theme::tk(cx).is_dark { palette } else { palette.for_light() }
}

fn metrics(cx: &gpui::App) -> Metrics {
    let size = px(crate::theme::code_size(cx));
    let font = gpui::font(crate::theme::mono(cx));
    let text = cx.text_system();
    let id = text.resolve_font(&font);
    let cell_w = text.advance(id, size, 'm').map_or(size * 0.6, |a| a.width);
    Metrics { cell_w, line_h: (size * 1.3).round(), size }
}

fn highlight(run: &vt::Run) -> HighlightStyle {
    HighlightStyle {
        color: Some(hsla(run.fg)),
        background_color: run.bg.map(hsla),
        font_weight: run.bold.then_some(FontWeight::BOLD),
        font_style: run.italic.then_some(FontStyle::Italic),
        underline: run.underline.then(|| UnderlineStyle { thickness: px(1.), color: Some(hsla(run.fg)), wavy: false }),
        strikethrough: run.strike.then(|| StrikethroughStyle { thickness: px(1.), color: Some(hsla(run.fg)) }),
        ..Default::default()
    }
}

impl Workspace {
    /// Starts the shell the first time the pane is shown, and again after Restart.
    pub(super) fn ensure_terminal(&mut self, cx: &mut Context<Self>) {
        if self.terminal.session.is_some() {
            return;
        }
        match Session::spawn(vt::Launch::login_shell(&self.cwd)) {
            Ok((session, mut events)) => {
                self.terminal.session = Some(session);
                self.terminal.exited = None;
                self.terminal.error = None;
                self.terminal.detection = None;
                self.terminal.generation += 1;
                let generation = self.terminal.generation;
                cx.spawn(async move |this, cx| {
                    while let Some(event) = events.recv().await {
                        // A burst of output is one redraw.
                        let mut batch = vec![event];
                        while let Ok(more) = events.try_recv() {
                            batch.push(more);
                        }
                        if this.update(cx, |this, cx| this.on_terminal_events(generation, batch, cx)).is_err() {
                            break;
                        }
                    }
                })
                .detach();
                self.watch_terminal(cx);
            }
            Err(error) => self.terminal.error = Some(format!("the shell could not start: {error}")),
        }
        cx.notify();
    }

    /// Looks at the terminal now and then: which agent runs there and what it is doing (`detect.rs`).
    fn watch_terminal(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.terminal.ticking, true) {
            return;
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(DETECT_EVERY).await;
                if this.update(cx, |this, cx| this.refresh_detection(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn refresh_detection(&mut self, cx: &mut Context<Self>) {
        let detection = match &self.terminal.session {
            Some(session) if self.terminal.exited.is_none() => session.foreground().and_then(|argv| {
                let lines = session.text();
                let view = detect::View { title: self.terminal.title.as_deref(), lines: &lines, active: self.terminal.active(Instant::now()) };
                self.terminal.rules.detect(&argv, view)
            }),
            _ => None,
        };
        if detection == self.terminal.detection {
            return;
        }
        let was_blocked = self.terminal.blocked();
        self.terminal.detection = detection;
        if self.terminal.blocked() && !was_blocked {
            let shown = self.panel_open && self.dock_tab == super::DockTab::Terminal && cx.active_window().is_some();
            if !shown && self.prefs.notify_approval
                && let Some(d) = &self.terminal.detection
            {
                crate::platform::notify(&format!("{} needs you", d.name), "It is waiting for an answer in Cordata's terminal.");
            }
        }
        cx.notify();
    }

    /// The terminal's title, its rows as text, and its exit code if the shell ended (`cordata-app read`).
    pub(super) fn terminal_text(&self, cx: &gpui::App) -> Option<TerminalText> {
        let session = self.terminal.session.as_ref()?;
        let screen = session.screen(&palette(cx));
        let mut lines: Vec<String> = screen.rows.into_iter().map(|r| r.text).collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        Some(TerminalText { title: self.terminal.title.clone(), lines, exited: self.terminal.exited })
    }

    /// Types `text` into the running shell; false when none runs.
    pub(super) fn terminal_type(&self, text: &str) -> bool {
        match &self.terminal.session {
            Some(session) if self.terminal.exited.is_none() => {
                session.write(text.as_bytes().to_vec());
                true
            }
            _ => false,
        }
    }

    fn on_terminal_events(&mut self, generation: u64, events: Vec<Event>, cx: &mut Context<Self>) {
        if generation != self.terminal.generation {
            return;
        }
        for event in events {
            match event {
                Event::Wakeup => self.terminal.last_output = Some(Instant::now()),
                Event::Title(title) => self.terminal.title = Some(shown(&title)),
                Event::ResetTitle => self.terminal.title = None,
                // Replies the program asked for (device attributes, cursor position).
                Event::PtyWrite(text) => {
                    if let Some(session) = &self.terminal.session {
                        session.write(text.into_bytes());
                    }
                }
                Event::TextAreaSizeRequest(format) => {
                    if let Some(session) = &self.terminal.session {
                        let (cols, lines) = session.size();
                        let m = metrics(cx);
                        let size = alacritty_terminal::event::WindowSize {
                            num_cols: cols,
                            num_lines: lines,
                            cell_width: f32::from(m.cell_w) as u16,
                            cell_height: f32::from(m.line_h) as u16,
                        };
                        session.write(format(size).into_bytes());
                    }
                }
                // OSC 52 copy: a program may put text on the clipboard. It may not read it (ClipboardLoad is ignored).
                Event::ClipboardStore(_, text) => cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)),
                Event::ChildExit(status) => self.terminal.exited = Some(status.code()),
                Event::Exit if self.terminal.exited.is_none() => self.terminal.exited = Some(None),
                _ => {}
            }
        }
        cx.notify();
    }

    pub(super) fn restart_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.terminal.session.take() {
            session.shutdown();
        }
        self.terminal.title = None;
        self.ensure_terminal(cx);
        { let focus = self.terminal.focus.clone(); window.focus(&focus, cx); }
    }

    fn terminal_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.terminal.session.clone() else { return };
        if self.terminal.exited.is_some() {
            return;
        }
        let k = &event.keystroke;
        // What a key makes the program print is its echo, not the agent working.
        self.terminal.last_input = Some(Instant::now());
        if k.modifiers.platform {
            // ⌘V pastes; every other ⌘ shortcut stays the window's.
            if k.key == "v"
                && let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
            {
                session.paste(&text);
                cx.stop_propagation();
            }
            return;
        }
        let mods = Mods { shift: k.modifiers.shift, alt: k.modifiers.alt, ctrl: k.modifiers.control };
        if let Some(bytes) = vt::encode_key(&k.key, k.key_char.as_deref(), mods, session.mode()) {
            session.write(bytes);
            cx.stop_propagation();
        }
    }

    fn terminal_wheel(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.terminal.session.clone() else { return };
        let m = metrics(cx);
        self.terminal.scroll_rest += f32::from(event.delta.pixel_delta(m.line_h).y) / f32::from(m.line_h);
        let lines = self.terminal.scroll_rest.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.terminal.scroll_rest -= lines as f32;
        let mode = session.mode();
        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) && !mode.intersects(TermMode::MOUSE_MODE) {
            // A full-screen program with no mouse support (less, man): the wheel is its arrow keys.
            let key = if lines > 0 { "up" } else { "down" };
            for _ in 0..lines.unsigned_abs().min(20) {
                if let Some(bytes) = vt::encode_key(key, None, Mods::default(), mode) {
                    session.write(bytes);
                }
            }
        } else {
            session.scroll(lines);
        }
        cx.notify();
    }

    pub(super) fn terminal_panel(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let term = &self.terminal;
        let palette = palette(cx);
        let m = metrics(cx);
        let exit_badge = term.exited.map(|code| match code {
            Some(0) => ("exited", Role::Ok),
            Some(_) | None => ("exited", Role::Danger),
        });
        let status_strip = div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(4.))
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_xs()
                    .font_family(crate::theme::mono(cx))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(color(cx, Role::TextDim))
                            .child(SharedString::from(term.title.clone().unwrap_or_else(|| shown(&super::tilde(&self.cwd))))),
                    )
                    .when_some(exit_badge, |el, (label, role)| {
                        let code = term.exited.flatten().map_or_else(|| "signal".to_owned(), |c| c.to_string());
                        el.child(div().font_weight(FontWeight::BOLD).text_color(color(cx, role)).child(SharedString::from(format!("{label} {code}"))))
                    })
                    // The agent running here, as herdr shows it: Claude Code · blocked.
                    .when_some(term.detection.clone(), |el, d| {
                        let role = match d.state {
                            detect::State::Blocked => Role::Attention,
                            detect::State::Working => Role::Accent,
                            detect::State::Idle => Role::TextSecondary,
                        };
                        el.child(
                            div()
                                .id("term-agent")
                                .flex_shrink_0()
                                .font_weight(FontWeight::BOLD)
                                .text_color(color(cx, role))
                                .child(SharedString::from(format!("{} · {}", d.name, d.state.label()))),
                        )
                    })
                    .when_some(term.rules_warning.clone(), |el, warning| {
                        el.child(div().id("term-rules-warning").flex_shrink_0().text_color(color(cx, Role::Danger)).child("detect.toml ✗").tooltip(crate::ui::tip(warning)))
                    }),
            )
            .child(
                Button::new("term-restart-btn")
                    .label(if term.exited.is_some() || term.error.is_some() { "Restart" } else { "New shell" })
                    .ghost()
                    .on_click(cx.listener(|this, _, window, cx| this.restart_terminal(window, cx))),
            );

        let focused = term.focus.is_focused(window);
        let screen = term.session.as_ref().map(|s| s.screen(&palette));
        let rows: Vec<AnyElement> = screen
            .as_ref()
            .map(|screen| {
                screen
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(index, row)| {
                        let row = match screen.cursor {
                            Some((line, col)) if line == index && focused => vt::with_cursor(row, col, palette.background, palette.cursor, palette.foreground),
                            _ => row.clone(),
                        };
                        let text = if row.text.is_empty() { " ".to_owned() } else { row.text.clone() };
                        let styled = StyledText::new(SharedString::from(text)).with_highlights(row.runs.iter().map(|r| (r.range.clone(), highlight(r))));
                        div().h(m.line_h).whitespace_nowrap().overflow_hidden().child(styled).into_any_element()
                    })
                    .collect()
            })
            .unwrap_or_default();

        let session = term.session.clone();
        let sizer = canvas(
            move |bounds, _, _| {
                // Fit the terminal to the pane: whole cells only.
                if let Some(session) = &session {
                    let cols = (f32::from(bounds.size.width) / f32::from(m.cell_w)).floor().max(2.0) as u16;
                    let lines = (f32::from(bounds.size.height) / f32::from(m.line_h)).floor().max(1.0) as u16;
                    session.resize(cols, lines, (f32::from(m.cell_w) as u16, f32::from(m.line_h) as u16));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();

        let grid = div()
            .id("term-grid")
            .key_context("CordataTerminal")
            .track_focus(&term.focus)
            .on_key_down(cx.listener(Self::terminal_key))
            .on_scroll_wheel(cx.listener(Self::terminal_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                this.ensure_terminal(cx);
                { let focus = this.terminal.focus.clone(); window.focus(&focus, cx); }
            }))
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .p(px(8.))
            .bg(hsla(palette.background))
            .text_color(hsla(palette.foreground))
            .font_family(crate::theme::mono(cx))
            .text_size(m.size)
            .line_height(m.line_h)
            .rounded(px(cordata_theme::tokens::RADIUS_CARD))
            .cursor_text()
            .child(div().relative().size_full().overflow_hidden().child(sizer).children(rows))
            .when_some(term.error.clone(), |el, error| el.child(div().text_color(color(cx, Role::Danger)).child(SharedString::from(error))))
            .when(term.session.is_none() && term.error.is_none(), |el| {
                el.child(div().text_color(color(cx, Role::TextDim)).child("Click to start a shell in the project."))
            });

        div()
            .id("terminal-dock-pane")
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(status_strip)
            .child(grid)
            .into_any_element()
    }
}
