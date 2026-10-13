//! The window (docs/app.md §4): the rail on the left (project, open sessions, the project's other conversations, what
//! needs you), the conversation in the centre (transcript, the approval card, the composer with its `/` and `@` menus,
//! the gauges), the dock on the right (Mission Control, the task list, the goal).
//!
//! Several sessions can be open at once, one shown; each is its own ACP connection (the backend is one process per
//! session). Keys follow the terminal's: Esc stops a turn, Shift+Tab cycles the mode, ⌘N opens a session, ⌘W closes
//! one, ⌘[ and ⌘] switch, ⌘K opens the command menu, ⌘L focuses the composer.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use acp_core::{ApprovalDecision, BackendTransport};
use cordata_theme::{Role, mode_role, tracked};
use cordata_view::app::{ConnState, PendingPermission, SwitchTarget};
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EntityId, InteractiveElement as _, IntoElement, KeyDownEvent, ListAlignment, ListState, ParentElement, Render,
    SharedString, StatefulInteractiveElement as _, Styled, Subscription, Window, actions, div, list, px, relative,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};

use crate::hud;
use crate::mission::{self, Mission};
use crate::platform::Display;
use crate::session::{Connection, Row, Runtime, Session, SessionEvent, Target, WindowRequest};
use crate::theme::{color, tint};

mod browser;
mod editor;
mod models;
mod review;
mod settings_view;
mod terminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockTab {
    #[default]
    Mission,
    Files,
    Editor,
    Review,
    Terminal,
    Browser,
    Models,
    Settings,
}

actions!(
    cordata,
    [
        StopTurn,
        CycleMode,
        FocusComposer,
        NewSession,
        CloseSession,
        NextSession,
        PrevSession,
        CommandMenu,
        ToggleThoughts,
        ToggleTools,
        Help,
        ShowMission,
        ShowFiles,
        ShowEditor,
        ShowReview,
        ShowTerminal,
        ShowBrowser,
        ShowModels,
        ShowSettings,
        OpenFiles,
        SaveFile,
    ]
);

const RAIL_WIDTH: f32 = 248.;
const DOCK_WIDTH: f32 = 320.;
const DOCK_WIDTH_WIDE: f32 = 560.;
/// How many of the newest blocks are re-measured when anything changes (a streaming reply, a tool card updating).
const REMEASURE_TAIL: usize = 8;
/// How often the rail re-reads the project's conversations.
const LISTING_EVERY: Duration = Duration::from_secs(15);

pub struct Workspace {
    connection: Connection,
    pub(super) cwd: PathBuf,
    /// Every open session, and the one shown.
    sessions: Vec<Entity<Session>>,
    pub(super) session: Entity<Session>,
    subscriptions: Vec<(EntityId, Vec<Subscription>)>,
    composer: Entity<InputState>,
    transcript: ListState,
    /// (session, messages, version) last laid out.
    seen: (EntityId, usize, u64),
    /// Tool cards opened to their full result.
    expanded: HashSet<String>,
    /// The project's conversations (the hub's, or the saved stdio sessions).
    listing: Vec<Row>,
    listing_error: Option<String>,
    model_menu: bool,
    /// `w` on an approval card: the next Enter sends the composer's text as the answer (the reason, or one's own words).
    saying: bool,
    /// Mission Control's state (the dock).
    mission: Mission,
    /// The dock's "start a run" box.
    goal_input: Entity<InputState>,
    /// Ticks of the dock's poll (runs list every few, the selected run every one).
    mission_tick: u64,
    /// `--run GOAL`: started once the session connects.
    start_run: Option<String>,
    display: Display,
    glass: bool,
    pub(super) dock_tab: DockTab,
    pub(super) files: editor::Files,
    pub(super) file_filter: Entity<InputState>,
    pub(super) review: review::Review,
    pub(super) terminal: terminal::Terminal,
    pub(super) browser_view: browser::BrowserView,
    pub(super) models_view: models::ModelsView,
    pub(super) settings_view: settings_view::SettingsView,
    /// `--selftest`: answer every approval with Allow and quit with a summary once the first turn ends.
    selftest: Option<SelfTest>,
    _subscriptions: Vec<Subscription>,
}

/// What the window was launched to do (command-line flags).
#[derive(Default)]
pub struct Launch {
    /// `--prompt`: sent once connected.
    pub prompt: Option<String>,
    /// `--run`: an orchestrator run started once connected.
    pub run: Option<String>,
    /// Liquid Glass went in behind the window.
    pub glass: bool,
    /// `--selftest` with a prompt or a run: drive it to its end and quit with a summary.
    pub selftest: bool,
}

#[derive(Default)]
struct SelfTest {
    approvals: usize,
    /// A turn has started (`turns_done` counts the real backend's `_cordata/turn` receipts, which a fake agent lacks).
    started: bool,
}

impl Workspace {
    pub fn new(connection: Connection, cwd: PathBuf, launch: Launch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Launch { prompt: initial_prompt, run: start_run, glass, selftest } = launch;
        let composer = cx.new(|cx| InputState::new(window, cx).placeholder("Ask Cordata…   / commands · @ files · ! shell"));
        let display = cx.try_global::<Display>().copied().unwrap_or_default();
        let first = cx.new(|cx| Session::start(&connection, Target::Default, cwd.clone(), initial_prompt, cx));
        let goal_input = cx.new(|cx| InputState::new(window, cx).placeholder("A multi-step goal for the orchestrator…"));
        let file_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files…"));
        let terminal = terminal::Terminal::new(window, cx);
        let browser_view = browser::BrowserView::new(window, cx);
        let models_view = models::ModelsView::new(window, cx);
        let settings_view = settings_view::SettingsView::new(window, cx);
        let composer_subscription = cx.subscribe_in(&composer, window, Self::on_composer);
        let file_filter_subscription = cx.subscribe_in(&file_filter, window, Self::on_file_filter);
        composer.update(cx, |state, cx| state.focus(window, cx));
        let mut this = Self {
            connection,
            cwd,
            sessions: Vec::new(),
            session: first.clone(),
            subscriptions: Vec::new(),
            composer,
            transcript: ListState::new(0, ListAlignment::Bottom, px(1200.)),
            seen: (first.entity_id(), 0, 0),
            expanded: HashSet::new(),
            listing: Vec::new(),
            listing_error: None,
            model_menu: false,
            saying: false,
            mission: Mission::default(),
            goal_input,
            mission_tick: 0,
            start_run,
            display,
            glass,
            dock_tab: DockTab::default(),
            files: editor::Files::default(),
            file_filter,
            review: review::Review::default(),
            terminal,
            browser_view,
            models_view,
            settings_view,
            selftest: selftest.then(SelfTest::default),
            _subscriptions: vec![composer_subscription, file_filter_subscription],
        };
        this.adopt(first, window, cx);
        this.schedule_listing(cx);
        this.schedule_mission(cx);
        this.load_tree(cx);
        this.load_review(cx);
        this.load_settings_data(cx);
        this
    }

    // -------------------------------------------------------------------------------------------- sessions

    /// Watch a session and show it.
    fn adopt(&mut self, session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) {
        let id = session.entity_id();
        let subs = vec![
            cx.observe(&session, move |this, observed, cx| {
                if observed.entity_id() == this.session.entity_id() {
                    this.step_selftest(cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&session, window, |this, _, event: &SessionEvent, window, cx| match event {
                // The Dock icon bounces when the window is not in front (an approval, a finished turn).
                SessionEvent::NeedsAttention => {
                    window.request_attention();
                    this.refresh_listing(cx);
                }
                SessionEvent::Listed => this.take_stdio_listing(cx),
                SessionEvent::Data(tag) => this.on_data(tag, cx),
                SessionEvent::Window(request) => this.on_window_request(request.clone(), window, cx),
            }),
        ];
        self.subscriptions.push((id, subs));
        self.sessions.push(session.clone());
        self.show(session, cx);
    }

    fn show(&mut self, session: Entity<Session>, cx: &mut Context<Self>) {
        self.session = session;
        self.model_menu = false;
        let len = self.session.read(cx).view.messages.len();
        self.transcript.reset(len);
        self.seen = (self.session.entity_id(), len, self.session.read(cx).version);
        cx.notify();
    }

    fn open(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        // Already open: show it rather than attach a second client (the daemon allows one driver per conversation).
        if let Some(existing) = self.sessions.iter().find(|s| matches_target(s.read(cx), &target)).cloned() {
            self.show(existing, cx);
            return;
        }
        let (connection, cwd) = (self.connection.clone(), self.cwd.clone());
        let session = cx.new(|cx| Session::start(&connection, target, cwd, None, cx));
        self.adopt(session, window, cx);
        self.focus_composer(&FocusComposer, window, cx);
    }

    fn new_target(&self) -> Target {
        match self.connection.transport {
            BackendTransport::Hub { .. } => Target::Conversation("new".into()),
            _ => Target::Default,
        }
    }

    fn close_active(&mut self, cx: &mut Context<Self>) {
        let id = self.session.entity_id();
        if self.sessions.len() <= 1 {
            return; // the window always shows one session; ⌘Q quits
        }
        let index = self.sessions.iter().position(|s| s.entity_id() == id).unwrap_or(0);
        self.sessions.remove(index);
        self.subscriptions.retain(|(sid, _)| *sid != id);
        let next = self.sessions[index.min(self.sessions.len() - 1)].clone();
        self.show(next, cx);
    }

    fn switch(&mut self, target: SwitchTarget, cx: &mut Context<Self>) {
        let len = self.sessions.len();
        let index = self.sessions.iter().position(|s| s.entity_id() == self.session.entity_id()).unwrap_or(0);
        let next = match target {
            SwitchTarget::Next => (index + 1) % len,
            SwitchTarget::Prev => (index + len - 1) % len,
            SwitchTarget::Slot(n) => n.saturating_sub(1).min(len - 1),
        };
        let session = self.sessions[next].clone();
        self.show(session, cx);
    }

    fn on_window_request(&mut self, request: WindowRequest, window: &mut Window, cx: &mut Context<Self>) {
        match request {
            WindowRequest::New => self.open(self.new_target(), window, cx),
            WindowRequest::Close => self.close_active(cx),
            WindowRequest::Switch(target) => self.switch(target, cx),
            WindowRequest::Open(target) => self.open(target, window, cx),
        }
    }

    /// A dropped connection, opened again on the same conversation (by id over stdio, by handle on the hub).
    fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let old = self.session.read(cx);
        let target = match (&self.connection.transport, old.session_id(), &old.target) {
            (BackendTransport::Hub { .. }, _, target) => match old.hub_handle() {
                Some(handle) => Target::Conversation(handle),
                None => match target {
                    Target::Conversation(handle) if handle != "new" => Target::Conversation(handle.clone()),
                    _ => Target::Default,
                },
            },
            (_, Some(id), _) => Target::Resume(id),
            (_, None, target) => target.clone(),
        };
        let id = self.session.entity_id();
        let (connection, cwd) = (self.connection.clone(), self.cwd.clone());
        let fresh = cx.new(|cx| Session::start(&connection, target, cwd, None, cx));
        self.sessions.retain(|s| s.entity_id() != id);
        self.subscriptions.retain(|(sid, _)| *sid != id);
        self.adopt(fresh, window, cx);
    }

    // -------------------------------------------------------------------------------------------- Mission Control

    fn schedule_mission(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(1200)).await;
                if this.update(cx, |this, cx| this.poll_mission(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn poll_mission(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        if !session.connected() {
            return;
        }
        if let Some(goal) = self.start_run.take() {
            session.request("_cordata/orchestrator/start", serde_json::json!({ "goal": goal }), mission::START_TAG);
        }
        self.mission_tick += 1;
        if self.mission_tick % 8 == 1 {
            session.request("_cordata/orchestrator/runs", serde_json::json!({ "limit": 20 }), mission::RUNS_TAG);
        }
        if let Some(run_id) = self.mission.selected.clone() {
            // A live run is followed closely; a finished one only rarely (its last events, a late resume).
            if !self.mission.polling && (self.mission.live() || self.mission.detail.is_none() || self.mission_tick.is_multiple_of(5)) {
                self.mission.polling = session.request("_cordata/orchestrator/events", serde_json::json!({ "runId": run_id, "offset": self.mission.offset }), mission::EVENTS_TAG);
            }
            if self.mission_tick.is_multiple_of(3) || self.mission.detail.is_none() {
                session.request("_cordata/orchestrator/show", serde_json::json!({ "runId": run_id }), mission::SHOW_TAG);
            }
        }
    }

    fn on_data(&mut self, tag: &str, cx: &mut Context<Self>) {
        if tag.starts_with(editor::LSP_TAG) {
            self.on_lsp(tag, cx);
            return;
        }
        let Some(result) = self.session.read(cx).data.get(tag).cloned() else { return };
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if tag != mission::EVENTS_TAG {
                    self.mission.notice = Some(error);
                }
                self.mission.polling = false;
                cx.notify();
                return;
            }
        };
        match tag {
            mission::RUNS_TAG => self.mission.runs = value.get("runs").and_then(|r| r.as_array()).cloned().unwrap_or_default(),
            mission::SHOW_TAG if value.get("runId").and_then(|r| r.as_str()) == self.mission.selected.as_deref() => self.mission.detail = Some(value),
            mission::EVENTS_TAG => {
                if self.mission.take_events(&value) {
                    self.poll_mission(cx);
                }
            }
            mission::CONTROL_TAG | mission::START_TAG => {
                let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                self.mission.notice = Some(if ok {
                    value.get("state").and_then(|v| v.as_str()).unwrap_or("done").to_owned()
                } else {
                    value.get("reason").and_then(|v| v.as_str()).unwrap_or("refused").to_owned()
                });
                if tag == mission::START_TAG
                    && let Some(run_id) = value.get("runId").and_then(|v| v.as_str())
                {
                    self.mission.select(Some(run_id.to_owned()));
                    self.mission.notice = Some("started".into());
                }
                self.mission_tick = 0;
                self.poll_mission(cx);
            }
            mission::ARTIFACT_TAG => {
                if let (Some(name), Some(text)) = (value.get("name").and_then(|v| v.as_str()), value.get("text").and_then(|v| v.as_str())) {
                    self.mission.artifact = Some((name.to_owned(), text.to_owned()));
                }
            }
            settings_view::TRUST_STATUS_TAG => {
                self.settings_view.take_trust_status(&value);
            }
            settings_view::SETTINGS_GET_TAG => {
                self.settings_view.take_settings_get(&value);
            }
            settings_view::SETTINGS_SCHEMA_TAG => {
                self.settings_view.take_schema(&value);
            }
            settings_view::READINESS_TAG => {
                self.settings_view.take_readiness(&value);
            }
            settings_view::DOCTOR_TAG => {
                self.settings_view.take_doctor(&value);
            }
            settings_view::TRUST_GRANT_TAG => {
                let ok = value.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false);
                if ok {
                    self.settings_view.set_notice("Folder trust granted successfully.".into(), false);
                    self.settings_view.trusted = true;
                    self.session.update(cx, |s, _| s.view.settings.trusted = true);
                    self.load_settings_data(cx);
                } else {
                    let reason = value.get("reason").and_then(serde_json::Value::as_str).unwrap_or("Grant trust failed");
                    self.settings_view.set_notice(reason.to_owned(), true);
                }
            }
            settings_view::SETTINGS_RULE_ADD_TAG => {
                let ok = value.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false);
                if ok {
                    self.settings_view.set_notice("Rule added successfully.".into(), false);
                    self.session.read(cx).request("_cordata/settings/get", serde_json::json!({}), settings_view::SETTINGS_GET_TAG);
                } else {
                    let reason = value.get("reason").and_then(serde_json::Value::as_str).unwrap_or("Failed to add rule");
                    self.settings_view.set_notice(reason.to_owned(), true);
                }
            }
            settings_view::SETTINGS_RULE_REMOVE_TAG => {
                let ok = value.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false);
                if ok {
                    self.settings_view.set_notice("Rule removed.".into(), false);
                    self.session.read(cx).request("_cordata/settings/get", serde_json::json!({}), settings_view::SETTINGS_GET_TAG);
                } else {
                    self.settings_view.set_notice("Failed to remove rule".into(), true);
                }
            }
            settings_view::SETTINGS_SET_TAG => {
                let ok = value.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false);
                if ok {
                    self.settings_view.set_notice("Setting updated.".into(), false);
                    self.session.read(cx).request("_cordata/settings/get", serde_json::json!({}), settings_view::SETTINGS_GET_TAG);
                } else {
                    let reason = value.get("reason").and_then(serde_json::Value::as_str).unwrap_or("Setting update failed");
                    self.settings_view.set_notice(reason.to_owned(), true);
                }
            }
            models::OLLAMA_STATUS_TAG => {
                self.models_view.take_status(&value);
            }
            _ => {}
        }
        self.step_selftest(cx);
        cx.notify();
    }

    fn select_run(&mut self, run_id: Option<String>, cx: &mut Context<Self>) {
        self.mission.select(run_id);
        self.poll_mission(cx);
        cx.notify();
    }

    fn control_run(&mut self, cmd: &str, extra: serde_json::Value, cx: &mut Context<Self>) {
        let Some(run_id) = self.mission.selected.clone() else { return };
        let mut params = serde_json::json!({ "runId": run_id });
        if let (Some(target), Some(more)) = (params.as_object_mut(), extra.as_object()) {
            target.extend(more.clone());
        }
        self.session.read(cx).request(&format!("_cordata/orchestrator/{cmd}"), params, mission::CONTROL_TAG);
        self.mission.notice = Some(format!("{cmd} requested…"));
        cx.notify();
    }

    fn start_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let goal = self.goal_input.read(cx).value().trim().to_owned();
        if goal.is_empty() {
            return;
        }
        self.goal_input.update(cx, |state, cx| state.set_value("", window, cx));
        self.session.read(cx).request("_cordata/orchestrator/start", serde_json::json!({ "goal": goal }), mission::START_TAG);
        self.mission.notice = Some("starting…".into());
        cx.notify();
    }

    fn open_artifact(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(run_id) = self.mission.selected.clone() else { return };
        self.session.read(cx).request("_cordata/artifacts/read", serde_json::json!({ "runId": run_id, "name": name }), mission::ARTIFACT_TAG);
    }

    fn mission_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some((name, text)) = &self.mission.artifact {
            return div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().flex().items_center().child(div().flex_1().text_xs().text_color(color(cx, Role::Accent)).child(SharedString::from(name.clone()))).child(Button::new("close-artifact").label("Close").ghost().on_click(cx.listener(|this, _, _, cx| {
                    this.mission.artifact = None;
                    cx.notify();
                }))))
                .child(div().id("artifact-text").max_h(px(560.)).overflow_y_scroll().text_xs().font_family(cordata_theme::fonts::MONO).text_color(color(cx, Role::TextSecondary)).child(SharedString::from(shown(text))))
                .into_any_element();
        }
        if self.mission.selected.is_some() {
            let mut buttons: Vec<AnyElement> = vec![Button::new("runs-back").label("← Runs").ghost().on_click(cx.listener(|this, _, _, cx| this.select_run(None, cx))).into_any_element()];
            if self.mission.can_pause() {
                buttons.push(Button::new("run-pause").label("Pause").on_click(cx.listener(|this, _, _, cx| this.control_run("pause", serde_json::json!({}), cx))).into_any_element());
            }
            if self.mission.can_resume() {
                buttons.push(Button::new("run-resume").label("Resume").primary().on_click(cx.listener(|this, _, _, cx| this.control_run("resume", serde_json::json!({}), cx))).into_any_element());
            }
            if self.mission.can_cancel() {
                buttons.push(Button::new("run-cancel").label("Cancel").danger().on_click(cx.listener(|this, _, _, cx| this.control_run("cancel", serde_json::json!({ "now": true }), cx))).into_any_element());
            }
            let artifacts: Vec<AnyElement> = self
                .mission
                .artifacts()
                .into_iter()
                .enumerate()
                .map(|(index, name)| {
                    let open = name.clone();
                    div()
                        .id(("artifact", index))
                        .cursor_pointer()
                        .text_xs()
                        .text_color(color(cx, Role::Agent))
                        .hover(|this| this.text_color(color(cx, Role::Accent)))
                        .child(SharedString::from(format!("▤ {name}")))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_artifact(open.clone(), cx)))
                        .into_any_element()
                })
                .collect();
            return div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(div().flex().flex_wrap().gap(px(6.)).children(buttons))
                .child(mission::run_detail(&self.mission, cx))
                .when(!artifacts.is_empty(), |this| this.child(hud::label("artifacts", cx)).child(div().flex().flex_col().gap(px(3.)).children(artifacts)))
                .into_any_element();
        }
        let rows: Vec<AnyElement> = self
            .mission
            .runs
            .iter()
            .enumerate()
            .map(|(index, run)| {
                let run_id = run.get("runId").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
                div().id(("run-row", index)).cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.select_run(Some(run_id.clone()), cx))).child(mission::run_row(run, cx)).into_any_element()
            })
            .collect();
        let live_here = self.session.read(cx).view.run.as_ref().filter(|r| r.is_live()).map(|r| r.run_id.clone());
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .when_some(live_here, |this, run_id| {
                this.child(Button::new("follow-live").label(SharedString::from(format!("Follow {run_id}"))).primary().on_click(cx.listener(move |this, _, _, cx| this.select_run(Some(run_id.clone()), cx))))
            })
            .child(div().p(px(2.)).rounded(px(3.)).border_1().border_color(color(cx, Role::Hairline)).child(Input::new(&self.goal_input).appearance(false)))
            .child(Button::new("start-run").label("Start run").on_click(cx.listener(|this, _, window, cx| this.start_run(window, cx))))
            .when_some(self.mission.notice.clone(), |this, notice| this.child(div().text_xs().text_color(color(cx, Role::Attention)).child(SharedString::from(shown(&notice)))))
            .child(hud::label("runs in this project", cx))
            .when(rows.is_empty(), |this| this.child(div().text_xs().text_color(color(cx, Role::TextDim)).child("No runs yet. Start one above, or ask for a multi-step change.")))
            .children(rows)
            .into_any_element()
    }

    // -------------------------------------------------------------------------------------------- the listing

    fn schedule_listing(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.refresh_listing(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(LISTING_EVERY).await;
            }
        })
        .detach();
    }

    fn refresh_listing(&mut self, cx: &mut Context<Self>) {
        match &self.connection.transport {
            BackendTransport::Hub { daemon, .. } => {
                let (daemon, cwd) = (daemon.clone(), self.cwd.clone());
                let job = cx.global::<Runtime>().0.spawn(async move { acp_core::hub::list(&daemon, Some(&cwd)).await });
                cx.spawn(async move |this, cx| {
                    let result = job.await;
                    let _ = this.update(cx, |this, cx| {
                        match result {
                            Ok(Ok(rows)) => {
                                this.listing = rows
                                    .into_iter()
                                    .map(|c| Row { target: Target::Conversation(c.handle.clone()), title: if c.title.is_empty() { c.handle.clone() } else { c.title.clone() }, when: c.last_used, live: c.live, busy: c.busy, origin: c.origin.clone() })
                                    .collect();
                                this.listing_error = None;
                            }
                            Ok(Err(error)) => this.listing_error = Some(error.to_string()),
                            Err(_) => {}
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            _ => self.session.read(cx).request_listing(),
        }
    }

    fn take_stdio_listing(&mut self, cx: &mut Context<Self>) {
        if let Some(rows) = self.session.read(cx).listing.clone() {
            self.listing = rows;
            cx.notify();
        }
    }

    // -------------------------------------------------------------------------------------------- selftest

    /// Driven by the session's own changes, not by frames: an occluded window draws nothing, and this must still run.
    fn step_selftest(&mut self, cx: &mut Context<Self>) {
        let Some(test) = self.selftest.as_mut() else { return };
        let (pending, done, active, closed) = {
            let view = &self.session.read(cx).view;
            (!view.pending.is_empty(), view.turns_done, view.turn_active, matches!(view.conn, ConnState::Closed { .. }))
        };
        if std::env::var_os("CORDATA_SELFTEST_TRACE").is_some() {
            eprintln!("selftest: pending={pending} turns_done={done} active={active} closed={closed}");
        }
        test.started |= active;
        if pending {
            test.approvals += 1;
            let session = self.session.clone();
            // Deferred: answering re-enters the session, which is being observed right now.
            cx.defer(move |cx| session.update(cx, |session, cx| session.answer(ApprovalDecision::Allow, cx)));
            return;
        }
        if self.mission.selected.is_some() || self.start_run.is_some() {
            // A `--run` self-test ends with the run, not with a turn.
            let state = self.mission.detail.as_ref().and_then(|d| d.get("live")).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
            if matches!(state.as_str(), "done" | "failed" | "cancelled" | "budget_exhausted") {
                let board = &self.mission.board;
                println!("SELFTEST run={} state={state} steps={} pods={} pools={} events_offset={}", self.mission.selected.clone().unwrap_or_default(), board.steps.len(), board.pods.len(), board.pools.len(), self.mission.offset);
                self.selftest = None;
                cx.quit();
            }
            return;
        }
        if (test.started && !active) || closed {
            let view = &self.session.read(cx).view;
            let tools = view.messages.iter().filter(|m| matches!(m, cordata_view::app::Message::Tool(_))).count();
            let agent = view.messages.iter().filter(|m| matches!(m, cordata_view::app::Message::Agent(_))).count();
            let errors: Vec<String> = view.messages.iter().filter_map(|m| if let cordata_view::app::Message::Error(e) = m { Some(e.clone()) } else { None }).collect();
            println!("SELFTEST connected={} turns={} messages={} agent_blocks={} tool_cards={} approvals={} errors={:?}", !closed, done, view.messages.len(), agent, tools, test.approvals, errors);
            self.selftest = None;
            cx.quit();
        }
    }

    fn sync_transcript(&mut self, cx: &App) {
        let session = self.session.read(cx);
        let (id, len, version) = (self.session.entity_id(), session.view.messages.len(), session.version);
        let (old_id, old_len, old_version) = self.seen;
        if id != old_id || len < old_len {
            self.transcript.reset(len);
        } else if len != old_len || version != old_version {
            let start = old_len.saturating_sub(REMEASURE_TAIL);
            self.transcript.splice(start..old_len, len - start);
        }
        self.seen = (id, len, version);
    }

    // -------------------------------------------------------------------------------------------- actions

    fn stop(&mut self, _: &StopTurn, _: &mut Window, cx: &mut Context<Self>) {
        if self.model_menu {
            self.model_menu = false;
            cx.notify();
            return;
        }
        self.session.update(cx, |session, cx| {
            if session.view.menu.is_some() {
                session.menu_dismiss(cx);
            } else if session.view.turn_active {
                session.cancel(cx);
            }
        });
    }

    fn cycle_mode(&mut self, _: &CycleMode, _: &mut Window, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| {
            let modes: Vec<String> = session.view.modes.iter().map(|m| m.id.clone()).collect();
            if modes.is_empty() {
                return;
            }
            let current = session.view.pending_mode.clone().unwrap_or_else(|| session.view.mode.clone());
            let next = modes.iter().position(|m| *m == current).map_or(0, |i| (i + 1) % modes.len());
            // `auto` is never reached by cycling: it is an answer to a plan card, or a deliberate pick (THREAT_MODEL.md).
            let next = if modes[next] == "auto" { (next + 1) % modes.len() } else { next };
            session.set_mode(&modes[next], cx);
        });
    }

    fn focus_composer(&mut self, _: &FocusComposer, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |state, cx| state.focus(window, cx));
    }

    fn new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        self.open(self.new_target(), window, cx);
    }

    fn close_session(&mut self, _: &CloseSession, _: &mut Window, cx: &mut Context<Self>) {
        if self.dock_tab == DockTab::Editor && !self.files.buffers.is_empty() {
            self.close_active_buffer(cx);
            return;
        }
        self.close_active(cx);
    }

    fn show_mission_action(&mut self, _: &ShowMission, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Mission, cx);
    }

    fn show_files_action(&mut self, _: &ShowFiles, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Files, cx);
    }

    fn show_editor_action(&mut self, _: &ShowEditor, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Editor, cx);
    }

    fn show_review_action(&mut self, _: &ShowReview, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Review, cx);
    }

    fn show_terminal_action(&mut self, _: &ShowTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Terminal, cx);
    }

    fn show_browser_action(&mut self, _: &ShowBrowser, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Browser, cx);
    }

    fn show_models_action(&mut self, _: &ShowModels, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Models, cx);
    }

    fn show_settings_action(&mut self, _: &ShowSettings, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Settings, cx);
    }

    fn open_files_action(&mut self, _: &OpenFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Files, cx);
        self.file_filter.update(cx, |state, cx| state.focus(window, cx));
    }

    fn save_file_action(&mut self, _: &SaveFile, _: &mut Window, cx: &mut Context<Self>) {
        self.save_active(cx);
    }

    pub(super) fn switch_dock(&mut self, tab: DockTab, cx: &mut Context<Self>) {
        self.dock_tab = tab;
        match tab {
            DockTab::Files => self.load_tree(cx),
            DockTab::Review => self.load_review(cx),
            DockTab::Settings => self.load_settings_data(cx),
            DockTab::Models => {
                self.session.read(cx).request("_cordata/ollama/status", serde_json::json!({}), models::OLLAMA_STATUS_TAG);
            }
            _ => {}
        }
        cx.notify();
    }

    fn dock_width(&self) -> f32 {
        match self.dock_tab {
            DockTab::Mission | DockTab::Files => DOCK_WIDTH,
            DockTab::Editor | DockTab::Review | DockTab::Terminal | DockTab::Browser | DockTab::Models | DockTab::Settings => DOCK_WIDTH_WIDE,
        }
    }

    fn next_session(&mut self, _: &NextSession, _: &mut Window, cx: &mut Context<Self>) {
        self.switch(SwitchTarget::Next, cx);
    }

    fn prev_session(&mut self, _: &PrevSession, _: &mut Window, cx: &mut Context<Self>) {
        self.switch(SwitchTarget::Prev, cx);
    }

    /// ⌘K: the command menu — the composer with `/` typed, so every command (and custom one) is listed and narrows as
    /// you type, exactly as in the terminal.
    fn command_menu(&mut self, _: &CommandMenu, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |state, cx| {
            state.set_value("/", window, cx);
            state.focus(window, cx);
        });
        self.session.update(cx, |session, cx| session.mirror_input("/", cx));
    }

    fn toggle_thoughts(&mut self, _: &ToggleThoughts, _: &mut Window, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| session.toggle_thoughts(cx));
        let len = self.session.read(cx).view.messages.len();
        self.transcript.reset(len);
    }

    fn toggle_tools(&mut self, _: &ToggleTools, _: &mut Window, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| session.toggle_tools(cx));
        let len = self.session.read(cx).view.messages.len();
        self.transcript.reset(len);
    }

    fn help(&mut self, _: &Help, _: &mut Window, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| session.submit("/help", cx));
    }

    // -------------------------------------------------------------------------------------------- the composer

    fn on_composer(&mut self, input: &Entity<InputState>, event: &InputEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            InputEvent::Change => {
                let text = input.read(cx).value().to_string();
                self.session.update(cx, |session, cx| session.mirror_input(&text, cx));
            }
            InputEvent::PressEnter { shift: false, .. } if self.saying => {
                let text = input.read(cx).value().to_string();
                self.saying = false;
                input.update(cx, |state, cx| {
                    state.set_value("", window, cx);
                    state.set_placeholder("Ask Cordata…   / commands · @ files · ! shell", window, cx);
                });
                if !text.trim().is_empty() {
                    self.session.update(cx, |session, cx| session.answer(ApprovalDecision::Say(text), cx));
                }
            }
            InputEvent::PressEnter { shift: false, .. } => {
                let has_menu = self.session.read(cx).view.menu.is_some();
                if has_menu {
                    let text = self.session.update(cx, |session, cx| session.menu_accept(true, cx));
                    input.update(cx, |state, cx| state.set_value(text, window, cx));
                    return;
                }
                let text = input.read(cx).value().to_string();
                if text.trim().is_empty() {
                    return;
                }
                input.update(cx, |state, cx| state.set_value("", window, cx));
                self.session.update(cx, |session, cx| session.submit(&text, cx));
            }
            _ => {}
        }
    }

    /// Arrow keys, Tab and Escape belong to the `/`/`@` menu while it is open (before the text box sees them).
    fn composer_keys(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let (has_menu, pending, plan, empty) = {
            let view = &self.session.read(cx).view;
            let pending = view.pending.front();
            (view.menu.is_some(), pending.is_some(), pending.is_some_and(|p| p.info.tool_name == "present_plan"), self.composer.read(cx).value().is_empty())
        };
        let plain = !event.keystroke.modifiers.modified();
        // An approval card takes single keys while the composer is empty, as the terminal's does: a/y allow, r/n
        // reject, s allow and save the rule, w reject (or answer) in your own words; on a plan b/e build or edit.
        if pending && empty && plain && !self.saying {
            let decision = match event.keystroke.key.as_str() {
                "a" | "y" if !plan => Some(ApprovalDecision::Allow),
                "s" if !plan => Some(ApprovalDecision::AllowAlways),
                "b" if plan => Some(ApprovalDecision::Choose("approve_build".into())),
                "e" if plan => Some(ApprovalDecision::Choose("approve_edit".into())),
                "r" | "n" | "k" => Some(ApprovalDecision::Reject),
                "w" => {
                    self.saying = true;
                    self.composer.update(cx, |state, cx| state.set_placeholder("Say why (or answer in your own words) — Enter sends, Esc goes back", window, cx));
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                _ => None,
            };
            if let Some(decision) = decision {
                self.session.update(cx, |session, cx| session.answer(decision, cx));
                cx.stop_propagation();
                return;
            }
        }
        if self.saying && event.keystroke.key == "escape" {
            self.saying = false;
            self.composer.update(cx, |state, cx| state.set_placeholder("Ask Cordata…   / commands · @ files · ! shell", window, cx));
            cx.stop_propagation();
            return;
        }
        if !has_menu {
            // ↑/↓ walk the prompts already sent, while the box holds one line.
            let single_line = !self.composer.read(cx).value().contains('\n');
            if plain && single_line && matches!(event.keystroke.key.as_str(), "up" | "down") {
                let back = event.keystroke.key == "up";
                let text = self.session.update(cx, |session, cx| session.history(back, cx));
                self.composer.update(cx, |state, cx| state.set_value(text, window, cx));
                cx.stop_propagation();
            }
            return;
        }
        match event.keystroke.key.as_str() {
            "up" => self.session.update(cx, |session, cx| session.menu_move(-1, cx)),
            "down" => self.session.update(cx, |session, cx| session.menu_move(1, cx)),
            "tab" => {
                let text = self.session.update(cx, |session, cx| session.menu_accept(false, cx));
                self.composer.update(cx, |state, cx| state.set_value(text, window, cx));
            }
            "escape" => self.session.update(cx, |session, cx| session.menu_dismiss(cx)),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn menu(&self, cx: &App) -> Option<AnyElement> {
        let menu = self.session.read(cx).view.menu.as_ref()?;
        let rows = menu.items.iter().enumerate().take(cordata_view::menu::MAX_ROWS + 3).map(|(index, item)| {
            let selected = index == menu.selected;
            div()
                .flex()
                .gap(px(10.))
                .px(px(10.))
                .py(px(3.))
                .rounded(px(2.))
                .when(selected, |this| this.bg(tint(cx, Role::Accent, 0.16)))
                .child(div().w(px(150.)).font_family(cordata_theme::fonts::MONO).text_sm().text_color(color(cx, if selected { Role::Accent } else { Role::Text })).child(SharedString::from(item.label.clone())))
                .child(div().text_xs().text_color(color(cx, Role::TextDim)).child(SharedString::from(item.hint.clone())))
                .child(div().flex_1().text_xs().text_color(color(cx, Role::TextSecondary)).truncate().child(SharedString::from(item.detail.clone())))
        });
        Some(
            div()
                .id("composer-menu")
                .role(gpui::Role::ListBox)
                .aria_label("Suggestions")
                .mx(px(16.))
                .mb(px(4.))
                .p(px(4.))
                .rounded(px(4.))
                .border_1()
                .border_color(color(cx, Role::Hairline))
                .bg(color(cx, Role::Raised))
                .children(rows)
                .child(div().px(px(10.)).pt(px(2.)).text_xs().text_color(color(cx, Role::TextDim)).child("↑↓ choose · Tab complete · Enter run · Esc close"))
                .into_any_element(),
        )
    }

    // -------------------------------------------------------------------------------------------- the rail

    fn rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let session = self.session.read(cx);
        let view = &session.view;
        let project = self.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.cwd.display().to_string());
        let branch = view.branch.clone();
        let ground = if self.glass { tint(cx, Role::Surface, 0.55) } else { color(cx, Role::Surface) };
        let waiting: usize = self.sessions.iter().map(|s| s.read(cx).view.pending.len()).sum();

        let open_rows: Vec<AnyElement> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                let s = entity.read(cx);
                let (dot, state) = state_of(&s.view);
                let shown_now = entity.entity_id() == self.session.entity_id();
                let title = s.view.title.clone().unwrap_or_else(|| "New conversation".into());
                let target = entity.clone();
                div()
                    .id(("open-session", index))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .when(shown_now, |this| this.bg(tint(cx, Role::Accent, 0.10)).border_l_2().border_color(color(cx, Role::Accent)))
                    .hover(|this| this.bg(tint(cx, Role::Accent, 0.06)))
                    .on_click(cx.listener(move |this, _, _, cx| this.show(target.clone(), cx)))
                    .child(hud::dot(Some(dot), cx))
                    .child(div().flex_1().text_sm().text_color(color(cx, Role::Text)).truncate().child(SharedString::from(shown(&title))))
                    .child(div().text_xs().text_color(color(cx, Role::TextDim)).child(SharedString::from(format!("{state} · ⌘{}", index + 1))))
                    .into_any_element()
            })
            .collect();

        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or_default();
        let recent_rows: Vec<AnyElement> = self
            .listing
            .iter()
            .filter(|row| !self.sessions.iter().any(|s| matches_target(s.read(cx), &row.target)))
            .take(12)
            .enumerate()
            .map(|(index, row)| {
                let target = row.target.clone();
                div()
                    .id(("recent", index))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .hover(|this| this.bg(tint(cx, Role::Accent, 0.06)))
                    .on_click(cx.listener(move |this, _, window, cx| this.open(target.clone(), window, cx)))
                    .child(hud::dot(Some(if row.busy { Role::Accent } else if row.live { Role::Ok } else { Role::Hairline }), cx))
                    .child(div().flex_1().text_sm().text_color(color(cx, Role::TextSecondary)).truncate().child(SharedString::from(shown(&row.title))))
                    .child(div().text_xs().text_color(color(cx, if row.origin == "discord" { Role::Agent } else { Role::TextDim })).child(SharedString::from(format!("{}{}", door_glyph(&row.origin), age(now - row.when)))))
                    .into_any_element()
            })
            .collect();

        div()
            .id("rail")
            .role(gpui::Role::Navigation)
            .aria_label("Projects and sessions")
            .w(px(RAIL_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(14.))
            .pt(px(48.))
            .px(px(12.))
            .bg(ground)
            .border_r_1()
            .border_color(color(cx, Role::Hairline))
            .child(div().px(px(2.)).child(hud::bracket_title("Cordata", cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .px(px(2.))
                    .child(hud::label("project", cx))
                    .child(div().text_sm().text_color(color(cx, Role::Text)).child(SharedString::from(project)))
                    .when_some(branch, |this, branch| this.child(div().text_xs().text_color(color(cx, Role::Agent)).child(SharedString::from(format!("⎇ {branch}")))))
                    .child({
                        let trusted = self.settings_view.trusted;
                        div()
                            .id("rail-trust-status")
                            .cursor_pointer()
                            .text_xs()
                            .font_family(cordata_theme::fonts::MONO)
                            .text_color(color(cx, if trusted { Role::Ok } else { Role::Attention }))
                            .child(if trusted { "✓ trusted" } else { "⚠ untrusted" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.switch_dock(DockTab::Settings, cx);
                            }))
                    }),
            )
            .when(waiting > 0, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .px(px(2.))
                        .child(hud::label("needs you", cx))
                        .child(div().text_sm().text_color(color(cx, Role::Attention)).child(SharedString::from(format!("{waiting} approval{} waiting", if waiting == 1 { "" } else { "s" })))),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().flex().items_center().px(px(2.)).child(div().flex_1().child(hud::label("open", cx))).child(Button::new("new-session").label("+ New").ghost().on_click(cx.listener(|this, _, window, cx| this.new_session(&NewSession, window, cx)))))
                    .children(open_rows),
            )
            .child(
                div()
                    .id("recent-list")
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(div().px(px(2.)).child(hud::label("recent", cx)))
                    .children(recent_rows)
                    .when_some(self.listing_error.clone(), |this, error| this.child(div().px(px(2.)).text_xs().text_color(color(cx, Role::Danger)).child(SharedString::from(shown(&error))))),
            )
            .child(div().pb(px(12.)).px(px(2.)).text_xs().text_color(color(cx, Role::TextDim)).child(SharedString::from(format!("via {}", session.transport))))
            .into_any_element()
    }

    // -------------------------------------------------------------------------------------------- centre

    fn header(&self, cx: &mut Context<Self>) -> AnyElement {
        let view = &self.session.read(cx).view;
        let mode = view.pending_mode.clone().unwrap_or_else(|| view.mode.clone());
        let model = view.config("model").map(|option| option.current_name().to_owned());
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(40.))
            .px(px(16.))
            .border_b_1()
            .border_color(color(cx, Role::Hairline))
            .child(div().flex_1().text_sm().text_color(color(cx, Role::TextSecondary)).truncate().child(SharedString::from(shown(view.title.as_deref().unwrap_or("")))))
            .when_some(model, |this, model| {
                this.child(
                    div()
                        .id("model-picker")
                        .role(gpui::Role::Button)
                        .aria_label("Model")
                        .cursor_pointer()
                        .px(px(6.))
                        .py(px(2.))
                        .rounded(px(2.))
                        .hover(|this| this.bg(tint(cx, Role::Accent, 0.08)))
                        .text_xs()
                        .text_color(color(cx, Role::TextSecondary))
                        .child(SharedString::from(format!("{model} ▾")))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model_menu = !this.model_menu;
                            cx.notify();
                        })),
                )
            })
            .when(!mode.is_empty(), |this| {
                this.child(
                    div()
                        .id("mode-pill")
                        .role(gpui::Role::Button)
                        .aria_label(SharedString::from(format!("Mode: {mode}. Shift+Tab to change")))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, window, cx| this.cycle_mode(&CycleMode, window, cx)))
                        .child(hud::pill(&mode, mode_role(&mode), cx)),
                )
            })
            .into_any_element()
    }

    fn model_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.model_menu {
            return None;
        }
        let option = self.session.read(cx).view.config("model")?.clone();
        let rows = option.choices.iter().enumerate().map(|(index, choice)| {
            let current = choice.value == option.current_value;
            let value = choice.value.clone();
            div()
                .id(("model", index))
                .px(px(10.))
                .py(px(4.))
                .rounded(px(2.))
                .cursor_pointer()
                .text_sm()
                .text_color(color(cx, if current { Role::Accent } else { Role::Text }))
                .hover(|this| this.bg(tint(cx, Role::Accent, 0.10)))
                .child(SharedString::from(format!("{} {}", if current { "●" } else { " " }, shown(&choice.name))))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let value = value.clone();
                    this.model_menu = false;
                    this.session.update(cx, |session, cx| session.set_config("model", &value, cx));
                }))
        });
        Some(
            div()
                .id("model-menu")
                .role(gpui::Role::Menu)
                .aria_label("Models")
                .absolute()
                .top(px(48.))
                .right(px(16.))
                .w(px(300.))
                .max_h(px(420.))
                .overflow_y_scroll()
                .p(px(4.))
                .rounded(px(4.))
                .border_1()
                .border_color(color(cx, Role::Hairline))
                .bg(color(cx, Role::Raised))
                .children(rows)
                .into_any_element(),
        )
    }

    /// The connection is gone: the transcript stays (it is what was said), the composer is useless, and one click opens
    /// the same conversation again.
    fn banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ConnState::Closed { error } = &self.session.read(cx).view.conn else { return None };
        let why = error.clone().unwrap_or_else(|| "the session ended".into());
        Some(
            div()
                .id("stale-banner")
                .role(gpui::Role::Alert)
                .flex()
                .items_center()
                .gap(px(10.))
                .mx(px(16.))
                .mb(px(8.))
                .px(px(12.))
                .py(px(8.))
                .rounded(px(4.))
                .border_1()
                .border_color(color(cx, Role::Danger))
                .bg(tint(cx, Role::Danger, 0.08))
                .child(hud::pill("disconnected", Role::Danger, cx))
                .child(div().flex_1().text_sm().text_color(color(cx, Role::TextSecondary)).child(SharedString::from(shown(&why))))
                .child(Button::new("reconnect").label("Reconnect").primary().on_click(cx.listener(|this, _, window, cx| this.reconnect(window, cx))))
                .into_any_element(),
        )
    }

    fn approval_card(&self, pending: &PendingPermission, cx: &App) -> AnyElement {
        let info = &pending.info;
        let summary = info.raw_input.as_ref().and_then(|input| cordata_view::view::summarize_input(input, false)).unwrap_or_default();
        let session = self.session.clone();
        let answer = move |decision: ApprovalDecision| {
            let session = session.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let decision = decision.clone();
                session.update(cx, |session, cx| session.answer(decision, cx));
            }
        };
        let (title, buttons): (String, Vec<AnyElement>) = match info.tool_name.as_str() {
            "present_plan" => (
                "The plan is ready".into(),
                [("approve_build", "Build", true), ("approve_edit", "Edit", false), ("approve_auto", "Auto", false)]
                    .into_iter()
                    .filter(|(id, ..)| info.option(id).is_some())
                    .map(|(id, label, primary)| {
                        let button = Button::new(SharedString::from(id)).label(label).on_click(answer(ApprovalDecision::Choose(id.into())));
                        if primary { button.primary().into_any_element() } else { button.into_any_element() }
                    })
                    .chain([Button::new("keep-planning").label("Keep planning").ghost().on_click(answer(ApprovalDecision::Reject)).into_any_element()])
                    .collect(),
            ),
            "ask_user" => (
                info.raw_input.as_ref().and_then(|input| input.get("question")).and_then(|q| q.as_str()).unwrap_or("The agent has a question").to_owned(),
                cordata_view::view::ask_choices(info)
                    .into_iter()
                    .enumerate()
                    .map(|(index, choice)| Button::new(SharedString::from(format!("choice-{index}"))).label(SharedString::from(format!("{} {}", index + 1, shown(&choice.name)))).on_click(answer(ApprovalDecision::Choose(choice.id.clone()))).into_any_element())
                    .chain([Button::new("skip").label("Skip").ghost().on_click(answer(ApprovalDecision::Reject)).into_any_element()])
                    .collect(),
            ),
            _ => (
                format!("Allow {}?", info.tool_name),
                [
                    Some(Button::new("allow").label("Allow").primary().on_click(answer(ApprovalDecision::Allow)).into_any_element()),
                    info.always_label.as_ref().map(|label| Button::new("always").label(SharedString::from(format!("Always: {}", shown(label)))).on_click(answer(ApprovalDecision::AllowAlways)).into_any_element()),
                    Some(Button::new("reject").label("Reject").danger().on_click(answer(ApprovalDecision::Reject)).into_any_element()),
                ]
                .into_iter()
                .flatten()
                .collect(),
            ),
        };
        let untrusted = info.untrusted_source;
        div()
            .id("approval")
            .role(gpui::Role::Dialog)
            .aria_label("Approval")
            .mx(px(16.))
            .mb(px(8.))
            .rounded(px(4.))
            .border_1()
            .border_color(color(cx, Role::Attention))
            .bg(tint(cx, Role::Attention, 0.06))
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().flex().items_center().gap(px(8.)).child(hud::pill("needs you", Role::Attention, cx)).child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).text_color(color(cx, Role::Text)).child(SharedString::from(shown(&title)))))
            .when(!summary.is_empty(), |this| {
                this.child(
                    div()
                        .p(px(8.))
                        .rounded(px(3.))
                        .bg(color(cx, Role::Surface))
                        .font_family(cordata_theme::fonts::MONO)
                        .text_xs()
                        .text_color(color(cx, if untrusted { Role::Agent } else { Role::TextSecondary }))
                        .child(SharedString::from(shown(&summary))),
                )
            })
            .child(div().flex().flex_wrap().gap(px(8.)).children(buttons))
            .into_any_element()
    }

    fn footer(&self, cx: &App) -> AnyElement {
        let view = &self.session.read(cx).view;
        let context = view.context.as_ref();
        let (fraction, value) = match context {
            Some(gauge) => (gauge.pct, match gauge.window {
                Some(window) => format!("{}k / {}k", gauge.used / 1000, window / 1000),
                None => format!("{}k", gauge.used / 1000),
            }),
            None => (None, "—".into()),
        };
        let (tokens_in, tokens_out) = view.session_tokens;
        div()
            .flex()
            .items_center()
            .gap(px(18.))
            .h(px(28.))
            .px(px(16.))
            .border_t_1()
            .border_color(color(cx, Role::Hairline))
            .child(hud::gauge("context", fraction, &value, cx))
            .child(div().text_xs().text_color(color(cx, Role::TextDim)).child(SharedString::from(format!("{} {}k in · {}k out", tracked("tokens"), tokens_in / 1000, tokens_out / 1000))))
            .child(div().flex_1())
            .when(view.turn_active, |this| this.child(div().text_xs().text_color(color(cx, Role::Accent)).child(SharedString::from(format!("● working · {} tool{} · Esc stops", view.turn_tools, if view.turn_tools == 1 { "" } else { "s" })))))
            .when_some(view.hint.clone(), |this, hint| this.child(div().text_xs().text_color(color(cx, Role::Attention)).child(SharedString::from(shown(&hint)))))
            .into_any_element()
    }

    // ------------------------------------------------------------------------------------------------ the dock

    fn dock(&self, cx: &mut Context<Self>) -> AnyElement {
        const TABS: [DockTab; 8] = [DockTab::Mission, DockTab::Files, DockTab::Editor, DockTab::Review, DockTab::Terminal, DockTab::Browser, DockTab::Models, DockTab::Settings];
        let tab_buttons = div()
            .id("dock-tabs")
            .flex()
            .items_center()
            .gap(px(4.))
            .border_b_1()
            .border_color(color(cx, Role::Hairline))
            .pb(px(8.))
            .children(TABS.iter().map(|&tab| {
                let selected = self.dock_tab == tab;
                let label = self.dock_label(tab);
                div()
                    .id(("dock-tab", tab as usize))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(2.))
                    .cursor_pointer()
                    .text_xs()
                    .font_family(cordata_theme::fonts::MONO)
                    .text_color(color(cx, if selected { Role::Accent } else { Role::TextSecondary }))
                    .when(selected, |el| el.bg(tint(cx, Role::Accent, 0.1)))
                    .hover(|el| el.text_color(color(cx, Role::Text)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.switch_dock(tab, cx);
                    }))
                    .child(SharedString::from(label))
            }));

        let body: AnyElement = match self.dock_tab {
            DockTab::Mission => {
                let mission = self.mission_panel(cx);
                let view = &self.session.read(cx).view;
                let checklist = (!view.checklist.is_empty()).then(|| {
                    div().flex().flex_col().gap(px(3.)).children(view.checklist.iter().map(|item| {
                        let (glyph, role) = match item.status {
                            cordata_view::extras::TodoStatus::Completed => ("☑", Role::Ok),
                            cordata_view::extras::TodoStatus::InProgress => ("◐", Role::Accent),
                            cordata_view::extras::TodoStatus::Pending => ("☐", Role::TextDim),
                        };
                        div().flex().gap(px(6.)).text_xs().child(div().text_color(color(cx, role)).child(glyph)).child(div().text_color(color(cx, Role::TextSecondary)).child(SharedString::from(shown(&item.content))))
                    }))
                });
                div()
                    .id("mission-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .pb(px(12.))
                    .child(hud::frame("mission control", mission, cx))
                    .when_some(checklist, |this, list| this.child(hud::frame("tasks", list, cx)))
                    .when_some(view.goal.as_ref(), |this, goal| this.child(hud::frame("goal", div().text_xs().text_color(color(cx, Role::TextSecondary)).child(SharedString::from(shown(&goal.condition))), cx)))
                    .into_any_element()
            }
            DockTab::Files => editor::pane("files", self.files_panel(cx), cx),
            DockTab::Editor => self.editor_panel(cx),
            DockTab::Review => self.review_panel(cx),
            DockTab::Terminal => editor::pane("terminal", self.terminal_panel(cx), cx),
            DockTab::Browser => editor::pane("browser agent", self.browser_panel(cx), cx),
            DockTab::Models => editor::pane("models & voice", self.models_panel(cx), cx),
            DockTab::Settings => editor::pane("settings & trust", self.settings_panel(cx), cx),
        };

        div()
            .id("dock")
            .role(gpui::Role::Complementary)
            .aria_label("Dock")
            .w(px(self.dock_width()))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .pt(px(48.))
            .px(px(12.))
            .bg(color(cx, Role::Ground))
            .border_l_1()
            .border_color(color(cx, Role::Hairline))
            .child(tab_buttons)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}


impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll_disk(window, cx);
        self.sync_transcript(cx);
        let session = self.session.clone();
        let expanded = self.expanded.clone();
        let thoughts = session.read(cx).view.thoughts_expanded;
        let this = cx.entity().downgrade();
        let transcript = list(self.transcript.clone(), move |ix, _window, cx| {
            let view = &session.read(cx).view;
            let Some(message) = view.messages.get(ix) else { return div().into_any_element() };
            let (card_id, is_thought) = match message {
                cordata_view::app::Message::Tool(card) => (Some(card.id.clone()), false),
                cordata_view::app::Message::Thought(_) => (None, true),
                _ => (None, false),
            };
            let expand = view.expand_tools || card_id.as_ref().is_some_and(|id| expanded.contains(id));
            let (this, session) = (this.clone(), session.clone());
            div()
                .id(("block", ix))
                .px(px(20.))
                .when(card_id.is_some() || is_thought, |el| el.cursor_pointer())
                .on_click(move |_, _, cx| {
                    if let Some(id) = &card_id {
                        let id = id.clone();
                        let _ = this.update(cx, |this, cx| {
                            if !this.expanded.remove(&id) {
                                this.expanded.insert(id);
                            }
                            // The card's height changes: lay it out again.
                            this.transcript.splice(ix..ix + 1, 1);
                            cx.notify();
                        });
                    } else if is_thought {
                        session.update(cx, |session, cx| session.toggle_thoughts(cx));
                    }
                })
                .child(crate::transcript::block(message, ix, expand, thoughts, cx))
                .into_any_element()
        })
        .size_full();
        let pending = self.session.clone().read(cx).view.pending.front().map(|pending| self.approval_card(pending, cx));
        let banner = self.banner(cx);
        let menu = self.menu(cx);
        let model_menu = self.model_menu(cx);
        let rail = self.rail(cx);
        let header = self.header(cx);

        div()
            .id("workspace")
            .key_context("Workspace")
            .on_action(cx.listener(Self::stop))
            .on_action(cx.listener(Self::cycle_mode))
            .on_action(cx.listener(Self::focus_composer))
            .on_action(cx.listener(Self::new_session))
            .on_action(cx.listener(Self::close_session))
            .on_action(cx.listener(Self::next_session))
            .on_action(cx.listener(Self::prev_session))
            .on_action(cx.listener(Self::command_menu))
            .on_action(cx.listener(Self::toggle_thoughts))
            .on_action(cx.listener(Self::toggle_tools))
            .on_action(cx.listener(Self::help))
            .on_action(cx.listener(Self::show_mission_action))
            .on_action(cx.listener(Self::show_files_action))
            .on_action(cx.listener(Self::show_editor_action))
            .on_action(cx.listener(Self::show_review_action))
            .on_action(cx.listener(Self::show_terminal_action))
            .on_action(cx.listener(Self::show_browser_action))
            .on_action(cx.listener(Self::show_models_action))
            .on_action(cx.listener(Self::show_settings_action))
            .on_action(cx.listener(Self::open_files_action))
            .on_action(cx.listener(Self::save_file_action))
            .size_full()
            .relative()
            .font_family(cordata_theme::fonts::SANS)
            .text_color(color(cx, Role::Text))
            .when(!self.glass, |this| this.child(hud::haze(self.display.atmosphere(), cx)))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .child(rail)
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .relative()
                            .flex()
                            .flex_col()
                            .when(self.glass, |this| this.child(div().absolute().inset_0().child(hud::haze(self.display.atmosphere(), cx))))
                            .pt(px(8.))
                            .child(header)
                            .child(div().id("transcript").role(gpui::Role::Log).aria_label("Transcript").flex_1().min_h_0().child(transcript))
                            .children(banner)
                            .children(pending)
                            .children(menu)
                            .child(
                                div()
                                    .id("composer")
                                    .role(gpui::Role::Group)
                                    .aria_label("Composer")
                                    .capture_key_down(cx.listener(Self::composer_keys))
                                    .mx(px(16.))
                                    .mb(px(8.))
                                    .p(px(2.))
                                    .rounded(px(4.))
                                    .border_1()
                                    .border_color(color(cx, Role::Accent))
                                    .bg(color(cx, Role::Surface))
                                    .child(Input::new(&self.composer).appearance(false)),
                            )
                            .child(self.footer(cx))
                            .children(model_menu),
                    )
                    .child(self.dock(cx)),
            )
            .child(div().absolute().bottom(px(36.)).right(px(self.dock_width() + 12.)).w(relative(0.)).when(std::env::var_os("CORDATA_FPS").is_some(), |this| this.child(gpui_fps::fps_monitor(window, cx))))
    }
}

fn state_of(view: &cordata_view::app::App) -> (Role, &'static str) {
    match (&view.conn, view.turn_active, view.pending.is_empty()) {
        (ConnState::Connecting, ..) => (Role::TextDim, "connecting"),
        (ConnState::Closed { .. }, ..) => (Role::Danger, "closed"),
        (_, _, false) => (Role::Attention, "needs you"),
        (_, true, _) => (Role::Accent, "running"),
        _ => (Role::Ok, "idle"),
    }
}

/// Is this open session the conversation a rail row names?
fn matches_target(session: &Session, target: &Target) -> bool {
    match target {
        Target::Default => false,
        Target::Conversation(handle) => matches!(&session.target, Target::Conversation(h) if h == handle) || session.hub_handle().as_deref() == Some(handle.as_str()),
        Target::Resume(id) => session.session_id().as_deref() == Some(id.as_str()) || matches!(&session.target, Target::Resume(r) if r == id),
    }
}

fn door_glyph(origin: &str) -> &'static str {
    match origin {
        "discord" => "◆ ",
        "menubar" => "◇ ",
        "web" => "○ ",
        _ => "",
    }
}

/// `3m`, `2h`, `4d`.
fn age(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("escape", StopTurn, Some("Workspace")),
        gpui::KeyBinding::new("cmd-.", StopTurn, Some("Workspace")),
        gpui::KeyBinding::new("shift-tab", CycleMode, Some("Workspace")),
        gpui::KeyBinding::new("cmd-l", FocusComposer, Some("Workspace")),
        gpui::KeyBinding::new("cmd-n", NewSession, Some("Workspace")),
        gpui::KeyBinding::new("cmd-w", CloseSession, Some("Workspace")),
        gpui::KeyBinding::new("cmd-w", CloseSession, Some("CordataEditor")),
        gpui::KeyBinding::new("cmd-]", NextSession, Some("Workspace")),
        gpui::KeyBinding::new("cmd-[", PrevSession, Some("Workspace")),
        gpui::KeyBinding::new("cmd-k", CommandMenu, Some("Workspace")),
        gpui::KeyBinding::new("ctrl-k", CommandMenu, Some("Workspace")),
        gpui::KeyBinding::new("ctrl-r", ToggleThoughts, Some("Workspace")),
        gpui::KeyBinding::new("ctrl-e", ToggleTools, Some("Workspace")),
        gpui::KeyBinding::new("f1", Help, Some("Workspace")),
        gpui::KeyBinding::new("cmd-1", ShowMission, Some("Workspace")),
        gpui::KeyBinding::new("cmd-2", ShowFiles, Some("Workspace")),
        gpui::KeyBinding::new("cmd-3", ShowEditor, Some("Workspace")),
        gpui::KeyBinding::new("cmd-4", ShowReview, Some("Workspace")),
        gpui::KeyBinding::new("cmd-5", ShowSettings, Some("Workspace")),
        gpui::KeyBinding::new("cmd-6", ShowTerminal, Some("Workspace")),
        gpui::KeyBinding::new("cmd-7", ShowBrowser, Some("Workspace")),
        gpui::KeyBinding::new("cmd-8", ShowModels, Some("Workspace")),
        gpui::KeyBinding::new("cmd-,", ShowSettings, Some("Workspace")),
        gpui::KeyBinding::new("cmd-p", OpenFiles, Some("Workspace")),
        gpui::KeyBinding::new("cmd-s", SaveFile, Some("Workspace")),
        gpui::KeyBinding::new("cmd-s", SaveFile, Some("CordataEditor")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::{age, door_glyph};

    #[test]
    fn ages_read_at_a_glance() {
        assert_eq!(age(5.0), "now");
        assert_eq!(age(150.0), "2m");
        assert_eq!(age(7200.0), "2h");
        assert_eq!(age(3.0 * 86_400.0), "3d");
        assert_eq!(door_glyph("discord"), "◆ ");
    }
}

#[cfg(test)]
mod window_tests {
    use super::Workspace;
    use crate::session::Connection;
    use gpui::{AppContext as _, TestAppContext, VisualTestContext};

    fn offline() -> Connection {
        // No daemon, no checkout: every session says why and spawns nothing — the window's own behaviour, in isolation.
        Connection {
            backend: acp_core::BackendConfig { backend_dir: ".".into(), python: "python3".into(), server: "acp_server.py".into() },
            transport: acp_core::BackendTransport::Stdio,
            problem: Some("offline for a test".into()),
        }
    }

    fn workspace(cx: &mut TestAppContext) -> (gpui::Entity<Workspace>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::install(false, cx);
            super::bind_keys(cx);
        });
        let (view, cx) = cx.add_window_view(|window, cx| Workspace::new(offline(), std::env::temp_dir(), super::Launch::default(), window, cx));
        cx.run_until_parked();
        (view, cx)
    }

    #[gpui::test]
    fn a_slash_opens_the_command_menu_and_enter_runs_the_choice(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        // Menus open only while the session can take input (as in the terminal): pretend it connected.
        view.update(cx, |this, cx| this.session.update(cx, |s, _| s.view.conn = cordata_view::app::ConnState::Connected));
        cx.simulate_input("/hel");
        cx.run_until_parked();
        let first = view.read_with(cx, |this, cx| this.session.read(cx).view.menu.as_ref().and_then(|m| m.current()).map(|i| i.label.clone()));
        assert_eq!(first.as_deref(), Some("/help"), "typing narrows the commands");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let (menu, help_shown) = view.read_with(cx, |this, cx| {
            let v = &this.session.read(cx).view;
            (v.menu.is_some(), v.messages.iter().any(|m| matches!(m, cordata_view::app::Message::Help)))
        });
        assert!(!menu && help_shown, "Enter runs /help locally (never sent to a model)");
    }

    #[gpui::test]
    fn sessions_open_switch_and_close_from_the_keyboard(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        cx.simulate_keystrokes("cmd-n");
        cx.run_until_parked();
        let (count, shown_is_last) = view.read_with(cx, |this, _| (this.sessions.len(), this.session.entity_id() == this.sessions[1].entity_id()));
        assert_eq!(count, 2, "⌘N opens a session");
        assert!(shown_is_last, "…and shows it");
        cx.simulate_keystrokes("cmd-]");
        cx.run_until_parked();
        assert!(view.read_with(cx, |this, _| this.session.entity_id() == this.sessions[0].entity_id()), "⌘] switches (and wraps)");
        cx.simulate_keystrokes("cmd-w");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.sessions.len()), 1, "⌘W closes the shown session");
        cx.simulate_keystrokes("cmd-w");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.sessions.len()), 1, "…but never the last one");
    }

    fn ask(view: &gpui::Entity<Workspace>, cx: &mut VisualTestContext, tool: &str) -> tokio::sync::oneshot::Receiver<acp_core::ApprovalDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let info = acp_core::PermissionInfo {
            tool_call_id: "call-1".into(),
            tool_name: tool.into(),
            title: format!("Run {tool}"),
            kind: acp_core::ToolKind::Execute,
            raw_input: Some(serde_json::json!({"command": "make deploy"})),
            untrusted_source: false,
            always_label: Some("shell(make deploy)".into()),
            options: Vec::new(),
        };
        view.update(cx, |this, cx| {
            this.session.update(cx, |s, cx| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.on_event(acp_core::AppEvent::PermissionRequested { info, decision: tx });
                cx.notify();
            })
        });
        cx.run_until_parked();
        rx
    }

    #[gpui::test]
    fn an_approval_card_answers_to_its_keys(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let mut rx = ask(&view, cx, "shell");
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        assert_eq!(rx.try_recv().ok(), Some(acp_core::ApprovalDecision::Allow), "a allows");
        assert!(view.read_with(cx, |this, app| this.composer.read(app).value().is_empty()), "…and is not typed into the composer");

        let mut rx = ask(&view, cx, "shell");
        cx.simulate_keystrokes("w");
        cx.simulate_input("not on a Friday");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(rx.try_recv().ok(), Some(acp_core::ApprovalDecision::Say("not on a Friday".into())), "w then Enter rejects in your own words");

        let mut rx = ask(&view, cx, "shell");
        cx.simulate_keystrokes("r");
        cx.run_until_parked();
        assert_eq!(rx.try_recv().ok(), Some(acp_core::ApprovalDecision::Reject), "r rejects");
    }

    #[gpui::test]
    fn an_offline_session_says_why_instead_of_an_empty_window(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let said = view.read_with(cx, |this, cx| this.session.read(cx).view.messages.iter().any(|m| matches!(m, cordata_view::app::Message::Error(e) if e.contains("offline for a test"))));
        assert!(said);
        let _ = cx.new(|_| ()); // the context stays usable
    }

    #[gpui::test]
    fn dock_tabs_and_keys_switch_dock(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Mission);

        cx.simulate_keystrokes("cmd-2");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Files);

        cx.simulate_keystrokes("cmd-3");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Editor);

        cx.simulate_keystrokes("cmd-4");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Review);

        cx.simulate_keystrokes("cmd-5");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Settings);

        cx.simulate_keystrokes("cmd-1");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Mission);

        cx.simulate_keystrokes("cmd-,");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Settings);

        cx.simulate_keystrokes("cmd-6");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Terminal);

        cx.simulate_keystrokes("cmd-7");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Browser);

        cx.simulate_keystrokes("cmd-8");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Models);

        cx.simulate_keystrokes("cmd-p");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Files);
    }

    #[gpui::test]
    fn settings_trust_data_routes(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        view.update(cx, |this, cx| {
            let tag = super::settings_view::TRUST_STATUS_TAG;
            this.session.update(cx, |s, _| {
                s.data.insert(tag.into(), Ok(serde_json::json!({
                    "trusted": false,
                    "ignored": ["permissions.defaultMode: auto ignored (folder not trusted)"],
                    "allowRules": ["shell(make deploy)"],
                    "blockedPlugins": [{
                        "name": "repo-tool",
                        "command": "python",
                        "args": ["-c", "print(1)"],
                        "envKeys": ["TOKEN"],
                        "reason": "folder not trusted"
                    }]
                })));
            });
            this.on_data(tag, cx);
            assert!(!this.settings_view.trusted);
            assert_eq!(this.settings_view.blocked_plugins.len(), 1);
            assert_eq!(this.settings_view.blocked_plugins[0].name, "repo-tool");

            // Grant trust
            let grant_tag = super::settings_view::TRUST_GRANT_TAG;
            this.session.update(cx, |s, _| {
                s.data.insert(grant_tag.into(), Ok(serde_json::json!({
                    "ok": true,
                    "trusted": true
                })));
            });
            this.on_data(grant_tag, cx);
            assert!(this.settings_view.trusted);
        });
    }

    #[gpui::test]
    fn cmd_w_closes_editor_tab_first(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        view.update(cx, |this, _| {
            this.dock_tab = super::DockTab::Editor;
        });
        let dir = tempfile_dir();
        let file_path = dir.join("foo.rs");
        std::fs::write(&file_path, "fn foo() {}\n").unwrap();
        let rel = "foo.rs".to_string();
        let buffer_state = cx.update(|window, cx| cx.new(|cx| gpui_component::input::EditorState::new(window, cx)));
        view.update(cx, |this, _| {
            this.add_test_buffer(&rel, file_path.clone(), buffer_state);
        });

        assert_eq!(view.read_with(cx, |this, _| this.files.buffers.len()), 1);
        cx.simulate_keystrokes("cmd-w");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.files.buffers.len()), 0, "⌘W closed the editor buffer");
        assert_eq!(view.read_with(cx, |this, _| this.sessions.len()), 1, "session remains open");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn lsp_diagnostics_reply_routes_to_editor(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let tag = format!("{}1", super::editor::LSP_TAG);
        view.update(cx, |this, cx| {
            this.session.update(cx, |s, _| {
                s.data.insert(tag.clone(), Ok(serde_json::json!({
                    "ok": false,
                    "needs": "trust"
                })));
            });
            this.on_data(&tag, cx);
        });
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cordata-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }
}
