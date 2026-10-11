//! UI-independent application state: everything the screen shows and every
//! key's effect lives here so it can be unit-tested without a terminal.

use std::collections::VecDeque;
use std::time::Instant;

use acp_core::{
    AppEvent, ApprovalDecision, ConfigChoice, ConfigOptionInfo, FileDiff, HarnessEvent, HealInfo,
    LayaDecision, LayaEvent, LayaOutcome, ModeInfo, PermissionInfo, SessionInfo, StopReason,
    TaskState, ToolCallInfo, ToolKind, ToolStatus,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::commands::{self, Command};
use crate::config_ui::ConfigPanel;
use crate::input::Input;
use crate::lab::LabView;
use crate::settings::{Settings, Store};

#[derive(Debug)]
pub struct ToolCard {
    pub id: String,
    pub name: String,
    pub kind: ToolKind,
    pub status: ToolStatus,
    pub raw_input: Option<Value>,
    /// The tool's result (what it returned).
    pub content: String,
    /// Live progress streamed while it runs (a sub-agent team's activity log);
    /// kept apart from `content` so the final result doesn't erase who did what.
    pub progress: String,
    /// Result text carries the backend's untrusted-output guard markers.
    pub untrusted: bool,
    /// Tool whose output the backend wraps as untrusted (a hint, pre-result).
    pub untrusted_source: bool,
    /// What an edit changes (old text read before it ran), for the diff viewer.
    pub diffs: Vec<FileDiff>,
    /// Laya's P(this command succeeds), predicted before it ran.
    pub p_ok: Option<f64>,
    /// …and how it actually went: (succeeded, log loss of the prediction).
    pub outcome: Option<(bool, Option<f64>)>,
}

#[derive(Debug)]
pub enum Message {
    /// Laya's decision for a prompt, shown as soon as it arrives.
    Laya(LayaDecision),
    /// What the LLM then actually did, appended when the turn ends.
    LayaOutcome(LayaOutcome),
    /// The key/command reference, printed on demand (`?`, `/help`).
    Help,
    /// A phase of the self-healing loop worth a block in the transcript.
    Heal(HealInfo),
    /// Session banner printed once when the backend connects.
    Welcome(SessionInfo),
    User(String),
    Agent(String),
    Thought(String),
    Tool(ToolCard),
    Divider(String),
    Info(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Closed { error: Option<String> },
}

pub struct PendingPermission {
    pub info: PermissionInfo,
    decision: Option<oneshot::Sender<ApprovalDecision>>,
}

impl PendingPermission {
    fn answer(&mut self, decision: ApprovalDecision) {
        if let Some(tx) = self.decision.take() {
            let _ = tx.send(decision);
        }
    }
}

pub struct Picker {
    pub config_id: String,
    pub title: String,
    pub choices: Vec<ConfigChoice>,
    pub selected: usize,
}

/// Side effects the event loop performs on the session (kept out of `App`
/// so state transitions stay pure).
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    SendPrompt(String),
    Cancel,
    SetMode(String),
    SetConfig {
        id: String,
        value: String,
    },
    /// Wipe the visible screen and scrollback (`/clear`).
    ClearScreen,
    /// An extension request to the backend; the reply comes back as
    /// `AppEvent::ExtResult` with the same tag.
    Ext {
        method: String,
        params: Value,
        tag: String,
    },
    /// Start another session (a new backend process) in the same project.
    NewSession,
    /// Show another session: `Next`, `Prev` or a 1-based slot number.
    SwitchSession(SwitchTarget),
    /// End the shown session (its backend shuts down).
    CloseSession,
    /// List the open sessions and the project's saved ones.
    ListSessions,
    /// List the hub's conversations.
    ListConversations,
    /// Continue a hub conversation (by handle) in a new slot.
    OpenConversation(String),
    Quit,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SwitchTarget {
    Next,
    Prev,
    Slot(usize),
}

/// One session's badge in the footer's tab strip (filled in by `main`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabBadge {
    pub number: usize,
    pub active: bool,
    pub state: TabState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabState {
    Connecting,
    Idle,
    Working,
    NeedsApproval,
    Closed,
}

pub struct App {
    /// An interrupted turn this client should resume on its own, once idle (see `take_auto_resume`).
    pub auto_resume: Option<String>,
    pub messages: Vec<Message>,
    pub input: Input,
    pub pending: VecDeque<PendingPermission>,
    pub picker: Option<Picker>,
    /// The `/config` panel, while open.
    pub config: Option<ConfigPanel>,
    /// Where the settings files are (None in unit tests that don't need them).
    pub store: Option<Store>,
    /// Merged settings as last read.
    pub settings: Settings,
    pub session: Option<SessionInfo>,
    pub modes: Vec<ModeInfo>,
    pub mode: String,
    /// Mode last requested but not yet confirmed by the server. Cycling
    /// starts from this so rapid Tab presses advance instead of re-sending
    /// the same target; the status bar keeps showing the *confirmed* mode.
    pub pending_mode: Option<String>,
    pub config_options: Vec<ConfigOptionInfo>,
    pub title: Option<String>,
    pub conn: ConnState,
    pub turn_active: bool,
    pub cancelling: bool,
    pub hint: Option<String>,
    pub expand_tools: bool,
    /// Reasoning shows as a one-line preview unless expanded (Ctrl+R, /thinking).
    pub thoughts_expanded: bool,
    /// Show the live "◆ laya" lines (`/laya off` hides them; they are kept).
    pub show_laya: bool,
    /// Messages already written to the terminal's scrollback. Everything
    /// before this index is final and never redrawn.
    pub committed: usize,
    /// Rows of the message at `committed` already written (a long reply is
    /// committed progressively as complete lines arrive).
    pub committed_rows: usize,
    /// Kind of the last committed message, for spacing between blocks.
    pub last_committed: Option<crate::ui::transcript::Kind>,
    /// Prompts sent this run, oldest first, for Up/Down recall.
    history: Vec<String>,
    history_idx: Option<usize>,
    /// What was typed before the first Up, restored when Down walks past the newest.
    draft: String,
    pub turn_started: Option<Instant>,
    pub turn_tools: usize,
    pub spinner: usize,
    /// Background work (plugins, language servers, self-heal, MCP progress).
    pub tasks: Vec<crate::ui::progress::TaskView>,
    /// The approval panel shows the whole diff instead of its first rows (`d`).
    pub diff_expanded: bool,
    /// Every open session, for the footer (empty / one entry: no tab strip).
    pub tabs: Vec<TabBadge>,
    /// The Experimental Lab view (F2 / `/lab`).
    pub lab: LabView,
}

impl App {
    pub fn new() -> Self {
        Self {
            auto_resume: None,
            messages: Vec::new(),
            input: Input::default(),
            pending: VecDeque::new(),
            picker: None,
            config: None,
            store: None,
            settings: Settings::default(),
            session: None,
            modes: Vec::new(),
            mode: String::new(),
            pending_mode: None,
            config_options: Vec::new(),
            title: None,
            conn: ConnState::Connecting,
            turn_active: false,
            cancelling: false,
            hint: None,
            expand_tools: false,
            thoughts_expanded: false,
            show_laya: true,
            committed: 0,
            committed_rows: 0,
            last_committed: None,
            history: Vec::new(),
            history_idx: None,
            draft: String::new(),
            turn_started: None,
            turn_tools: 0,
            spinner: 0,
            tasks: Vec::new(),
            diff_expanded: false,
            tabs: Vec::new(),
            lab: LabView::default(),
        }
    }

    /// For the footer of *another* session's tab strip.
    pub fn tab_state(&self) -> TabState {
        match self.conn {
            ConnState::Connecting => TabState::Connecting,
            ConnState::Closed { .. } => TabState::Closed,
            ConnState::Connected if !self.pending.is_empty() => TabState::NeedsApproval,
            ConnState::Connected if self.turn_active => TabState::Working,
            ConnState::Connected => TabState::Idle,
        }
    }

    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
        let now = Instant::now();
        self.tasks.retain(|task| task.visible(now));
    }

    pub fn input_enabled(&self) -> bool {
        self.conn == ConnState::Connected
            && !self.turn_active
            && self.pending.is_empty()
            && self.picker.is_none()
            && self.config.is_none()
            && !self.lab.open
    }

    /// The turn to resume on its own, once the session is idle: set when the backend says a loaded session has
    /// an interrupted turn and `selfHeal.autoResume` allows it. Taken at most once.
    pub fn take_auto_resume(&mut self) -> Option<Effect> {
        if self.auto_resume.is_none() || !self.input_enabled() {
            return None;
        }
        let line = self.auto_resume.take()?;
        self.messages.push(Message::Info(line));
        Some(self.begin_turn(AUTO_RESUME_MARKER.into()))
    }

    /// Can a prompt be sent right now? (Same as `input_enabled`, named for the
    /// commands that send one on the user's behalf.)
    pub(crate) fn input_enabled_for_prompt(&self) -> bool {
        self.input_enabled()
    }

    /// Start a turn with `text` as the prompt.
    pub(crate) fn begin_turn(&mut self, text: String) -> Effect {
        self.auto_resume = None;
        self.turn_active = true;
        self.turn_started = Some(Instant::now());
        self.turn_tools = 0;
        Effect::SendPrompt(text)
    }

    pub fn awaiting_approval(&self, tool_call_id: &str) -> bool {
        self.pending
            .front()
            .is_some_and(|pending| pending.info.tool_call_id == tool_call_id)
    }

    pub fn config(&self, id: &str) -> Option<&ConfigOptionInfo> {
        self.config_options.iter().find(|option| option.id == id)
    }

    // ---------------------------------------------------------------- events

    pub fn on_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Connected(info) => {
                self.conn = ConnState::Connected;
                self.mode = info.current_mode.clone();
                self.modes = info.modes.clone();
                self.config_options = info.config_options.clone();
                self.messages.push(Message::Welcome(info.clone()));
                for notice in self.settings_notices() {
                    self.messages.push(Message::Info(notice));
                }
                self.session = Some(info);
            }
            AppEvent::UserText(text) => self.messages.push(Message::User(sanitize(&text))),
            AppEvent::AgentText(text) => self.append_agent(&text),
            AppEvent::AgentThought(text) => self.append_thought(&text),
            AppEvent::ReplayComplete => {
                self.messages.push(Message::Divider(
                    "resumed — earlier tool calls not shown (the backend replays text only); mode reset to build".into(),
                ));
            }
            AppEvent::Laya(LayaEvent::Decision(decision)) => self.on_laya_decision(decision),
            AppEvent::Laya(LayaEvent::Outcome(outcome)) => {
                self.messages.push(Message::LayaOutcome(outcome));
            }
            AppEvent::Laya(LayaEvent::Command { tool_use_id, p_ok }) => {
                if let Some(card) = self.card_mut(&tool_use_id) {
                    card.p_ok = Some(p_ok);
                }
            }
            AppEvent::Laya(LayaEvent::CommandOutcome {
                tool_use_id,
                ok,
                p_ok,
                log_loss,
            }) => {
                if let Some(card) = self.card_mut(&tool_use_id) {
                    card.p_ok = card.p_ok.or(p_ok);
                    card.outcome = Some((ok, log_loss));
                }
            }
            AppEvent::Harness(event) => self.on_harness(event),
            AppEvent::ExtResult { tag, result } => self.on_ext_result(&tag, result),
            AppEvent::ToolCallStarted(info) => {
                self.turn_tools += 1;
                self.messages.push(Message::Tool(card_from(info)));
            }
            AppEvent::ToolCallUpdated(patch) => {
                if let Some(card) = self.card_mut(&patch.id) {
                    if let Some(status) = patch.status {
                        card.status = status;
                    }
                    if let Some(input) = patch.raw_input {
                        card.raw_input = Some(input);
                    }
                    if let Some(diffs) = patch.diffs {
                        card.diffs = diffs;
                    }
                    if let Some(content) = patch.content {
                        // An in-progress update is a live log (replacing the
                        // last one); only a terminal update is the result.
                        if matches!(patch.status, Some(ToolStatus::InProgress)) {
                            card.progress = sanitize(&content);
                        } else {
                            card.content = sanitize(&content);
                        }
                    }
                    card.untrusted |= patch.untrusted;
                }
            }
            AppEvent::TitleChanged(title) => self.title = Some(sanitize(&title)),
            AppEvent::ModeChanged(mode) => {
                if self.pending_mode.as_deref() == Some(mode.as_str()) {
                    self.pending_mode = None;
                }
                // The one mode with no safety net: say so every time it is entered,
                // so a stray Tab can't leave you there unnoticed.
                if mode == "auto" && self.mode != "auto" {
                    self.info("AUTO mode: tools run WITHOUT asking for approval — Tab (or /mode build) goes back");
                }
                self.mode = mode;
            }
            AppEvent::ConfigOptionsChanged(options) => self.config_options = options,
            AppEvent::PermissionRequested { info, decision } => {
                // The card's own input is whatever was streaming when the
                // call started (often partial); the request carries the final one.
                if let Some(input) = info.raw_input.clone()
                    && let Some(card) = self.card_mut(&info.tool_call_id)
                {
                    card.raw_input = Some(input);
                }
                self.pending.push_back(PendingPermission {
                    info,
                    decision: Some(decision),
                });
            }
            AppEvent::TurnComplete(reason) => {
                self.end_turn();
                if reason != StopReason::EndTurn {
                    self.messages
                        .push(Message::Divider(format!("turn ended: {}", reason.as_str())));
                }
            }
            AppEvent::TurnFailed(error) => {
                self.end_turn();
                self.messages
                    .push(Message::Error(format!("turn failed: {}", sanitize(&error))));
            }
            AppEvent::Notice(note) => {
                // A failed set_mode/set_config is the only notice source; the
                // requested mode never took effect.
                self.pending_mode = None;
                self.messages.push(Message::Info(sanitize(&note)));
            }
            AppEvent::ConnectionError(error) => {
                self.close(Some(error.clone()));
                self.messages.push(Message::Error(format!(
                    "connection error: {}",
                    sanitize(&error)
                )));
            }
            AppEvent::Closed => {
                if !matches!(self.conn, ConnState::Closed { .. }) {
                    self.close(None);
                    self.messages.push(Message::Info("backend closed".into()));
                }
            }
        }
    }

    fn on_laya_decision(&mut self, decision: LayaDecision) {
        self.messages.push(Message::Laya(decision));
    }

    fn on_harness(&mut self, event: HarnessEvent) {
        match event {
            HarnessEvent::Task(info) => {
                let finished = (info.state != TaskState::Running).then(Instant::now);
                match self.tasks.iter_mut().find(|task| task.info.id == info.id) {
                    Some(task) => {
                        task.finished = task.finished.or(finished);
                        task.info = info;
                    }
                    None => self.tasks.push(crate::ui::progress::TaskView {
                        info,
                        started: Instant::now(),
                        finished,
                    }),
                }
            }
            HarnessEvent::Heal(info) if crate::ui::heal::is_shown(&info.phase) => {
                self.messages.push(Message::Heal(info));
            }
            HarnessEvent::Heal(_) => {}
            HarnessEvent::Lab(state) => self.lab.on_state(*state),
            HarnessEvent::Resume(event) => {
                self.auto_resume = event
                    .get("auto")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    .then(|| {
                        let attempts = event.get("attempts").and_then(Value::as_u64).unwrap_or(0);
                        let max = event.get("max").and_then(Value::as_u64).unwrap_or(2);
                        format!("↻ resuming the interrupted turn automatically (attempt {}/{max})", attempts + 1)
                    });
            }
            HarnessEvent::Turn(event) => {
                if let Some(line) = crate::commands::format_turn_receipt(&event) {
                    self.messages.push(Message::Info(sanitize(&line)));
                }
            }
            HarnessEvent::Orchestrator(event) => {
                if let Some(line) = crate::commands::format_orchestrator_event(&event) {
                    self.messages.push(Message::Info(sanitize(&line)));
                }
            }
            HarnessEvent::Checkpoint(event) => {
                if let Some(line) = crate::commands::format_checkpoint_event(&event) {
                    self.messages.push(Message::Info(sanitize(&line)));
                }
            }
            HarnessEvent::Plugin(row) => {
                // Start-up noise stays in the progress bars; problems are worth a line.
                let status = row
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if matches!(status, "failed" | "blocked") {
                    let name = row.get("name").and_then(Value::as_str).unwrap_or("?");
                    let error = row.get("error").and_then(Value::as_str).unwrap_or_default();
                    self.messages.push(Message::Info(sanitize(&format!(
                        "plugin {name}: {status} — {error}"
                    ))));
                }
            }
            HarnessEvent::Lsp(event) => {
                let server = event.get("server").and_then(Value::as_str).unwrap_or("?");
                match event.get("event").and_then(Value::as_str) {
                    Some("started") => {
                        let command = event
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or(server);
                        self.messages.push(Message::Info(sanitize(&format!(
                            "language server for {server} started ({command}) — edits now report diagnostics"
                        ))));
                    }
                    Some("failed") => {
                        let error = event
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.messages.push(Message::Info(sanitize(&format!(
                            "language server for {server} failed: {error}"
                        ))));
                    }
                    _ => {}
                }
            }
        }
    }

    fn on_ext_result(&mut self, tag: &str, result: Result<Value, String>) {
        // The Lab reports its own controls (a dial turned five times is not five
        // lines of scrollback).
        if tag.starts_with("lab/") {
            self.lab.on_ext_result(tag, result);
            return;
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                self.messages
                    .push(Message::Error(sanitize(&format!("/{tag}: {error}"))));
                return;
            }
        };
        // The agent forgot some turns: the transcript on screen forgets them too, so what is shown is what it remembers.
        if tag == "back" && value.get("ok").and_then(Value::as_bool) == Some(true) {
            let forgotten = value
                .get("forgot")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            self.drop_turns(forgotten);
        }
        let text = crate::commands::format_ext(tag, &value);
        self.messages.push(Message::Info(sanitize(&text)));
    }

    /// Remove the last `turns` turns from the transcript: from the `turns`-th most recent user message to the end.
    fn drop_turns(&mut self, turns: usize) {
        if turns == 0 {
            return;
        }
        let starts: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m, Message::User(_)))
            .map(|(i, _)| i)
            .collect();
        if let Some(&cut) = starts.iter().rev().nth(turns - 1) {
            self.messages.truncate(cut);
        }
    }

    fn end_turn(&mut self) {
        self.turn_active = false;
        self.turn_started = None;
        self.cancelling = false;
        self.hint = None;
    }

    fn close(&mut self, error: Option<String>) {
        self.conn = ConnState::Closed { error };
        self.end_turn();
        // Dropping the senders answers every waiting request as a denial.
        self.pending.clear();
    }

    fn append_agent(&mut self, text: &str) {
        let text = sanitize(text);
        if text.is_empty() {
            return;
        }
        match self.messages.last_mut() {
            Some(Message::Agent(existing)) => existing.push_str(&text),
            _ => self.messages.push(Message::Agent(text)),
        }
    }

    fn append_thought(&mut self, text: &str) {
        let text = sanitize(text);
        if text.is_empty() {
            return;
        }
        match self.messages.last_mut() {
            Some(Message::Thought(existing)) => existing.push_str(&text),
            _ => self.messages.push(Message::Thought(text)),
        }
    }

    pub fn card(&self, id: &str) -> Option<&ToolCard> {
        self.messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Tool(card) if card.id == id => Some(card),
                _ => None,
            })
    }

    fn card_mut(&mut self, id: &str) -> Option<&mut ToolCard> {
        self.messages
            .iter_mut()
            .rev()
            .find_map(|message| match message {
                Message::Tool(card) if card.id == id => Some(card),
                _ => None,
            })
    }

    // ------------------------------------------------------------------ keys

    /// Bracketed paste: the text goes into the prompt box verbatim (newlines
    /// included) instead of arriving as a burst of key presses that would
    /// submit on the first newline.
    pub fn on_paste(&mut self, text: &str) {
        if !self.input_enabled() {
            return;
        }
        for c in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
            match c {
                '\n' => self.input.insert('\n'),
                '\t' => (0..4).for_each(|_| self.input.insert(' ')),
                c if c.is_control() => {}
                c => self.input.insert(c),
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl && key.code == KeyCode::Char('q') {
            return Some(Effect::Quit);
        }

        if key.code == KeyCode::F(1) && self.pending.is_empty() {
            self.messages.push(Message::Help);
            return None;
        }

        // Session tabs work from anywhere — even with an approval open, so a
        // waiting session never traps you.
        if ctrl && key.code == KeyCode::Char('n') {
            return Some(Effect::NewSession);
        }
        if ctrl && key.code == KeyCode::Right {
            return Some(Effect::SwitchSession(SwitchTarget::Next));
        }
        if ctrl && key.code == KeyCode::Left {
            return Some(Effect::SwitchSession(SwitchTarget::Prev));
        }
        if alt && let KeyCode::Char(c @ '1'..='9') = key.code {
            return Some(Effect::SwitchSession(SwitchTarget::Slot(
                c as usize - '0' as usize,
            )));
        }

        // F2 opens the Lab from anywhere — with an approval open too, so the
        // station can be looked at in context.
        if key.code == KeyCode::F(2) {
            return self.lab.toggle();
        }

        if !self.pending.is_empty() {
            return self.on_key_permission(key);
        }
        if self.lab.open {
            return self.on_key_lab(key);
        }
        if self.picker.is_some() {
            return self.on_key_picker(key);
        }
        if self.config.is_some() {
            return self.on_key_config(key);
        }

        if ctrl {
            return match key.code {
                KeyCode::Char('c') => self.on_ctrl_c(),
                KeyCode::Char('p') => self.cycle_mode(true),
                // Ctrl+M is byte-identical to Enter in nearly every terminal,
                // so the model picker also lives on Ctrl+O.
                KeyCode::Char('o' | 'm') => {
                    self.open_picker("model", "Model");
                    None
                }
                KeyCode::Char('t') => {
                    self.open_picker("temperature", "Temperature");
                    None
                }
                KeyCode::Char('e') => {
                    self.expand_tools = !self.expand_tools;
                    None
                }
                KeyCode::Char('r') => {
                    self.thoughts_expanded = !self.thoughts_expanded;
                    None
                }
                KeyCode::Char('u') => {
                    if self.input_enabled() {
                        self.input.clear();
                    }
                    None
                }
                KeyCode::Char('j') => {
                    if self.input_enabled() {
                        self.input.insert('\n');
                    }
                    None
                }
                _ => None,
            };
        }

        match key.code {
            KeyCode::Tab => return self.cycle_mode(true),
            KeyCode::BackTab => return self.cycle_mode(false),
            KeyCode::Esc => return self.on_escape(),
            _ => {}
        }

        if !self.input_enabled() {
            return None;
        }
        // An empty prompt box + `?` is "help", not a question mark.
        if key.code == KeyCode::Char('?') && self.input.is_empty() {
            self.messages.push(Message::Help);
            return None;
        }
        match key.code {
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.input.insert('\n')
            }
            KeyCode::Enter => {
                let text = self.input.take();
                let text = text.trim_end().to_owned();
                self.history_idx = None;
                if text.is_empty() {
                    return None;
                }
                if self.history.last() != Some(&text) {
                    self.history.push(text.clone());
                }
                if let Some(command) = commands::parse(&text) {
                    return self.run_command(command);
                }
                self.messages.push(Message::User(sanitize(&text)));
                return Some(self.begin_turn(text));
            }
            KeyCode::Up if self.input.on_first_line() => self.history_back(),
            KeyCode::Down if self.input.on_last_line() => self.history_forward(),
            KeyCode::Char(c) => self.input.insert(c),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            _ => {}
        }
        None
    }

    /// Esc: interrupt a running turn (like Ctrl+C), else clear what's typed.
    fn on_escape(&mut self) -> Option<Effect> {
        if self.turn_active {
            return self.on_ctrl_c();
        }
        if self.input_enabled() {
            self.input.clear();
            self.history_idx = None;
        }
        None
    }

    fn history_back(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_idx {
            None => {
                self.draft = self.input.text().to_owned();
                self.history.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        };
        self.history_idx = Some(next);
        self.input.set_text(&self.history[next]);
    }

    fn history_forward(&mut self) {
        let Some(index) = self.history_idx else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_idx = Some(index + 1);
            self.input.set_text(&self.history[index + 1]);
        } else {
            self.history_idx = None;
            let draft = std::mem::take(&mut self.draft);
            self.input.set_text(&draft);
        }
    }

    fn info(&mut self, text: impl Into<String>) {
        self.messages.push(Message::Info(text.into()));
    }

    /// Run a `/command` typed into the input box. Nothing here is sent to the
    /// model: commands change the client or the session's settings only.
    fn run_command(&mut self, command: Command) -> Option<Effect> {
        match command {
            Command::Help => self.messages.push(Message::Help),
            Command::Quit => return Some(Effect::Quit),
            Command::Clear => {
                self.messages.clear();
                self.committed = 0;
                self.committed_rows = 0;
                self.last_committed = None;
                return Some(Effect::ClearScreen);
            }
            Command::Thinking => {
                self.thoughts_expanded = !self.thoughts_expanded;
                self.info(if self.thoughts_expanded {
                    "reasoning expanded"
                } else {
                    "reasoning collapsed"
                });
            }
            Command::Laya(Some(arg)) if arg == "stats" => {
                return Some(ext("_sierralab/laya/stats", Value::Null, "laya stats"));
            }
            Command::Laya(arg) => {
                self.show_laya = match arg.as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    None => !self.show_laya,
                    Some(other) => {
                        self.info(format!("/laya takes 'on' or 'off', not '{other}'"));
                        return None;
                    }
                };
                self.info(if self.show_laya {
                    "laya line shown"
                } else {
                    "laya line hidden (still logged by the backend)"
                });
            }
            Command::Tools => {
                self.expand_tools = !self.expand_tools;
                self.info(if self.expand_tools {
                    "tool results expanded"
                } else {
                    "tool results collapsed"
                });
            }
            Command::Mode(None) => {
                let ids: Vec<&str> = self.modes.iter().map(|m| m.id.as_str()).collect();
                self.info(format!("mode: {} ({})", self.mode, ids.join(" | ")));
            }
            Command::Mode(Some(wanted)) => {
                let wanted = wanted.to_ascii_lowercase();
                if self.modes.iter().any(|mode| mode.id == wanted) {
                    self.pending_mode = Some(wanted.clone());
                    return Some(Effect::SetMode(wanted));
                }
                let ids: Vec<&str> = self.modes.iter().map(|m| m.id.as_str()).collect();
                self.info(format!(
                    "unknown mode '{wanted}' — choose one of: {}",
                    ids.join(", ")
                ));
            }
            Command::Model(None) => self.open_picker("model", "Model"),
            Command::Temp(None) => self.open_picker("temperature", "Temperature"),
            Command::Model(Some(query)) => return self.set_config_by_name("model", &query),
            Command::Temp(Some(query)) => return self.set_config_by_name("temperature", &query),
            Command::Config(arg) => return self.run_config(arg),
            Command::Permissions(arg) => self.run_permissions(arg),
            Command::Trust => self.run_trust(),
            Command::Memory => self.run_memory(),
            Command::Init => return self.run_init(),
            Command::Status => {
                self.run_status();
                // The orchestrator's pools and handoffs, read from the ledger by the backend — no model call.
                return Some(ext("_sierralab/orchestrator/status", Value::Null, "orchestrator"));
            }
            Command::Plugins(arg) => return self.run_plugins(arg),
            Command::Lsp => return Some(ext("_sierralab/lsp/status", Value::Null, "lsp")),
            Command::Heal => return Some(ext("_sierralab/heal/status", Value::Null, "heal")),
            Command::Checkpoints => {
                return Some(ext("_sierralab/checkpoints/list", Value::Null, "checkpoints"));
            }
            Command::Undo(arg) => return self.run_undo(arg),
            Command::Back(arg) => return self.run_back(arg),
            Command::Agents => self.info(
                "specialists: architect (plans changes, read-only) · debugger (diagnoses, patches, verifies) — \
                 Laya dispatches confident routes to them directly; otherwise the agent calls them as tools. \
                 /lab (F2) shows them live",
            ),
            Command::Lab => return self.lab.toggle(),
            Command::New => return Some(Effect::NewSession),
            Command::Sessions => return Some(Effect::ListSessions),
            Command::Conversations => return Some(Effect::ListConversations),
            Command::Open(Some(handle)) if !handle.trim().is_empty() => return Some(Effect::OpenConversation(handle.trim().to_owned())),
            Command::Open(_) => self.info("/open <handle> — the handle is in /conversations, in Discord, and in the menu bar"),
            Command::Switch(arg) => {
                return match arg.as_deref().map(str::parse::<usize>) {
                    Some(Ok(n)) if n > 0 => Some(Effect::SwitchSession(SwitchTarget::Slot(n))),
                    None => Some(Effect::SwitchSession(SwitchTarget::Next)),
                    _ => {
                        self.info("/switch takes a session number (see /sessions)");
                        None
                    }
                };
            }
            Command::Close => return Some(Effect::CloseSession),
            Command::ResumeTurn => {
                if !self.input_enabled_for_prompt() {
                    return None;
                }
                self.messages.push(Message::User("↻ resume the interrupted turn".into()));
                return Some(self.begin_turn(RESUME_TURN_MARKER.into()));
            }
            Command::Unknown(name) => self.info(format!("unknown command /{name} — try /help")),
        }
        None
    }

    /// `/back` takes the *conversation* back one turn (the agent forgets it); `/back <n>` n turns; `/back list` shows the
    /// checkpoints; `/back to <number>` restores one (including the state a previous `/back` left).
    fn run_back(&mut self, arg: Option<String>) -> Option<Effect> {
        let words: Vec<&str> = arg.as_deref().unwrap_or("").split_whitespace().collect();
        let params = match words.as_slice() {
            ["list" | "ls"] => return Some(ext("_sierralab/conversation/list", Value::Null, "back list")),
            [] => serde_json::json!({ "turns": 1 }),
            [n] if n.parse::<u32>().is_ok() => serde_json::json!({ "turns": n.parse::<u32>().unwrap_or(1) }),
            ["to", n] if n.parse::<u32>().is_ok() => serde_json::json!({ "index": n.parse::<u32>().unwrap_or(1) }),
            _ => {
                self.info("usage: /back [n | list | to <number>] — takes the conversation back; /back list shows the checkpoints");
                return None;
            }
        };
        if self.turn_active {
            self.info("a turn is running — stop it (Esc) first, then /back");
            return None;
        }
        Some(ext("_sierralab/conversation/rewind", params, "back"))
    }

    /// `/undo` previews undoing the latest turn's changes; `/undo apply` does it. `/undo <turn>` and
    /// `/undo <turn> apply` name an earlier turn (ids are in `/checkpoints`).
    fn run_undo(&mut self, arg: Option<String>) -> Option<Effect> {
        let words: Vec<&str> = arg.as_deref().unwrap_or("").split_whitespace().collect();
        let (turn, apply) = match words.as_slice() {
            [] => (None, false),
            ["apply" | "yes" | "now"] => (None, true),
            [turn] => (Some(*turn), false),
            [turn, "apply" | "yes" | "now"] => (Some(*turn), true),
            _ => {
                self.info("usage: /undo [turn] [apply] — without 'apply' it only shows what would change");
                return None;
            }
        };
        if apply && self.turn_active {
            self.info("a turn is running — stop it (Esc) first, then /undo apply");
            return None;
        }
        let mut params = serde_json::json!({ "apply": apply });
        if let Some(turn) = turn {
            params["turn"] = Value::String(turn.to_owned());
        }
        Some(ext(
            "_sierralab/checkpoints/undo",
            params,
            if apply { "undo applied" } else { "undo" },
        ))
    }

    /// `/plugins`, `/plugins enable|disable <name>`, `/plugins reload`.
    fn run_plugins(&mut self, arg: Option<String>) -> Option<Effect> {
        let words: Vec<&str> = arg.as_deref().unwrap_or("").split_whitespace().collect();
        match words.as_slice() {
            [] | ["list"] => Some(ext("_sierralab/plugins/list", Value::Null, "plugins")),
            ["reload"] => Some(ext("_sierralab/plugins/reload", Value::Null, "plugins")),
            [verb @ ("enable" | "disable"), name] => Some(ext(
                &format!("_sierralab/plugins/{verb}"),
                serde_json::json!({ "name": name }),
                &format!("plugins {verb}"),
            )),
            _ => {
                self.info("usage: /plugins [list | reload | enable <name> | disable <name>]");
                None
            }
        }
    }

    /// Set a select option by the value or display name the user typed
    /// (exact match first, then a unique substring).
    fn set_config_by_name(&mut self, config_id: &str, query: &str) -> Option<Effect> {
        let query = query.to_ascii_lowercase();
        let Some(option) = self.config(config_id) else {
            self.info(format!("no {config_id} option available"));
            return None;
        };
        let matches = |choice: &ConfigChoice| {
            choice.value.to_ascii_lowercase().contains(&query)
                || choice.name.to_ascii_lowercase().contains(&query)
        };
        let exact = option.choices.iter().find(|c| {
            c.value.to_ascii_lowercase() == query || c.name.to_ascii_lowercase() == query
        });
        let candidates: Vec<&ConfigChoice> = option.choices.iter().filter(|c| matches(c)).collect();
        let choice = exact.or(if candidates.len() == 1 {
            Some(candidates[0])
        } else {
            None
        });
        match choice {
            Some(choice) => Some(Effect::SetConfig {
                id: config_id.to_owned(),
                value: choice.value.clone(),
            }),
            None => {
                let names: Vec<&str> = option.choices.iter().map(|c| c.value.as_str()).collect();
                let message = if candidates.is_empty() {
                    format!(
                        "no {config_id} matches '{query}' — choices: {}",
                        names.join(", ")
                    )
                } else {
                    format!(
                        "'{query}' matches several {config_id} options — be more specific: {}",
                        names.join(", ")
                    )
                };
                self.info(message);
                None
            }
        }
    }

    fn on_key_permission(&mut self, key: KeyEvent) -> Option<Effect> {
        let offers_always = self
            .pending
            .front()
            .is_some_and(|pending| pending.info.always_label.is_some());
        let decision = match key.code {
            KeyCode::Char('d' | 'D') => {
                self.diff_expanded = !self.diff_expanded;
                return None;
            }
            // The HITL station's own keys, alongside the prompt's: Y approves and
            // executes, N rejects and discards.
            KeyCode::Char('a' | 'A' | 'y' | 'Y') => ApprovalDecision::Allow,
            KeyCode::Char('s' | 'S') if offers_always => ApprovalDecision::AllowAlways,
            KeyCode::Char('r' | 'R' | 'n' | 'N') | KeyCode::Esc => ApprovalDecision::Reject,
            // The Lab's E-Stop, from the station: reject this call *and* isolate
            // the pod that asked for it.
            KeyCode::Char('k' | 'K') if self.lab.open => {
                let effect = self.lab.estop_for_pending();
                self.diff_expanded = false;
                if let Some(mut pending) = self.pending.pop_front() {
                    pending.answer(ApprovalDecision::Reject);
                }
                self.hint = None;
                return effect;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Documented, not a bug: session/cancel is only checked
                // between streamed events and the server is parked on this
                // very request, so cancelling now would do nothing.
                self.hint = Some(
                    "answer the permission request first — Ctrl+C can't cancel while it's open"
                        .into(),
                );
                return None;
            }
            _ => return None,
        };
        self.diff_expanded = false;
        if let Some(mut pending) = self.pending.pop_front() {
            if decision == ApprovalDecision::AllowAlways
                && let Some(label) = &pending.info.always_label
            {
                self.messages.push(Message::Info(format!(
                    "{} — saved for this project (/permissions to review)",
                    sanitize(label)
                )));
            }
            pending.answer(decision);
        }
        self.hint = None;
        None
    }

    fn on_key_lab(&mut self, key: KeyEvent) -> Option<Effect> {
        // Ctrl+C still cancels a running turn from inside the Lab.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.on_ctrl_c();
        }
        self.lab.on_key(key)
    }

    fn on_key_picker(&mut self, key: KeyEvent) -> Option<Effect> {
        let picker = self.picker.as_mut()?;
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up | KeyCode::Char('k') => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                picker.selected = (picker.selected + 1).min(picker.choices.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let picker = self.picker.take()?;
                let choice = picker.choices.get(picker.selected)?;
                return Some(Effect::SetConfig {
                    id: picker.config_id,
                    value: choice.value.clone(),
                });
            }
            _ => {}
        }
        None
    }

    fn on_ctrl_c(&mut self) -> Option<Effect> {
        if self.turn_active && !self.cancelling {
            self.cancelling = true;
            self.hint = Some("cancelling…".into());
            return Some(Effect::Cancel);
        }
        if self.input_enabled() && !self.input.is_empty() {
            self.input.clear();
        } else if !self.turn_active {
            self.hint = Some("Ctrl+Q quits".into());
        }
        None
    }

    fn cycle_mode(&mut self, forward: bool) -> Option<Effect> {
        if self.modes.is_empty() || self.conn != ConnState::Connected {
            return None;
        }
        let base = self.pending_mode.as_deref().unwrap_or(&self.mode);
        let current = self
            .modes
            .iter()
            .position(|mode| mode.id == base)
            .unwrap_or(0);
        let len = self.modes.len();
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        let next = self.modes[next].id.clone();
        self.pending_mode = Some(next.clone());
        Some(Effect::SetMode(next))
    }

    fn open_picker(&mut self, config_id: &str, title: &str) {
        let Some(option) = self.config(config_id) else {
            self.hint = Some(format!("no {config_id} option available"));
            return;
        };
        let selected = option
            .choices
            .iter()
            .position(|choice| choice.value == option.current_value)
            .unwrap_or(0);
        self.picker = Some(Picker {
            config_id: config_id.to_owned(),
            title: title.to_owned(),
            choices: option.choices.clone(),
            selected,
        });
    }
}

/// What `/resume-turn` sends: the backend resumes the session's checkpoint.
pub const RESUME_TURN_MARKER: &str = "[resume interrupted turn]";
/// What the TUI sends when it resumes on its own (`selfHeal.autoResume`): the backend counts these and
/// stops after two from the same checkpoint.
pub const AUTO_RESUME_MARKER: &str = "[auto-resume interrupted turn]";

fn ext(method: &str, params: Value, tag: &str) -> Effect {
    Effect::Ext {
        method: method.to_owned(),
        params: if params.is_null() {
            serde_json::json!({})
        } else {
            params
        },
        tag: tag.to_owned(),
    }
}

fn card_from(info: ToolCallInfo) -> ToolCard {
    ToolCard {
        id: info.id,
        name: sanitize(&info.name),
        kind: info.kind,
        status: info.status,
        raw_input: info.raw_input,
        content: sanitize(&info.content),
        progress: String::new(),
        untrusted: info.untrusted,
        untrusted_source: info.untrusted_source,
        diffs: info.diffs,
        p_ok: None,
        outcome: None,
    }
}

/// Agent/tool text is untrusted input to *our terminal*: drop escape
/// sequences and control characters so a hostile web page or file can't
/// redraw the screen, retitle the window, or hide text. Tabs become spaces.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // CSI: ESC [ params... final-byte(0x40..=0x7e); other escapes: drop ESC + next char.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            '\r' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use acp_core::{ConfigChoice, ToolCallPatch};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn an_interrupted_turn_resumes_once_when_the_backend_allows_it() {
        let mut app = connected();
        let notice = |auto: bool, attempts: u64| {
            AppEvent::Harness(HarnessEvent::Resume(serde_json::json!({
                "auto": auto, "attempts": attempts, "max": 2, "where": "after_model, cycle 0"
            })))
        };
        app.on_event(notice(false, 2));
        assert!(app.take_auto_resume().is_none(), "the backend said no: nothing is sent");

        app.on_event(notice(true, 0));
        let Some(Effect::SendPrompt(text)) = app.take_auto_resume() else {
            panic!("an idle session with an interrupted turn resumes");
        };
        assert_eq!(text, AUTO_RESUME_MARKER);
        assert!(app.turn_active, "it is a turn like any other");
        assert!(
            app.messages.iter().any(|m| matches!(m, Message::Info(t) if t.contains("attempt 1/2"))),
            "the person is told, with the count"
        );
        assert!(app.take_auto_resume().is_none(), "and it is taken once");

        let mut busy = connected();
        busy.on_event(notice(true, 1));
        busy.turn_active = true;
        assert!(busy.take_auto_resume().is_none(), "never on top of a turn in progress");
        busy.turn_active = false;
        let Some(Effect::SendPrompt(_)) = busy.take_auto_resume() else {
            panic!("it waits until the session is idle");
        };
    }

    fn connected() -> App {
        let mut app = App::new();
        app.on_event(AppEvent::Connected(SessionInfo {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            transport: "stdio".into(),
            resumed: false,
            current_mode: "build".into(),
            modes: ["plan", "build", "auto"]
                .into_iter()
                .map(|id| ModeInfo {
                    id: id.into(),
                    name: id.into(),
                    description: None,
                })
                .collect(),
            config_options: vec![ConfigOptionInfo {
                id: "model".into(),
                name: "Model".into(),
                current_value: "a".into(),
                choices: ["a", "b"]
                    .into_iter()
                    .map(|v| ConfigChoice {
                        value: v.into(),
                        name: v.into(),
                    })
                    .collect(),
            }],
        }));
        // The banner is covered by its own test; most tests want an empty transcript.
        app.messages.clear();
        app
    }

    #[test]
    fn connecting_adds_the_session_banner_to_the_transcript() {
        let mut app = App::new();
        app.on_event(AppEvent::Connected(SessionInfo {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            transport: "stdio".into(),
            resumed: false,
            current_mode: "build".into(),
            modes: vec![],
            config_options: vec![],
        }));
        assert!(
            matches!(app.messages.as_slice(), [Message::Welcome(info)] if info.session_id == "s1")
        );
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn enter_sends_prompt_and_locks_input_until_turn_ends() {
        let mut app = connected();
        type_text(&mut app, "hello");
        let effect = app.on_key(key(KeyCode::Enter));
        assert_eq!(effect, Some(Effect::SendPrompt("hello".into())));
        assert!(app.turn_active && !app.input_enabled());

        type_text(&mut app, "ignored");
        assert!(app.input.is_empty(), "input is disabled mid-turn");

        app.on_event(AppEvent::TurnComplete(StopReason::EndTurn));
        assert!(app.input_enabled());
    }

    #[test]
    fn streamed_text_appends_to_the_last_bubble() {
        let mut app = connected();
        app.on_event(AppEvent::AgentText("po".into()));
        app.on_event(AppEvent::AgentText("ng".into()));
        assert!(matches!(app.messages.as_slice(), [Message::Agent(text)] if text == "pong"));
        app.on_event(AppEvent::AgentThought("hm".into()));
        app.on_event(AppEvent::AgentText("x".into()));
        assert_eq!(app.messages.len(), 3);
    }

    #[test]
    fn permission_modal_takes_focus_and_a_r_answer_it() {
        let mut app = connected();
        let (tx, mut rx) = oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: "t1".into(),
                tool_name: "shell".into(),
                title: "Run shell".into(),
                kind: ToolKind::Execute,
                raw_input: None,
                untrusted_source: false,
                always_label: None,
            },
            decision: tx,
        });
        assert!(!app.input_enabled());
        type_text(&mut app, "zzz");
        assert!(app.input.is_empty());
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(rx.try_recv().unwrap(), ApprovalDecision::Allow);
        assert!(app.pending.is_empty());

        let (tx, mut rx) = oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: "t2".into(),
                tool_name: "shell".into(),
                title: "Run shell".into(),
                kind: ToolKind::Execute,
                raw_input: None,
                untrusted_source: false,
                always_label: None,
            },
            decision: tx,
        });
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(rx.try_recv().unwrap(), ApprovalDecision::Reject);
    }

    #[test]
    fn ctrl_c_cancels_a_turn_but_is_inert_under_the_modal() {
        let mut app = connected();
        type_text(&mut app, "go");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.on_key(ctrl('c')), Some(Effect::Cancel));
        assert_eq!(app.on_key(ctrl('c')), None, "second Ctrl+C doesn't re-send");

        let mut app = connected();
        type_text(&mut app, "go");
        app.on_key(key(KeyCode::Enter));
        let (tx, _rx) = oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: "t".into(),
                tool_name: "shell".into(),
                title: "Run shell".into(),
                kind: ToolKind::Execute,
                raw_input: None,
                untrusted_source: false,
                always_label: None,
            },
            decision: tx,
        });
        assert_eq!(app.on_key(ctrl('c')), None);
        assert!(app.hint.as_deref().unwrap().contains("permission"));
    }

    #[test]
    fn tab_cycles_modes_and_wraps() {
        let mut app = connected();
        assert_eq!(
            app.on_key(key(KeyCode::Tab)),
            Some(Effect::SetMode("auto".into()))
        );
        app.on_event(AppEvent::ModeChanged("auto".into()));
        assert_eq!(
            app.on_key(key(KeyCode::Tab)),
            Some(Effect::SetMode("plan".into()))
        );
        app.on_event(AppEvent::ModeChanged("plan".into()));
        assert_eq!(
            app.on_key(key(KeyCode::BackTab)),
            Some(Effect::SetMode("auto".into()))
        );
    }

    #[test]
    fn rapid_tabs_advance_from_the_requested_mode_not_the_stale_one() {
        let mut app = connected();
        assert_eq!(
            app.on_key(key(KeyCode::Tab)),
            Some(Effect::SetMode("auto".into()))
        );
        // No ModeChanged yet: the server hasn't answered.
        assert_eq!(
            app.on_key(key(KeyCode::Tab)),
            Some(Effect::SetMode("plan".into()))
        );
        assert_eq!(app.mode, "build", "display stays on the confirmed mode");
        app.on_event(AppEvent::ModeChanged("auto".into()));
        assert_eq!(
            app.pending_mode.as_deref(),
            Some("plan"),
            "an older confirmation doesn't clear a newer request"
        );
        app.on_event(AppEvent::ModeChanged("plan".into()));
        assert_eq!(app.pending_mode, None);
    }

    fn laya_decision(route: &str, late: bool, error: Option<&str>) -> AppEvent {
        AppEvent::Laya(LayaEvent::Decision(LayaDecision {
            mode: "route".into(),
            route: error.is_none().then(|| route.to_owned()),
            route_confidence: 0.9,
            gate: 0.85,
            needs_team: 0.1,
            complexity: 1.0,
            complexity_confidence: 0.7,
            latency_ms: Some(60.0),
            waited_ms: Some(60.0),
            hint: None,
            tier: None,
            late,
            error: error.map(Into::into),
            ..Default::default()
        }))
    }

    fn laya_outcome(actual: &str) -> AppEvent {
        AppEvent::Laya(LayaEvent::Outcome(LayaOutcome {
            actual_route: actual.into(),
            agreed: Some(true),
            tools: vec![],
            tiers: vec![],
            stop_reason: Some("end_turn".into()),
            turn_ms: Some(1.0),
            ..Default::default()
        }))
    }

    fn laya_decisions(app: &App) -> Vec<&LayaDecision> {
        app.messages
            .iter()
            .filter_map(|m| match m {
                Message::Laya(decision) => Some(decision),
                _ => None,
            })
            .collect()
    }

    fn laya_outcomes(app: &App) -> usize {
        app.messages
            .iter()
            .filter(|m| matches!(m, Message::LayaOutcome(_)))
            .count()
    }

    #[test]
    fn each_turn_gets_its_decision_line_and_its_outcome_line() {
        let mut app = connected();
        submit(&mut app, "first");
        app.on_event(laya_decision("chat", false, None));
        app.on_event(laya_outcome("chat"));
        app.on_event(AppEvent::TurnComplete(StopReason::EndTurn));
        submit(&mut app, "second");
        app.on_event(laya_decision("memory", false, None));
        app.on_event(laya_outcome("memory"));
        let decisions = laya_decisions(&app);
        assert_eq!(decisions.len(), 2);
        assert_eq!(laya_outcomes(&app), 2);
        assert_eq!(decisions[1].route.as_deref(), Some("memory"));
    }

    #[test]
    fn a_late_decision_follows_the_timeout_notice_because_scrollback_cannot_be_edited() {
        let mut app = connected();
        submit(&mut app, "slow");
        app.on_event(laya_decision(
            "",
            false,
            Some("timed out after 2s — the LLM decides alone"),
        ));
        app.on_event(laya_decision("web_research", true, None));
        let decisions = laya_decisions(&app);
        assert_eq!(decisions.len(), 2, "the notice stays; the answer is added");
        assert!(decisions[0].error.is_some());
        assert!(decisions[1].late && decisions[1].error.is_none());
    }

    #[test]
    fn slash_laya_toggles_and_validates() {
        let mut app = connected();
        assert!(app.show_laya);
        submit(&mut app, "/laya off");
        assert!(!app.show_laya);
        submit(&mut app, "/laya");
        assert!(app.show_laya, "no argument toggles");
        submit(&mut app, "/laya maybe");
        assert_info(&app, "takes 'on' or 'off'");
        assert!(app.show_laya);
    }

    #[test]
    fn entering_auto_mode_warns_once_and_other_modes_do_not() {
        let mut app = connected();
        app.on_event(AppEvent::ModeChanged("plan".into()));
        assert!(app.messages.is_empty());
        app.on_event(AppEvent::ModeChanged("auto".into()));
        assert_info(&app, "WITHOUT asking");
        let count = app.messages.len();
        app.on_event(AppEvent::ModeChanged("auto".into()));
        assert_eq!(
            app.messages.len(),
            count,
            "a repeat confirmation doesn't repeat the warning"
        );
        app.on_event(AppEvent::ModeChanged("build".into()));
        assert_eq!(app.messages.len(), count);
    }

    #[test]
    fn failed_mode_change_clears_the_pending_request() {
        let mut app = connected();
        app.on_key(key(KeyCode::Tab));
        app.on_event(AppEvent::Notice("set_mode failed: nope".into()));
        assert_eq!(app.pending_mode, None);
        assert_eq!(
            app.on_key(key(KeyCode::Tab)),
            Some(Effect::SetMode("auto".into()))
        );
    }

    #[test]
    fn model_picker_selects_and_emits_set_config() {
        let mut app = connected();
        app.on_key(ctrl('o'));
        assert!(app.picker.is_some());
        app.on_key(key(KeyCode::Down));
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Effect::SetConfig {
                id: "model".into(),
                value: "b".into()
            })
        );
        assert!(app.picker.is_none());
    }

    #[test]
    fn tool_update_replaces_content_and_flags_untrusted() {
        let mut app = connected();
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: "t1".into(),
            name: "http_request".into(),
            kind: ToolKind::Fetch,
            status: ToolStatus::Pending,
            raw_input: None,
            content: String::new(),
            untrusted_source: true,
            untrusted: false,
            diffs: vec![],
        }));
        app.on_event(AppEvent::ToolCallUpdated(ToolCallPatch {
            id: "t1".into(),
            status: Some(ToolStatus::Completed),
            content: Some("<<<UNTRUSTED_TOOL_OUTPUT>>>\nx".into()),
            untrusted: true,
            ..Default::default()
        }));
        let Message::Tool(card) = &app.messages[0] else {
            panic!()
        };
        assert_eq!(card.status, ToolStatus::Completed);
        assert!(card.untrusted && card.content.contains("x"));
    }

    #[test]
    fn connection_error_closes_and_denies_pending_requests() {
        let mut app = connected();
        let (tx, mut rx) = oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: "t".into(),
                tool_name: "shell".into(),
                title: "Run shell".into(),
                kind: ToolKind::Execute,
                raw_input: None,
                untrusted_source: false,
                always_label: None,
            },
            decision: tx,
        });
        app.on_event(AppEvent::ConnectionError("boom".into()));
        assert!(matches!(app.conn, ConnState::Closed { .. }));
        assert!(
            rx.try_recv().is_err(),
            "sender dropped => receiver sees closure (deny)"
        );
        assert!(!app.input_enabled());
    }

    fn submit(app: &mut App, text: &str) -> Option<Effect> {
        type_text(app, text);
        app.on_key(key(KeyCode::Enter))
    }

    fn assert_info(app: &App, needle: &str) {
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, Message::Info(text) if text.contains(needle))),
            "no info containing {needle:?}: {:?}",
            app.messages
        );
    }

    #[test]
    fn back_takes_the_conversation_back_and_the_screen_forgets_what_the_agent_forgot() {
        let mut app = connected();
        // /back → one turn; /back 2 → two; /back list → the checkpoints; /back to 3 → that checkpoint.
        let Some(Effect::Ext { method, params, tag }) = submit(&mut app, "/back") else {
            panic!("/back asks the backend");
        };
        assert_eq!((method.as_str(), tag.as_str()), ("_sierralab/conversation/rewind", "back"));
        assert_eq!(params["turns"], 1);
        let Some(Effect::Ext { params, .. }) = submit(&mut app, "/back 2") else { panic!() };
        assert_eq!(params["turns"], 2);
        let Some(Effect::Ext { method, tag, .. }) = submit(&mut app, "/back list") else { panic!() };
        assert_eq!((method.as_str(), tag.as_str()), ("_sierralab/conversation/list", "back list"));
        let Some(Effect::Ext { params, .. }) = submit(&mut app, "/back to 3") else { panic!() };
        assert_eq!(params["index"], 3);
        assert!(submit(&mut app, "/back sideways").is_none());
        assert_info(&app, "usage: /back");

        // never under a running turn
        app.turn_active = true;
        assert!(app.run_back(None).is_none());
        assert_info(&app, "a turn is running");
        app.turn_active = false;

        // the reply drops the forgotten turns from the transcript — and only those
        app.messages.clear();
        for n in 1..=3 {
            app.messages.push(Message::User(format!("question {n}")));
            app.messages.push(Message::Agent(format!("answer {n}")));
        }
        app.on_ext_result(
            "back",
            Ok(serde_json::json!({"ok": true, "now": 1, "forgot": ["question 2", "question 3"]})),
        );
        let users: Vec<&str> = app
            .messages
            .iter()
            .filter_map(|m| if let Message::User(t) = m { Some(t.as_str()) } else { None })
            .collect();
        assert_eq!(users, vec!["question 1"], "the screen is back to what the agent remembers");
        assert!(
            app.messages.iter().any(|m| matches!(m, Message::Agent(t) if t == "answer 1")),
            "the kept turn keeps its answer"
        );
        assert_info(&app, "went back 2 turn(s)");

        // a refusal drops nothing
        let before = app.messages.len();
        app.on_ext_result("back", Ok(serde_json::json!({"ok": false, "reason": "nothing to go back to"})));
        assert_eq!(app.messages.len(), before + 1);
        assert_info(&app, "back: nothing to go back to");
    }

    #[test]
    fn in_progress_updates_are_a_live_log_and_the_terminal_update_is_the_result() {
        let mut app = connected();
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: "t".into(),
            name: "local_graph".into(),
            kind: ToolKind::Other,
            status: ToolStatus::Pending,
            raw_input: None,
            content: String::new(),
            untrusted_source: false,
            untrusted: false,
            diffs: vec![],
        }));
        for log in ["started: a", "started: a\nfinished: a - ok"] {
            app.on_event(AppEvent::ToolCallUpdated(ToolCallPatch {
                id: "t".into(),
                status: Some(ToolStatus::InProgress),
                content: Some(log.into()),
                untrusted: false,
                ..Default::default()
            }));
        }
        app.on_event(AppEvent::ToolCallUpdated(ToolCallPatch {
            id: "t".into(),
            status: Some(ToolStatus::Completed),
            content: Some("final answer".into()),
            untrusted: false,
            ..Default::default()
        }));
        let Message::Tool(card) = &app.messages[0] else {
            panic!()
        };
        assert_eq!(
            card.progress, "started: a\nfinished: a - ok",
            "the last progress log is kept, not replaced by the result"
        );
        assert_eq!(card.content, "final answer");
        assert_eq!(app.turn_tools, 1);
    }

    #[test]
    fn slash_commands_run_locally_and_are_never_sent_to_the_model() {
        let mut app = connected();
        assert_eq!(
            submit(&mut app, "/mode plan"),
            Some(Effect::SetMode("plan".into()))
        );
        assert_eq!(app.pending_mode.as_deref(), Some("plan"));
        assert!(!app.turn_active && app.messages.iter().all(|m| !matches!(m, Message::User(_))));

        assert_eq!(submit(&mut app, "/mode yolo"), None);
        assert_info(&app, "unknown mode 'yolo'");
        submit(&mut app, "/mode");
        assert_info(&app, "plan | build | auto");

        assert_eq!(
            submit(&mut app, "/model b"),
            Some(Effect::SetConfig {
                id: "model".into(),
                value: "b".into()
            })
        );
        assert_eq!(submit(&mut app, "/model zzz"), None);
        assert_info(&app, "no model matches 'zzz'");

        submit(&mut app, "/model");
        assert!(app.picker.is_some(), "/model with no name opens the picker");
        app.on_key(key(KeyCode::Esc));

        submit(&mut app, "/thinking");
        assert!(app.thoughts_expanded);
        submit(&mut app, "/tools");
        assert!(app.expand_tools);
        submit(&mut app, "/frobnicate");
        assert_info(&app, "unknown command /frobnicate");
        submit(&mut app, "/clear");
        assert!(app.messages.is_empty());
        assert_eq!(submit(&mut app, "/quit"), Some(Effect::Quit));
    }

    #[test]
    fn a_path_in_a_prompt_is_still_a_prompt() {
        let mut app = connected();
        assert_eq!(
            submit(&mut app, "/usr/bin/env is missing"),
            Some(Effect::SendPrompt("/usr/bin/env is missing".into()))
        );
    }

    #[test]
    fn help_is_printed_by_question_mark_f1_and_slash_help() {
        let helps = |app: &App| {
            app.messages
                .iter()
                .filter(|m| matches!(m, Message::Help))
                .count()
        };
        let mut app = connected();
        app.on_key(key(KeyCode::Char('?')));
        assert!(
            helps(&app) == 1 && app.input.is_empty(),
            "'?' on an empty box prints help instead of typing"
        );
        app.on_key(key(KeyCode::F(1)));
        assert_eq!(helps(&app), 2);
        submit(&mut app, "/help");
        assert_eq!(helps(&app), 3);

        type_text(&mut app, "why?");
        assert_eq!(
            app.input.text(),
            "why?",
            "a '?' inside a prompt is just a character"
        );
    }

    #[test]
    fn pasted_text_keeps_its_newlines_and_does_not_submit() {
        let mut app = connected();
        app.on_paste("line one\r\nline two\tindented\n");
        assert_eq!(app.input.text(), "line one\nline two    indented\n");
        assert!(app.messages.iter().all(|m| !matches!(m, Message::User(_))));
        let mut busy = connected();
        busy.turn_active = true;
        busy.on_paste("ignored");
        assert!(busy.input.is_empty(), "no pasting into a disabled box");
    }

    #[test]
    fn up_and_down_walk_prompt_history_and_restore_the_draft() {
        let mut app = connected();
        for prompt in ["first", "second"] {
            submit(&mut app, prompt);
            app.on_event(AppEvent::TurnComplete(StopReason::EndTurn));
        }
        type_text(&mut app, "draft");
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "second");
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "first");
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "first", "stops at the oldest");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.input.text(), "second");
        app.on_key(key(KeyCode::Down));
        assert_eq!(
            app.input.text(),
            "draft",
            "walking past the newest restores what was being typed"
        );
    }

    #[test]
    fn up_inside_a_multiline_prompt_does_not_hijack_the_cursor() {
        let mut app = connected();
        submit(&mut app, "old");
        app.on_event(AppEvent::TurnComplete(StopReason::EndTurn));
        type_text(&mut app, "line one");
        app.on_key(ctrl('j'));
        type_text(&mut app, "line two");
        app.on_key(key(KeyCode::Up));
        assert_eq!(
            app.input.text(),
            "line one\nline two",
            "Up on line 2 is not history"
        );
    }

    #[test]
    fn escape_cancels_a_running_turn_or_clears_the_box() {
        let mut app = connected();
        submit(&mut app, "go");
        assert_eq!(app.on_key(key(KeyCode::Esc)), Some(Effect::Cancel));
        assert_eq!(
            app.on_key(key(KeyCode::Esc)),
            None,
            "a second Esc doesn't re-send"
        );
        app.on_event(AppEvent::TurnComplete(StopReason::Cancelled));
        type_text(&mut app, "half typed");
        app.on_key(key(KeyCode::Esc));
        assert!(app.input.is_empty());
    }

    #[test]
    fn the_turn_timer_and_tool_count_reset_each_turn() {
        let mut app = connected();
        submit(&mut app, "go");
        assert!(app.turn_started.is_some());
        app.on_event(AppEvent::TurnComplete(StopReason::EndTurn));
        assert!(app.turn_started.is_none());
    }

    #[test]
    fn sanitize_strips_escapes_and_control_chars() {
        assert_eq!(
            sanitize("a\u{1b}[31mred\u{1b}[0m b\u{7}c\r\nd\te"),
            "ared bc\nd    e"
        );
        // Non-CSI escapes lose their ESC (+ one byte); the leftover is inert text.
        assert_eq!(sanitize("x\u{1b}]0;evil\u{7}y"), "x0;evily");
    }

    // ------------------------------------------------------------------- Lab

    fn lab_state(pods: &[(&str, acp_core::PodStatus)]) -> AppEvent {
        AppEvent::Harness(HarnessEvent::Lab(Box::new(acp_core::LabState {
            seq: 1,
            pods: pods
                .iter()
                .map(|(name, status)| acp_core::LabPod {
                    id: (*name).into(),
                    name: (*name).into(),
                    role: (*name).into(),
                    status: *status,
                    dial: 0.8,
                    dial_default: 0.8,
                    ..acp_core::LabPod::default()
                })
                .collect(),
            ..acp_core::LabState::default()
        })))
    }

    fn permission(app: &mut App, id: &str) -> oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: id.into(),
                tool_name: "shell".into(),
                title: "Run shell".into(),
                kind: ToolKind::Execute,
                raw_input: None,
                untrusted_source: false,
                always_label: None,
            },
            decision: tx,
        });
        rx
    }

    fn ext_of(effect: Option<Effect>) -> (String, Value) {
        match effect.expect("an effect") {
            Effect::Ext { method, params, .. } => (method, params),
            other => panic!("expected Ext, got {other:?}"),
        }
    }

    #[test]
    fn f2_opens_the_lab_and_it_takes_the_keyboard() {
        let mut app = connected();
        let (method, _) = ext_of(app.on_key(key(KeyCode::F(2))));
        assert_eq!(
            method, "_sierralab/lab/state",
            "no snapshot yet: it asks for one"
        );
        assert!(app.lab.open && !app.input_enabled());
        type_text(&mut app, "zzz");
        assert!(
            app.input.is_empty(),
            "keys go to the Lab, not the prompt box"
        );
        app.on_event(lab_state(&[("orchestrator", acp_core::PodStatus::Idle)]));
        assert!(
            app.on_key(key(KeyCode::F(2))).is_none(),
            "F2 again closes it"
        );
        assert!(!app.lab.open && app.input_enabled());
    }

    #[test]
    fn slash_lab_toggles_the_view() {
        let mut app = connected();
        app.lab.seen = true;
        assert!(submit(&mut app, "/lab").is_none());
        assert!(app.lab.open);
        assert!(app.on_key(key(KeyCode::Esc)).is_none());
        assert!(!app.lab.open, "Esc closes it");
    }

    #[test]
    fn k_in_the_lab_isolates_the_selected_pod_through_the_backend() {
        let mut app = connected();
        app.on_event(lab_state(&[
            ("orchestrator", acp_core::PodStatus::Executing),
            ("debugger", acp_core::PodStatus::Synthesizing),
        ]));
        app.on_key(key(KeyCode::F(2)));
        app.on_key(key(KeyCode::Right));
        let (method, params) = ext_of(app.on_key(key(KeyCode::Char('K'))));
        assert_eq!(method, "_sierralab/lab/isolate");
        assert_eq!(params, serde_json::json!({"pod": "debugger"}));
        assert_eq!(
            app.lab.pods()[1].status,
            acp_core::PodStatus::Synthesizing,
            "the pod keeps its status until the backend reports otherwise"
        );
        app.on_event(AppEvent::ExtResult {
            tag: "lab/isolate".into(),
            result: Ok(serde_json::json!({"pod": "debugger", "isolated": true, "also": []})),
        });
        assert!(
            !app.messages
                .iter()
                .any(|m| matches!(m, Message::Info(_) | Message::Error(_))),
            "a Lab control reports in the Lab, not the transcript"
        );
        assert!(app.lab.notice.as_ref().unwrap().0.contains("ISOLATED"));
    }

    #[test]
    fn lab_snapshots_replace_each_other_and_keep_the_selection() {
        let mut app = connected();
        app.on_event(lab_state(&[
            ("orchestrator", acp_core::PodStatus::Idle),
            ("debugger", acp_core::PodStatus::Idle),
        ]));
        app.on_key(key(KeyCode::F(2)));
        app.on_key(key(KeyCode::Right));
        app.on_event(lab_state(&[
            ("orchestrator", acp_core::PodStatus::Idle),
            ("architect", acp_core::PodStatus::Idle),
            ("debugger", acp_core::PodStatus::Executing),
        ]));
        assert_eq!(
            app.lab.selected_pod().unwrap().name,
            "debugger",
            "selected by name, not position"
        );
        assert_eq!(app.lab.busy(), 1);
    }

    #[test]
    fn y_and_n_answer_the_hitl_station_like_a_and_r() {
        let mut app = connected();
        let mut rx = permission(&mut app, "t1");
        app.on_key(key(KeyCode::F(2)));
        assert!(
            app.lab.open && !app.pending.is_empty(),
            "the station is open on a pending request"
        );
        app.on_key(key(KeyCode::Char('Y')));
        assert_eq!(
            rx.try_recv().unwrap(),
            ApprovalDecision::Allow,
            "Y = Approve & Execute"
        );
        assert!(app.pending.is_empty());

        let mut rx = permission(&mut app, "t2");
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(
            rx.try_recv().unwrap(),
            ApprovalDecision::Reject,
            "N = Reject & Discard"
        );

        let mut rx = permission(&mut app, "t3");
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(
            rx.try_recv().unwrap(),
            ApprovalDecision::Allow,
            "the prompt's own keys still work"
        );
    }

    #[test]
    fn lab_keys_other_than_the_stations_cannot_answer_or_dodge_a_pending_approval() {
        let mut app = connected();
        let mut rx = permission(&mut app, "t1");
        app.on_key(key(KeyCode::F(2)));
        for c in ['i', ']', '[', 'l', 'h'] {
            assert!(
                app.on_key(key(KeyCode::Char(c))).is_none(),
                "{c} produced an effect"
            );
            assert!(
                !app.pending.is_empty() && rx.try_recv().is_err(),
                "{c} slipped past the approval"
            );
        }
        assert!(
            app.on_key(key(KeyCode::Char('r'))).is_none(),
            "r is Reject here, not release"
        );
        assert_eq!(rx.try_recv().unwrap(), ApprovalDecision::Reject);
    }

    #[test]
    fn k_at_the_station_rejects_the_call_and_isolates_the_pod_that_asked() {
        let mut app = connected();
        app.on_event(AppEvent::Harness(HarnessEvent::Lab(Box::new(
            acp_core::LabState {
                pods: vec![acp_core::LabPod {
                    name: "debugger".into(),
                    ..acp_core::LabPod::default()
                }],
                hitl: vec![acp_core::LabHitl {
                    id: "i1".into(),
                    pod: "debugger".into(),
                    status: "pending".into(),
                    ..acp_core::LabHitl::default()
                }],
                ..acp_core::LabState::default()
            },
        ))));
        let mut rx = permission(&mut app, "t1");
        app.on_key(key(KeyCode::F(2)));
        let (method, params) = ext_of(app.on_key(key(KeyCode::Char('K'))));
        assert_eq!(method, "_sierralab/lab/isolate");
        assert_eq!(params, serde_json::json!({"pod": "debugger"}));
        assert_eq!(
            rx.try_recv().unwrap(),
            ApprovalDecision::Reject,
            "the parked call is rejected too"
        );
        assert!(app.pending.is_empty());

        // outside the Lab, k is just a key the modal ignores
        let mut app = connected();
        let _rx = permission(&mut app, "t2");
        assert!(app.on_key(key(KeyCode::Char('K'))).is_none() && !app.pending.is_empty());
    }

    #[test]
    fn ctrl_c_still_cancels_a_turn_from_inside_the_lab() {
        let mut app = connected();
        type_text(&mut app, "go");
        app.on_key(key(KeyCode::Enter));
        app.lab.seen = true;
        app.on_key(key(KeyCode::F(2)));
        assert_eq!(app.on_key(ctrl('c')), Some(Effect::Cancel));
    }
}
