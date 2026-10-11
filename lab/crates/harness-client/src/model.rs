//! The state the UI reads: a plain value, no async, no GPUI, no I/O.
//!
//! Everything the GUI knows about the daemon is `LabModel`. The client's background tasks never
//! touch it; they push [`ClientEvent`]s into the inbox, and the UI thread drains the inbox and
//! calls [`LabModel::apply`] — one thread mutates the model, so there is no lock to contend and
//! nothing the network can block. Being a plain value it is also what the tests assert against.
//!
//! The rules it keeps:
//! * **Offline keeps the last picture, flagged stale.** A dead daemon turns the UI into a read-only
//!   view of how things were, not a blank screen; `stale` says so and the approval buttons go dark.
//! * **Open interventions are never carried across a reconnect.** A new connection starts with
//!   none and the daemon replays what is still open; a stale one is marked, never approvable.
//! * **Secrets are masked on the way in** (`redact::scrub`), so no view has to remember to.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use harness_proto::v1::{
    Conversation, ConversationEvent, ConversationList, DaemonState, DiffArtifact, DiffRef, IngestSummary, InterventionEvent, InterventionRequest, KnobReply, LabState, MemoryEvent, MemoryHit, ModeReply, OmlxState, Outcome, PodControlReply,
    TelemetryFrame, TurnReply, UnloadReply, intervention_event,
};

use crate::redact;
use crate::words::{Tone, lab_changes};

const KEEP_RESOLVED: usize = 20;
const KEEP_MEMORY_EVENTS: usize = 20;
const KEEP_MEMORY_HITS: usize = 32;
const KEEP_NOTICES: usize = 20;
const KEEP_FEED: usize = 150;

#[derive(Debug, Clone)]
pub enum ConnectionState {
    Connecting,
    Online { since: Instant },
    /// The daemon is unreachable (or went away). `attempt` counts consecutive failures; the client
    /// will try again at `retry_at`.
    Offline { since: Instant, attempt: u32, retry_at: Instant, error: String },
}

impl ConnectionState {
    pub fn is_online(&self) -> bool {
        matches!(self, Self::Online { .. })
    }
}

/// The outcome of a command the UI sent.
#[derive(Debug, Clone)]
pub enum Reply {
    Knob { pod: String, knob: String, reply: KnobReply },
    Unload(UnloadReply),
    Diff(DiffArtifact),
    /// The ingest stream ended and the daemon summarised it.
    Ingest(IngestSummary),
    /// The session's permission mode changed (or was refused).
    Mode { mode: String, reply: ModeReply },
    /// An agent was stopped, released or steered (or the daemon refused).
    Pod { pod: String, what: &'static str, reply: PodControlReply },
    /// A turn the menu bar asked for: accepted (and running in the background), queued as guidance, or
    /// refused. What the person is told is the route and the notices — the reply never carries the
    /// agent's words, which arrive on the telemetry stream like any other turn's.
    Turn { reply: TurnReply },
    /// The hub's conversations, newest first.
    Conversations(ConversationList),
    Failed { what: String, error: String },
}

#[derive(Debug, Clone)]
pub enum ClientEvent {
    Connection(ConnectionState),
    Telemetry(Box<TelemetryFrame>),
    Intervention(InterventionEvent),
    Reply(Reply),
    /// A turn in the followed hub conversation: someone asked, a piece of the reply, the end.
    Conversation(ConversationEvent),
}

impl ClientEvent {
    /// Events the UI must never miss: connection changes, anything about an approval, and the start
    /// and end of a conversation's turn (the pieces of a reply in between may be dropped under load).
    pub fn is_critical(&self) -> bool {
        matches!(self, Self::Connection(_) | Self::Intervention(_) | Self::Reply(Reply::Failed { .. }))
            || matches!(self, Self::Conversation(event) if event.kind == "turn" || event.kind == "done")
    }
}

/// How many turns of the followed conversation are kept for drawing, and how much of each reply.
pub const KEEP_EXCHANGES: usize = 6;
pub const KEEP_REPLY_CHARS: usize = 12_000;

/// One turn of the followed conversation, as the deck draws it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Exchange {
    /// Which door asked: `terminal`, `discord`, `menubar`, `web`.
    pub origin: String,
    pub asked: String,
    pub reply: String,
    /// Empty while it runs; the stop reason once it ended.
    pub stop: String,
}

/// The followed conversation: which one, its recent turns, and what its agent is doing right now.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub handle: String,
    pub exchanges: VecDeque<Exchange>,
    pub activity: Option<String>,
}

impl Transcript {
    pub fn running(&self) -> bool {
        self.exchanges.back().is_some_and(|exchange| exchange.stop.is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct OpenIntervention {
    pub request: InterventionRequest,
    pub since: Instant,
    /// The connection it arrived on is gone. Shown, but it cannot be approved: the agent may have
    /// been resumed or restarted since, and a click must never answer a question it was not asked.
    pub stale: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedNote {
    pub intervention_id: String,
    pub outcome: Outcome,
    pub decided_by: String,
    pub at: Instant,
}

/// One line of "what just happened": an agent started something, was held for an answer, failed,
/// was stopped; you answered something; memory moved. Derived from telemetry and from the
/// replies to your own actions, in plain language (`words`), newest last.
#[derive(Debug, Clone)]
pub struct FeedItem {
    pub at: Instant,
    /// The agent it is about, when it is about one (the activity tab filters on this).
    pub pod: Option<String>,
    pub tone: Tone,
    pub text: String,
    /// The same line repeated back to back collapses into one with a count.
    pub count: u32,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

/// One retrieval an agent made, as the canvas draws it: the wire record plus when *this* client saw
/// it. The flight animation runs off `at`, so it is unaffected by any clock skew between the daemon
/// and the window; `hit.ts_ms` is what dedupes a replayed frame.
#[derive(Debug, Clone)]
pub struct MemoryHitNotice {
    pub hit: MemoryHit,
    pub at: Instant,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Frames applied to the model (after the inbox conflated some).
    pub frames: u64,
    /// Frames the daemon reports having folded into the ones it sent.
    pub dropped_by_daemon: u64,
    pub last_seq: u64,
    pub last_lab_seq: u64,
    pub last_frame_at: Option<Instant>,
    pub applies: u64,
    pub max_apply: Duration,
}

#[derive(Debug)]
pub struct LabModel {
    pub connection: ConnectionState,
    /// Newest Lab state per session.
    pub labs: BTreeMap<String, LabState>,
    pub omlx: Option<OmlxState>,
    pub daemon: Option<DaemonState>,
    pub diffs: Vec<DiffRef>,
    /// The daemon's own count of unresolved pauses (a cross-check on `interventions`).
    pub daemon_pending: usize,
    pub interventions: VecDeque<OpenIntervention>,
    pub resolved: VecDeque<ResolvedNote>,
    pub memory_events: VecDeque<MemoryEvent>,
    /// Recent store reads per agent, newest last — the canvas's memory orbitals.
    pub memory_hits: VecDeque<MemoryHitNotice>,
    /// The newest `ts_ms` enqueued per pod, so a re-sent or replayed frame does not re-animate.
    memory_seen: BTreeMap<String, i64>,
    pub notices: VecDeque<Notice>,
    pub feed: VecDeque<FeedItem>,
    pub stats: Stats,
    /// What is shown is the last picture, not the live one.
    pub stale: bool,
    /// The session the UI is looking at; the one that spoke last unless the user picked another.
    pub focus: Option<String>,
    /// The hub's conversations (newest first), and the one being followed.
    pub conversations: Vec<Conversation>,
    pub transcript: Transcript,
}

impl Default for LabModel {
    fn default() -> Self {
        Self {
            connection: ConnectionState::Connecting,
            labs: BTreeMap::new(),
            omlx: None,
            daemon: None,
            diffs: Vec::new(),
            daemon_pending: 0,
            interventions: VecDeque::new(),
            resolved: VecDeque::new(),
            memory_events: VecDeque::new(),
            memory_hits: VecDeque::new(),
            memory_seen: BTreeMap::new(),
            notices: VecDeque::new(),
            feed: VecDeque::new(),
            stats: Stats::default(),
            stale: false,
            focus: None,
            conversations: Vec::new(),
            transcript: Transcript::default(),
        }
    }
}

fn push_capped<T>(queue: &mut VecDeque<T>, item: T, cap: usize) {
    queue.push_back(item);
    while queue.len() > cap {
        queue.pop_front();
    }
}

/// An `InterventionRequest` with every credential-shaped span masked, whatever the daemon sent.
pub fn masked(mut request: InterventionRequest) -> InterventionRequest {
    let mut extra = 0usize;
    for field in [&mut request.command, &mut request.diff, &mut request.input_json, &mut request.reason] {
        let (clean, n) = redact::scrub(field);
        if n > 0 {
            *field = clean;
            extra += n;
        }
    }
    request.redactions += extra as u32;
    request
}

impl LabModel {
    /// The Lab of the session in focus (or the only one).
    pub fn lab(&self) -> Option<&LabState> {
        self.focus.as_ref().and_then(|id| self.labs.get(id)).or_else(|| self.labs.values().next_back())
    }

    pub fn open_interventions(&self) -> impl Iterator<Item = &OpenIntervention> {
        self.interventions.iter()
    }

    /// The oldest question that can still be answered.
    pub fn current_intervention(&self) -> Option<&OpenIntervention> {
        self.interventions.front()
    }

    /// Add a line to the activity feed (collapsing an immediate repeat).
    pub fn log(&mut self, pod: Option<String>, tone: Tone, text: impl Into<String>, now: Instant) {
        let text = text.into();
        if let Some(last) = self.feed.back_mut()
            && last.pod == pod
            && last.text == text
        {
            last.count += 1;
            last.at = now;
            return;
        }
        push_capped(&mut self.feed, FeedItem { at: now, pod, tone, text, count: 1 }, KEEP_FEED);
    }

    pub fn notice(&mut self, text: impl Into<String>, error: bool, now: Instant) {
        push_capped(&mut self.notices, Notice { text: text.into(), error, at: now }, KEEP_NOTICES);
    }

    pub fn apply(&mut self, event: ClientEvent, now: Instant) {
        let started = Instant::now();
        match event {
            ClientEvent::Connection(state) => self.apply_connection(state),
            ClientEvent::Telemetry(frame) => self.apply_frame(*frame, now),
            ClientEvent::Intervention(event) => self.apply_intervention(event, now),
            ClientEvent::Reply(reply) => self.apply_reply(reply, now),
            ClientEvent::Conversation(event) => self.apply_conversation(event),
        }
        self.stats.applies += 1;
        self.stats.max_apply = self.stats.max_apply.max(started.elapsed());
    }

    fn apply_connection(&mut self, state: ConnectionState) {
        let now = Instant::now();
        match (&self.connection, &state) {
            (ConnectionState::Online { .. }, ConnectionState::Offline { .. }) => self.log(None, Tone::Bad, "Lost the daemon — showing the last picture", now),
            (ConnectionState::Offline { .. }, ConnectionState::Online { .. }) => self.log(None, Tone::Good, "Reconnected to the daemon", now),
            _ => {}
        }
        match &state {
            ConnectionState::Offline { .. } => {
                self.stale = true;
                for open in &mut self.interventions {
                    open.stale = true;
                }
            }
            ConnectionState::Online { .. } => {
                self.stale = false;
                // A new connection: the daemon replays whatever is still open.
                self.interventions.clear();
            }
            ConnectionState::Connecting => {}
        }
        self.connection = state;
    }

    fn apply_frame(&mut self, frame: TelemetryFrame, now: Instant) {
        self.stats.frames += 1;
        self.stats.dropped_by_daemon += frame.dropped;
        self.stats.last_seq = frame.seq;
        self.stats.last_frame_at = Some(now);
        if let Some(lab) = frame.lab {
            if let Some(old) = self.labs.get(&frame.session_id) {
                for (pod, tone, text) in lab_changes(old, &lab) {
                    self.log(pod, tone, text, now);
                }
            }
            self.stats.last_lab_seq = lab.lab_seq;
            if self.focus.as_deref().is_none_or(str::is_empty) {
                self.focus = Some(frame.session_id.clone());
            }
            // Memory orbitals: enqueue only reads newer than the last one seen for that pod, so a
            // conflated frame re-sent, or the whole snapshot replayed on reconnect, does not make
            // the canvas replay an animation that already ran.
            for hit in &lab.memory {
                let newest = self.memory_seen.get(&hit.pod).copied().unwrap_or(i64::MIN);
                if hit.ts_ms <= newest {
                    continue;
                }
                self.memory_seen.insert(hit.pod.clone(), hit.ts_ms);
                let line = format!("Checked memory — {} hit{} from {}", hit.count, if hit.count == 1 { "" } else { "s" }, hit.source);
                self.log(Some(hit.pod.clone()), Tone::Info, line, now);
                push_capped(&mut self.memory_hits, MemoryHitNotice { hit: hit.clone(), at: now }, KEEP_MEMORY_HITS);
            }
            self.labs.insert(frame.session_id.clone(), lab);
        }
        if let Some(omlx) = frame.omlx {
            self.omlx = Some(omlx);
        }
        if let Some(daemon) = frame.daemon {
            self.daemon = Some(daemon);
        }
        if let Some(diffs) = frame.diffs {
            self.diffs = diffs.items;
        }
        if let Some(pending) = frame.pending {
            self.daemon_pending = pending.items.len();
        }
        if let Some(event) = frame.memory_event {
            let line = match event.event.as_str() {
                "unload_all" => Some("Local models unloaded to free memory".to_owned()),
                "heavy_task" => Some("Freed memory for a heavy task".to_owned()),
                "lease_load" => Some(format!("Loaded {} for a local task", event.model)),
                "lease_unload" => Some(format!("Unloaded {}", event.model)),
                _ => None,
            };
            if let Some(line) = line {
                self.log(None, Tone::Info, line, now);
            }
            push_capped(&mut self.memory_events, event, KEEP_MEMORY_EVENTS);
        }
    }

    fn apply_intervention(&mut self, event: InterventionEvent, now: Instant) {
        match event.event {
            Some(intervention_event::Event::Request(request)) => {
                let request = masked(request);
                if let Some(existing) = self.interventions.iter_mut().find(|o| o.request.intervention_id == request.intervention_id) {
                    existing.request = request;
                    existing.stale = false;
                } else {
                    self.interventions.push_back(OpenIntervention { request, since: now, stale: false });
                }
            }
            Some(intervention_event::Event::Resolved(resolved)) => {
                let outcome = Outcome::try_from(resolved.outcome).unwrap_or(Outcome::Unspecified);
                if let Some(open) = self.interventions.iter().find(|o| o.request.intervention_id == resolved.intervention_id) {
                    let who = if resolved.decided_by == "gui" { "You".to_owned() } else { resolved.decided_by.clone() };
                    let (verb, tone) = match outcome {
                        Outcome::Approved => ("approved", Tone::Good),
                        Outcome::Rejected => ("rejected", Tone::Warn),
                        _ => ("did not answer", Tone::Dim),
                    };
                    let pod = open.request.pod.clone();
                    let text = format!("{who} {verb} {} for {pod}", open.request.tool);
                    self.log(Some(pod), tone, text, now);
                }
                self.interventions.retain(|o| o.request.intervention_id != resolved.intervention_id);
                push_capped(
                    &mut self.resolved,
                    ResolvedNote { intervention_id: resolved.intervention_id, outcome, decided_by: resolved.decided_by, at: now },
                    KEEP_RESOLVED,
                );
            }
            Some(intervention_event::Event::Heartbeat(_)) | None => {}
        }
    }

    /// Follow a conversation (or none): the transcript starts over, because what it held was another
    /// conversation's.
    pub fn follow(&mut self, handle: Option<&str>) {
        let handle = handle.unwrap_or_default();
        if self.transcript.handle != handle {
            self.transcript = Transcript { handle: handle.to_owned(), ..Transcript::default() };
        }
    }

    fn apply_conversation(&mut self, event: ConversationEvent) {
        if event.handle != self.transcript.handle {
            return; // a late event from the conversation followed before
        }
        let transcript = &mut self.transcript;
        match event.kind.as_str() {
            "history" => {
                // A turn from before this deck started following — replayed by the daemon, finished.
                let exchange = Exchange { origin: event.origin, asked: redact::scrub(&event.text).0, reply: redact::scrub(&event.delta).0, stop: if event.stop.is_empty() { "end_turn".into() } else { event.stop } };
                push_capped(&mut transcript.exchanges, exchange, KEEP_EXCHANGES);
            }
            "turn" => {
                let (asked, _) = redact::scrub(&event.text);
                push_capped(&mut transcript.exchanges, Exchange { origin: event.origin, asked, ..Exchange::default() }, KEEP_EXCHANGES);
                transcript.activity = None;
            }
            "text" => {
                if let Some(exchange) = transcript.exchanges.back_mut().filter(|exchange| exchange.stop.is_empty()) {
                    if exchange.reply.len() < KEEP_REPLY_CHARS {
                        exchange.reply.push_str(&event.delta);
                    }
                } else {
                    // A reply already under way when following began: its question was missed.
                    push_capped(&mut transcript.exchanges, Exchange { origin: event.origin, reply: event.delta, ..Exchange::default() }, KEEP_EXCHANGES);
                }
            }
            "activity" => transcript.activity = (!event.label.is_empty()).then_some(event.label),
            "done" => {
                if let Some(exchange) = transcript.exchanges.back_mut() {
                    exchange.stop = if event.stop.is_empty() { "end_turn".into() } else { event.stop };
                    exchange.reply = redact::scrub(&exchange.reply).0;
                }
                transcript.activity = None;
            }
            _ => {}
        }
    }

    fn apply_reply(&mut self, reply: Reply, now: Instant) {
        match reply {
            Reply::Knob { pod, knob, reply } => {
                if reply.ok {
                    self.notice(format!("{pod}: {knob} applied"), false, now);
                } else {
                    self.notice(format!("{pod}: {knob} refused — {}", reply.error), true, now);
                }
            }
            Reply::Unload(reply) => {
                if reply.ok {
                    let what = if reply.unloaded.is_empty() { "nothing was loaded".to_owned() } else { reply.unloaded.join(", ") };
                    self.notice(format!("memory unloaded: {what} ({:.1} GB still held)", reply.loaded_gb_after), false, now);
                } else {
                    self.notice(format!("unload incomplete: {}", reply.errors.join("; ")), true, now);
                }
            }
            Reply::Diff(artifact) => {
                let lines = artifact.unified.lines().count();
                self.notice(format!("diff {} loaded ({lines} lines)", artifact.r#ref.map(|r| r.id).unwrap_or_default()), false, now);
            }
            Reply::Mode { mode, reply } => {
                if reply.ok {
                    self.log(None, if mode == "auto" { Tone::Warn } else { Tone::Info }, format!("You set the mode to {mode}"), now);
                } else {
                    self.notice(format!("mode not changed — {}", reply.error), true, now);
                    self.log(None, Tone::Warn, format!("Mode not changed: {}", reply.error), now);
                }
            }
            Reply::Pod { pod, what, reply } => {
                if reply.ok {
                    let text = match what {
                        "stop" if reply.affected.len() > 1 => format!("You stopped {pod} and {} it started", reply.affected.len() - 1),
                        "stop" => format!("You stopped {pod}"),
                        "release" => format!("You released {pod}"),
                        _ => format!("You sent {pod} a directive ({} waiting for its next step)", reply.queued.max(1)),
                    };
                    self.log(Some(pod), if what == "stop" { Tone::Warn } else { Tone::Good }, text, now);
                } else {
                    self.notice(format!("{pod}: {what} refused — {}", reply.error), true, now);
                    self.log(Some(pod.clone()), Tone::Warn, format!("{pod}: {what} refused — {}", reply.error), now);
                }
            }
            Reply::Conversations(list) => {
                self.conversations = list.conversations;
            }
            Reply::Turn { reply } => {
                if reply.ok {
                    let mut what = match reply.route.as_str() {
                        "steer" => "You sent guidance to the running agent".to_owned(),
                        "new" => "You started a new conversation".to_owned(),
                        _ => "Your turn is running".to_owned(),
                    };
                    if !reply.session_id.is_empty() {
                        what = format!("{what} ({} · {})", reply.session_id.chars().take(8).collect::<String>(), reply.turn_id);
                    }
                    self.log(None, Tone::Good, what, now);
                    // The notices are the honest part: that it was guidance and not a turn, that the
                    // screenshot was not attached. Shown as notices, not buried in the feed.
                    for notice in reply.notices {
                        self.notice(notice, false, now);
                    }
                } else {
                    self.notice(format!("turn not started — {}", reply.error), true, now);
                    self.log(None, Tone::Warn, format!("Turn not started: {}", reply.error), now);
                }
            }
            Reply::Ingest(summary) => {
                self.notice(format!("ingest closed: {} accepted, {} duplicate, {} rejected", summary.accepted, summary.deduplicated, summary.rejected), false, now);
            }
            Reply::Failed { what, error } => self.notice(format!("{what}: {error}"), true, now),
        }
    }
}
