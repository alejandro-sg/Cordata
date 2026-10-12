//! The menu bar's view of the harness: one app-scoped model, no window required.
//!
//! The Lab's lifetime model cannot be reused here. There, the client is owned by the `LabApp`
//! entity that *is* the window root, and dropping the handle shuts the client down — so a tray in
//! that process would go dark the moment the window closed. Here the client and the model outlive
//! every window: the deck borrows them, and closing the deck must not drop the daemon connection.
//!
//! Everything the tray needs is derived from the same [`LabModel`] the Lab draws, through
//! [`harness_client::deck`] — one source of truth for "is the agent waiting for me", so the icon
//! and the deck can never disagree.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{App, AppContext as _, Context, Entity, Global};
use harness_client::deck::{self, TrayState};
use harness_client::OpenIntervention;
use harness_client::{ClientConfig, Command, DeckContext, Handle, LabModel, Reply};
use harness_capture::{Capture, CaptureSource, Frontmost, Refusal, Shot};

use crate::settings::{self, CaptureSettings};

/// How often the tray reconsidered when nothing wakes it. The inbox has its own notification, so
/// this is only a floor under a stream that has gone quiet.
pub const TICK: Duration = Duration::from_millis(100);

pub struct MenubarApp {
    pub model: LabModel,
    /// `None` while there is no daemon to talk to — the tray still appears and still works.
    pub client: Option<Arc<Handle>>,
    /// Held so the runtime outlives the client's tasks.
    #[allow(dead_code)]
    runtime: Option<tokio::runtime::Runtime>,
    pub config_error: Option<String>,
    /// The tray state last acted on, so the icon is only rebuilt when it actually changes.
    pub shown: TrayState,
    /// What the deck was last told, so a status change can be noticed without a window.
    pub status: Option<String>,
    /// Clicks the status item has received. The deck opens on one of these from the next phase; the
    /// count is kept now so a click is visibly received rather than silently swallowed.
    pub tray_clicks: u64,
    /// Requests the person has waved away. They stay pending — the agent stays suspended — they
    /// just stop pulling the deck open. Keyed by intervention id, exactly as the Lab does it.
    pub dismissed: HashSet<String>,
    /// The hotkey that was claimed, as something to show a person ("cmd+shift+space").
    pub hotkey_binding: Option<String>,
    /// Why no hotkey could be claimed. Rendered as a chip in the deck, because a keystroke that
    /// silently does nothing is indistinguishable from an app that is broken.
    pub hotkey_error: Option<String>,
    /// Whether the deck's window is up. Kept here rather than only in the window so the *rule*
    /// ("does this request take over?") can be answered without a window server.
    pub deck_open: bool,
    /// What the person allowed to be read, from their own settings file.
    pub capture: CaptureSettings,
    /// Where `capture` is written back when the person allows an app from the deck. `None` keeps it
    /// in memory only — the headless tests, which must never touch a real settings file.
    pub settings_path: Option<PathBuf>,
    /// The app the person was looking at when they summoned the deck. Recorded *then*, because once
    /// the deck has the keyboard it is the frontmost app itself. Costs no permission: it is the app's
    /// identity, not anything in its windows.
    pub target: Option<Frontmost>,
    /// The latest capture — a preview to send, or a refusal to explain. Taken by the next turn and
    /// never reused: a screen the person has left must not ride along with a later prompt.
    pub attached: Option<Capture>,
    /// The OS reads, behind the trait so the tests drive a fake. Only ever called from a click.
    pub source: Rc<dyn CaptureSource>,
    /// The window screenshot, by pid. Behind a function so the tests need no window server; on macOS
    /// it is ScreenCaptureKit, which blocks, so it is only ever run on a background task.
    pub shooter: Arc<dyn Fn(i32) -> Result<Shot, Refusal> + Send + Sync>,
    /// A screenshot is being taken; the button says so instead of offering a second one.
    pub shooting: bool,
    /// The hub conversation the deck is in — shared with the terminal, Discord and the web app.
    /// `None` means the next turn starts a new one. Picked automatically (the newest) the first time
    /// the list arrives, which is the handoff: open the deck and you are where you left off anywhere.
    pub conversation: Option<String>,
    /// The person chose "new" on purpose: the newest conversation is not re-selected under them.
    pub chose_new: bool,
}

impl MenubarApp {
    /// Follow the daemon the way the Lab does: find it, and find it again after it moves. Opening
    /// with no daemon is a supported state, not a failure — a menu-bar app that refuses to appear
    /// cannot even explain why.
    pub fn discovering(cx: &mut Context<Self>) -> Self {
        let mut app = Self::blank();
        app.capture = CaptureSettings::load();
        app.settings_path = Some(settings::user_path());
        #[cfg(target_os = "macos")]
        {
            app.source = Rc::new(harness_capture::MacCaptureSource::new());
            app.shooter = Arc::new(harness_capture::screenshot::capture_window);
        }
        for warning in app.capture.warnings.clone() {
            app.model.notice(warning, true, Instant::now());
        }
        app.connect(ClientConfig::discovering().with_name("harness-menubar"), cx);
        app
    }

    pub(crate) fn blank() -> Self {
        Self {
            model: LabModel::default(),
            client: None,
            runtime: None,
            config_error: None,
            shown: TrayState::Idle,
            status: None,
            tray_clicks: 0,
            dismissed: HashSet::new(),
            hotkey_binding: None,
            hotkey_error: None,
            deck_open: false,
            capture: CaptureSettings::default(),
            settings_path: None,
            target: None,
            attached: None,
            // Nothing frontmost and nothing readable until a real source is installed: a blank app
            // can never read a window by accident.
            source: Rc::new(harness_capture::fake::FakeCaptureSource::nothing_frontmost()),
            shooter: Arc::new(|_| Err(Refusal::Unsupported)),
            shooting: false,
            conversation: None,
            chose_new: false,
        }
    }

    fn connect(&mut self, config: ClientConfig, cx: &mut Context<Self>) {
        let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                self.config_error = Some(format!("cannot start the client runtime: {error}"));
                return;
            }
        };
        let handle = Arc::new(harness_client::spawn(config, runtime.handle()));
        self.client = Some(handle);
        self.runtime = Some(runtime);
        self.tick(cx);
    }

    /// The foreground task: wake on the inbox or the tick, apply, and let the caller redraw.
    ///
    /// This is the only writer of `model` — the same rule the Lab follows, for the same reason: two
    /// writers to one model is how a UI ends up drawing half of one update and half of the next.
    pub fn tick(&self, cx: &mut Context<Self>) {
        let Some(handle) = self.client.clone() else { return };
        let inbox = handle.inbox().clone();
        cx.spawn(async move |this, cx| {
            loop {
                let timer = cx.background_executor().timer(TICK);
                let wait = inbox.notified();
                tokio::pin!(timer, wait);
                tokio::select! {
                    _ = &mut timer => {}
                    _ = &mut wait => {}
                }
                if this.update(cx, |app, cx| app.drain(cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Apply everything the client has queued. Returns how many events were applied.
    pub fn drain(&mut self, cx: &mut Context<Self>) -> usize {
        let Some(handle) = self.client.clone() else { return 0 };
        let mut applied = 0usize;
        for event in handle.inbox().drain() {
            let follow = self.conversation_to_follow(&event);
            self.model.apply(event, Instant::now());
            if let Some(handle) = follow {
                self.select_conversation(Some(handle), false);
            }
            applied += 1;
        }
        if applied > 0 {
            cx.notify();
        }
        applied
    }

    /// A click on the status item. Until the deck exists this only records that it happened; a
    /// click that did nothing at all would be impossible to tell from a broken event channel.
    pub fn note_click(&mut self, cx: &mut Context<Self>) {
        self.tray_clicks += 1;
        cx.notify();
    }

    /// The state the tray should show right now.
    pub fn tray_state(&self) -> TrayState {
        deck::tray_state(&self.model)
    }

    pub fn status_line(&self) -> Option<String> {
        deck::status_line(&self.model)
    }

    /// Answers waiting for a person — the icon's badge.
    pub fn waiting(&self) -> usize {
        deck::waiting_count(&self.model)
    }

    /// The request that should pull the deck open by itself, if any — the same tiering rule the
    /// Lab applies to its containment modal, read from the same place (`deck::interrupts`): a
    /// destructive reading opens, everything else waits in the list and never blocks.
    pub fn auto_open_id(&self) -> Option<&str> {
        open_id(self.model.interventions.iter(), &self.dismissed)
    }

    /// Answer one request. A request from a connection that is gone is refused loudly here, for the
    /// same reason the Lab refuses it: the agent may have moved on, and a click must never answer a
    /// question it was not asked.
    pub fn decide(&mut self, id: &str, approve: bool, isolate: bool, cx: &mut Context<Self>) {
        let Some(open) = self.model.interventions.iter().find(|o| o.request.intervention_id == id) else { return };
        if open.stale {
            self.model.notice("that request is from a connection that is gone; it cannot be answered from here", true, Instant::now());
            cx.notify();
            return;
        }
        let note = match (approve, isolate) {
            (true, _) => "approved from the menu bar",
            (false, false) => "rejected from the menu bar",
            (false, true) => "rejected and isolated from the menu bar",
        };
        self.send(Command::Decide { intervention_id: id.to_owned(), approve, isolate, note: note.into() }, cx);
        cx.notify();
    }

    /// Put a request aside without answering it: it keeps waiting, it stops interrupting.
    pub fn send_to_tray(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let opened = self.dismissed.insert(id.to_owned());
        cx.notify();
        opened
    }

    /// Send a command, or say plainly why nothing was sent.
    pub fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        let Some(handle) = &self.client else {
            self.model.notice("no daemon connection: nothing was sent", true, Instant::now());
            cx.notify();
            return;
        };
        if let Err(error) = handle.send(command) {
            self.model.notice(format!("{error}"), true, Instant::now());
            cx.notify();
        }
    }

    /// Start a turn from the deck, in the selected hub conversation (or a new one). `context` is what
    /// the capture was allowed to read; empty is `None` on the wire, so the daemon is never sent an
    /// empty `OsContext` to interpret.
    pub fn submit_turn(&mut self, session_id: String, text: String, context: DeckContext, cx: &mut Context<Self>) {
        let context = if context.is_empty() { DeckContext::default() } else { context };
        let conversation = self.conversation.clone().unwrap_or_else(|| "new".into());
        self.send(Command::SubmitTurn { session_id, text, context, client: "harness-menubar".into(), conversation, cwd: String::new() }, cx);
    }

    /// Ask the daemon for the hub's conversations — when the deck opens, and after a new one starts.
    pub fn refresh_conversations(&mut self, cx: &mut Context<Self>) {
        if self.client.is_some() {
            self.send(Command::ListConversations { cwd: String::new() }, cx);
        }
    }

    /// Follow one conversation (its turns stream into the deck) or none (the next turn starts a new
    /// one). `by_person` marks a choice the automatic "newest" must not override.
    pub fn select_conversation(&mut self, handle: Option<String>, by_person: bool) {
        if by_person {
            self.chose_new = handle.is_none();
        }
        self.model.follow(handle.as_deref());
        if let Some(client) = &self.client {
            client.watch_conversation(handle.clone());
        }
        self.conversation = handle;
    }

    /// The conversation an arriving event says the deck should now be in: a new one the deck's own
    /// turn just opened, or — the first time the list arrives — the newest one anywhere.
    fn conversation_to_follow(&self, event: &harness_client::ClientEvent) -> Option<String> {
        match event {
            harness_client::ClientEvent::Reply(Reply::Turn { reply }) if reply.ok && !reply.conversation.is_empty() && self.conversation.as_deref() != Some(reply.conversation.as_str()) => {
                Some(reply.conversation.clone())
            }
            harness_client::ClientEvent::Reply(Reply::Conversations(list)) if self.conversation.is_none() && !self.chose_new => list.conversations.first().map(|c| c.handle.clone()),
            _ => None,
        }
    }

    /// The deck is being summoned over `frontmost`. Remember it as the app a capture would read — unless
    /// it is this process (a second summon while the deck already had the keyboard), which is never
    /// context. A capture of a *different* app is dropped: it belongs to a screen the person has left.
    pub fn note_target(&mut self, frontmost: Option<Frontmost>) {
        let own = std::process::id() as i32;
        let Some(frontmost) = frontmost.filter(|app| app.pid != own) else { return };
        if self.target.as_ref().map(|app| &app.bundle_id) != Some(&frontmost.bundle_id) {
            self.attached = None;
        }
        self.target = Some(frontmost);
    }

    /// Read the target app's context, because the person clicked Capture. Never called on a tick.
    ///
    /// With capture switched off the answer is a refusal that still names the app, so the deck can
    /// offer the one click that turns it on *for that app* — the only way it is ever turned on.
    pub fn capture_context(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            self.model.notice("no app to read: open the deck over the app you want to share", true, Instant::now());
            cx.notify();
            return;
        };
        self.attached = Some(if self.capture.enabled {
            harness_capture::capture_app(self.source.as_ref(), &target, &self.capture.allowlist)
        } else if harness_capture::policy::is_never_read(&target.bundle_id) {
            // Off or on, a rule answers first — there must be no "allow" button on a password manager.
            harness_capture::capture_app(self.source.as_ref(), &target, &harness_capture::Allowlist::empty())
        } else {
            Capture::refused_app(format!("screen context is off — allowing {} turns it on for that app only", target.name), &target)
        });
        cx.notify();
    }

    /// "Always allow" for the app the last capture refused: add it to the person's own allowlist,
    /// switch capture on, write the user settings file, and capture again. A rule (the never-read
    /// list, an app with no bundle id) has no such button, and this refuses it anyway.
    pub fn allow_target(&mut self, cx: &mut Context<Self>) {
        let Some(bundle_id) = self.attached.as_ref().and_then(|capture| capture.allowable()).map(str::to_owned) else { return };
        self.capture.allowlist.allow(&bundle_id);
        self.capture.enabled = true;
        if let Some(path) = &self.settings_path {
            match self.capture.save_to(path) {
                Ok(()) => self.model.notice(format!("{bundle_id} may be read from now on — remove it from capture.allowlist to stop"), false, Instant::now()),
                Err(error) => self.model.notice(format!("allowed for now, but not saved: {error}"), true, Instant::now()),
            }
        }
        self.capture_context(cx);
    }

    /// Add a screenshot of the target's frontmost window to the attached capture, because the person
    /// clicked for one. Only onto a capture that was *allowed* — the app's verdict is the image's
    /// verdict — and never while secure input is on. The click is also the consent: it switches
    /// `capture.screenshot` on in the person's own settings, the way "Always allow" switches capture on.
    pub fn add_screenshot(&mut self, cx: &mut Context<Self>) {
        let Some(Capture::Captured(preview)) = &self.attached else { return };
        if self.shooting {
            return;
        }
        let pid = preview.pid;
        if self.source.secure_input() {
            if let Some(Capture::Captured(preview)) = &mut self.attached {
                preview.screenshot(Err(Refusal::Secure));
            }
            cx.notify();
            return;
        }
        if !self.capture.screenshot {
            self.capture.screenshot = true;
            if let Some(path) = &self.settings_path
                && let Err(error) = self.capture.save_to(path)
            {
                self.model.notice(format!("screenshots allowed for now, but not saved: {error}"), true, Instant::now());
            }
        }
        self.shooting = true;
        cx.notify();
        let shooter = self.shooter.clone();
        let cloud_ok = self.capture.screenshot_cloud;
        cx.spawn(async move |this, cx| {
            let shot = cx.background_executor().spawn(async move { shooter(pid) }).await;
            let _ = this.update(cx, |app, cx| {
                app.shooting = false;
                // Only onto the capture it was taken for: a person who moved on meanwhile (another
                // app, Don't send) gets nothing attached behind their back.
                if let Some(Capture::Captured(preview)) = &mut app.attached
                    && preview.pid == pid
                {
                    preview.screenshot(shot);
                    preview.cloud_ok = cloud_ok;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Ask macOS for Screen Recording. It only takes effect after the app is reopened, so the notice
    /// says that rather than leaving a person to wonder why the next screenshot still fails.
    pub fn grant_screen_recording(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        let granted = harness_capture::screenshot::request_screen_recording();
        #[cfg(not(target_os = "macos"))]
        let granted = false;
        let note = if granted {
            "Screen Recording is granted — add the screenshot again"
        } else {
            "System Settings › Privacy & Security › Screen Recording: switch this app on, then quit and reopen it"
        };
        self.model.notice(note, !granted, Instant::now());
        cx.notify();
    }

    /// Drop the attached capture without sending it.
    pub fn detach(&mut self, cx: &mut Context<Self>) {
        self.attached = None;
        cx.notify();
    }

    /// Ask macOS to show its own Accessibility prompt. Only from the deck's button: a prompt at launch
    /// would be asking before the person has any reason to say yes.
    pub fn grant_accessibility(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        let trusted = harness_capture::macos::request_accessibility();
        #[cfg(not(target_os = "macos"))]
        let trusted = false;
        let note = if trusted {
            "Accessibility is granted — capture again to read the title and selection"
        } else {
            "System Settings › Privacy & Security › Accessibility: switch this app on, then capture again"
        };
        self.model.notice(note, !trusted, Instant::now());
        cx.notify();
    }

    /// The context for the turn about to be sent, taken so it is sent once. A refusal sends nothing.
    pub fn take_context(&mut self) -> DeckContext {
        let captured_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|since| since.as_millis() as i64).unwrap_or_default();
        self.attached.take().and_then(|capture| capture.preview().map(|preview| preview.to_context(captured_ms))).unwrap_or_default()
    }
}

/// Which pending request takes the deck over: the oldest that *interrupts* and has not been waved
/// away. Pure — takes any iterator, so it is tested without a model, a window, or a client — and the
/// counterpart of the Lab's `modal_id`, which answers the same question for the containment modal.
pub fn open_id<'a>(open: impl DoubleEndedIterator<Item = &'a OpenIntervention>, dismissed: &HashSet<String>) -> Option<&'a str> {
    open.into_iter()
        .find(|o| !o.stale && deck::interrupts(&o.request) && !dismissed.contains(&o.request.intervention_id))
        .map(|o| o.request.intervention_id.as_str())
}

/// Where the app-scoped model is kept. GPUI globals are how an entity outlives every window: as
/// long as the app holds this, the client stays connected with no window open anywhere.
#[derive(Clone)]
pub struct MenubarGlobal(pub Entity<MenubarApp>);

impl Global for MenubarGlobal {}

/// Install the app-scoped model. Called once, inside the run closure.
pub fn install(cx: &mut App) -> Entity<MenubarApp> {
    let app = cx.new(MenubarApp::discovering);
    cx.set_global(MenubarGlobal(app.clone()));
    app
}

/// The app-scoped model, if this process installed one.
pub fn current(cx: &App) -> Option<Entity<MenubarApp>> {
    cx.try_global::<MenubarGlobal>().map(|g| g.0.clone())
}

/// True when a reply is the kind the deck must not miss: a refusal, or a turn that did not start.
/// The Lab treats a failed reply as critical for the same reason — it is the only way a person
/// finds out that something they did never happened.
pub fn is_failure(reply: &Reply) -> bool {
    matches!(reply, Reply::Failed { .. }) || matches!(reply, Reply::Turn { reply } if !reply.ok)
}

#[cfg(test)]
mod tests {
    use harness_client::model::ClientEvent;
    use harness_proto::v1::{InterventionEvent, InterventionRequest, intervention_event};

    use super::*;

    /// A held request as the daemon sends one.
    fn held(id: &str, risk: Option<f64>, armed: bool) -> ClientEvent {
        ClientEvent::Intervention(InterventionEvent {
            event: Some(intervention_event::Event::Request(InterventionRequest {
                intervention_id: id.into(),
                pod: "p1".into(),
                tool: "shell".into(),
                command: "rm -rf build".into(),
                risk_score: risk,
                armed,
                ..Default::default()
            })),
        })
    }

    #[test]
    fn a_serious_request_takes_the_deck_and_a_mild_one_only_waits() {
        // The Lab's rule, in the menu bar: 0.58 waits in the list and never blocks; 0.93 opens.
        let now = Instant::now();
        let mut app = MenubarApp::blank();
        app.model.apply(held("q/1", Some(0.58), false), now);
        assert_eq!(app.auto_open_id(), None, "a mild reading must not pull the deck open");
        assert_eq!(app.model.interventions.len(), 1, "…but it is still open and unanswered");
        assert_eq!(app.waiting(), 0, "the badge counts only what interrupts, so a mild reading does not badge");

        app.model.apply(held("q/2", Some(0.93), false), now);
        assert_eq!(app.auto_open_id(), Some("q/2"), "a serious reading does");
    }

    #[test]
    fn waving_a_request_away_stops_it_taking_the_deck_and_keeps_it_waiting() {
        let now = Instant::now();
        let mut app = MenubarApp::blank();
        app.model.apply(held("q/1", Some(0.93), false), now);
        assert!(app.dismissed.insert("q/1".to_owned()));
        assert_eq!(app.auto_open_id(), None, "a waved-away request stops interrupting");
        // Waving it away is about the deck, not about the badge: it still interrupts for the icon,
        // because the agent is still suspended and the person still has not answered.
        assert_eq!(app.waiting(), 1, "the badge still counts it");
        assert_eq!(app.model.interventions.len(), 1, "and it is still unanswered — the agent is still suspended");
    }

    #[test]
    fn the_deck_never_opens_itself_for_a_request_from_a_connection_that_is_gone() {
        let now = Instant::now();
        let mut app = MenubarApp::blank();
        app.model.apply(held("q/1", Some(0.95), false), now);
        app.model.apply(ClientEvent::Connection(harness_client::model::ConnectionState::Offline { since: now, attempt: 1, retry_at: now, error: "gone".into() }), now);
        assert_eq!(app.auto_open_id(), None, "the connection is gone, so the request cannot be answered: it never takes over");
    }

    #[test]
    fn a_client_that_cannot_start_says_why_instead_of_not_appearing() {
        let mut app = MenubarApp::blank();
        // No daemon config at all is the state the app must survive; it is not an error.
        assert!(app.client.is_none() && app.config_error.is_none());
        assert_eq!(app.tray_state(), TrayState::Idle);
        assert_eq!(app.waiting(), 0);
        assert!(app.status_line().is_none());

        // A real connection failure is reported, and the tray still has a state to show.
        app.config_error = Some("cannot start the client runtime".into());
        assert_eq!(app.tray_state(), TrayState::Idle);
    }

    #[test]
    fn a_failed_reply_is_never_missed() {
        assert!(is_failure(&Reply::Failed { what: "turn".into(), error: "offline".into() }));
        assert!(is_failure(&Reply::Turn { reply: harness_proto::v1::TurnReply { ok: false, error: "refused".into(), ..Default::default() } }));
        assert!(!is_failure(&Reply::Turn { reply: harness_proto::v1::TurnReply { ok: true, route: "turn".into(), ..Default::default() } }));
    }

    #[test]
    fn the_model_is_the_only_writer_and_starts_empty() {
        let app = MenubarApp::blank();
        assert!(app.model.notices.is_empty());
        assert!(app.model.feed.is_empty());
        assert!(app.client.is_none());
    }

    // ------------------------------------------------------------------ the capture, through a fake

    use gpui::TestAppContext;
    use harness_capture::fake::FakeCaptureSource;

    fn safari() -> Frontmost {
        Frontmost { name: "Safari".into(), bundle_id: "com.apple.Safari".into(), pid: 4242 }
    }

    /// A blank app over a fake Safari with a title and a selection, settings written to a temp file.
    fn over_safari(cx: &mut TestAppContext, dir: &std::path::Path) -> Entity<MenubarApp> {
        let path = dir.join("settings.json");
        cx.update(|cx| {
            cx.new(|_| {
                let mut app = MenubarApp::blank();
                app.settings_path = Some(path);
                app.source = Rc::new(FakeCaptureSource::new("Safari", "com.apple.Safari").with_title("PR #123 — key is sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUV-0123456789").with_selection("fn main() {}"));
                app.note_target(Some(safari()));
                app
            })
        })
    }

    #[gpui::test]
    fn capture_is_off_until_a_person_allows_one_named_app(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let app = over_safari(cx, dir.path());
        app.update(cx, |app, cx| app.capture_context(cx));
        app.read_with(cx, |app, _| {
            let attached = app.attached.as_ref().expect("a click always answers");
            assert!(attached.preview().is_none(), "off is the shipped state: nothing was read");
            assert_eq!(attached.allowable(), Some("com.apple.Safari"), "and the deck can offer to allow it");
        });
        assert!(!dir.path().join("settings.json").exists(), "a capture never writes settings by itself");

        app.update(cx, |app, cx| app.allow_target(cx));
        app.read_with(cx, |app, _| {
            assert!(app.capture.enabled && app.capture.allowlist.contains("com.apple.Safari"));
            let preview = app.attached.as_ref().and_then(|capture| capture.preview()).expect("allowed, so read");
            assert_eq!(preview.app_name, "Safari");
        });
        let saved = CaptureSettings::load_from(&dir.path().join("settings.json"));
        assert!(saved.enabled && saved.allowlist.contains("com.apple.Safari"), "the person's answer outlives the process");

        let context = app.update(cx, |app, _| app.take_context());
        assert_eq!(context.app_bundle_id, "com.apple.Safari");
        assert_eq!(context.selected_text, "fn main() {}");
        assert!(context.window_title.starts_with("PR #123") && !context.window_title.contains("ABCDEFGHIJKLMNOPQRSTUV"), "masked before it leaves: {}", context.window_title);
        assert!(context.client_redactions >= 1);
        assert!(app.update(cx, |app, _| app.take_context()).is_empty(), "taken once: a later turn does not inherit it");
    }

    #[gpui::test]
    fn a_password_manager_never_gets_an_allow_button(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let app = over_safari(cx, dir.path());
        app.update(cx, |app, cx| {
            app.note_target(Some(Frontmost { name: "1Password".into(), bundle_id: "com.1password.1password".into(), pid: 77 }));
            app.capture_context(cx);
        });
        app.read_with(cx, |app, _| {
            let attached = app.attached.as_ref().unwrap();
            assert_eq!(attached.allowable(), None, "a rule, not a preference");
            assert!(attached.reason().is_some_and(|reason| reason.contains("never read")), "{:?}", attached.reason());
        });
        app.update(cx, |app, cx| app.allow_target(cx));
        app.read_with(cx, |app, _| assert!(!app.capture.allowlist.contains("com.1password.1password") && !app.capture.enabled));
        assert!(app.update(cx, |app, _| app.take_context()).is_empty(), "a refusal sends nothing");
    }

    #[test]
    fn the_deck_itself_is_never_the_target_and_a_new_app_drops_the_old_capture() {
        let mut app = MenubarApp::blank();
        app.note_target(Some(safari()));
        app.attached = Some(Capture::refused_app("x", &safari()));
        app.note_target(Some(Frontmost { name: "deck".into(), bundle_id: "dev.cordata.menubar".into(), pid: std::process::id() as i32 }));
        assert_eq!(app.target.as_ref().map(|t| t.name.as_str()), Some("Safari"), "summoning over the deck keeps the real target");
        assert!(app.attached.is_some(), "and keeps its capture");
        app.note_target(Some(Frontmost { name: "Mail".into(), bundle_id: "com.apple.mail".into(), pid: 9 }));
        assert!(app.attached.is_none(), "a capture of a screen the person has left does not ride along");
    }

    #[gpui::test]
    fn a_screenshot_is_taken_off_the_main_thread_for_an_allowed_app_and_switches_its_setting_on(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let app = over_safari(cx, dir.path());
        app.update(cx, |app, cx| {
            app.shooter = Arc::new(|pid| if pid == 4242 { Ok(Shot { jpeg: vec![0xFF, 0xD8, 0xFF], width: 800, height: 500 }) } else { Err(Refusal::Nothing) });
            app.add_screenshot(cx);
        });
        app.read_with(cx, |app, _| assert!(!app.shooting && !app.capture.screenshot, "no allowed capture yet, so nothing is taken or switched on"));

        app.update(cx, |app, cx| {
            app.capture_context(cx);
            app.allow_target(cx);
            app.add_screenshot(cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(app.capture.screenshot, "the click is the consent, and it is remembered");
            let preview = app.attached.as_ref().and_then(|capture| capture.preview()).unwrap();
            assert_eq!(preview.shot.as_ref().map(|shot| shot.width), Some(800));
        });
        assert!(CaptureSettings::load_from(&dir.path().join("settings.json")).screenshot);
        let context = app.update(cx, |app, _| app.take_context());
        assert_eq!(context.screenshot_mime, "image/jpeg");
        assert!(!context.screenshot_cloud_ok, "local only by default");
    }

    #[gpui::test]
    fn secure_input_means_no_screenshot(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let app = over_safari(cx, dir.path());
        app.update(cx, |app, cx| {
            app.capture_context(cx);
            app.allow_target(cx);
            app.source = Rc::new(FakeCaptureSource::new("Safari", "com.apple.Safari").with_secure_input(true));
            app.shooter = Arc::new(|_| panic!("the window server must not be asked while a password field has focus"));
            app.add_screenshot(cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let preview = app.attached.as_ref().and_then(|capture| capture.preview()).unwrap();
            assert!(preview.shot.is_none() && preview.omissions().iter().any(|o| o.starts_with("screenshot")));
        });
    }

    #[test]
    fn the_deck_follows_the_newest_conversation_unless_the_person_chose_a_new_one() {
        use harness_client::ClientEvent;
        use harness_proto::v1::{Conversation, ConversationList, TurnReply};

        let list = |handles: &[&str]| ClientEvent::Reply(Reply::Conversations(ConversationList { conversations: handles.iter().map(|h| Conversation { handle: (*h).into(), ..Default::default() }).collect() }));
        let mut app = MenubarApp::blank();
        assert_eq!(app.conversation_to_follow(&list(&["cnewest", "colder"])), Some("cnewest".into()), "the handoff: the newest conversation, wherever it was started");

        app.select_conversation(None, true);
        assert_eq!(app.conversation_to_follow(&list(&["cnewest"])), None, "a person who chose 'new' keeps it");

        let opened = ClientEvent::Reply(Reply::Turn { reply: TurnReply { ok: true, conversation: "cfresh1".into(), ..Default::default() } });
        assert_eq!(app.conversation_to_follow(&opened), Some("cfresh1".into()), "the conversation the deck's own turn opened is followed");
        app.select_conversation(Some("cfresh1".into()), false);
        assert_eq!(app.model.transcript.handle, "cfresh1", "and its transcript is the one drawn");
        let refused = ClientEvent::Reply(Reply::Turn { reply: TurnReply { ok: false, conversation: "cother9".into(), ..Default::default() } });
        assert_eq!(app.conversation_to_follow(&refused), None, "a refused turn moves nothing");
    }
}
