//! One conversation: the shared reducer (`cordata_view::app::App`) fed by acp-core, and its effects carried out.
//!
//! The UI thread never blocks: acp-core runs on the tokio runtime ([`Runtime`]), its events arrive on an unbounded
//! channel that a GPUI task drains (a burst costs one redraw), and every effect is a non-blocking send or a
//! background job whose result comes back as a `LocalEvent`. Dropping the `Session` drops the handle, which closes
//! the connection; an approval that was never answered is denied by acp-core when its sender drops.

use std::path::PathBuf;
use std::sync::Arc;

use acp_core::{AppEvent, BackendTransport, SessionHandle, SessionOptions};
use cordata_view::app::{App as View, Effect, LocalEvent, Message};
use cordata_view::shell;
use gpui::{Context, Global};
use tokio::sync::mpsc;

/// The process's tokio runtime: acp-core's agent loop, background shell jobs. Two workers are
/// plenty — everything on it is I/O.
pub struct Runtime(pub Arc<tokio::runtime::Runtime>);

impl Global for Runtime {}

impl Runtime {
    pub fn new() -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("cordata-io").enable_all().build()?;
        Ok(Self(Arc::new(runtime)))
    }
}

/// How this app reaches the agent: in this process (`acp_core::native`).
#[derive(Clone)]
pub struct Connection {
    pub transport: BackendTransport,
    /// What to say when there is no way to connect (shown instead of a transcript).
    pub problem: Option<String>,
}

impl Connection {
    pub fn discover() -> Self {
        Self { transport: BackendTransport::Native, problem: None }
    }

    pub fn describe(&self) -> &'static str {
        "native"
    }
}

pub struct Session {
    pub view: View,
    handle: Option<SessionHandle>,
    pub cwd: PathBuf,
    pub transport: &'static str,
    /// Bumped on every change, so a view can tell its cached layout is stale.
    pub version: u64,
    /// Images and files going out with the prompt being submitted right now (`submit_with`).
    outgoing: Vec<acp_core::Attachment>,
    local: mpsc::UnboundedSender<LocalEvent>,
    /// The saved conversation this thread was told to continue, until the engine confirms it. The engine first
    /// announces a fresh id and only then swaps in the resumed one; the window's list of open threads (`threads.rs`)
    /// must name the conversation throughout, not the placeholder.
    resuming: Option<String>,
}

impl Session {
    pub fn start(connection: &Connection, cwd: PathBuf, initial_prompt: Option<String>, model: Option<String>, cx: &mut Context<Self>) -> Self {
        // Resolved once, so the panes, the worktree rail and git agree on which folder a thread belongs to.
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        let (local_tx, mut local_rx) = mpsc::unbounded_channel::<LocalEvent>();
        let mut view = View::new();
        view.attach_store(cordata_view::settings::Store::discover(&cwd));
        view.initial_prompt = initial_prompt;
        // The engine starts in the user file's `chat.mode`; the view shows the same from the start.
        view.mode = acp_core::policy::Policy::load(&cwd).mode;
        let mut session = Self { view, handle: None, cwd: cwd.clone(), transport: connection.describe(), version: 0, outgoing: Vec::new(), local: local_tx, resuming: None };
        if let Some(problem) = &connection.problem {
            session.view.messages.push(Message::Error(problem.clone()));
            return session;
        }

        let runtime = cx.global::<Runtime>().0.clone();
        let (handle, mut events) = {
            // acp-core spawns its connection task with a bare `tokio::spawn`: it needs this runtime entered.
            let _guard = runtime.enter();
            SessionHandle::spawn(SessionOptions {
                cwd,
                transport: connection.transport.clone(),
                debug_log: std::env::var_os("CORDATA_APP_DEBUG_LOG").map(PathBuf::from),
                model,
                history: true,
            })
        };
        session.handle = Some(handle);

        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                let mut batch = vec![event];
                while let Ok(more) = events.try_recv() {
                    batch.push(more);
                }
                if this.update(cx, |session, cx| session.apply(batch, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            while let Some(event) = local_rx.recv().await {
                if this
                    .update(cx, |session, cx| {
                        session.view.on_local(event);
                        session.settle(Vec::new(), cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        session
    }

    /// The id of the conversation file this thread writes to, once known (`None` before the engine has said).
    pub fn saved_id(&self) -> Option<&str> {
        self.resuming.as_deref().or_else(|| self.view.session.as_ref().map(|s| s.session_id.as_str()))
    }

    /// Continue the saved conversation `id` in this (new) thread.
    pub fn resume(&mut self, id: String, cx: &mut Context<Self>) {
        self.resuming = Some(id.clone());
        self.settle(vec![Effect::Resume(id)], cx);
    }

    fn apply(&mut self, events: Vec<AppEvent>, cx: &mut Context<Self>) {
        for event in events {
            // The engine answers a resume with the conversation or with why not (it is open in another window).
            if matches!(event, AppEvent::ResumeRefused { .. }) {
                self.resuming = None;
            }
            self.view.on_event(event)
        }
        // Confirmed: the engine now reports the conversation itself.
        if self.resuming.is_some() && self.resuming.as_deref() == self.view.session.as_ref().map(|s| s.session_id.as_str()) {
            self.resuming = None;
        }
        self.settle(Vec::new(), cx);
    }

    /// Mirror the composer's text into the reducer so its `/` and `@` menus follow what is typed.
    pub fn mirror_input(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.view.input.text() != text {
            self.view.input.set_text(text);
            self.view.refresh_menu();
            cx.notify();
        }
    }

    pub fn menu_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.view.menu_move(delta);
        cx.notify();
    }

    pub fn menu_dismiss(&mut self, cx: &mut Context<Self>) {
        self.view.menu_dismiss();
        cx.notify();
    }

    /// Accept the highlighted menu choice; returns the composer's new text.
    pub fn menu_accept(&mut self, run: bool, cx: &mut Context<Self>) -> String {
        let effect = self.view.menu_accept(run);
        let text = self.view.input.text().to_owned();
        self.settle(effect.into_iter().collect(), cx);
        text
    }

    /// ↑/↓ in an empty or single-line composer: walk the prompts sent before. Returns the composer's new text.
    pub fn history(&mut self, back: bool, cx: &mut Context<Self>) -> String {
        if back {
            self.view.history_back();
        } else {
            self.view.history_forward();
        }
        cx.notify();
        self.view.input.text().to_owned()
    }

    pub fn toggle_tools(&mut self, cx: &mut Context<Self>) {
        self.view.expand_tools = !self.view.expand_tools;
        self.version += 1;
        cx.notify();
    }

    pub fn toggle_thoughts(&mut self, cx: &mut Context<Self>) {
        self.view.thoughts_expanded = !self.view.thoughts_expanded;
        self.version += 1;
        cx.notify();
    }

    /// After anything changed: what the reducer wants to start on its own (an auto-resume, the first prompt, the next
    /// queued one), then the effects, then a redraw.
    fn settle(&mut self, mut effects: Vec<Effect>, cx: &mut Context<Self>) {
        effects.extend(self.view.take_initial_prompt());
        effects.extend(self.view.take_queued());
        if std::mem::take(&mut self.view.attention) {
            cx.emit(SessionEvent::NeedsAttention);
        }
        for effect in effects {
            self.run(effect, cx);
        }
        self.version += 1;
        cx.notify();
    }

    /// What the composer sends (a prompt, a `/command`, `!shell`, …), exactly as the terminal would treat it.
    pub fn submit(&mut self, text: &str, cx: &mut Context<Self>) {
        let effects = self.view.submit_text(text).into_iter().collect();
        self.settle(effects, cx);
    }

    /// A prompt with attachments. They go only with a prompt sent now: if the text was a command, or the prompt was
    /// queued behind a running turn, they are dropped (the window refuses both before calling this).
    pub fn submit_with(&mut self, text: &str, attachments: Vec<acp_core::Attachment>, cx: &mut Context<Self>) {
        self.outgoing = attachments;
        self.submit(text, cx);
        self.outgoing.clear();
    }

    pub fn answer(&mut self, decision: acp_core::ApprovalDecision, cx: &mut Context<Self>) {
        self.view.answer(decision);
        self.settle(Vec::new(), cx);
    }

    /// `ask` ↔ `auto` (Shift+Tab, or the composer's mode chip): shown at once, applied by the engine from its next call.
    pub fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        let effect = self.view.cycle_mode();
        self.settle(vec![effect], cx);
    }

    /// Pins or clears `auto` mode's relative write-scope prefixes on this thread.
    pub fn set_scope(&mut self, scope: Vec<String>, cx: &mut Context<Self>) {
        let effect = self.view.set_scope(scope);
        self.settle(vec![effect], cx);
    }

    /// The model switcher: same path as `/model <name>`, so the reducer's checks apply.
    pub fn set_model(&mut self, model: &str, cx: &mut Context<Self>) {
        self.submit(&format!("/model {model}"), cx);
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.settle(vec![Effect::Cancel], cx);
    }

    fn run(&mut self, effect: Effect, cx: &mut Context<Self>) {
        let sent = match effect {
            Effect::SendPrompt(text) => {
                let attachments = std::mem::take(&mut self.outgoing);
                if attachments.is_empty() { self.with_handle(|h| h.send_prompt(text)) } else { self.with_handle(|h| h.send_prompt_with(text, attachments)) }
            }
            Effect::Cancel => self.with_handle(SessionHandle::cancel),
            Effect::Resume(wanted) => self.with_handle(|h| h.resume(wanted)),
            Effect::Forget => self.with_handle(SessionHandle::forget),
            Effect::SetModel(model) => {
                let sent = self.with_handle(|h| h.set_model(model.clone()));
                if sent.is_ok() {
                    cx.emit(SessionEvent::ModelChosen(model));
                }
                sent
            }
            Effect::SetMode(mode) => self.with_handle(|h| h.set_mode(mode)),
            Effect::SetScope(scope) => self.with_handle(|h| h.set_scope(scope)),
            Effect::Copy(text) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                Ok(())
            }
            Effect::Shell { command, share, as_diff } => {
                let (cwd, tx) = (self.view.project_dir(), self.local.clone());
                cx.global::<Runtime>().0.spawn_blocking(move || {
                    let outcome = shell::run(&command, &cwd, shell::TIMEOUT);
                    let _ = tx.send(LocalEvent::ShellDone { command, outcome, share, as_diff });
                });
                Ok(())
            }
            Effect::ClearScreen => {
                self.view.messages.clear();
                Ok(())
            }
            Effect::Quit => {
                cx.quit();
                Ok(())
            }
            // Multi-session, conversation switching and the diff review are the window's (docs/app.md §4); until
            // they land here, say so instead of doing nothing.
            // Window-level: the workspace owns the tabs and the rail.
            Effect::NewSession => {
                cx.emit(SessionEvent::Window(WindowRequest::New));
                Ok(())
            }
            Effect::SwitchSession(target) => {
                cx.emit(SessionEvent::Window(WindowRequest::Switch(target)));
                Ok(())
            }
            Effect::CloseSession => {
                cx.emit(SessionEvent::Window(WindowRequest::Close));
                Ok(())
            }
            Effect::Review => {
                cx.emit(SessionEvent::Window(WindowRequest::Review));
                Ok(())
            }
        };
        if let Err(error) = sent {
            self.view.messages.push(Message::Error(format!("session unavailable: {error}")));
        }
    }

    fn with_handle(&self, f: impl FnOnce(&SessionHandle) -> acp_core::Result<()>) -> acp_core::Result<()> {
        match &self.handle {
            Some(handle) => f(handle),
            None => Err(acp_core::Error::SessionClosed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// An approval or a finished turn wants the person (bounce the Dock icon, badge the rail).
    NeedsAttention,
    /// A `/new`, `/close`, `/review` or session switch: the window's to carry out.
    Window(WindowRequest),
    /// The person picked a model: new threads start with it too (the window keeps it in prefs).
    ModelChosen(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRequest {
    New,
    Close,
    Review,
    Switch(cordata_view::app::SwitchTarget),
}

impl gpui::EventEmitter<SessionEvent> for Session {}
