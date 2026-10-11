//! The deck: a compact window that opens under the status item.
//!
//! It answers three questions and nothing else — *what is happening*, *does anything need me*, and
//! *what do I want to say*. It is not a smaller Lab: the canvas, the gauges and the trace belong to
//! a window someone has gone to, and a menu-bar popover that tried to hold them would be a window
//! with a menu bar attached.
//!
//! It borrows the Lab's visual language on purpose ([`harness_lab::theme`], [`harness_lab::ui::kit`],
//! the approval row's payload and impact readings), so the two front doors read as one product
//! rather than as two apps that agree about nothing.
//!
//! The one rule carried over unchanged is the interruption tiering: **the deck never force-opens for
//! a request that does not interrupt.** A `0.58` reading waits in the list; a `0.93` one opens the
//! deck. That rule lives in [`crate::app::open_id`], not here, so it holds whether or not a window
//! exists.

use gpui::{Context, Entity, FocusHandle, IntoElement, KeyDownEvent, MouseButton, Render, SharedString, Window, div, prelude::*, px};

use harness_capture::{Capture, FieldState};
use harness_client::OpenIntervention;
use harness_lab::theme::Theme;
use harness_lab::ui::kit;
use harness_lab::ui::overlays::{Impact, Payload, impact_of, payload_of};

use crate::app::MenubarApp;

/// A sentence in the deck. Not the Lab's `kit::caption` — that is a 10-point uppercase label for
/// equipment, and in a popover read at a glance it turns every sentence into small grey shouting.
fn prose(value: &str, theme: &Theme) -> gpui::Div {
    div().text_sm().line_height(px(19.)).text_color(theme.dim).child(SharedString::from(value.to_owned()))
}

/// The deck's size. Wide enough for a diff preview line to be readable, short enough to read as a
/// popover rather than as a window.
pub const DECK: (f32, f32) = (440.0, 600.0);

/// The deck's view. Holds only what is *about the window* — the composer's text, focus, the hotkey
/// warning. The model, the client and every decision stay in [`MenubarApp`], which outlives this
/// window; that is the whole reason the two are separate entities.
pub struct DeckView {
    app: Entity<MenubarApp>,
    theme: Theme,
    focus: FocusHandle,
    /// The composer's text and whether it has the keyboard. Typing must never be able to approve
    /// something: while the composer is focused, every printable key goes into the box.
    composer: String,
    composing: bool,
    /// Hidden once the person has started typing, so the deck's shape does not change under the
    /// cursor mid-sentence.
    hint_dismissed: bool,
    /// The deck follows the app-scoped model: a request that arrives while the deck is open has to
    /// appear, and the model is updated by the tick task, not by anything in this window. Dropping
    /// this would leave the deck drawing whatever was true when it last redrew itself. Held, never
    /// read: dropping a subscription is how it unsubscribes.
    _observed: gpui::Subscription,
}

impl DeckView {
    pub fn new(app: Entity<MenubarApp>, cx: &mut Context<Self>) -> Self {
        let observed = cx.observe(&app, |_, _, cx| cx.notify());
        // The palette follows the backdrop the window got, so one layout reads as glass either way.
        let theme = crate::deck_window::Backdrop::detect().theme(Theme::dark());
        Self { app, theme, focus: cx.focus_handle(), composer: String::new(), composing: false, hint_dismissed: false, _observed: observed }
    }

    /// A read of the app-scoped model, cloned to whatever this render needs.
    fn snapshot(&self, cx: &Context<Self>) -> Snapshot {
        self.app.read_with(cx, |app, _| Snapshot {
            state: app.tray_state(),
            status: app.status_line(),
            waiting: app.waiting(),
            hotkey: app.hotkey_error.clone(),
            notices: app.model.notices.iter().rev().take(3).map(|n| (n.text.clone(), n.error)).collect(),
            approvals: app.model.interventions.iter().take(4).cloned().collect(),
            target: app.target.as_ref().map(|target| target.name.clone()),
            attached: app.attached.clone(),
            shooting: app.shooting,
            conversations: app.model.conversations.iter().take(4).cloned().collect(),
            selected: app.conversation.clone(),
            exchanges: app.model.transcript.exchanges.iter().rev().take(2).rev().cloned().collect(),
            activity: app.model.transcript.activity.clone(),
        })
    }

    fn begin_composing(&mut self, cx: &mut Context<Self>) {
        self.composing = true;
        cx.notify();
    }

    fn end_composing(&mut self, cx: &mut Context<Self>) {
        self.composing = false;
        cx.notify();
    }

    /// Send the composer's text as a turn, with whatever capture is attached — previewed, masked, and
    /// taken so it goes with this turn only. No capture is an empty context, which the daemon reads as
    /// "nothing was shared".
    fn send(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer.clear();
        self.composing = false;
        self.hint_dismissed = true;
        let session = self.app.read_with(cx, |app, _| app.model.focus.clone().unwrap_or_default());
        self.app.update(cx, |app, cx| {
            let context = app.take_context();
            app.submit_turn(session, text, context, cx);
        });
        cx.notify();
    }

    fn type_text(&mut self, typed: &str, cx: &mut Context<Self>) {
        if self.composer.len() < 4_000 {
            self.composer.push_str(typed);
            cx.notify();
        }
    }

    fn backspace(&mut self, cx: &mut Context<Self>) {
        self.composer.pop();
        cx.notify();
    }

    /// The keys. Esc unwinds in the Lab's order: close the composer first, then the deck — so a
    /// keystroke can never take two things away at once, and Esc twice is always "done".
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        let platform = stroke.modifiers.platform;
        if self.composing {
            match stroke.key.as_str() {
                "enter" => self.send(cx),
                "escape" => self.end_composing(cx),
                "backspace" => self.backspace(cx),
                "v" if platform => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.type_text(&text, cx);
                    }
                }
                _ => {
                    if !platform
                        && !stroke.modifiers.control
                        && let Some(typed) = &stroke.key_char
                    {
                        self.type_text(typed, cx);
                    }
                }
            }
            return;
        }
        match stroke.key.as_str() {
            "escape" => self.hide(window, cx),
            "/" | "i" => self.begin_composing(cx),
            "y" => self.answer(true, false, cx),
            "n" => self.answer(false, false, cx),
            "k" => self.answer(false, true, cx),
            "r" | "enter" => self.review(cx),
            "c" => self.app.update(cx, |app, cx| app.capture_context(cx)),
            _ => {}
        }
    }

    /// Answer the oldest request that is asking for a person.
    fn answer(&mut self, approve: bool, isolate: bool, cx: &mut Context<Self>) {
        let id = self.app.read_with(cx, |app, _| app.model.interventions.front().map(|o| o.request.intervention_id.clone()));
        let Some(id) = id else { return };
        self.app.update(cx, |app, cx| app.decide(&id, approve, isolate, cx));
        cx.notify();
    }

    /// Send the oldest asking request back to the list without answering it.
    fn review(&mut self, cx: &mut Context<Self>) {
        let id = self.app.read_with(cx, |app, _| app.model.interventions.front().map(|o| o.request.intervention_id.clone()));
        if let Some(id) = id {
            self.app.update(cx, |app, cx| {
                app.send_to_tray(&id, cx);
            });
            cx.notify();
        }
    }

    /// Close the deck. The window is removed rather than hidden: a transparent popover window left
    /// alive keeps its focus and its shadow, and the app is not a window with a menu bar attached.
    fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composing = false;
        self.app.update(cx, |app, cx| {
            app.deck_open = false;
            cx.notify();
        });
        window.remove_window();
    }
}

/// What one render needs, read out of the app-scoped model in one go — so the element tree never
/// holds a borrow of the model while it dispatches an action back into it.
struct Snapshot {
    state: harness_client::deck::TrayState,
    status: Option<String>,
    waiting: usize,
    hotkey: Option<String>,
    notices: Vec<(String, bool)>,
    approvals: Vec<OpenIntervention>,
    /// The app the deck was summoned over, by name.
    target: Option<String>,
    attached: Option<Capture>,
    shooting: bool,
    /// The hub's newest conversations, the one the deck is in, and its last two turns.
    conversations: Vec<harness_proto::v1::Conversation>,
    selected: Option<String>,
    exchanges: Vec<harness_client::model::Exchange>,
    activity: Option<String>,
}

impl Render for DeckView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.composing {
            window.focus(&self.focus);
        }
        let theme = self.theme;
        let snapshot = self.snapshot(cx);
        let accent = match snapshot.state {
            harness_client::deck::TrayState::ActionRequired => theme.warn,
            harness_client::deck::TrayState::Executing => theme.accent,
            harness_client::deck::TrayState::Idle => theme.dim,
        };
        let mut root = div()
            .debug_selector(|| "deck-root".to_owned())
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.panel)
            .text_color(theme.text)
            .rounded(px(crate::deck_window::RADIUS))
            .border_1()
            .border_color(theme.line_strong)
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| this.key(event, window, cx)))
            .child(self.status_strip(&snapshot, accent, theme))
            .child(self.approvals(&snapshot, theme, cx))
            .child(self.conversation(&snapshot, theme, cx))
            .child(self.notices(&snapshot, theme))
            .child(self.context(&snapshot, theme, cx))
            .child(self.composer(theme, cx));
        if snapshot.waiting == 0 && snapshot.notices.is_empty() {
            root = root.child(self.hint(theme));
        }
        root
    }
}

impl DeckView {
    /// The status strip: the state in words, and the hotkey chip when there is no hotkey.
    fn status_strip(&self, snapshot: &Snapshot, accent: gpui::Rgba, theme: Theme) -> impl IntoElement {
        let mut strip = div()
            .debug_selector(|| "deck-status".to_owned())
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(kit::led("deck-led", accent, snapshot.state == harness_client::deck::TrayState::Executing))
            .child(kit::text(snapshot.state.label(), theme.text));
        if let Some(status) = &snapshot.status {
            strip = strip.child(prose(status, &theme));
        }
        if snapshot.waiting > 0 {
            strip = strip.child(kit::chip(format!("{} waiting", snapshot.waiting), theme.warn));
        }
        if let Some(hotkey) = &snapshot.hotkey {
            // A hotkey that could not be claimed is said here, once, in place of the keycap a person
            // would otherwise be looking for.
            strip = strip.child(div().debug_selector(|| "deck-hotkey-error".to_owned()).child(kit::chip(hotkey.clone(), theme.danger)));
        }
        strip
    }

    /// Approvals: the whole point of the deck. Only the asking requests appear; the quiet ones wait
    /// in the Lab, where a person has gone to look.
    fn approvals(&self, snapshot: &Snapshot, theme: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = div().debug_selector(|| "deck-approvals".to_owned()).flex().flex_col().gap_2().px_3().py_2();
        if snapshot.approvals.is_empty() {
            return section.child(prose("Nothing is waiting for you.", &theme));
        }
        for (index, open) in snapshot.approvals.iter().enumerate() {
            let id = open.request.intervention_id.clone();
            let asking = harness_client::deck::interrupts(&open.request);
            let risk = if open.request.risk_band.is_empty() { "unscored".to_owned() } else { open.request.risk_band.clone() };
            let color = harness_lab::theme::threat_color(open.request.risk_score, &theme);
            let request = open.request.clone();
            let stale = open.stale;
            section = section.child(
                div()
                    .debug_selector(move || format!("deck-approval-{index}"))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded(px(10.))
                    .bg(theme.card)
                    .border_1()
                    .border_color(if asking { color } else { theme.border })
                    .child(kit::text(format!("{} — {}", open.request.pod, open.request.tool), theme.text))
                    .child(prose(&summary(&request), &theme))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(kit::chip(risk, color))
                            .child(prose(if stale { "the connection it came from is gone" } else { "" }, &theme))
                            .children(buttons(&id, stale, theme, cx)),
                    ),
            );
        }
        section
    }

    /// The conversation the deck is in — the same one a terminal, Discord or the web app continues —
    /// as chips to switch between, and its last two turns: who asked, what, and the reply as it streams.
    fn conversation(&self, snapshot: &Snapshot, theme: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let mut chips = div().flex().flex_row().flex_wrap().gap_1();
        for (index, conversation) in snapshot.conversations.iter().enumerate() {
            let selected = snapshot.selected.as_deref() == Some(conversation.handle.as_str());
            let label = if conversation.title.is_empty() { conversation.handle.clone() } else { kit::truncate(&conversation.title, 22) };
            let label = if conversation.busy { format!("● {label}") } else { label };
            let handle = conversation.handle.clone();
            chips = chips.child(
                kit::button_dyn(label, if selected { theme.accent } else { theme.dim }, false, &theme, format!("deck-conversation-{index}"))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                        let handle = handle.clone();
                        this.app.update(cx, |app, cx| {
                            app.select_conversation(Some(handle), true);
                            cx.notify();
                        });
                    })),
            );
        }
        chips = chips.child(
            kit::button("+ New", if snapshot.selected.is_none() { theme.accent } else { theme.dim }, false, &theme, "deck-conversation-new").on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.app.update(cx, |app, cx| {
                        app.select_conversation(None, true);
                        cx.notify();
                    })
                }),
            ),
        );
        let mut section = div().debug_selector(|| "deck-conversation".to_owned()).flex().flex_col().gap_2().px_3().py_2().border_t_1().border_color(theme.border).child(chips);
        if snapshot.selected.is_none() {
            return section.child(prose("Your next message starts a new conversation — it will show up in the terminal, Discord and the web app too.", &theme));
        }
        if snapshot.exchanges.is_empty() {
            return section.child(prose("Nothing new here since the deck opened. Whatever anyone asks in this conversation appears here as it happens.", &theme));
        }
        for (index, exchange) in snapshot.exchanges.iter().enumerate() {
            let who = door(&exchange.origin);
            let mut turn = div().debug_selector(move || format!("deck-exchange-{index}")).flex().flex_col().gap_1();
            if !exchange.asked.is_empty() {
                turn = turn.child(prose(&format!("{who}: {}", kit::truncate(&exchange.asked, 160)), &theme));
            }
            let tail = tail_of(&exchange.reply, 700);
            if !tail.is_empty() {
                turn = turn.child(div().text_sm().line_height(px(19.)).text_color(theme.text).child(SharedString::from(tail)));
            }
            if exchange.stop.is_empty() {
                let doing = snapshot.activity.clone().unwrap_or_else(|| "working".into());
                turn = turn.child(div().text_xs().text_color(theme.accent).child(SharedString::from(format!("… {doing}"))));
            } else if exchange.stop != "end_turn" {
                turn = turn.child(div().text_xs().text_color(theme.warn).child(SharedString::from(format!("stopped: {}", exchange.stop))));
            }
            section = section.child(turn);
        }
        section
    }

    /// Notices: the three newest, because this deck is 520 points tall and a person reads the one
    /// that just happened.
    fn notices(&self, snapshot: &Snapshot, theme: Theme) -> impl IntoElement {
        let mut section = div().debug_selector(|| "deck-notices".to_owned()).flex().flex_col().gap_1().px_3();
        for (text, error) in &snapshot.notices {
            section = section.child(prose(&kit::truncate(text, 120), &theme).text_color(if *error { theme.danger } else { theme.dim }));
        }
        section
    }

    /// What would be shared with the next turn, as a checklist a person reads before sending: each
    /// field read, masked or not read — and why — so partial success looks like what it is rather than
    /// like a bug. Nothing here reads anything by itself: the Capture button (or `c`) does.
    fn context(&self, snapshot: &Snapshot, theme: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = div().debug_selector(|| "deck-context".to_owned()).flex().flex_col().gap_1().px_3().py_2().border_t_1().border_color(theme.border);
        let Some(target) = snapshot.target.clone() else {
            return section.child(prose("Open this over another app to share what you are looking at.", &theme));
        };
        let mut header = div().flex().flex_row().items_center().gap_2().child(kit::text(format!("Context · {target}"), theme.text));
        header = match &snapshot.attached {
            Some(Capture::Captured(_)) => header.child(
                kit::button("Don't send", theme.dim, false, &theme, "deck-detach").on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.app.update(cx, |app, cx| app.detach(cx)))),
            ),
            _ => header.child(
                kit::button("Capture", theme.accent, false, &theme, "deck-capture").on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.app.update(cx, |app, cx| app.capture_context(cx)))),
            ),
        };
        section = section.child(header);
        match &snapshot.attached {
            None => section.child(prose("Nothing is attached. Capture reads the title and selection, if you allow this app.", &theme)),
            Some(refused @ Capture::Refused { reason, .. }) => {
                section = section.child(div().debug_selector(|| "deck-context-refused".to_owned()).child(prose(&kit::truncate(reason, 160), &theme)));
                if let Some(bundle_id) = refused.allowable() {
                    section = section.child(
                        kit::button_dyn(format!("Always allow {target}"), theme.ok, false, &theme, "deck-allow".to_owned())
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.app.update(cx, |app, cx| app.allow_target(cx)))),
                    );
                    section = section.child(prose(&format!("Adds {bundle_id} to capture.allowlist in your own settings."), &theme));
                }
                section
            }
            Some(Capture::Captured(preview)) => {
                let mut needs_grant = false;
                for (index, field) in preview.fields.iter().enumerate() {
                    let (mark, color, detail) = match &field.state {
                        FieldState::Captured => ("✓", theme.ok, kit::truncate(&field.value, 90)),
                        FieldState::Masked(spans) => ("✓", theme.warn, format!("{} ({spans} masked)", kit::truncate(&field.value, 70))),
                        FieldState::Omitted(reason) => {
                            needs_grant |= reason.contains("Accessibility");
                            ("✗", theme.dim, reason.clone())
                        }
                    };
                    section = section.child(
                        div()
                            .debug_selector(move || format!("deck-context-field-{index}"))
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(kit::text(mark, color).text_sm())
                            .child(kit::text(field.label, theme.text).text_sm().flex_none())
                            .child(prose(&detail, &theme)),
                    );
                }
                let needs_screen = preview.omissions().iter().any(|omission| omission.starts_with("screenshot: Screen Recording"));
                let mut actions = div().flex().flex_row().gap_2();
                if needs_grant {
                    actions = actions.child(
                        kit::button("Grant Accessibility", theme.warn, false, &theme, "deck-grant")
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.app.update(cx, |app, cx| app.grant_accessibility(cx)))),
                    );
                }
                if needs_screen {
                    actions = actions.child(
                        kit::button("Grant Screen Recording", theme.warn, false, &theme, "deck-grant-screen")
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.app.update(cx, |app, cx| app.grant_screen_recording(cx)))),
                    );
                }
                if preview.shot.is_none() {
                    // A screenshot is its own decision, after the text has been seen — never part of
                    // the first capture.
                    let label = if snapshot.shooting { "Taking screenshot…" } else { "Add screenshot" };
                    actions = actions.child(
                        kit::button(label, theme.accent, false, &theme, "deck-screenshot")
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.app.update(cx, |app, cx| app.add_screenshot(cx)))),
                    );
                }
                section = section.child(actions);
                if preview.shot.is_some() {
                    let note = if preview.cloud_ok { "The image may go to a cloud model (capture.screenshotCloud)." } else { "The image goes to a local vision model only, or is not sent." };
                    section = section.child(div().debug_selector(|| "deck-screenshot-route".to_owned()).child(prose(note, &theme)));
                }
                section
            }
        }
    }

    /// The composer: the Lab has none, and a menu bar without one would be a status display rather
    /// than a door.
    fn composer(&self, theme: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let in_conversation = self.app.read(cx).conversation.is_some();
        let placeholder = match (self.composer.is_empty(), in_conversation) {
            (false, _) => "",
            (true, true) => "Continue this conversation…   (/ to focus)",
            (true, false) => "Start a new conversation…   (/ to focus)",
        };
        let color = if self.composer.is_empty() { theme.dim } else { theme.text };
        div()
            .debug_selector(|| "deck-composer".to_owned())
            .mt_auto()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(theme.border)
            .child(kit::keycap("/", &theme))
            .child(
                div()
                    .id("deck-composer-line")
                    .flex_1()
                    .truncate()
                    .text_color(color)
                    .when(self.composing, |line| line.border_b_1().border_color(theme.accent))
                    .child(SharedString::from(if self.composer.is_empty() { placeholder.to_owned() } else { self.composer.clone() })),
            )
            .child(
                kit::button("Send", if self.composer.is_empty() { theme.faint } else { theme.accent }, true, &theme, "deck-send")
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.send(cx))),
            )
    }

    fn hint(&self, theme: Theme) -> impl IntoElement {
        div()
            .debug_selector(|| "deck-hint".to_owned())
            .px_3()
            .pb_2()
            .child(prose("Click the menu bar or press ⌘⇧Space to open this from anywhere.", &theme))
    }
}

/// A door as the deck names whoever asked: "You (menu bar)" is this deck.
fn door(origin: &str) -> String {
    match origin {
        "menubar" => "You (menu bar)".into(),
        "terminal" => "Terminal".into(),
        "discord" => "Discord".into(),
        "web" => "Web app".into(),
        "" => "Someone".into(),
        other => other.to_owned(),
    }
}

/// The end of a reply, which is where a streaming answer is being written — on a char boundary, with
/// an ellipsis when something was cut.
fn tail_of(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let skip = text.chars().count() - max;
    format!("…{}", text.chars().skip(skip).collect::<String>())
}

/// The one line that says what is being asked, from the payload the Lab already computes — so the
/// deck never invents a reading of a command the Lab reads differently.
fn summary(request: &harness_proto::v1::InterventionRequest) -> String {
    match payload_of(request) {
        Payload::Shell(command) => kit::truncate(command.lines().next().unwrap_or(""), 160),
        Payload::Diff(diff) => format!("a patch of {} lines", diff.lines().count()),
        Payload::Text(text) => kit::truncate(text, 160),
    }
}

/// What the approval is about to touch, in a few words — from the Lab's own reading of it.
pub fn impact_word(request: &harness_proto::v1::InterventionRequest) -> &'static str {
    match impact_of(request) {
        Impact::Diff(_) => "touches files",
        Impact::Command { .. } => "runs a command",
        Impact::None => "cannot be read here",
    }
}

/// The three answers, as buttons. A stale request gets none: it cannot be answered from here.
fn buttons(id: &str, stale: bool, theme: Theme, cx: &mut Context<DeckView>) -> Vec<impl IntoElement> {
    if stale {
        return Vec::new();
    }
    let approve = id.to_owned();
    let reject = id.to_owned();
    let isolate = id.to_owned();
    vec![
        kit::button_dyn("Approve", theme.ok, true, &theme, "deck-approve".to_owned()).on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
            let id = approve.clone();
            this.app.update(cx, |app, cx| app.decide(&id, true, false, cx));
            cx.notify();
        })),
        kit::button_dyn("Reject", theme.warn, false, &theme, "deck-reject".to_owned()).on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
            let id = reject.clone();
            this.app.update(cx, |app, cx| app.decide(&id, false, false, cx));
            cx.notify();
        })),
        kit::button_dyn("Stop it", theme.danger, false, &theme, "deck-isolate".to_owned()).on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
            let id = isolate.clone();
            this.app.update(cx, |app, cx| app.decide(&id, false, true, cx));
            cx.notify();
        })),
    ]
}

#[cfg(test)]
mod tests {
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use harness_proto::v1::{InterventionEvent, InterventionRequest, intervention_event};

    use super::*;
    use crate::app::MenubarApp;

    /// A deck on its own window, with an app-scoped model and no daemon at all.
    fn deck(cx: &mut TestAppContext) -> (Entity<MenubarApp>, Entity<DeckView>, &mut VisualTestContext) {
        let app = cx.update(|cx| cx.new(|_| MenubarApp::blank()));
        let root = app.clone();
        let (view, cx) = cx.add_window_view(move |_window, cx| DeckView::new(root.clone(), cx));
        (app, view, cx)
    }

    fn held(id: &str, risk: f64) -> InterventionEvent {
        InterventionEvent {
            event: Some(intervention_event::Event::Request(InterventionRequest {
                intervention_id: id.into(),
                pod: "reviewer".into(),
                tool: "shell".into(),
                command: "rm -rf build".into(),
                risk_score: Some(risk),
                risk_band: "elevated".into(),
                ..Default::default()
            })),
        }
    }

    #[gpui::test]
    fn a_quiet_deck_says_so_and_offers_a_way_to_speak(cx: &mut TestAppContext) {
        let (_, _, cx) = deck(cx);
        cx.run_until_parked();
        assert!(cx.debug_bounds("deck-root").is_some(), "the deck draws at all");
        assert!(cx.debug_bounds("deck-status").is_some(), "with the state in words");
        assert!(cx.debug_bounds("deck-composer").is_some(), "and a way to send a turn");
        assert!(cx.debug_bounds("deck-hint").is_some(), "and how to call it back");
    }

    #[gpui::test]
    fn a_waiting_request_gets_a_row_with_the_three_answers(cx: &mut TestAppContext) {
        let (app, _, cx) = deck(cx);
        app.update(cx, |app, cx| {
            app.model.apply(harness_client::model::ClientEvent::Intervention(held("q/1", 0.93)), std::time::Instant::now());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("deck-approval-0").is_some(), "the request is on screen");
        assert!(cx.debug_bounds("button-deck-approve").is_some(), "with approve");
        assert!(cx.debug_bounds("button-deck-reject").is_some(), "reject");
        assert!(cx.debug_bounds("button-deck-isolate").is_some(), "and a way to stop it");
        assert!(cx.debug_bounds("deck-hint").is_none(), "nothing is quiet enough to need a hint");
    }

    #[gpui::test]
    fn a_hotkey_that_could_not_be_claimed_is_on_screen_not_in_a_log(cx: &mut TestAppContext) {
        let (app, _, cx) = deck(cx);
        app.update(cx, |app, cx| {
            app.hotkey_error = Some("no summon hotkey is available".into());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("deck-hotkey-error").is_some(), "the failure is visible where a person is looking");
    }

    #[gpui::test]
    fn escape_closes_the_deck_and_says_so_in_the_model(cx: &mut TestAppContext) {
        let (app, _, cx) = deck(cx);
        app.update(cx, |app, _| app.deck_open = true);
        cx.simulate_keystrokes("escape");
        // No `cx.update` past this point: the window it would reach for is the one that just went.
        let open = cx.read(|cx| app.read(cx).deck_open);
        assert!(!open, "the model agrees the deck is gone");
        assert_eq!(cx.windows().len(), 0, "and the window went with it");
    }

    #[gpui::test]
    fn escape_closes_the_composer_before_it_closes_the_deck(cx: &mut TestAppContext) {
        let (app, _, cx) = deck(cx);
        app.update(cx, |app, _| app.deck_open = true);
        cx.simulate_keystrokes("/");
        cx.simulate_input("hi");
        cx.simulate_keystrokes("escape");
        // One Esc takes one thing away: the composer's hold on the keyboard, not the deck.
        let open = cx.read(|cx| app.read(cx).deck_open);
        assert!(open, "the deck is still up after the first escape");
        assert_eq!(cx.windows().len(), 1, "and its window is still there");
    }

    #[gpui::test]
    fn the_context_checklist_follows_the_capture(cx: &mut TestAppContext) {
        let (app, _, cx) = deck(cx);
        cx.run_until_parked();
        assert!(cx.debug_bounds("deck-context").is_some(), "the section is always there, so a person knows it exists");
        assert!(cx.debug_bounds("button-deck-capture").is_none(), "no target, nothing to capture");

        app.update(cx, |app, cx| {
            app.source = std::rc::Rc::new(harness_capture::fake::FakeCaptureSource::new("Safari", "com.apple.Safari").with_title("Inbox").refusing_selection(harness_capture::Refusal::NotPermitted));
            app.note_target(Some(harness_capture::Frontmost { name: "Safari".into(), bundle_id: "com.apple.Safari".into(), pid: 4242 }));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("button-deck-capture").is_some(), "summoned over an app: Capture is offered");

        cx.simulate_keystrokes("c");
        cx.run_until_parked();
        assert!(cx.debug_bounds("deck-context-refused").is_some(), "off by default, and it says so");
        assert!(cx.debug_bounds("button-deck-allow").is_some(), "with the one click that allows this app");

        app.update(cx, |app, cx| {
            app.capture.enabled = true;
            app.capture.allowlist.allow("com.apple.Safari");
            app.capture_context(cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("deck-context-field-0").is_some(), "the title, read");
        assert!(cx.debug_bounds("deck-context-field-1").is_some(), "the selection, not read — with its reason");
        assert!(cx.debug_bounds("button-deck-grant").is_some(), "and the switch that would let it be read");
        assert!(cx.debug_bounds("button-deck-detach").is_some(), "and a way not to send it");
        assert!(cx.debug_bounds("button-deck-screenshot").is_some(), "a screenshot is offered as its own step");

        app.update(cx, |app, cx| {
            app.shooter = std::sync::Arc::new(|_| Ok(harness_capture::Shot { jpeg: vec![0xFF, 0xD8, 0xFF], width: 640, height: 400 }));
            app.add_screenshot(cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("deck-screenshot-route").is_some(), "with where the image may go, said before it is sent");
        // Absence is read from the model: a vanished element's bounds can linger in the test window.
        let shot = cx.read(|cx| app.read(cx).attached.as_ref().and_then(|capture| capture.preview()).and_then(|preview| preview.shot.clone()));
        assert!(shot.is_some(), "the image is attached, so the button that offers one is not drawn");
    }

    #[gpui::test]
    fn the_deck_is_in_a_conversation_and_shows_who_asked_what(cx: &mut TestAppContext) {
        use harness_client::model::{ClientEvent, Reply};
        use harness_proto::v1::{Conversation, ConversationEvent, ConversationList};

        let (app, _, cx) = deck(cx);
        app.update(cx, |app, cx| {
            let list = ConversationList { conversations: vec![Conversation { handle: "c3f9a1e".into(), title: "fix the flaky test".into(), ..Default::default() }] };
            app.model.apply(ClientEvent::Reply(Reply::Conversations(list)), std::time::Instant::now());
            app.select_conversation(Some("c3f9a1e".into()), false);
            let event = |kind: &str| ConversationEvent { handle: "c3f9a1e".into(), kind: kind.into(), ..Default::default() };
            app.model.apply(ClientEvent::Conversation(ConversationEvent { origin: "discord".into(), text: "why is it flaky?".into(), ..event("turn") }), std::time::Instant::now());
            app.model.apply(ClientEvent::Conversation(ConversationEvent { delta: "Because the clock is not mocked.".into(), ..event("text") }), std::time::Instant::now());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("deck-conversation").is_some(), "the conversation is on the deck");
        assert!(cx.debug_bounds("button-deck-conversation-0").is_some(), "with a chip for it");
        assert!(cx.debug_bounds("button-deck-conversation-new").is_some(), "and a way to start a new one");
        assert!(cx.debug_bounds("deck-exchange-0").is_some(), "the turn Discord started is drawn here, streaming");

        let new = cx.debug_bounds("button-deck-conversation-new").expect("the + New chip");
        cx.simulate_click(new.center(), gpui::Modifiers::default());
        let chosen = cx.read(|cx| (app.read(cx).conversation.clone(), app.read(cx).chose_new));
        assert_eq!(chosen, (None, true), "'+ New' means the next message starts a new conversation");
    }

    #[test]
    fn a_long_reply_shows_its_end_where_it_is_being_written() {
        assert_eq!(tail_of("short", 10), "short");
        let tail = tail_of(&"é".repeat(50), 10);
        assert!(tail.starts_with('…') && tail.chars().count() == 11, "{tail}");
        assert_eq!(door("menubar"), "You (menu bar)");
    }

    #[test]
    fn the_impact_of_a_command_is_a_word_a_person_reads() {
        let request = InterventionRequest { command: "rm -rf build".into(), ..Default::default() };
        assert_eq!(impact_word(&request), "runs a command");
        let diff = InterventionRequest { diff: "--- a\n+++ b\n@@\n-x\n+y\n".into(), ..Default::default() };
        assert_eq!(impact_word(&diff), "touches files");
    }
}
