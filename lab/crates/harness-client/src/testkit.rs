//! A fake daemon for tests: the real `Harness` service over a real loopback socket, scripted.
//!
//! It runs in its **own tokio runtime on its own thread**, so `stop()` can kill it the way a crash
//! does — every connection dropped mid-stream, the port closed — instead of the polite drain a
//! graceful shutdown gives. `start_on` brings a new one up on the same port, which is what the
//! reconnect tests need. The Python bridge (`backend/daemon/grpc_bridge.py`) has its own tests;
//! this exists so the client can be tested without a Python process.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::Stream;
use harness_proto::TOKEN_KEY;
use harness_proto::v1::harness_server::{Harness, HarnessServer};
use harness_proto::v1::*;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{BroadcastStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

pub struct FakeState {
    token: String,
    frames: broadcast::Sender<TelemetryFrame>,
    events: broadcast::Sender<InterventionEvent>,
    first: Mutex<TelemetryFrame>,
    open: Mutex<Vec<InterventionRequest>>,
    diff: Mutex<Option<DiffArtifact>>,
    pub decisions: Mutex<Vec<InterventionDecision>>,
    pub knobs: Mutex<Vec<KnobRequest>>,
    pub modes: Mutex<Vec<ModeRequest>>,
    pub controls: Mutex<Vec<PodControlRequest>>,
    pub unloads: Mutex<Vec<UnloadRequest>>,
    pub ingested: Mutex<Vec<AstSnippet>>,
    /// Turns the menu bar submitted, in order — the test asserts what actually crossed the wire.
    pub turns: Mutex<Vec<TurnRequest>>,
    /// The hub's conversations as `ListConversations` answers, and every followed conversation's
    /// events (a test publishes; a `WatchConversation` stream relays those for its handle).
    pub conversations: Mutex<Vec<Conversation>>,
    conversation_events: broadcast::Sender<ConversationEvent>,
    pub heartbeat: Duration,
    /// When set, the telemetry stream stays open but sends nothing after the first frame, and
    /// heartbeats stop — a daemon that is wedged rather than gone.
    pub mute: std::sync::atomic::AtomicBool,
}

impl FakeState {
    /// Publish one event on a conversation, as if a door had started a turn there.
    pub fn conversation_event(&self, event: ConversationEvent) {
        let _ = self.conversation_events.send(event);
    }

    /// The telemetry sender, for a test that wants to publish from its own thread.
    pub fn frames(&self) -> &broadcast::Sender<TelemetryFrame> {
        &self.frames
    }

    fn authorise<T>(&self, request: &Request<T>) -> Result<(), Status> {
        match request.metadata().get(TOKEN_KEY).and_then(|v| v.to_str().ok()) {
            Some(token) if token == self.token => Ok(()),
            _ => Err(Status::unauthenticated("bad or missing daemon token")),
        }
    }
}

struct Service(Arc<FakeState>);

#[tonic::async_trait]
impl Harness for Service {
    type StreamTelemetryStream = BoxStream<TelemetryFrame>;
    type InterventionStreamStream = BoxStream<InterventionEvent>;
    type WatchConversationStream = BoxStream<ConversationEvent>;

    async fn list_conversations(&self, request: Request<ConversationsRequest>) -> Result<Response<ConversationList>, Status> {
        self.0.authorise(&request)?;
        Ok(Response::new(ConversationList { conversations: self.0.conversations.lock().unwrap().clone() }))
    }

    async fn open_conversation(&self, request: Request<ConversationOpen>) -> Result<Response<Conversation>, Status> {
        self.0.authorise(&request)?;
        let asked = request.into_inner();
        let mut conversations = self.0.conversations.lock().unwrap();
        if let Some(found) = conversations.iter().find(|c| c.handle == asked.conversation) {
            return Ok(Response::new(found.clone()));
        }
        let opened = Conversation { handle: format!("c{:06x}", conversations.len() + 1), title: asked.title, origin: asked.origin, ..Default::default() };
        conversations.insert(0, opened.clone());
        Ok(Response::new(opened))
    }

    async fn watch_conversation(&self, request: Request<ConversationRef>) -> Result<Response<Self::WatchConversationStream>, Status> {
        self.0.authorise(&request)?;
        let handle = request.into_inner().handle;
        if !self.0.conversations.lock().unwrap().iter().any(|c| c.handle == handle) {
            return Err(Status::not_found(format!("no conversation named {handle:?}")));
        }
        let first = ConversationEvent { handle: handle.clone(), kind: "watching".into(), ..Default::default() };
        let live = BroadcastStream::new(self.0.conversation_events.subscribe()).filter_map(move |item| match item {
            Ok(event) if event.handle == handle => Some(Ok(event)),
            _ => None,
        });
        Ok(Response::new(Box::pin(tokio_stream::once(Ok(first)).chain(live))))
    }

    async fn stream_telemetry(&self, request: Request<TelemetryRequest>) -> Result<Response<Self::StreamTelemetryStream>, Status> {
        self.0.authorise(&request)?;
        let first = self.0.first.lock().unwrap().clone();
        let state = self.0.clone();
        let live = BroadcastStream::new(self.0.frames.subscribe()).filter_map(move |item| match item {
            Ok(frame) if !state.mute.load(std::sync::atomic::Ordering::Relaxed) => Some(Ok(frame)),
            _ => None, // lagged (the burst outran us) or muted: skip, like a latest-wins daemon
        });
        let stream = tokio_stream::once(Ok(first)).chain(live);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn intervention_stream(&self, request: Request<Streaming<InterventionReply>>) -> Result<Response<Self::InterventionStreamStream>, Status> {
        self.0.authorise(&request)?;
        let mut inbound = request.into_inner();
        let state = self.0.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<InterventionEvent, Status>>(64);
        // Replay what is still open, then forward live events and heartbeats.
        for open in self.0.open.lock().unwrap().iter().cloned() {
            let _ = tx.try_send(Ok(InterventionEvent { event: Some(intervention_event::Event::Request(open)) }));
        }
        let mut live = self.0.events.subscribe();
        let sender = tx.clone();
        let heartbeat = self.0.heartbeat;
        let state_for_events = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(heartbeat);
            loop {
                tokio::select! {
                    event = live.recv() => match event {
                        Ok(event) => if sender.send(Ok(event)).await.is_err() { return },
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return,
                    },
                    _ = tick.tick() => {
                        if state_for_events.mute.load(std::sync::atomic::Ordering::Relaxed) { continue; }
                        let beat = InterventionEvent { event: Some(intervention_event::Event::Heartbeat(Heartbeat { ts_ms: 0 })) };
                        if sender.send(Ok(beat)).await.is_err() { return }
                    }
                }
            }
        });
        tokio::spawn(async move {
            while let Ok(Some(reply)) = inbound.message().await {
                if let Some(intervention_reply::Reply::Decision(decision)) = reply.reply {
                    state.decisions.lock().unwrap().push(decision.clone());
                    let was_open = {
                        let mut open = state.open.lock().unwrap();
                        let before = open.len();
                        open.retain(|r| r.intervention_id != decision.intervention_id);
                        open.len() != before
                    };
                    let resolved = InterventionResolved {
                        intervention_id: decision.intervention_id.clone(),
                        outcome: if !was_open { Outcome::Aborted } else if decision.approve { Outcome::Approved } else { Outcome::Rejected } as i32,
                        decided_by: if was_open { "gui".into() } else { "stale".into() },
                    };
                    let _ = state.events.send(InterventionEvent { event: Some(intervention_event::Event::Resolved(resolved)) });
                }
            }
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))))
    }

    async fn proactive_ingest(&self, request: Request<Streaming<AstSnippet>>) -> Result<Response<IngestSummary>, Status> {
        self.0.authorise(&request)?;
        let mut inbound = request.into_inner();
        let mut summary = IngestSummary::default();
        while let Some(snippet) = inbound.message().await? {
            summary.received += 1;
            summary.accepted += 1;
            self.0.ingested.lock().unwrap().push(snippet);
        }
        Ok(Response::new(summary))
    }

    async fn set_knob(&self, request: Request<KnobRequest>) -> Result<Response<KnobReply>, Status> {
        self.0.authorise(&request)?;
        let knob = request.into_inner();
        self.0.knobs.lock().unwrap().push(knob);
        Ok(Response::new(KnobReply { ok: true, applied: "{}".into(), error: String::new() }))
    }

    async fn set_mode(&self, request: Request<ModeRequest>) -> Result<Response<ModeReply>, Status> {
        self.0.authorise(&request)?;
        let asked = request.into_inner();
        self.0.modes.lock().unwrap().push(asked.clone());
        let ok = ["plan", "build", "auto"].contains(&asked.mode.as_str());
        Ok(Response::new(ModeReply { ok, mode: if ok { asked.mode } else { String::new() }, error: if ok { String::new() } else { "mode must be one of plan, build, auto".into() } }))
    }

    async fn control_pod(&self, request: Request<PodControlRequest>) -> Result<Response<PodControlReply>, Status> {
        self.0.authorise(&request)?;
        let asked = request.into_inner();
        self.0.controls.lock().unwrap().push(asked.clone());
        let steer_empty = asked.action == PodAction::Steer as i32 && asked.text.trim().is_empty();
        Ok(Response::new(PodControlReply {
            ok: !steer_empty,
            error: if steer_empty { "a directive needs some text".into() } else { String::new() },
            affected: vec![asked.pod.clone()],
            queued: u32::from(asked.action == PodAction::Steer as i32),
        }))
    }

    async fn unload_memory(&self, request: Request<UnloadRequest>) -> Result<Response<UnloadReply>, Status> {
        self.0.authorise(&request)?;
        self.0.unloads.lock().unwrap().push(request.into_inner());
        Ok(Response::new(UnloadReply { ok: true, unloaded: vec!["Ternary-Bonsai-27B-mlx-2bit".into()], loaded_gb_after: 0.0, errors: vec![] }))
    }

    async fn get_diff(&self, request: Request<DiffRequest>) -> Result<Response<DiffArtifact>, Status> {
        self.0.authorise(&request)?;
        self.0.diff.lock().unwrap().clone().map(Response::new).ok_or_else(|| Status::not_found("no such diff"))
    }

    async fn ping(&self, request: Request<PingRequest>) -> Result<Response<PingReply>, Status> {
        self.0.authorise(&request)?;
        Ok(Response::new(PingReply { version: "1".into(), ts_ms: 0, daemon: "fake".into() }))
    }

    async fn submit_turn(&self, request: Request<TurnRequest>) -> Result<Response<TurnReply>, Status> {
        self.0.authorise(&request)?;
        let asked = request.into_inner();
        // Scripted like the real daemon's own refusals, so the client's handling of both is exercised:
        // a never-read app, and an empty turn. The real refusal text lives in `os_context.py`.
        if asked.context.as_ref().is_some_and(|c| c.app_bundle_id == "com.1password.1password") {
            return Ok(Response::new(TurnReply { ok: false, error: "this app is never read".into(), ..Default::default() }));
        }
        if asked.text.trim().is_empty() {
            return Ok(Response::new(TurnReply { ok: false, error: "a turn needs some text".into(), ..Default::default() }));
        }
        let mut notices = Vec::new();
        if asked.context.as_ref().is_some_and(|c| !c.screenshot.is_empty()) {
            notices.push("The screenshot was captured and recorded, but it was not attached — the harness has no way to send an image yet.".into());
        }
        let route = if asked.session_id.is_empty() { "new" } else { "turn" };
        self.0.turns.lock().unwrap().push(asked.clone());
        let conversation = if asked.conversation == "new" { "cnew001".to_owned() } else { asked.conversation.clone() };
        Ok(Response::new(TurnReply { ok: true, error: String::new(), session_id: asked.session_id, turn_id: "t-1".into(), notices, route: route.into(), conversation }))
    }
}
pub struct FakeDaemon {
    pub addr: SocketAddr,
    pub token: String,
    pub state: Arc<FakeState>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl FakeDaemon {
    pub fn start(token: &str) -> Self {
        Self::start_on("127.0.0.1:0".parse().unwrap(), token)
    }

    /// Bind `addr` (use the previous daemon's address to simulate a restart).
    pub fn start_on(addr: SocketAddr, token: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("fake-daemon").build().unwrap();
        let (frames, _) = broadcast::channel(16_384);
        let (events, _) = broadcast::channel(256);
        let (conversation_events, _) = broadcast::channel(1024);
        let state = Arc::new(FakeState {
            token: token.to_owned(),
            frames,
            events,
            first: Mutex::new(Self::whole_frame()),
            open: Mutex::new(Vec::new()),
            diff: Mutex::new(None),
            decisions: Mutex::default(),
            knobs: Mutex::default(),
            modes: Mutex::default(),
            controls: Mutex::default(),
            unloads: Mutex::default(),
            ingested: Mutex::default(),
            turns: Mutex::default(),
            conversations: Mutex::default(),
            conversation_events,
            heartbeat: Duration::from_millis(200),
            mute: Default::default(),
        });
        let listener = runtime.block_on(async {
            // A just-closed port can linger for a moment; a restart test retries briefly.
            let mut attempts = 0;
            loop {
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => return listener,
                    Err(error) if attempts < 50 => {
                        attempts += 1;
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        let _ = error;
                    }
                    Err(error) => panic!("fake daemon could not bind {addr}: {error}"),
                }
            }
        });
        let bound = listener.local_addr().unwrap();
        let service = HarnessServer::new(Service(state.clone())).max_decoding_message_size(16 << 20).max_encoding_message_size(16 << 20);
        runtime.spawn(async move {
            let _ = tonic::transport::Server::builder().add_service(service).serve_with_incoming(TcpListenerStream::new(listener)).await;
        });
        Self { addr: bound, token: token.to_owned(), state, runtime: Some(runtime) }
    }

    fn whole_frame() -> TelemetryFrame {
        TelemetryFrame {
            seq: 1,
            session_id: "s1".into(),
            lab: Some(LabState {
                lab_seq: 1,
                default_dial: 0.8,
                pods: vec![Pod { id: "orchestrator".into(), name: "orchestrator".into(), role: "orchestrator".into(), status: PodStatus::Idle as i32, dial: 0.8, dial_default: 0.8, effort_adjustable: true, ..Default::default() }],
                ..Default::default()
            }),
            daemon: Some(DaemonState { version: "1".into(), sessions: 1, alive: 1, ..Default::default() }),
            omlx: Some(OmlxState { up: true, budget_gb: 38.0, free_gb: 38.0, ..Default::default() }),
            diffs: Some(DiffSet::default()),
            pending: Some(PendingSet::default()),
            ..Default::default()
        }
    }

    pub fn config(&self) -> crate::DaemonConfig {
        crate::DaemonConfig::new(self.addr.to_string(), self.token.clone())
    }

    /// Publish one telemetry frame to everyone connected.
    pub fn push_frame(&self, frame: TelemetryFrame) {
        let _ = self.state.frames.send(frame);
    }

    /// A Lab-state frame for `session` whose snapshot counter is `lab_seq`.
    pub fn lab_frame(session: &str, seq: u64, lab_seq: u64, status: PodStatus) -> TelemetryFrame {
        TelemetryFrame {
            seq,
            session_id: session.into(),
            lab: Some(LabState {
                lab_seq,
                default_dial: 0.8,
                pods: vec![Pod { id: "orchestrator".into(), name: "orchestrator".into(), role: "orchestrator".into(), status: status as i32, calls: lab_seq as u32, ..Default::default() }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Open an intervention: connected clients are shown it, and it is replayed to late joiners.
    pub fn request_intervention(&self, request: InterventionRequest) {
        self.state.open.lock().unwrap().push(request.clone());
        let _ = self.state.events.send(InterventionEvent { event: Some(intervention_event::Event::Request(request)) });
    }

    pub fn set_diff(&self, artifact: DiffArtifact) {
        *self.state.diff.lock().unwrap() = Some(artifact);
    }

    /// Kill it like a crash: every connection drops, the port closes.
    pub fn crash(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.crash();
    }
}
