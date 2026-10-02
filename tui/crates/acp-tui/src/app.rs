//! UI-independent application state: everything the screen shows and every
//! key's effect lives here so it can be unit-tested without a terminal.

use std::collections::VecDeque;

use acp_core::{
    AppEvent, ApprovalDecision, ConfigChoice, ConfigOptionInfo, ModeInfo, PermissionInfo,
    SessionInfo, StopReason, ToolCallInfo, ToolKind, ToolStatus,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::input::Input;

#[derive(Debug)]
pub struct ToolCard {
    pub id: String,
    pub name: String,
    pub kind: ToolKind,
    pub status: ToolStatus,
    pub raw_input: Option<Value>,
    pub content: String,
    /// Result text carries the backend's untrusted-output guard markers.
    pub untrusted: bool,
    /// Tool whose output the backend wraps as untrusted (a hint, pre-result).
    pub untrusted_source: bool,
}

#[derive(Debug)]
pub enum Message {
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
    SetConfig { id: String, value: String },
    Quit,
}

pub struct App {
    pub messages: Vec<Message>,
    pub input: Input,
    pub pending: VecDeque<PendingPermission>,
    pub picker: Option<Picker>,
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
    pub show_thoughts: bool,
    /// Lines scrolled up from the bottom; 0 follows new output.
    pub scroll: usize,
    pub last_total_lines: usize,
    pub spinner: usize,
}

impl App {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            input: Input::default(),
            pending: VecDeque::new(),
            picker: None,
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
            show_thoughts: true,
            scroll: 0,
            last_total_lines: 0,
            spinner: 0,
        }
    }

    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
    }

    pub fn input_enabled(&self) -> bool {
        self.conn == ConnState::Connected
            && !self.turn_active
            && self.pending.is_empty()
            && self.picker.is_none()
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
            AppEvent::ToolCallStarted(info) => self.messages.push(Message::Tool(card_from(info))),
            AppEvent::ToolCallUpdated(patch) => {
                if let Some(card) = self.card_mut(&patch.id) {
                    if let Some(status) = patch.status {
                        card.status = status;
                    }
                    if let Some(content) = patch.content {
                        card.content = sanitize(&content);
                    }
                    card.untrusted |= patch.untrusted;
                }
            }
            AppEvent::TitleChanged(title) => self.title = Some(sanitize(&title)),
            AppEvent::ModeChanged(mode) => {
                if self.pending_mode.as_deref() == Some(mode.as_str()) {
                    self.pending_mode = None;
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

    fn end_turn(&mut self) {
        self.turn_active = false;
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

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl && key.code == KeyCode::Char('q') {
            return Some(Effect::Quit);
        }

        // Scrolling works everywhere, including under a modal.
        match key.code {
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10);
                return None;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                return None;
            }
            _ => {}
        }

        if !self.pending.is_empty() {
            return self.on_key_permission(key);
        }
        if self.picker.is_some() {
            return self.on_key_picker(key);
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
                    self.show_thoughts = !self.show_thoughts;
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
            _ => {}
        }

        if !self.input_enabled() {
            return None;
        }
        match key.code {
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.input.insert('\n')
            }
            KeyCode::Enter => {
                let text = self.input.take();
                let text = text.trim_end().to_owned();
                if text.is_empty() {
                    return None;
                }
                self.messages.push(Message::User(sanitize(&text)));
                self.turn_active = true;
                self.scroll = 0;
                return Some(Effect::SendPrompt(text));
            }
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

    fn on_key_permission(&mut self, key: KeyEvent) -> Option<Effect> {
        let decision = match key.code {
            KeyCode::Char('a' | 'A') => ApprovalDecision::Allow,
            KeyCode::Char('r' | 'R') | KeyCode::Esc => ApprovalDecision::Reject,
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
        if let Some(mut pending) = self.pending.pop_front() {
            pending.answer(decision);
        }
        self.hint = None;
        None
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

fn card_from(info: ToolCallInfo) -> ToolCard {
    ToolCard {
        id: info.id,
        name: sanitize(&info.name),
        kind: info.kind,
        status: info.status,
        raw_input: info.raw_input,
        content: sanitize(&info.content),
        untrusted: info.untrusted,
        untrusted_source: info.untrusted_source,
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

    fn connected() -> App {
        let mut app = App::new();
        app.on_event(AppEvent::Connected(SessionInfo {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
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
        app
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
        }));
        app.on_event(AppEvent::ToolCallUpdated(ToolCallPatch {
            id: "t1".into(),
            status: Some(ToolStatus::Completed),
            content: Some("<<<UNTRUSTED_TOOL_OUTPUT>>>\nx".into()),
            untrusted: true,
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

    #[test]
    fn sanitize_strips_escapes_and_control_chars() {
        assert_eq!(
            sanitize("a\u{1b}[31mred\u{1b}[0m b\u{7}c\r\nd\te"),
            "ared bc\nd    e"
        );
        // Non-CSI escapes lose their ESC (+ one byte); the leftover is inert text.
        assert_eq!(sanitize("x\u{1b}]0;evil\u{7}y"), "x0;evily");
    }
}
