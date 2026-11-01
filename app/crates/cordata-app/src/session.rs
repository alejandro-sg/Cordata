//! One conversation: the shared reducer (`cordata_view::app::App`) fed by acp-core, and its effects carried out.
//!
//! The UI thread never blocks: acp-core runs on the tokio runtime ([`Runtime`]), its events arrive on an unbounded
//! channel that a GPUI task drains (a burst costs one redraw), and every effect is a non-blocking send or a
//! background job whose result comes back as a `LocalEvent`. Dropping the `Session` drops the handle, which closes
//! the connection; an approval that was never answered is denied by acp-core when its sender drops.

use std::path::PathBuf;
use std::sync::Arc;

use acp_core::{AppEvent, BackendConfig, BackendTransport, DaemonConfig, SessionHandle, SessionOptions};
use cordata_view::app::{App as View, Effect, LocalEvent, Message};
use cordata_view::shell;
use gpui::{Context, Global};
use tokio::sync::mpsc;

/// The process's tokio runtime: acp-core's connections, the daemon client, background shell jobs. Two workers are
/// plenty — everything on it is I/O.
pub struct Runtime(pub Arc<tokio::runtime::Runtime>);

impl Global for Runtime {}

impl Runtime {
    pub fn new() -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("cordata-io").enable_all().build()?;
        Ok(Self(Arc::new(runtime)))
    }
}

/// How this app reaches the agent.
#[derive(Clone)]
pub struct Connection {
    pub backend: BackendConfig,
    pub transport: BackendTransport,
    /// What to say when there is no way to connect (shown instead of a transcript).
    pub problem: Option<String>,
}

/// `--transport`: the daemon's hub (default), or a private stdio child (needs a checkout).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prefer {
    Hub,
    Stdio,
}

impl Connection {
    /// A bundle launched from Finder has no checkout around it and a cwd of `/`: the daemon's `~/.cordata/daemon.json`
    /// is how it finds the harness, not a walk up the filesystem (docs/app.md §3 D3b). A run from the repo can still
    /// use a stdio child.
    pub fn discover(prefer: Prefer, conversation: &str) -> Self {
        let backend = BackendConfig::discover(None).ok();
        let placeholder = || BackendConfig { backend_dir: PathBuf::from("."), python: PathBuf::from("python3"), server: PathBuf::from("acp_server.py") };
        match prefer {
            Prefer::Hub => match DaemonConfig::discover() {
                Ok(daemon) => Self { backend: backend.unwrap_or_else(placeholder), transport: BackendTransport::Hub { daemon, conversation: conversation.to_owned() }, problem: None },
                Err(error) => match backend {
                    // No daemon, but a checkout: talk to a child of our own rather than show nothing.
                    Some(backend) if backend.validate().is_ok() => Self { backend, transport: BackendTransport::Stdio, problem: None },
                    _ => Self {
                        backend: placeholder(),
                        transport: BackendTransport::Stdio,
                        problem: Some(format!("{error}. Start the daemon (`make daemon-install`), then reopen Cordata.")),
                    },
                },
            },
            Prefer::Stdio => match backend {
                // `CORDATA_APP_SERVER_SCRIPT`: another ACP agent in place of acp_server.py — acp-core's scripted fake
                // (`tui/crates/acp-core/tests/fixtures/fake_agent.py`) for driving the window without a model.
                Some(backend) => {
                    let backend = match std::env::var_os("CORDATA_APP_SERVER_SCRIPT") {
                        Some(script) => backend.with_server_script(PathBuf::from(script)),
                        None => backend,
                    };
                    Self { backend, transport: BackendTransport::Stdio, problem: None }
                }
                None => Self { backend: placeholder(), transport: BackendTransport::Stdio, problem: Some("no Cordata checkout found for a stdio session (set ACP_TUI_BACKEND_ROOT)".into()) },
            },
        }
    }

    pub fn describe(&self) -> &'static str {
        match self.transport {
            BackendTransport::Hub { .. } => "hub",
            BackendTransport::Ws { .. } => "daemon",
            BackendTransport::Stdio => "stdio",
            BackendTransport::Native => "native",
        }
    }
}

/// Which conversation a session window shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// What `--conversation` named (the hub's `latest`/`new`/a handle) or a fresh stdio session.
    Default,
    /// A hub conversation by handle (`new` makes one).
    Conversation(String),
    /// A saved session by id, over stdio (`session/load`).
    Resume(String),
}

/// One conversation the rail can offer: open in another tab, or saved for this project.
#[derive(Clone, Debug)]
pub struct Row {
    pub target: Target,
    pub title: String,
    /// Seconds since the epoch.
    pub when: f64,
    pub live: bool,
    pub busy: bool,
    /// Which front door started it (`terminal`, `discord`, `menubar`, `app`).
    pub origin: String,
}

/// What `session/list` (stdio) reports, parsed.
pub fn rows_from_session_list(value: &serde_json::Value) -> Vec<Row> {
    let parse_time = |text: &str| -> f64 {
        // `2026-10-12T16:49:12Z` → seconds; the rail only orders and ages them.
        let digits: Vec<u64> = text.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
        if digits.len() < 6 {
            return 0.0;
        }
        let days = days_from_civil(digits[0] as i64, digits[1] as u32, digits[2] as u32);
        (days * 86_400 + (digits[3] * 3600 + digits[4] * 60 + digits[5]) as i64) as f64
    };
    value
        .get("sessions")
        .and_then(|rows| rows.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = row.get("sessionId")?.as_str()?.to_owned();
                    Some(Row {
                        title: row.get("title").and_then(|t| t.as_str()).filter(|t| !t.is_empty()).map(str::to_owned).unwrap_or_else(|| id.chars().take(8).collect()),
                        when: row.get("updatedAt").and_then(|t| t.as_str()).map(parse_time).unwrap_or(0.0),
                        target: Target::Resume(id),
                        live: false,
                        busy: false,
                        origin: "terminal".into(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm): no date crate for one conversion.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The reply to `steer` (accepted, how many wait) and to `steer/drain` at a turn's end.
pub const STEER_TAG: &str = "gui-steer";
pub const STEER_DRAIN_TAG: &str = "gui-steer-drain";

pub struct Session {
    pub view: View,
    handle: Option<SessionHandle>,
    pub cwd: PathBuf,
    pub transport: &'static str,
    /// What this session was opened on — reconnecting opens it again.
    pub target: Target,
    /// The project's saved sessions, when this session asked for them (stdio's `session/list`).
    pub listing: Option<Vec<Row>>,
    /// Answers to the window's own requests (`gui-*` tags: Mission Control's runs, a run's events, an artifact).
    pub data: std::collections::HashMap<String, Result<serde_json::Value, String>>,
    /// Bumped on every change, so a view can tell its cached layout is stale.
    pub version: u64,
    /// Images and files going out with the prompt being submitted right now (`submit_with`).
    outgoing: Vec<acp_core::Attachment>,
    local: mpsc::UnboundedSender<LocalEvent>,
}

impl Session {
    pub fn start(connection: &Connection, target: Target, cwd: PathBuf, initial_prompt: Option<String>, cx: &mut Context<Self>) -> Self {
        let (local_tx, mut local_rx) = mpsc::unbounded_channel::<LocalEvent>();
        let mut view = View::new();
        view.attach_store(cordata_view::settings::Store::discover(&cwd));
        view.initial_prompt = initial_prompt;
        let mut session = Self { view, handle: None, cwd: cwd.clone(), transport: connection.describe(), target: target.clone(), listing: None, data: Default::default(), version: 0, outgoing: Vec::new(), local: local_tx };
        if let Some(problem) = &connection.problem {
            session.view.messages.push(Message::Error(problem.clone()));
            return session;
        }

        let runtime = cx.global::<Runtime>().0.clone();
        let (handle, mut events) = {
            // acp-core spawns its connection task with a bare `tokio::spawn`: it needs this runtime entered.
            let _guard = runtime.enter();
            let (transport, resume) = match (&connection.transport, &target) {
                (BackendTransport::Hub { daemon, .. }, Target::Conversation(handle)) => (BackendTransport::Hub { daemon: daemon.clone(), conversation: handle.clone() }, None),
                (transport, Target::Resume(id)) => (transport.clone(), Some(id.clone())),
                (transport, _) => (transport.clone(), None),
            };
            SessionHandle::spawn(SessionOptions {
                backend: connection.backend.clone(),
                cwd,
                transport,
                resume,
                debug_log: std::env::var_os("CORDATA_APP_DEBUG_LOG").map(PathBuf::from),
                continue_latest: false,
                hub_handle: None,
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

    fn apply(&mut self, events: Vec<AppEvent>, cx: &mut Context<Self>) {
        for event in events {
            match event {
                // The window's own `session/list`: data for the rail, not a message for the transcript.
                AppEvent::ExtResult { tag, result } if tag == LIST_TAG => {
                    self.listing = result.ok().map(|value| rows_from_session_list(&value));
                    cx.emit(SessionEvent::Listed);
                }
                AppEvent::ExtResult { tag, result } if tag.starts_with("gui-") => {
                    self.data.insert(tag.clone(), result);
                    cx.emit(SessionEvent::Data(tag));
                }
                event => self.view.on_event(event),
            }
        }
        self.settle(Vec::new(), cx);
    }

    /// A request of the window's own (`_cordata/…`); the answer lands in `data[tag]` and `SessionEvent::Data(tag)`.
    pub fn request(&self, method: &str, params: serde_json::Value, tag: &str) -> bool {
        debug_assert!(tag.starts_with("gui-"));
        self.with_handle(|h| h.ext(method.to_owned(), params, tag.to_owned())).is_ok()
    }

    pub fn connected(&self) -> bool {
        matches!(self.view.conn, cordata_view::app::ConnState::Connected)
    }

    /// Ask for the project's saved sessions (stdio); the answer lands in `listing`.
    pub fn request_listing(&self) {
        let cwd = self.cwd.display().to_string();
        let _ = self.with_handle(|h| h.ext("session/list", serde_json::json!({ "cwd": cwd }), LIST_TAG));
    }

    /// The hub conversation this session is on, once connected — `new` resolves to a real handle only then (acp-core
    /// reports the transport as `hub · <handle>`).
    pub fn hub_handle(&self) -> Option<String> {
        let transport = &self.view.session.as_ref()?.transport;
        transport.strip_prefix("hub · ").map(|handle| handle.split_whitespace().next().unwrap_or(handle).to_owned())
    }

    /// The id this session is known by, once connected (for reconnecting to it).
    pub fn session_id(&self) -> Option<String> {
        self.view.session.as_ref().map(|info| info.session_id.clone())
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

    pub fn set_config(&mut self, id: &str, value: &str, cx: &mut Context<Self>) {
        self.settle(vec![Effect::SetConfig { id: id.to_owned(), value: value.to_owned() }], cx);
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
        effects.extend(self.view.take_auto_resume());
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

    /// A note for the running turn: the agent reads it at its next model call (`backend/steering.py`).
    pub fn steer(&self, text: &str) -> bool {
        self.request("_cordata/steer", serde_json::json!({ "text": text }), STEER_TAG)
    }

    pub fn answer(&mut self, decision: acp_core::ApprovalDecision, cx: &mut Context<Self>) {
        self.view.answer(decision);
        self.settle(Vec::new(), cx);
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.settle(vec![Effect::Cancel], cx);
    }

    pub fn set_mode(&mut self, mode: &str, cx: &mut Context<Self>) {
        self.view.pending_mode = Some(mode.to_owned());
        self.settle(vec![Effect::SetMode(mode.to_owned())], cx);
    }

    fn run(&mut self, effect: Effect, cx: &mut Context<Self>) {
        let sent = match effect {
            Effect::SendPrompt(text) => {
                let attachments = std::mem::take(&mut self.outgoing);
                if attachments.is_empty() { self.with_handle(|h| h.send_prompt(text)) } else { self.with_handle(|h| h.send_prompt_with(text, attachments)) }
            }
            Effect::Cancel => self.with_handle(SessionHandle::cancel),
            Effect::SetMode(mode) => self.with_handle(|h| h.set_mode(mode)),
            Effect::SetConfig { id, value } => self.with_handle(|h| h.set_config_option(id, value)),
            Effect::Ext { method, params, tag } => self.with_handle(|h| h.ext(method, params, tag)),
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
            Effect::OpenConversation(handle) => {
                cx.emit(SessionEvent::Window(WindowRequest::Open(Target::Conversation(handle))));
                Ok(())
            }
            Effect::ListSessions | Effect::ListConversations => {
                self.view.messages.push(Message::Info("the project's conversations are in the rail on the left".into()));
                Ok(())
            }
            Effect::Review => {
                self.view.messages.push(Message::Info("the diff review lands with the editor (docs/app.md P3); /diff shows the working tree's diff now".into()));
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

/// The ext tag the window's own `session/list` goes out under (kept out of the transcript).
const LIST_TAG: &str = "gui-session-list";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// An approval or a finished turn wants the person (bounce the Dock icon, badge the rail).
    NeedsAttention,
    /// `listing` changed.
    Listed,
    /// A `/new`, `/close`, `/open` or session switch: the window's to carry out.
    Window(WindowRequest),
    /// An answer to `request(…, tag)` arrived.
    Data(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRequest {
    New,
    Close,
    Switch(cordata_view::app::SwitchTarget),
    Open(Target),
}

impl gpui::EventEmitter<SessionEvent> for Session {}
