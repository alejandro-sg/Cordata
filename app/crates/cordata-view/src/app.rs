//! UI-independent application state: everything the screen shows and every
//! key's effect lives here so it can be unit-tested without a terminal.

use std::collections::VecDeque;
use std::time::Instant;

use acp_core::{AppEvent, ApprovalDecision, FileDiff, PermissionInfo, Restored, SessionInfo, StopReason, ToolCallInfo, ToolKind, ToolStatus};
use crate::key::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::commands::{self, Command};
use crate::input::Input;
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
    /// What an edit changes (old text read before it ran), for the diff viewer.
    pub diffs: Vec<FileDiff>,
}

#[derive(Debug)]
pub enum Message {
    /// The key/command reference, printed on demand (`?`, `/help`).
    Help,
    /// Session banner printed once when the backend connects.
    Welcome(SessionInfo),
    User(String),
    Agent(String),
    Thought(String),
    Tool(ToolCard),
    Divider(String),
    Info(String),
    /// A unified diff, drawn with colour (`/diff`).
    Diff(String),
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

/// Side effects the event loop performs on the session (kept out of `App`
/// so state transitions stay pure).
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    SendPrompt(String),
    Cancel,
    /// Wipe the visible screen and scrollback (`/clear`).
    ClearScreen,
    /// Start another session (a new engine) in the same project.
    NewSession,
    /// Show another session: `Next`, `Prev` or a 1-based slot number.
    SwitchSession(SwitchTarget),
    /// End the shown session (its engine shuts down).
    CloseSession,
    /// Load a saved session into the shown one (`SessionHandle::resume`).
    Resume(String),
    /// Delete this project's saved sessions and stop saving the shown one (`SessionHandle::forget`).
    Forget,
    /// A command run in the project folder, reported back as a `LocalEvent`. `share` hands its output to the agent
    /// with the next prompt (`!command`); `as_diff` shows it as a coloured diff (`/diff`).
    Shell {
        command: String,
        share: bool,
        as_diff: bool,
    },
    /// Put text on the clipboard.
    Copy(String),
    /// Read the working tree's diff for the review overlay; it comes back as `LocalEvent::ReviewDone`.
    Review,
    Quit,
}

/// Something that happened outside the session's own event stream (`main` delivers it).
#[derive(Debug)]
pub enum LocalEvent {
    /// The review overlay's `git diff`.
    ReviewDone(crate::shell::Outcome),
    ShellDone {
        command: String,
        outcome: crate::shell::Outcome,
        share: bool,
        as_diff: bool,
    },
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
    pub messages: Vec<Message>,
    pub input: Input,
    pub pending: VecDeque<PendingPermission>,
    /// Where the settings files are (None in unit tests that don't need them).
    pub store: Option<Store>,
    /// Merged settings as last read.
    pub settings: Settings,
    pub session: Option<SessionInfo>,
    pub conn: ConnState,
    pub turn_active: bool,
    pub cancelling: bool,
    pub hint: Option<String>,
    pub expand_tools: bool,
    /// Reasoning shows as a one-line preview unless expanded (Ctrl+R, /thinking).
    pub thoughts_expanded: bool,
    /// Messages already written to the terminal's scrollback. Everything
    /// before this index is final and never redrawn.
    pub committed: usize,
    /// Rows of the message at `committed` already written (a long reply is
    /// committed progressively as complete lines arrive).
    pub committed_rows: usize,
    /// Kind of the last committed message, for spacing between blocks.
    pub last_committed: Option<crate::view::Kind>,
    /// Prompts sent this run, oldest first, for Up/Down recall.
    history: Vec<String>,
    history_idx: Option<usize>,
    /// What was typed before the first Up, restored when Down walks past the newest.
    draft: String,
    pub turn_started: Option<Instant>,
    pub turn_tools: usize,
    pub spinner: usize,
    /// The approval panel shows the whole diff instead of its first rows (`d`).
    pub diff_expanded: bool,
    /// Every open session, for the footer (empty / one entry: no tab strip).
    pub tabs: Vec<TabBadge>,
    /// An initial prompt provided from CLI invocation (e.g. `cordata "prompt"`).
    pub initial_prompt: Option<String>,
    /// The completion menu above the prompt box (`/` commands, `@` files), derived from the text.
    pub menu: Option<crate::menu::Menu>,
    pub menu_dismissed: Option<String>,
    /// The project's files, listed the first time an `@` asks.
    pub files: Option<Vec<String>>,
    /// Slash commands from `commands/*.md` files.
    pub custom: Vec<crate::custom::Custom>,
    /// Prompts typed while the agent was still working, sent one by one as it frees up.
    pub queue: VecDeque<String>,
    /// What `!commands` printed since the last prompt: handed to the agent with the next one.
    pub shell_context: Vec<String>,
    /// Tokens in and out over this client's session, and the turns that spent them.
    pub session_tokens: (u64, u64),
    pub turns_done: u64,
    /// The project's git branch, for the footer.
    pub branch: Option<String>,
    /// A prompt offered as grey text in the empty box after a turn; Tab takes it, typing replaces it.
    pub suggestion: Option<String>,
    last_turn_secs: f64,
    /// Something wants the person (an approval, a long turn finished): the terminal is rung once, then this clears.
    pub attention: bool,
    /// The full-screen overlay in front of the conversation (command deck, HUD, diff review), if one is open.
    pub overlay: Option<crate::overlay::Overlay>,
    /// Builds an overlay when one is asked for: the front end's (`overlay::none` for one that draws these its own way).
    pub overlay_factory: crate::overlay::Factory,
    /// The person's own words for the approval card in front of them (`w`): a reason for refusing, what a plan should
    /// change, or an answer to a question the offered options did not cover.
    pub note: Option<Input>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            initial_prompt: None,
            messages: Vec::new(),
            input: Input::default(),
            pending: VecDeque::new(),
            store: None,
            settings: Settings::default(),
            session: None,
            conn: ConnState::Connecting,
            turn_active: false,
            cancelling: false,
            hint: None,
            expand_tools: false,
            thoughts_expanded: false,
            committed: 0,
            committed_rows: 0,
            last_committed: None,
            history: Vec::new(),
            history_idx: None,
            draft: String::new(),
            turn_started: None,
            turn_tools: 0,
            spinner: 0,
            diff_expanded: false,
            tabs: Vec::new(),
            menu: None,
            menu_dismissed: None,
            files: None,
            custom: Vec::new(),
            queue: VecDeque::new(),
            shell_context: Vec::new(),
            session_tokens: (0, 0),
            turns_done: 0,
            branch: None,
            suggestion: None,
            last_turn_secs: 0.0,
            attention: false,
            overlay: None,
            overlay_factory: crate::overlay::none,
            note: None,
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

    /// What the thread is called in lists: the first thing asked, shortened.
    pub fn title(&self) -> String {
        self.messages
            .iter()
            .find_map(|m| match m {
                Message::User(text) if !text.trim().is_empty() => Some(text.split_whitespace().collect::<Vec<_>>().join(" ")),
                _ => None,
            })
            .map_or_else(|| "New thread".to_owned(), |text| text.chars().take(48).collect())
    }

    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
    }

    /// Can the person type? Also while the agent works: what is typed then waits in the queue.
    pub fn can_type(&self) -> bool {
        self.conn == ConnState::Connected
            && self.pending.is_empty()
            && self.overlay.is_none()
    }

    /// Can a prompt be sent right now?
    pub fn input_enabled(&self) -> bool {
        self.can_type() && !self.turn_active
    }

    /// The next queued prompt, once the agent is free. Taken one at a time: each is a turn of its own.
    pub fn take_queued(&mut self) -> Option<Effect> {
        if !self.input_enabled() || self.queue.is_empty() {
            return None;
        }
        let line = self.queue.pop_front()?;
        self.run_line(line)
    }

    /// A turn that did not finish (cancelled, failed): what was queued behind it goes back into the prompt box, so
    /// the person decides — it was typed for a conversation that went somewhere else.
    fn return_queue_to_input(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let queued: Vec<String> = self.queue.drain(..).collect();
        let mut text = queued.join("\n");
        if !self.input.is_empty() {
            text = format!("{text}\n{}", self.input.text());
        }
        self.input.set_text(&text);
        let count = queued.len();
        self.info(format!(
            "{count} queued message{} back in the prompt box — the turn ended early",
            if count == 1 { "" } else { "s" }
        ));
    }

    /// An initial prompt provided from CLI invocation, sent once the session connects and is idle. Taken at most once.
    pub fn take_initial_prompt(&mut self) -> Option<Effect> {
        if self.initial_prompt.is_none() || !self.input_enabled() {
            return None;
        }
        let prompt = self.initial_prompt.take()?;
        let trimmed = prompt.trim().to_owned();
        if trimmed.is_empty() {
            return None;
        }
        self.messages.push(Message::User(sanitize(&trimmed)));
        Some(self.begin_turn(trimmed))
    }

    /// Can a prompt be sent right now? (Same as `input_enabled`, named for the
    /// commands that send one on the user's behalf.)
    pub fn input_enabled_for_prompt(&self) -> bool {
        self.input_enabled()
    }

    /// Start a turn with `text` as the prompt.
    pub fn begin_turn(&mut self, text: String) -> Effect {
        self.suggestion = None;
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

    // ---------------------------------------------------------------- events

    pub fn on_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Connected(info) => {
                self.conn = ConnState::Connected;
                self.messages.push(Message::Welcome(info.clone()));
                self.branch = crate::extras::git_branch(std::path::Path::new(&info.cwd));
                for notice in self.settings_notices() {
                    self.messages.push(Message::Info(notice));
                }
                self.session = Some(info);
            }
            AppEvent::AgentText(text) => self.append_agent(&text),
            AppEvent::AgentThought(text) => self.append_thought(&text),
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
                        card.content = sanitize(&content);
                    }
                }
            }
            AppEvent::PermissionRequested { info, decision } => {
                // The card's own input is whatever was streaming when the
                // call started (often partial); the request carries the final one.
                if let Some(input) = info.raw_input.clone()
                    && let Some(card) = self.card_mut(&info.tool_call_id)
                {
                    card.raw_input = Some(input);
                }
                self.attention = true;
                self.pending.push_back(PendingPermission {
                    info,
                    decision: Some(decision),
                });
            }
            AppEvent::TurnComplete(reason) => {
                if !matches!(reason, StopReason::Cancelled) {
                    self.turns_done += 1;
                }
                self.last_turn_secs = self.turn_started.map_or(0.0, |t| t.elapsed().as_secs_f64());
                self.end_turn();
                // A turn may have switched branches.
                self.branch = crate::extras::git_branch(&self.project_dir());
                self.suggestion = matches!(reason, StopReason::EndTurn).then(|| crate::extras::suggest_next(&self.messages)).flatten();
                if self.last_turn_secs >= 20.0 {
                    self.attention = true;
                }
                if matches!(reason, StopReason::Cancelled) {
                    self.return_queue_to_input();
                }
                if reason != StopReason::EndTurn {
                    self.messages
                        .push(Message::Divider(format!("turn ended: {}", reason.as_str())));
                }
            }
            AppEvent::TurnFailed(error) => {
                self.end_turn();
                self.return_queue_to_input();
                let clean_err = if error.contains("ApiKeyEnvNotFound") || error.contains("NoAuthFound") {
                    let provider = if error.contains("Anthropic") { "Anthropic" } else { "the configured" };
                    format!(
                        "turn failed: missing API key for {provider} model.\n→ Run `make key` in your terminal to save it to macOS Keychain, or export ANTHROPIC_API_KEY."
                    )
                } else {
                    format!("turn failed: {}", sanitize(&error))
                };
                self.messages.push(Message::Error(clean_err));
            }
            AppEvent::Notice(note) => {
                self.messages.push(Message::Info(sanitize(&note)));
            }
            AppEvent::SessionRestored { session_id, entries } => {
                self.messages.push(Message::Divider(format!("resumed session {session_id}")));
                for entry in entries {
                    self.messages.push(match entry {
                        Restored::User(text) => Message::User(sanitize(&text)),
                        Restored::Agent(text) => Message::Agent(sanitize(&text)),
                        Restored::Tool { name, summary } => Message::Info(sanitize(&format!("↳ {name} {summary}"))),
                    });
                }
                self.messages.push(Message::Divider("continuing".into()));
                if let Some(session) = &mut self.session {
                    session.session_id = session_id;
                }
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
                    self.messages.push(Message::Info("engine stopped".into()));
                }
            }
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
        if !self.can_type() {
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
        self.refresh_menu();
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

        // An open overlay owns the keyboard (session tabs and the approval below still work from inside it).
        if self.overlay.is_some() && !(ctrl && matches!(key.code, KeyCode::Char('n' | 'N'))) && !(alt || ctrl && matches!(key.code, KeyCode::Right | KeyCode::Left)) {
            return self.on_key_overlay(key);
        }
        if self.pending.is_empty() {
            if ctrl && key.code == KeyCode::Char('k') {
                return self.open_overlay(crate::overlay::Kind::Palette);
            }
            if let KeyCode::F(4) = key.code { return self.open_overlay(crate::overlay::Kind::Review) }
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

        if !self.pending.is_empty() {
            return self.on_key_permission(key);
        }

        if ctrl {
            return match key.code {
                KeyCode::Char('c') => self.on_ctrl_c(),
                KeyCode::Char('e') => {
                    self.expand_tools = !self.expand_tools;
                    None
                }
                KeyCode::Char('r') => {
                    self.thoughts_expanded = !self.thoughts_expanded;
                    None
                }
                KeyCode::Char('u') => {
                    if self.can_type() {
                        self.input.clear();
                        self.refresh_menu();
                    }
                    None
                }
                KeyCode::Char('j') => {
                    if self.can_type() {
                        self.input.insert('\n');
                    }
                    None
                }
                _ => None,
            };
        }

        self.handle_input_key(key)
    }

    pub fn handle_input_key(&mut self, key: KeyEvent) -> Option<Effect> {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // While the completion menu is open it owns the keys that would otherwise walk history, cycle the mode or
        // send the prompt.
        if self.menu.is_some() && self.can_type() {
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            match key.code {
                KeyCode::Up => {
                    self.menu_move(-1);
                    return None;
                }
                KeyCode::Down => {
                    self.menu_move(1);
                    return None;
                }
                KeyCode::Tab => return self.menu_accept(false),
                KeyCode::Enter if !alt && !shift => return self.menu_accept(true),
                KeyCode::Esc => {
                    self.menu_dismiss();
                    return None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Tab if self.input.is_empty() && self.suggestion.is_some() && self.can_type() => {
                if let Some(text) = self.suggestion.take() {
                    self.input.set_text(&text);
                }
                return None;
            }
            KeyCode::Esc => return self.on_escape(),
            _ => {}
        }

        if !self.can_type() {
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
            KeyCode::Enter => return self.submit(),
            KeyCode::Up if self.input.on_first_line() => self.history_back(),
            KeyCode::Down if self.input.on_last_line() => self.history_forward(),
            KeyCode::Char(c) => {
                self.suggestion = None;
                self.input.insert(c)
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            _ => {}
        }
        self.refresh_menu();
        None
    }

    /// Open an overlay (closing any other). The review needs the diff, which the event loop fetches.
    pub fn open_overlay(&mut self, kind: crate::overlay::Kind) -> Option<Effect> {
        self.overlay = (self.overlay_factory)(kind, self);
        (kind == crate::overlay::Kind::Review).then_some(Effect::Review)
    }

    /// Run what an overlay asked for: close it, open the next one, hand the effect on.
    fn apply_outcome(&mut self, overlay: crate::overlay::Overlay, outcome: crate::overlay::Outcome) -> Option<Effect> {
        let mut effect = outcome.effect;
        if outcome.close {
            self.overlay = None;
            if let Some(kind) = outcome.open {
                let fetch = self.open_overlay(kind);
                effect = effect.or(fetch);
            }
        } else {
            self.overlay = Some(overlay);
        }
        effect
    }

    fn on_key_overlay(&mut self, key: KeyEvent) -> Option<Effect> {
        let mut overlay = self.overlay.take()?;
        let outcome = overlay.on_key(self, key);
        self.apply_outcome(overlay, outcome)
    }

    /// A mouse event, delivered only while an overlay is open (the terminal's own mouse handling is untouched otherwise).
    pub fn on_mouse(&mut self, mouse: crate::key::MouseEvent) -> Option<Effect> {
        let mut overlay = self.overlay.take()?;
        let outcome = overlay.on_mouse(self, mouse);
        self.apply_outcome(overlay, outcome)
    }

    /// Enter: send what is in the box (or queue it, or run it as a command).
    pub fn submit(&mut self) -> Option<Effect> {
        let text = self.input.take();
        let text = text.trim_end().to_owned();
        self.history_idx = None;
        self.menu = None;
        self.suggestion = None;
        if text.is_empty() {
            return None;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.run_line(text)
    }

    /// One line the person entered, from the box or from the queue: a `!command`, a slash command (built in or a
    /// file), or a prompt. While the agent is working, only what would start a turn waits; the rest happens now.
    pub fn run_line(&mut self, text: String) -> Option<Effect> {
        if let Some(command) = text.strip_prefix('!').map(str::trim).filter(|c| !c.is_empty()) {
            return Some(Effect::Shell { command: command.to_owned(), share: true, as_diff: false });
        }
        let custom = self.custom_for(&text);
        let command = if custom.is_some() { None } else { commands::parse(&text) };
        let starts_a_turn = custom.is_some()
            || match &command {
                None => true,
                Some(c) => matches!(c, Command::Init),
            };
        if starts_a_turn && self.turn_active {
            self.queue.push_back(text);
            return None;
        }
        if let Some(custom) = custom {
            let args = text.trim().split_once(char::is_whitespace).map_or("", |(_, a)| a);
            let prompt = custom.render(args);
            return self.send_prompt(&text, prompt);
        }
        match command {
            Some(command) => self.run_command(command),
            None => self.send_prompt(&text.clone(), text),
        }
    }

    /// The custom command a typed line names, if it is one (a built-in of the same name wins).
    fn custom_for(&self, text: &str) -> Option<crate::custom::Custom> {
        let rest = text.trim().strip_prefix('/')?;
        let name = rest.split_whitespace().next()?;
        if !matches!(commands::parse(&format!("/{name}")), Some(Command::Unknown(_))) {
            return None;
        }
        self.custom.iter().find(|c| c.name.eq_ignore_ascii_case(name)).cloned()
    }

    /// Start a turn. `shown` is what the transcript echoes; `prompt` is what the model gets, with the files named by
    /// `@` and whatever `!commands` printed since the last turn put in front of it.
    fn send_prompt(&mut self, shown: &str, prompt: String) -> Option<Effect> {
        let expanded = crate::mentions::expand(&prompt, &self.project_dir());
        self.messages.push(Message::User(sanitize(shown)));
        if !expanded.attached.is_empty() {
            self.info(format!("attached {}", expanded.attached.join(", ")));
        }
        for (mention, why) in &expanded.skipped {
            self.info(format!("@{mention}: {why}"));
        }
        let mut prompt = expanded.prompt;
        if !self.shell_context.is_empty() {
            let context = std::mem::take(&mut self.shell_context).join("\n\n");
            prompt = format!("{context}\n\n{prompt}");
        }
        Some(self.begin_turn(prompt))
    }

    /// The result of a `!command`, shown to the person and kept for the agent's next turn.
    pub fn on_local(&mut self, event: LocalEvent) {
        let (command, outcome, share, as_diff) = match event {
            LocalEvent::ReviewDone(outcome) => {
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.review_loaded(&outcome);
                }
                return;
            }
            LocalEvent::ShellDone { command, outcome, share, as_diff } => (command, outcome, share, as_diff),
        };
        let status = match (outcome.timed_out, outcome.code) {
            (true, _) => "timed out after 60s".to_owned(),
            (_, Some(0)) => "exit 0".to_owned(),
            (_, Some(code)) => format!("exit {code}"),
            (_, None) => "killed".to_owned(),
        };
        let output = sanitize(outcome.output.trim_end());
        let shown_lines: Vec<&str> = output.lines().collect();
        let tail_start = shown_lines.len().saturating_sub(40);
        let mut text = format!("$ {command}\n");
        if tail_start > 0 {
            text.push_str(&format!("… {tail_start} earlier lines\n"));
        }
        text.push_str(&shown_lines[tail_start..].join("\n"));
        text.push_str(&format!("\n[{status}]"));
        if as_diff {
            self.messages.push(Message::Diff(if output.trim().is_empty() { "no changes".to_owned() } else { output.clone() }));
            return;
        }
        self.messages.push(Message::Info(text));
        if !share {
            return;
        }
        let cut = if outcome.cut { " (the start was cut)" } else { "" };
        self.shell_context.push(format!(
            "[The user ran `{command}` in their terminal ({status}){cut}. It printed:]\n{}",
            output
        ));
    }

    /// Esc: interrupt a running turn (like Ctrl+C), else clear what's typed.
    fn on_escape(&mut self) -> Option<Effect> {
        // Text typed ahead is cleared first; a second Esc stops the turn. Throwing away a half-written message must
        // not cost the agent's work.
        if self.turn_active && self.can_type() && !self.input.is_empty() {
            self.input.clear();
            self.history_idx = None;
            self.refresh_menu();
            return None;
        }
        if self.turn_active {
            return self.on_ctrl_c();
        }
        if self.input_enabled() {
            self.input.clear();
            self.history_idx = None;
        }
        None
    }

    pub fn history_back(&mut self) {
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

    pub fn history_forward(&mut self) {
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

    pub fn info(&mut self, text: impl Into<String>) {
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
            Command::Tools => {
                self.expand_tools = !self.expand_tools;
                self.info(if self.expand_tools {
                    "tool results expanded"
                } else {
                    "tool results collapsed"
                });
            }
            Command::Memory => self.run_memory(),
            Command::Init => return self.run_init(),
            Command::Status => {
                self.run_status();
            }
            Command::Diff => return self.run_diff(),
            Command::Copy => return self.run_copy(),
            Command::Export(arg) => self.run_export(arg),
            Command::Palette => return self.open_overlay(crate::overlay::Kind::Palette),
            Command::Review => return self.open_overlay(crate::overlay::Kind::Review),
            Command::Floorplan(arg) => return self.run_floorplan(arg),
            Command::New => return Some(Effect::NewSession),
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
            Command::History => self.run_history(),
            Command::Resume(arg) => return self.run_resume(arg),
            Command::Forget(arg) => return self.run_forget(arg),
            Command::Permissions => self.run_permissions(),
            Command::Unknown(name) => self.info(format!("unknown command /{name} — try /help")),
        }
        None
    }

    pub fn on_key_permission(&mut self, key: KeyEvent) -> Option<Effect> {
        let offers_always = self
            .pending
            .front()
            .is_some_and(|pending| pending.info.always_label.is_some());
        if self.note.is_some() {
            return self.on_key_note(key);
        }
        let decision = match key.code {
            KeyCode::Char('d' | 'D') => {
                self.diff_expanded = !self.diff_expanded;
                return None;
            }
            // The person's own words: a reason for refusing.
            KeyCode::Char('w' | 'W') => {
                self.note = Some(Input::default());
                return None;
            }
            // `a`/`y` allow once, `s` allows the tool for the session, `r`/`n`/Esc reject.
            KeyCode::Char('a' | 'A' | 'y' | 'Y') => ApprovalDecision::Allow,
            KeyCode::Char('s' | 'S') if offers_always => ApprovalDecision::AllowAlways,
            KeyCode::Char('r' | 'R' | 'n' | 'N') | KeyCode::Esc => ApprovalDecision::Reject,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Documented, not a bug: a cancel is only checked between streamed events, and the engine is parked on
                // this very request, so cancelling now would do nothing.
                self.hint = Some(
                    "answer the permission request first — Ctrl+C can't cancel while it's open"
                        .into(),
                );
                return None;
            }
            _ => return None,
        };
        self.answer(decision);
        None
    }

    /// Answer the approval at the front of the queue — from a key above, or from a front end's button. An "always"
    /// says so in the transcript.
    pub fn answer(&mut self, decision: ApprovalDecision) {
        self.diff_expanded = false;
        if let Some(mut pending) = self.pending.pop_front() {
            if decision == ApprovalDecision::AllowAlways
                && let Some(label) = &pending.info.always_label
            {
                self.messages.push(Message::Info(sanitize(label)));
            }
            pending.answer(decision);
        }
        self.hint = None;
    }

    /// Type `text` into the composer and press Enter: what a front end without its own key handling calls to send.
    /// Slash commands, `!` shell, `@` mentions, custom commands and the type-ahead queue all apply exactly as typed.
    pub fn submit_text(&mut self, text: &str) -> Option<Effect> {
        self.input.set_text(text);
        self.submit()
    }

    /// Typing in the note box under an approval card: Enter sends it as the answer (or the reason), Esc goes back to the card.
    fn on_key_note(&mut self, key: KeyEvent) -> Option<Effect> {
        let note = self.note.as_mut()?;
        match key.code {
            KeyCode::Esc => self.note = None,
            KeyCode::Enter => {
                let text = note.take().trim().to_owned();
                self.note = None;
                if !text.is_empty()
                    && let Some(mut pending) = self.pending.pop_front()
                {
                    self.diff_expanded = false;
                    pending.answer(ApprovalDecision::Say(text));
                    self.hint = None;
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.note = None,
            KeyCode::Char(c) => note.insert(c),
            KeyCode::Backspace => note.backspace(),
            KeyCode::Delete => note.delete(),
            KeyCode::Left => note.left(),
            KeyCode::Right => note.right(),
            KeyCode::Home => note.home(),
            KeyCode::End => note.end(),
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

}


fn card_from(info: ToolCallInfo) -> ToolCard {
    ToolCard {
        id: info.id,
        name: sanitize(&info.name),
        kind: info.kind,
        status: info.status,
        raw_input: info.raw_input,
        content: sanitize(&info.content),
        diffs: info.diffs,
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
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn connected() -> App {
        let mut app = App::new();
        app.on_event(AppEvent::Connected(SessionInfo { session_id: "s1".into(), cwd: "/tmp".into(), model: "m1".into(), transport: "native · m1".into() }));
        // The banner is covered by its own test; most tests want an empty transcript.
        app.messages.clear();
        app
    }

    #[test]
    fn connecting_adds_the_session_banner_to_the_transcript() {
        let mut app = App::new();
        app.on_event(AppEvent::Connected(SessionInfo { session_id: "s1".into(), cwd: "/tmp".into(), model: "m1".into(), transport: "native · m1".into() }));
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
    fn enter_sends_a_prompt_and_what_is_typed_meanwhile_waits_its_turn() {
        let mut app = connected();
        type_text(&mut app, "hello");
        let effect = app.on_key(key(KeyCode::Enter));
        assert_eq!(effect, Some(Effect::SendPrompt("hello".into())));
        assert!(app.turn_active && !app.input_enabled() && app.can_type());

        // The agent is working: typing still works, and Enter lines the message up instead of sending it.
        type_text(&mut app, "then run the tests");
        assert_eq!(app.input.text(), "then run the tests");
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert!(app.input.is_empty());
        assert_eq!(app.queue.len(), 1);
        assert!(app.take_queued().is_none(), "not while the turn runs");
        assert!(
            app.messages.iter().all(|m| !matches!(m, Message::User(t) if t == "then run the tests")),
            "a queued message is not in the transcript until it is sent"
        );

        // The turn ends: the queued message goes out as a turn of its own.
        app.on_event(AppEvent::TurnComplete(StopReason::EndTurn));
        assert!(app.input_enabled());
        assert_eq!(app.take_queued(), Some(Effect::SendPrompt("then run the tests".into())));
        assert!(app.turn_active && app.queue.is_empty());
        assert!(matches!(app.messages.last(), Some(Message::User(t)) if t == "then run the tests"));
    }

    #[test]
    fn a_cancelled_turn_hands_the_queue_back_instead_of_sending_it() {
        let mut app = connected();
        type_text(&mut app, "first");
        app.on_key(key(KeyCode::Enter));
        for line in ["second", "third"] {
            type_text(&mut app, line);
            app.on_key(key(KeyCode::Enter));
        }
        assert_eq!(app.queue.len(), 2);
        app.on_event(AppEvent::TurnComplete(StopReason::Cancelled));
        assert!(app.queue.is_empty() && app.take_queued().is_none());
        assert_eq!(app.input.text(), "second\nthird");
        assert_info(&app, "2 queued messages back in the prompt box");

        // A failed turn does the same.
        let mut failed = connected();
        type_text(&mut failed, "go");
        failed.on_key(key(KeyCode::Enter));
        type_text(&mut failed, "later");
        failed.on_key(key(KeyCode::Enter));
        failed.on_event(AppEvent::TurnFailed("boom".into()));
        assert_eq!(failed.input.text(), "later");
    }

    #[test]
    fn slash_commands_typed_mid_turn_run_now_except_those_that_start_a_turn() {
        let mut app = connected();
        type_text(&mut app, "work");
        app.on_key(key(KeyCode::Enter));
        // /tools only flips a setting on this side.
        type_text(&mut app, "/tools");
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert!(app.expand_tools, "ran immediately");
        assert!(app.queue.is_empty());
        // /init starts a turn, so it waits.
        type_text(&mut app, "/init");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.queue.front().map(String::as_str), Some("/init"));
    }

    #[test]
    fn a_bang_runs_the_command_for_the_person_and_tells_the_agent_what_it_printed() {
        let mut app = connected();
        type_text(&mut app, "!git status");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Some(Effect::Shell { command: "git status".into(), share: true, as_diff: false }));
        assert!(!app.turn_active, "it is the person's command, not a turn");
        app.on_local(LocalEvent::ShellDone {
            command: "git status".into(),
            outcome: crate::shell::Outcome { output: "On branch main\n".into(), code: Some(0), timed_out: false, cut: false },
            share: true,
            as_diff: false,
        });
        assert_info(&app, "$ git status\nOn branch main\n[exit 0]");
        // The next prompt carries it.
        type_text(&mut app, "why is it dirty?");
        let Some(Effect::SendPrompt(prompt)) = app.on_key(key(KeyCode::Enter)) else { panic!("a prompt") };
        assert!(prompt.starts_with("[The user ran `git status` in their terminal (exit 0)."), "{prompt}");
        assert!(prompt.contains("On branch main") && prompt.ends_with("why is it dirty?"), "{prompt}");
        // …and only that one.
        type_text(&mut app, "thanks");
        app.on_event(AppEvent::TurnComplete(StopReason::EndTurn));
        let Some(Effect::SendPrompt(next)) = app.on_key(key(KeyCode::Enter)) else { panic!("a prompt") };
        assert_eq!(next, "thanks");
    }

    #[test]
    fn a_custom_command_sends_its_file_as_the_prompt_and_a_built_in_keeps_its_name() {
        let mut app = connected();
        app.custom = vec![
            crate::custom::Custom { name: "review".into(), description: String::new(), hint: String::new(), body: "Review the diff, focus: $ARGUMENTS".into(), scope: "project" },
            crate::custom::Custom { name: "help".into(), description: String::new(), hint: String::new(), body: "impostor".into(), scope: "project" },
        ];
        type_text(&mut app, "/review security ");
        let Some(Effect::SendPrompt(prompt)) = app.on_key(key(KeyCode::Enter)) else { panic!("a prompt") };
        assert_eq!(prompt, "Review the diff, focus: security");
        assert!(matches!(app.messages.last(), Some(Message::User(t)) if t == "/review security"), "the transcript echoes what was typed");

        app.on_event(AppEvent::TurnComplete(StopReason::EndTurn));
        type_text(&mut app, "/help ");
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert!(matches!(app.messages.last(), Some(Message::Help)));
    }

    #[test]
    fn at_mentions_put_the_files_in_front_of_the_agent_and_say_so() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "remember the milk\n").unwrap();
        let mut app = connected();
        app.session.as_mut().unwrap().cwd = dir.path().to_string_lossy().into_owned();
        type_text(&mut app, "summarise @notes.txt and @missing.rs ");
        let effect = app.on_key(key(KeyCode::Enter));
        let Some(Effect::SendPrompt(prompt)) = effect else { panic!("a prompt, got {effect:?}") };
        assert!(prompt.starts_with("summarise @notes.txt and @missing.rs"), "{prompt}");
        assert!(prompt.contains("remember the milk"), "{prompt}");
        assert_info(&app, "attached notes.txt");
        assert_info(&app, "@missing.rs: no such file");
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
                always_label: None,
            },
            decision: tx,
        });
        assert_eq!(app.on_key(ctrl('c')), None);
        assert!(app.hint.as_deref().unwrap().contains("permission"));
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
    fn slash_commands_run_locally_and_are_never_sent_to_the_model() {
        let mut app = connected();
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
        busy.on_paste("queued for later");
        assert_eq!(busy.input.text(), "queued for later", "the box takes text while the agent works");
        let mut away = connected();
        away.conn = ConnState::Closed { error: None };
        away.on_paste("nowhere to go");
        assert!(away.input.is_empty(), "no pasting into a closed session");
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
    fn escape_clears_what_was_typed_ahead_before_it_stops_the_agent() {
        let mut app = connected();
        submit(&mut app, "go");
        type_text(&mut app, "never mind this");
        assert_eq!(app.on_key(key(KeyCode::Esc)), None, "the first Esc only clears the draft");
        assert!(app.input.is_empty() && app.turn_active && !app.cancelling);
        assert_eq!(app.on_key(key(KeyCode::Esc)), Some(Effect::Cancel), "the second one stops the turn");
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

    #[test]
    fn initial_prompt_is_dispatched_once_connected() {
        let mut app = App::new();
        app.initial_prompt = Some("what is this project".into());
        assert!(app.take_initial_prompt().is_none(), "not connected yet");
        app.conn = ConnState::Connected;
        let effect = app.take_initial_prompt();
        assert_eq!(effect, Some(Effect::SendPrompt("what is this project".into())));
        assert!(app.take_initial_prompt().is_none(), "taken once");
        assert!(matches!(app.messages.last(), Some(Message::User(u)) if u == "what is this project"));
    }

    #[test]
    fn typing_a_slash_opens_the_menu_and_tab_completes_without_running() {
        let mut app = connected();
        type_text(&mut app, "/hel");
        assert!(app.menu.is_some(), "the menu follows the text");
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.input.text(), "/help ");
        assert!(app.menu.is_none(), "a space ends the command word");
        assert!(app.messages.iter().all(|m| !matches!(m, Message::Help)), "Tab never runs it");

        app.input.clear();
        type_text(&mut app, "/c");
        let labels: Vec<_> = app.menu.as_ref().unwrap().items.iter().map(|i| i.label.clone()).collect();
        assert!(labels.contains(&"/clear".to_owned()) && labels.contains(&"/copy".to_owned()), "{labels:?}");
        // Down moves the highlight instead of walking prompt history.
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.menu.as_ref().unwrap().selected, 1);
        // Esc closes the menu and keeps the text.
        app.on_key(key(KeyCode::Esc));
        assert!(app.menu.is_none() && app.input.text() == "/c");
    }

}
