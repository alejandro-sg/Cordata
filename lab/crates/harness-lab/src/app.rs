//! The application entity: it owns the model, the client, the observer, and the keys.
//!
//! The Python daemon keeps every decision (Strands routing, Laya gating, LanceDB, the vault);
//! this process draws what the daemon reports and forwards a human's clicks, keystrokes and file
//! saves. Nothing here decides anything about an agent.
//!
//! Threading, which is the whole reason this file is shaped the way it is:
//!
//! * the gRPC client runs on its own tokio runtime and talks to the UI only through an [`Inbox`]
//!   and bounded `try_send` channels — the UI thread never awaits the network, and a slow frame
//!   cannot stall a stream;
//! * the observer runs on its own OS thread and hands snippets to the client's ingest channel
//!   through a `try_send`, dropping (and counting) rather than blocking;
//! * one GPUI foreground task wakes on either the inbox or a 100 ms tick, drains, applies, and
//!   calls `notify()` — the only place the model is mutated, so there is no lock to contend.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui::{Bounds, Context, FocusHandle, Pixels, Point, SharedString};
use harness_client::{ClientConfig, ClientEvent, Command, ConnectionState, Handle, Knob, LabModel, PodControl, Reply};
use harness_client::config::DaemonConfig;
use harness_observer::{Change, Observer, ObserverConfig, Snippet};
use harness_proto::v1::AstSnippet;

use crate::theme::Theme;

/// The side column's tabs. Only one is open at a time: the point of tabs here is that nothing the
/// person is not looking at is competing for their attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// What just happened, in plain language.
    #[default]
    Activity,
    /// What the team produced: diffs.
    Outputs,
    /// Everything diagnostic — the daemon, Laya's reading, memory, the observer, the keys.
    System,
}

/// The three permission modes, in the order `m` cycles them.
pub const MODES: [&str; 3] = ["plan", "build", "auto"];

/// How often the foreground task wakes even when the daemon is silent: the clock, the reconnect
/// countdown and the observer's counters live in the header, and they must move.
const TICK: Duration = Duration::from_millis(100);

/// Counters the observer thread writes and the UI thread reads. Plain atomics: no channel, no
/// wake-up, no lock shared with the drawing code.
#[derive(Default)]
pub struct ObserverCounters {
    /// Batches of changes the observer reported: a save, or a burst of saves it debounced into one.
    pub saves: AtomicU64,
    /// Changed symbols (functions, classes) across all of them.
    pub snippets: AtomicU64,
    /// Symbols the client's ingest queue refused because it was full — dropped, never blocked on.
    pub skipped: AtomicU64,
    /// Symbols handed to the ingest stream.
    pub sent: AtomicU64,
    /// The last few symbols, for the observer panel. The only lock here; the UI thread only ever
    /// `try_lock`s it, so a busy observer cannot make a frame wait.
    recent: std::sync::Mutex<std::collections::VecDeque<String>>,
}

const RECENT_SYMBOLS: usize = 12;

impl ObserverCounters {
    pub(crate) fn snapshot(&self) -> (u64, u64, u64, u64) {
        (self.saves.load(Ordering::Relaxed), self.snippets.load(Ordering::Relaxed), self.skipped.load(Ordering::Relaxed), self.sent.load(Ordering::Relaxed))
    }

    pub(crate) fn remember(&self, line: String) {
        if let Ok(mut recent) = self.recent.lock() {
            recent.push_back(line);
            while recent.len() > RECENT_SYMBOLS {
                recent.pop_front();
            }
        }
    }

    pub(crate) fn recent(&self) -> Vec<String> {
        self.recent.try_lock().map(|r| r.iter().cloned().collect()).unwrap_or_default()
    }
}

/// A diff body the user asked for.
pub struct DiffView {
    pub id: String,
    pub source: String,
    pub files: Vec<String>,
    pub lines: u32,
    pub truncated: bool,
    pub error: Option<String>,
    /// The body's lines, split once so 10,000 of them are not re-split per frame, and shared
    /// (`Arc`) so a redraw hands the overlay a pointer rather than 10,000 clones.
    pub rendered: Arc<Vec<SharedString>>,
}

impl DiffView {
    pub(crate) fn new(id: String, source: String, files: Vec<String>, lines: u32, truncated: bool) -> Self {
        Self { id, source, files, lines, truncated, error: None, rendered: Arc::default() }
    }

    pub(crate) fn set_body(&mut self, body: String) {
        // `SharedString` from a borrowed slice wants `'static`, so each line is copied once here
        // rather than borrowed from a body this struct also has to own.
        self.rendered = Arc::new(body.lines().map(|line| SharedString::from(line.to_owned())).collect());
    }
}

pub struct LabApp {
    pub model: LabModel,
    pub theme: Theme,
    /// `None` when the daemon config could not be read — the window still opens and says why,
    /// which is the whole of "degrade gracefully": a GUI that refuses to start cannot explain.
    pub client: Option<Arc<Handle>>,
    #[allow(dead_code)] // held so the runtime outlives the client tasks
    runtime: Option<tokio::runtime::Runtime>,
    pub config_error: Option<String>,
    pub observer: Option<Observer>,
    pub observer_error: Option<String>,
    pub observer_root: PathBuf,
    pub observer_counters: Arc<ObserverCounters>,
    /// The pod whose detail (trace, stream, memory) is drawn; defaults to the focused pod.
    pub selected: Option<String>,
    pub diff: Option<DiffView>,
    pub started: Instant,
    pub redrawn: u64,
    /// The view's focus handle: the root element tracks focus so the letter keys land here.
    pub focus: FocusHandle,
    pub(crate) focused_once: bool,
    /// Approvals the user sent to the tray with esc. They stay pending — the agent stays suspended —
    /// they just stop interrupting. Keyed by intervention id.
    pub dismissed: HashSet<String>,
    /// A tray item opened for full review (the modal shows it until it is answered or sent back).
    pub reviewing: Option<String>,
    pub tab: Tab,
    /// The steer bar: text being typed, and whether it has the keyboard.
    pub composer: String,
    pub composing: bool,
    /// Activity tab: show only the selected agent's lines.
    pub feed_only_selected: bool,
    /// The memory ring is under the pointer, so it offers the Unload it can do rather than a number.
    pub memory_hovered: bool,
    /// The pod whose Laya dial is being dragged, if any. The dial's own bounds are recorded while it
    /// is painted (the same way GPUI records one for a test), so the pointer's window position can
    /// be turned into an angle without measuring anything by hand.
    pub dial_drag: Option<String>,
    pub dial_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl LabApp {
    /// Build the app with a fixed daemon address (or none, to show the no-daemon state). The binary
    /// uses [`LabApp::discovering`]; this is the entry the tests and fixed-address callers use.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(config: Result<DaemonConfig, String>, root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut app = Self::blank(root, cx);
        match config {
            Ok(config) => app.connect(ClientConfig::new(config), cx),
            Err(reason) => {
                app.config_error = Some(reason);
                app.model.notice("the daemon is not reachable: see the banner", true, Instant::now());
            }
        }
        app
    }

    /// The app as the binary runs it: the client finds the daemon itself, on every attempt. Opening
    /// the window before the daemon exists — or across a daemon restart that moved its port or
    /// rotated its token — is the same state as a daemon that is merely slow: offline, retrying,
    /// and online the moment `daemon.json` names something that answers.
    pub fn discovering(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut app = Self::blank(root, cx);
        app.connect(ClientConfig::discovering(), cx);
        app
    }

    fn blank(root: PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            model: LabModel::default(),
            theme: Theme::dark(),
            client: None,
            runtime: None,
            config_error: None,
            observer: None,
            observer_error: None,
            observer_root: root,
            observer_counters: Arc::new(ObserverCounters::default()),
            selected: None,
            diff: None,
            started: Instant::now(),
            redrawn: 0,
            focus: cx.focus_handle(),
            focused_once: false,
            dismissed: HashSet::new(),
            reviewing: None,
            tab: Tab::default(),
            composer: String::new(),
            composing: false,
            feed_only_selected: false,
            memory_hovered: false,
            dial_drag: None,
            dial_bounds: Rc::new(Cell::new(None)),
        }
    }

    /// Start the gRPC client and the workspace observer.
    fn connect(&mut self, config: ClientConfig, cx: &mut Context<Self>) {
        let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                self.config_error = Some(format!("cannot start the client runtime: {error}"));
                return;
            }
        };
        let handle = Arc::new(harness_client::spawn(config, runtime.handle()));
        self.start_observer(handle.clone(), cx);
        self.runtime = Some(runtime);
        self.client = Some(handle);
        self.tick(cx);
    }

    /// The proactive observer: watch the workspace, parse saved files locally, hand only the
    /// changed symbols to the daemon. It is optional in both directions — a workspace that cannot
    /// be watched costs the GUI a line in the header, not a failure to start.
    fn start_observer(&mut self, handle: Arc<Handle>, cx: &mut Context<Self>) {
        let counters = self.observer_counters.clone();
        let sink = move |snippets: Vec<Snippet>| {
            counters.saves.fetch_add(1, Ordering::Relaxed);
            for snippet in snippets {
                counters.snippets.fetch_add(1, Ordering::Relaxed);
                if handle.ingest(to_snippet(&snippet)) {
                    counters.sent.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.skipped.fetch_add(1, Ordering::Relaxed);
                }
                let verb = match snippet.change {
                    Change::Added => "added",
                    Change::Modified => "modified",
                };
                counters.remember(format!("{verb} {}::{}", snippet.path, snippet.name));
            }
        };
        let config = ObserverConfig::new(self.observer_root.clone());
        match Observer::start(config, sink) {
            Ok(observer) => self.observer = Some(observer),
            Err(error) => self.observer_error = Some(error.to_string()),
        }
        let _ = cx;
    }

    /// The foreground task: wake on the inbox or the tick, apply, redraw.
    fn tick(&self, cx: &mut Context<Self>) {
        let (Some(handle), Some(runtime)) = (self.client.clone(), self.runtime.as_ref()) else { return };
        let inbox = handle.inbox().clone();
        let _guard = runtime.enter();
        cx.spawn(async move |this, cx| {
            loop {
                let timer = cx.background_executor().timer(TICK);
                let wait = inbox.notified();
                tokio::pin!(timer, wait);
                tokio::select! {
                    _ = &mut timer => {}
                    _ = &mut wait => {}
                }
                let alive = this.update(cx, |app, cx| app.drain(cx)).is_ok();
                if !alive {
                    return;
                }
            }
        })
        .detach();
    }

    /// Apply everything the client and the UI have queued. The only writer of `model`.
    pub fn drain(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.client.clone() else { return };
        let mut applied = 0usize;
        for event in handle.inbox().drain() {
            // A diff body the client fetched is the one reply the model cannot hold: it belongs to
            // the overlay that asked for it, and it is what makes `d` show something.
            if let (ClientEvent::Reply(Reply::Diff(artifact)), Some(id)) = (&event, self.diff.as_ref().map(|diff| diff.id.clone())) {
                let answered = artifact.r#ref.as_ref().map(|r| r.id.clone()).unwrap_or_default();
                if answered.is_empty() || answered == id {
                    if artifact.unified.is_empty() {
                        self.set_diff_error(&id, "the daemon sent an empty diff".into());
                    } else {
                        self.set_diff_body(&id, artifact.unified.clone());
                    }
                }
            }
            self.model.apply(event, Instant::now());
            applied += 1;
        }
        if applied > 0 {
            // What has been answered (here or on another front end) no longer needs remembering.
            let answered: Vec<&str> = self.model.resolved.iter().map(|r| r.intervention_id.as_str()).collect();
            self.dismissed.retain(|id| !answered.contains(&id.as_str()));
            if self.reviewing.as_deref().is_some_and(|id| answered.contains(&id)) {
                self.reviewing = None;
            }
            cx.notify();
        }
        self.select_default_pod();
    }

    /// Keep a sensible pod selected as pods come and go.
    fn select_default_pod(&mut self) {
        let pods: Vec<String> = self.model.lab().map(|lab| lab.pods.iter().map(|p| p.id.clone()).collect()).unwrap_or_default();
        let still_there = self.selected.as_ref().is_some_and(|id| pods.contains(id));
        if still_there {
            return;
        }
        let focus = self.model.lab().and_then(|lab| if lab.focus.is_empty() { None } else { Some(lab.focus.clone()) });
        self.selected = focus.filter(|id| pods.contains(id)).or_else(|| pods.first().cloned());
    }

    // ------------------------------------------------------------------ commands

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
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

    // ------------------------------------------------------------- approvals
    //
    // Interruption is tiered. A *destructive* reading (Laya at or above the router's gate) opens the
    // containment modal on its own — that is the one thing allowed to take over the window. Anything
    // else the daemon asks about waits in a docked tray that never blocks: answer it from there, open
    // it for review (`r`), or ignore it while you work; the agent is suspended either way. `esc` on
    // the modal sends it to the tray too — dismissing a view never answers the question.

    /// The request the modal is showing, if any: the one opened for review, else the oldest
    /// destructive one the user has not sent to the tray.
    pub fn modal_intervention(&self) -> Option<&harness_client::OpenIntervention> {
        modal_id(&self.model.interventions, &self.dismissed, self.reviewing.as_deref()).and_then(|id| self.model.interventions.iter().find(|o| o.request.intervention_id == id))
    }

    /// Everything pending that the modal is *not* showing, oldest first — what the tray lists.
    pub fn tray_items(&self) -> Vec<&harness_client::OpenIntervention> {
        let shown = self.modal_intervention().map(|o| o.request.intervention_id.as_str());
        self.model.interventions.iter().filter(|o| Some(o.request.intervention_id.as_str()) != shown).collect()
    }

    /// `y` / `n` / `k` act on the request on screen in the modal — never on one the user has not seen.
    pub fn approve(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.modal_intervention().map(|o| o.request.intervention_id.clone()) {
            self.decide(&id, true, false, cx);
        }
    }

    pub fn reject(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.modal_intervention().map(|o| o.request.intervention_id.clone()) {
            self.decide(&id, false, false, cx);
        }
    }

    /// Reject *and* isolate the pod that asked, and everything below it.
    pub fn isolate(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.modal_intervention().map(|o| o.request.intervention_id.clone()) {
            self.decide(&id, false, true, cx);
        }
    }

    /// Answer one request by id (a modal button, a key, or a tray button). A request from a
    /// connection that is gone is refused here, loudly: the agent may already have moved on.
    pub fn decide(&mut self, id: &str, approve: bool, isolate: bool, cx: &mut Context<Self>) {
        let Some(open) = self.model.interventions.iter().find(|o| o.request.intervention_id == id) else { return };
        if open.stale {
            self.model.notice("that request is from a connection that is gone; it cannot be answered from here", true, Instant::now());
            cx.notify();
            return;
        }
        let note = match (approve, isolate) {
            (true, _) => "approved in the GPUI lab",
            (false, false) => "rejected in the GPUI lab",
            (false, true) => "rejected and isolated in the GPUI lab",
        };
        if self.reviewing.as_deref() == Some(id) {
            self.reviewing = None;
        }
        self.send(Command::Decide { intervention_id: id.to_owned(), approve, isolate, note: note.into() }, cx);
        cx.notify();
    }

    /// `esc` on the modal: send it to the tray. Nothing is answered.
    pub fn send_to_tray(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.modal_intervention().map(|o| o.request.intervention_id.clone()) else { return false };
        self.reviewing = None;
        if self.dismissed.len() > 64 {
            self.dismissed.clear();
        }
        self.dismissed.insert(id);
        cx.notify();
        true
    }

    /// `r` / a tray card's Review: open the full view of one pending request.
    pub fn review(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        let target = id.or_else(|| self.tray_items().first().map(|o| o.request.intervention_id.clone()));
        if let Some(id) = target {
            self.dismissed.remove(&id);
            self.reviewing = Some(id);
            cx.notify();
        }
    }

    /// `u` / the OOM button — ask the daemon to drop oMLX and Ollama weights.
    pub fn unload_memory(&mut self, include_ollama: bool, cx: &mut Context<Self>) {
        self.model.notice(if include_ollama { "asking the daemon to unload every resident model…" } else { "asking the daemon to unload the oMLX models…" }, false, Instant::now());
        self.send(Command::UnloadMemory { reason: "the GPUI lab's emergency unload".into(), include_ollama }, cx);
    }

    /// `d` — fetch the body of a diff the telemetry only referenced.
    pub fn fetch_diff(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        let id = id.or_else(|| self.model.diffs.first().map(|d| d.id.clone()));
        let Some(id) = id else {
            self.model.notice("no diff to fetch", true, Instant::now());
            cx.notify();
            return;
        };
        let reference = self.model.diffs.iter().find(|d| d.id == id).cloned();
        let (source, files, lines, truncated) = reference.map(|d| (d.source, d.files, d.lines, d.truncated)).unwrap_or_default();
        self.diff = Some(DiffView::new(id.clone(), source, files, lines, truncated));
        self.send(Command::FetchDiff { id }, cx);
    }

    /// `esc` — close the diff overlay (never the approval modal: that one is answered, not dismissed).
    pub fn close_diff(&mut self, cx: &mut Context<Self>) {
        if self.diff.take().is_some() {
            cx.notify();
        }
    }

    /// Set a pod's `reasoning_effort` mid-execution.
    pub fn set_effort(&mut self, level: &str, cx: &mut Context<Self>) {
        // The pod the inspector is showing — `selected` when set, else the first — so a knob always
        // acts on the pod whose values are on screen.
        let (Some(session), Some(pod)) = (self.session_id(), self.selected_pod().map(|p| p.id.clone())) else { return };
        self.send(Command::SetKnob { session_id: session, pod, knob: Knob::ReasoningEffort(level.to_owned()) }, cx);
    }

    /// Dial the Laya gate for a pod. The daemon clamps to [0.30, 0.80] and only ever tightens.
    pub fn set_dial(&mut self, value: f64, cx: &mut Context<Self>) {
        let (Some(session), Some(pod)) = (self.session_id(), self.selected_pod().map(|p| p.id.clone())) else { return };
        self.send(Command::SetKnob { session_id: session, pod, knob: Knob::LayaDial(value.clamp(0.30, 0.80)) }, cx);
    }

    /// `[` / `]` and the − / + buttons: move the selected pod's dial one step from where it is now.
    /// A pointer position over the dial, as the value it points at. `None` when the dial has not
    /// been painted yet (or is not on screen), so a drag before the first frame does nothing.
    pub fn dial_value_at_pointer(&self, position: Point<Pixels>) -> Option<f64> {
        let bounds = self.dial_bounds.get()?;
        if bounds.size.width <= gpui::px(1.) {
            return None;
        }
        let centre = gpui::point(bounds.origin.x + bounds.size.width / 2.0, bounds.origin.y + bounds.size.height / 2.0);
        Some(crate::ui::dial::dial_value_at(position, centre))
    }

    /// Start dragging a pod's dial, taking the value the press itself points at — so a click sets
    /// the dial as well as starting the drag.
    pub fn begin_dial_drag(&mut self, pod: &str, position: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(session), Some(value)) = (self.session_id(), self.dial_value_at_pointer(position)) else { return };
        self.dial_drag = Some(pod.to_owned());
        self.send(Command::SetKnob { session_id: session, pod: pod.to_owned(), knob: Knob::LayaDial(value) }, cx);
    }

    /// Move the dial being dragged. Only sends when a drag is live, so the window's own mouse
    /// tracking costs nothing when the pointer is merely over the window.
    pub fn drag_dial(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(session), Some(pod), Some(value)) = (self.session_id(), self.dial_drag.clone(), self.dial_value_at_pointer(position)) else { return };
        self.send(Command::SetKnob { session_id: session, pod, knob: Knob::LayaDial(value) }, cx);
    }

    pub fn end_dial_drag(&mut self, cx: &mut Context<Self>) {
        if self.dial_drag.take().is_some() {
            cx.notify();
        }
    }

    pub fn nudge_dial(&mut self, delta: f64, cx: &mut Context<Self>) {
        let current = self.selected_pod().map(|p| p.dial).filter(|d| *d > 0.0).unwrap_or(0.8);
        self.set_dial(((current + delta) * 100.0).round() / 100.0, cx);
    }

    // ------------------------------------------------------------ mode + agents

    /// The session's permission mode as the child reports it (`""` from one that predates it).
    pub fn current_mode(&self) -> Option<&str> {
        self.model.lab().map(|lab| lab.mode.as_str()).filter(|m| !m.is_empty())
    }

    /// Switch the session between `plan`, `build` and `auto`. The daemon relays it to the session's
    /// own `session/set_mode` — what `auto` still stops for is decided there, not here.
    pub fn set_mode(&mut self, mode: &str, cx: &mut Context<Self>) {
        if self.current_mode() == Some(mode) {
            return;
        }
        let Some(session) = self.session_id() else {
            self.model.notice("no live session to change the mode of", true, Instant::now());
            cx.notify();
            return;
        };
        self.send(Command::SetMode { session_id: session, mode: mode.to_owned() }, cx);
    }

    /// `m`: plan → build → auto → plan.
    pub fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        let at = MODES.iter().position(|m| Some(*m) == self.current_mode()).unwrap_or(1);
        self.set_mode(MODES[(at + 1) % MODES.len()], cx);
    }

    /// E-Stop one agent (and everything it spawned).
    pub fn stop_pod(&mut self, pod: &str, cx: &mut Context<Self>) {
        let Some(session) = self.session_id() else { return };
        self.send(Command::ControlPod { session_id: session, pod: pod.to_owned(), control: PodControl::Isolate }, cx);
    }

    /// The global E-Stop: isolate every agent of the session in focus, in one gesture.
    ///
    /// It refuses rather than kills. A pod is an agent inside the daemon's own process — there is no
    /// sandbox to send a signal to (see THREAT_MODEL.md), and an isolate stops the agent at its next
    /// loop turn and cascade-stops its children. Sending one per pod (rather than one for the root)
    /// is what makes "all of them, right now" true even for a pod that has no parent on the canvas.
    pub fn estop_all(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session_id() else { return };
        let pods: Vec<String> = self.model.lab().map(|lab| lab.pods.iter().map(|p| p.id.clone()).collect()).unwrap_or_default();
        if pods.is_empty() {
            return;
        }
        for pod in pods {
            self.send(Command::ControlPod { session_id: session.clone(), pod, control: PodControl::Isolate }, cx);
        }
    }

    /// How many pods would a global E-Stop reach right now.
    pub fn estop_reach(&self) -> usize {
        self.model.lab().map(|lab| lab.pods.iter().filter(|p| !p.isolated).count()).unwrap_or(0)
    }

    pub fn release_pod(&mut self, pod: &str, cx: &mut Context<Self>) {
        let Some(session) = self.session_id() else { return };
        self.send(Command::ControlPod { session_id: session, pod: pod.to_owned(), control: PodControl::Release }, cx);
    }

    /// Who the steer bar talks to: the agent selected on the board, else the orchestrator.
    pub fn steer_target(&self) -> Option<String> {
        self.selected_pod().map(|p| p.id.clone())
    }

    /// Send the steer bar's text to its target as an operator directive and clear it.
    pub fn steer(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let (Some(session), Some(pod)) = (self.session_id(), self.steer_target()) else {
            self.model.notice("no live session to steer", true, Instant::now());
            cx.notify();
            return;
        };
        self.send(Command::ControlPod { session_id: session, pod, control: PodControl::Steer(text) }, cx);
        self.composer.clear();
        cx.notify();
    }

    pub fn begin_composing(&mut self, cx: &mut Context<Self>) {
        self.composing = true;
        cx.notify();
    }

    pub fn end_composing(&mut self, cx: &mut Context<Self>) {
        self.composing = false;
        cx.notify();
    }

    /// Typed text goes into the steer bar, capped (the bridge cuts a directive at 1,500 characters).
    pub fn type_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let room = 1500usize.saturating_sub(self.composer.chars().count());
        self.composer.extend(text.chars().filter(|c| !c.is_control()).take(room));
        cx.notify();
    }

    pub fn backspace(&mut self, cx: &mut Context<Self>) {
        self.composer.pop();
        cx.notify();
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
            self.type_text(&one_line, cx);
        }
    }

    pub fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    fn session_id(&self) -> Option<String> {
        self.model.focus.clone().filter(|s| !s.is_empty()).or_else(|| self.model.labs.keys().next_back().cloned())
    }

    /// The pods of the session in focus, selected pod first when it is set.
    pub fn selected_pod(&self) -> Option<&harness_proto::v1::Pod> {
        let lab = self.model.lab()?;
        self.selected.as_ref().and_then(|id| lab.pods.iter().find(|p| &p.id == id)).or_else(|| lab.pods.first())
    }

    pub fn step_pod(&mut self, delta: isize, cx: &mut Context<Self>) {
        let pods: Vec<String> = self.model.lab().map(|lab| lab.pods.iter().map(|p| p.id.clone()).collect()).unwrap_or_default();
        if pods.is_empty() {
            return;
        }
        let current = self.selected.as_ref().and_then(|id| pods.iter().position(|p| p == id)).unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(pods.len() as isize) as usize;
        self.selected = Some(pods[next].clone());
        cx.notify();
    }

    pub fn next_session(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.model.labs.keys().cloned().collect();
        if ids.len() < 2 {
            return;
        }
        let current = self.model.focus.as_ref().and_then(|f| ids.iter().position(|i| i == f)).unwrap_or(0);
        self.model.focus = Some(ids[(current + 1) % ids.len()].clone());
        self.selected = None;
        self.select_default_pod();
        cx.notify();
    }

    /// The connection, for the header. Kept here so the view never reaches into the client.
    pub fn connection(&self) -> &ConnectionState {
        &self.model.connection
    }

    pub fn quit(&mut self, cx: &mut Context<Self>) {
        cx.quit();
    }
}

/// Does this request take over the window by itself? A destructive reading does — at or above the
/// router's own 0.80 gate — and so does a request Laya marked armed without scoring it (when in
/// doubt, interrupt). Everything else waits in the tray.
pub fn interrupts(request: &harness_proto::v1::InterventionRequest) -> bool {
    match request.risk_score {
        Some(score) => score >= 0.8,
        None => request.armed,
    }
}

/// Which pending request the modal shows: the one under review, else the oldest that interrupts
/// and has not been sent to the tray. Pure, so the rule that keeps `y` from answering a question
/// nobody has seen is tested directly.
pub fn modal_id<'a>(open: &'a std::collections::VecDeque<harness_client::OpenIntervention>, dismissed: &HashSet<String>, reviewing: Option<&str>) -> Option<&'a str> {
    if let Some(id) = reviewing
        && let Some(found) = open.iter().find(|o| o.request.intervention_id == id)
    {
        return Some(found.request.intervention_id.as_str());
    }
    open.iter().find(|o| interrupts(&o.request) && !dismissed.contains(&o.request.intervention_id)).map(|o| o.request.intervention_id.as_str())
}

/// Observer snippet → the wire message. Split out so it is unit-tested without a filesystem.
pub fn to_snippet(snippet: &Snippet) -> AstSnippet {
    AstSnippet {
        workspace: snippet.workspace.to_string_lossy().into_owned(),
        path: snippet.path.clone(),
        language: snippet.language.to_owned(),
        node_kind: snippet.node_kind.clone(),
        name: snippet.name.clone(),
        start_line: snippet.start_line,
        end_line: snippet.end_line,
        text: snippet.text.clone(),
        content_hash: snippet.content_hash.clone(),
        change: match snippet.change {
            Change::Added => "added".to_owned(),
            Change::Modified => "modified".to_owned(),
        },
        saved_ms: snippet.saved_ms,
    }
}


/// The window title, which says what is on screen without a status line.
pub fn window_title(app: &LabApp) -> String {
    let title = title_text(app.config_error.is_some(), app.model.connection.is_online(), app.model.labs.len());
    match app.model.interventions.len() {
        0 => title,
        n => format!("● {n} approval{} waiting · {title}", if n == 1 { "" } else { "s" }),
    }
}

/// What is wrong first, then what is on screen. Pure, so it is tested without a window or a
/// daemon — the two things this line exists to describe.
pub fn title_text(no_config: bool, online: bool, sessions: usize) -> String {
    let plural = if sessions == 1 { "" } else { "s" };
    match (no_config, online) {
        (true, _) => "sierralab · Lab (no daemon config)".into(),
        (false, true) => format!("sierralab · Lab · {sessions} session{plural}"),
        (false, false) => format!("sierralab · Lab · offline · {sessions} session{plural}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn snippet() -> Snippet {
        Snippet {
            workspace: PathBuf::from("/tmp/work"),
            path: "src/lib.rs".into(),
            language: "rust",
            node_kind: "function_item".into(),
            name: "parse".into(),
            start_line: 10,
            end_line: 24,
            text: "fn parse() {}".into(),
            content_hash: "abc".into(),
            change: Change::Modified,
            saved_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn a_changed_symbol_becomes_the_wire_message_the_daemon_dedupes_on() {
        let wire = to_snippet(&snippet());
        assert_eq!(wire.change, "modified");
        assert_eq!(wire.content_hash, "abc");
        assert_eq!((wire.start_line, wire.end_line), (10, 24));
        assert_eq!(wire.language, "rust");
        assert_eq!(wire.workspace, "/tmp/work");
        assert_eq!(wire.path, "src/lib.rs");
        let mut added = snippet();
        added.change = Change::Added;
        assert_eq!(to_snippet(&added).change, "added");
    }

    fn open(id: &str, score: Option<f64>, armed: bool) -> harness_client::OpenIntervention {
        harness_client::OpenIntervention {
            request: harness_proto::v1::InterventionRequest { intervention_id: id.into(), risk_score: score, armed, ..Default::default() },
            since: Instant::now(),
            stale: false,
        }
    }

    #[test]
    fn only_a_destructive_reading_interrupts() {
        let req = |score, armed| open("x", score, armed).request;
        assert!(interrupts(&req(Some(0.93), true)));
        assert!(interrupts(&req(Some(0.80), false)), "the gate itself is destructive");
        assert!(!interrupts(&req(Some(0.58), false)), "an elevated reading waits in the tray");
        assert!(!interrupts(&req(Some(0.10), true)), "a tool that merely always asks is not a destructive reading");
        assert!(interrupts(&req(None, true)), "armed and unscored: when in doubt, interrupt");
        assert!(!interrupts(&req(None, false)));
    }

    #[test]
    fn the_modal_shows_the_destructive_one_not_merely_the_oldest() {
        use std::collections::VecDeque;
        let queue: VecDeque<_> = [open("old-elevated", Some(0.5), false), open("new-destructive", Some(0.95), true)].into();
        let none = HashSet::new();
        assert_eq!(modal_id(&queue, &none, None), Some("new-destructive"), "y must answer what is on screen, not the older question nobody sees");
        let sent_away: HashSet<String> = ["new-destructive".to_owned()].into();
        assert_eq!(modal_id(&queue, &sent_away, None), None, "esc sent it to the tray; it does not pop back up");
        assert_eq!(modal_id(&queue, &sent_away, Some("old-elevated")), Some("old-elevated"), "review opens a tray item on demand");
        assert_eq!(modal_id(&queue, &none, Some("gone")), Some("new-destructive"), "a review of something already answered falls back");
    }

    #[test]
    fn the_window_title_says_what_is_wrong_first() {
        assert!(title_text(true, false, 0).contains("no daemon config"));
        assert!(title_text(true, true, 3).contains("no daemon config"), "a missing config outranks a live daemon");
        assert!(title_text(false, false, 1).contains("offline"));
        assert_eq!(title_text(false, true, 1), "sierralab · Lab · 1 session");
        assert_eq!(title_text(false, true, 2), "sierralab · Lab · 2 sessions");
    }
}
