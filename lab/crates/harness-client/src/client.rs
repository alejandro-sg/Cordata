//! The gRPC client: connects, streams, and — above all — survives the daemon not being there.
//!
//! One supervisor task owns the connection lifecycle:
//!
//! ```text
//!   Connecting ──ping ok──▶ Online ──stream ends / errors / goes silent──▶ Offline{attempt, retry_at}
//!        ▲                                                                        │
//!        └──────────────────── backoff (jittered, capped) ◀───────────────────────┘
//! ```
//!
//! Every way a daemon can disappear ends the same way: a refused connect, a reset, a clean close
//! (the daemon restarted), a stream that errors, a connection that stays open but says nothing for
//! `stale_after` (the daemon's own heartbeat is every 5 s). The UI is told `Offline` with the
//! reason and when the next attempt is; the last picture stays on screen, flagged stale; the
//! moment a `Ping` is answered again it is `Online`, with a fresh first frame that makes the
//! picture whole. No path panics, and none blocks the UI thread: the client talks to the UI only
//! through the [`Inbox`] and bounded command channels with `try_send`.
//!
//! Approvals are never queued across an outage. A decision made while the connection is down is
//! *refused loudly* ("the agent is still waiting; nothing was sent") rather than held and replayed
//! into whatever the daemon looks like when it returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use harness_proto::TOKEN_KEY;
use harness_proto::v1::harness_client::HarnessClient;
use harness_proto::v1::{
    AstSnippet, DiffRequest, InterventionDecision, InterventionHello, InterventionReply, KnobRequest, ModeRequest, PingRequest, PodAction, PodControlRequest, TelemetryRequest, UnloadRequest,
    intervention_reply, knob_request,
};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

use crate::config::DaemonConfig;
use crate::inbox::Inbox;
use crate::model::{ClientEvent, ConnectionState, Reply};

/// The schema this client speaks (`PingReply.version`).
pub const SCHEMA_VERSION: &str = "1";
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
const COMMAND_QUEUE: usize = 256;
const INGEST_QUEUE: usize = 1024;
const UNARY_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the daemon's address and token come from, asked again on *every* connection attempt.
///
/// A window opened before the daemon exists, or across a daemon restart that moved its port or
/// rotated its token, must find the new one by itself: reading the file once at startup would
/// leave it retrying a dead address forever, which is "offline" with no way back.
#[derive(Clone)]
pub struct ConfigSource(Arc<dyn Fn() -> Result<DaemonConfig, String> + Send + Sync>);

impl ConfigSource {
    pub fn new(source: impl Fn() -> Result<DaemonConfig, String> + Send + Sync + 'static) -> Self {
        Self(Arc::new(source))
    }

    /// `~/.sierralab/daemon.json` (or `SIERRALAB_HOME` / `SIERRALAB_DAEMON_CONFIG`), re-read each time.
    pub fn discover() -> Self {
        Self::new(|| DaemonConfig::discover().map_err(|error| error.to_string()))
    }

    fn get(&self) -> Result<DaemonConfig, String> {
        (self.0)()
    }
}

impl std::fmt::Debug for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfigSource(..)")
    }
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Used when there is no `source`.
    pub daemon: DaemonConfig,
    /// When set, asked before every attempt; `daemon` is then only a placeholder.
    pub source: Option<ConfigSource>,
    pub connect_timeout: Duration,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// A connection that delivers nothing for this long is treated as dead.
    pub stale_after: Duration,
    pub name: String,
}

impl ClientConfig {
    /// A client that finds the daemon itself, and finds it again after it moves.
    pub fn discovering() -> Self {
        Self { source: Some(ConfigSource::discover()), ..Self::new(DaemonConfig::new("", "")) }
    }

    pub fn new(daemon: DaemonConfig) -> Self {
        Self {
            daemon,
            source: None,
            connect_timeout: Duration::from_secs(2),
            backoff_base: Duration::from_millis(250),
            backoff_max: Duration::from_secs(5),
            stale_after: Duration::from_secs(20),
            name: "harness-lab".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Knob {
    ReasoningEffort(String),
    LayaDial(f64),
}

/// What a person can do to one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodControl {
    /// E-Stop: the pod and everything it spawned stop and refuse new calls.
    Isolate,
    Release,
    /// Queue a directive for the pod's next model call.
    Steer(String),
}

impl PodControl {
    pub fn label(&self) -> &'static str {
        match self {
            PodControl::Isolate => "stop",
            PodControl::Release => "release",
            PodControl::Steer(_) => "steer",
        }
    }
}

/// What the UI can ask for. Every one is non-blocking to send and answers with a `Reply` event.
#[derive(Debug, Clone)]
pub enum Command {
    /// `[Approve & Execute]` (`approve`) or `[Reject]` / `[Reject & Isolate]` (`!approve`, `isolate`).
    Decide { intervention_id: String, approve: bool, isolate: bool, note: String },
    SetKnob { session_id: String, pod: String, knob: Knob },
    UnloadMemory { reason: String, include_ollama: bool },
    FetchDiff { id: String },
    /// `plan` | `build` | `auto`.
    SetMode { session_id: String, mode: String },
    /// Stop, release or steer one agent.
    ControlPod { session_id: String, pod: String, control: PodControl },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SendError {
    #[error("the client is busy (command queue full)")]
    Busy,
    #[error("the client has shut down")]
    Closed,
}

#[derive(Default)]
struct Counters {
    reconnects: AtomicU64,
    ingest_dropped: AtomicU64,
    commands_failed: AtomicU64,
}

/// The UI's handle on the client. Cheap to hold; dropping it shuts the client down.
pub struct Handle {
    inbox: Inbox,
    commands: mpsc::Sender<Command>,
    ingest: mpsc::Sender<AstSnippet>,
    counters: Arc<Counters>,
    shutdown: watch::Sender<bool>,
}

impl Handle {
    pub fn inbox(&self) -> &Inbox {
        &self.inbox
    }

    /// Queue a command. Never blocks; `Busy` if 256 are already waiting.
    pub fn send(&self, command: Command) -> Result<(), SendError> {
        self.commands.try_send(command).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => SendError::Busy,
            mpsc::error::TrySendError::Closed(_) => SendError::Closed,
        })
    }

    /// Hand a snippet to the ingest stream. Never blocks the UI thread: when the queue is full
    /// (the daemon is slow, or offline long enough to fill it) the snippet is dropped and counted.
    pub fn ingest(&self, snippet: AstSnippet) -> bool {
        match self.ingest.try_send(snippet) {
            Ok(()) => true,
            Err(_) => {
                self.counters.ingest_dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    pub fn ingest_dropped(&self) -> u64 {
        self.counters.ingest_dropped.load(Ordering::Relaxed)
    }

    /// How many times the client has had to reconnect (not counting the first connect).
    pub fn reconnects(&self) -> u64 {
        self.counters.reconnects.load(Ordering::Relaxed)
    }

    pub fn commands_failed(&self) -> u64 {
        self.counters.commands_failed.load(Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
struct TokenInterceptor(MetadataValue<Ascii>);

impl Interceptor for TokenInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request.metadata_mut().insert(TOKEN_KEY, self.0.clone());
        Ok(request)
    }
}

type Client = HarnessClient<InterceptedService<Channel, TokenInterceptor>>;

/// Start the client on `runtime`. Returns immediately; the first event is `Connecting`.
pub fn spawn(config: ClientConfig, runtime: &tokio::runtime::Handle) -> Handle {
    let inbox = Inbox::new();
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE);
    let (ingest_tx, ingest_rx) = mpsc::channel(INGEST_QUEUE);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let counters = Arc::new(Counters::default());
    runtime.spawn(supervise(config, inbox.clone(), cmd_rx, ingest_rx, shutdown_rx, counters.clone()));
    Handle { inbox, commands: cmd_tx, ingest: ingest_tx, counters, shutdown: shutdown_tx }
}

/// `base · 2^(attempt-1)`, capped, ±20% so a fleet of clients does not retry in lockstep.
fn backoff(config: &ClientConfig, attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(16);
    let raw = config.backoff_base.saturating_mul(1u32 << exponent).min(config.backoff_max);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let jitter = 0.8 + (nanos % 400) as f64 / 1000.0; // 0.8 ..= 1.2
    raw.mul_f64(jitter)
}

struct Ended {
    was_online: bool,
    reason: String,
}

async fn supervise(
    config: ClientConfig,
    inbox: Inbox,
    mut commands: mpsc::Receiver<Command>,
    mut ingest: mpsc::Receiver<AstSnippet>,
    mut shutdown: watch::Receiver<bool>,
    counters: Arc<Counters>,
) {
    inbox.push_event(ClientEvent::Connection(ConnectionState::Connecting));
    let mut attempt: u32 = 0;
    let mut outage_since: Option<Instant> = None;
    let mut ever_online = false;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let ended = tokio::select! {
            ended = run_connection(&config, &inbox, &mut commands, &mut ingest, &counters, &mut ever_online) => ended,
            _ = shutdown.changed() => return,
        };
        // Whatever the old connection had in flight is not the new one's.
        inbox.clear_telemetry();
        if ended.was_online {
            attempt = 0;
            outage_since = None;
        }
        attempt += 1;
        let since = *outage_since.get_or_insert_with(Instant::now);
        let delay = backoff(&config, attempt);
        let retry_at = Instant::now() + delay;
        inbox.push_event(ClientEvent::Connection(ConnectionState::Offline { since, attempt, retry_at, error: ended.reason }));
        // Wait out the backoff, refusing any command that arrives in the meantime.
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                command = commands.recv() => match command {
                    Some(command) => refuse_offline(&inbox, &counters, command),
                    None => return,
                },
                _ = shutdown.changed() => return,
            }
        }
    }
}

fn refuse_offline(inbox: &Inbox, counters: &Counters, command: Command) {
    counters.commands_failed.fetch_add(1, Ordering::Relaxed);
    let (what, error) = match command {
        Command::Decide { .. } => ("decision", "the daemon is offline: the agent is still waiting and nothing was sent — answer when it is back"),
        Command::SetKnob { .. } => ("knob", "the daemon is offline"),
        Command::UnloadMemory { .. } => ("unload memory", "the daemon is offline; nothing was unloaded"),
        Command::FetchDiff { .. } => ("diff", "the daemon is offline"),
        Command::SetMode { .. } => ("mode", "the daemon is offline; the mode was not changed"),
        Command::ControlPod { control, .. } => (control.label(), "the daemon is offline; nothing was sent to the agent"),
    };
    inbox.push_event(ClientEvent::Reply(Reply::Failed { what: what.into(), error: error.into() }));
}

async fn connect(config: &ClientConfig, daemon: &DaemonConfig) -> Result<Client, String> {
    let token: MetadataValue<Ascii> = daemon.token.parse().map_err(|_| "the daemon token is not valid header text".to_owned())?;
    let endpoint = Endpoint::from_shared(daemon.uri())
        .map_err(|e| format!("bad daemon address {}: {e}", daemon.addr))?
        .connect_timeout(config.connect_timeout)
        .tcp_nodelay(true)
        .http2_keep_alive_interval(Duration::from_secs(10))
        .keep_alive_timeout(Duration::from_secs(5))
        .keep_alive_while_idle(true);
    let channel = endpoint.connect().await.map_err(|e| format!("cannot reach the daemon at {} ({})", daemon.addr, describe(&e)))?;
    Ok(HarnessClient::with_interceptor(channel, TokenInterceptor(token)).max_decoding_message_size(MAX_MESSAGE).max_encoding_message_size(MAX_MESSAGE))
}

fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        let part = inner.to_string();
        if !text.contains(&part) {
            text = format!("{text}: {part}");
        }
        source = inner.source();
    }
    text
}

async fn run_connection(
    config: &ClientConfig,
    inbox: &Inbox,
    commands: &mut mpsc::Receiver<Command>,
    ingest: &mut mpsc::Receiver<AstSnippet>,
    counters: &Counters,
    ever_online: &mut bool,
) -> Ended {
    let daemon = match &config.source {
        Some(source) => match source.get() {
            Ok(daemon) => daemon,
            Err(reason) => return Ended { was_online: false, reason: format!("no daemon to connect to yet: {reason}") },
        },
        None => config.daemon.clone(),
    };
    let mut client = match connect(config, &daemon).await {
        Ok(client) => client,
        Err(reason) => return Ended { was_online: false, reason },
    };
    match timeout(config.connect_timeout, client.ping(PingRequest {})).await {
        Ok(Ok(reply)) if reply.get_ref().version == SCHEMA_VERSION => {}
        Ok(Ok(reply)) => {
            return Ended { was_online: false, reason: format!("the daemon speaks schema v{}, this client speaks v{SCHEMA_VERSION}", reply.get_ref().version) };
        }
        Ok(Err(status)) => return Ended { was_online: false, reason: format!("the daemon refused the handshake: {}", status.message()) },
        Err(_) => return Ended { was_online: false, reason: "the daemon accepted the connection but did not answer Ping".into() },
    }
    // Commands that piled up while we were connecting were typed against a picture that is gone.
    while let Ok(command) = commands.try_recv() {
        refuse_offline(inbox, counters, command);
    }
    if std::mem::replace(ever_online, true) {
        counters.reconnects.fetch_add(1, Ordering::Relaxed);
    }
    inbox.push_event(ClientEvent::Connection(ConnectionState::Online { since: Instant::now() }));

    let (decide_tx, decide_rx) = mpsc::channel::<InterventionReply>(64);
    let reason = tokio::select! {
        reason = telemetry_loop(client.clone(), inbox, config.stale_after) => reason,
        reason = intervention_loop(client.clone(), inbox, decide_rx, config) => reason,
        reason = command_loop(client.clone(), inbox, commands, decide_tx, counters) => reason,
        reason = ingest_loop(client.clone(), inbox, ingest) => reason,
    };
    Ended { was_online: true, reason }
}

async fn telemetry_loop(mut client: Client, inbox: &Inbox, stale_after: Duration) -> String {
    let mut stream = match client.stream_telemetry(TelemetryRequest::default()).await {
        Ok(response) => response.into_inner(),
        Err(status) => return format!("the telemetry stream was refused: {}", status.message()),
    };
    loop {
        match timeout(stale_after, stream.message()).await {
            Err(_) => return format!("no telemetry for {}s: the daemon stopped answering", stale_after.as_secs()),
            Ok(Ok(Some(frame))) => inbox.push_telemetry(frame),
            Ok(Ok(None)) => return "the daemon closed the telemetry stream (restarting?)".into(),
            Ok(Err(status)) => return format!("the telemetry stream failed: {}", status.message()),
        }
    }
}

async fn intervention_loop(mut client: Client, inbox: &Inbox, decisions: mpsc::Receiver<InterventionReply>, config: &ClientConfig) -> String {
    use futures::StreamExt;
    let hello = InterventionReply { reply: Some(intervention_reply::Reply::Hello(InterventionHello { client: config.name.clone() })) };
    let outgoing = futures::stream::once(async move { hello }).chain(ReceiverStream::new(decisions));
    let mut stream = match client.intervention_stream(outgoing).await {
        Ok(response) => response.into_inner(),
        Err(status) => return format!("the intervention stream was refused: {}", status.message()),
    };
    loop {
        // The daemon sends a heartbeat every 5 s, so silence is a dead connection.
        match timeout(config_stale(config), stream.message()).await {
            Err(_) => return "the intervention stream went silent".into(),
            Ok(Ok(Some(event))) => {
                if !matches!(event.event, Some(harness_proto::v1::intervention_event::Event::Heartbeat(_))) {
                    inbox.push_event(ClientEvent::Intervention(event));
                }
            }
            Ok(Ok(None)) => return "the daemon closed the intervention stream".into(),
            Ok(Err(status)) => return format!("the intervention stream failed: {}", status.message()),
        }
    }
}

fn config_stale(config: &ClientConfig) -> Duration {
    config.stale_after
}

async fn command_loop(client: Client, inbox: &Inbox, commands: &mut mpsc::Receiver<Command>, decisions: mpsc::Sender<InterventionReply>, counters: &Counters) -> String {
    while let Some(command) = commands.recv().await {
        match command {
            Command::Decide { intervention_id, approve, isolate, note } => {
                let reply = InterventionReply { reply: Some(intervention_reply::Reply::Decision(InterventionDecision { intervention_id, approve, isolate, note })) };
                if decisions.try_send(reply).is_err() {
                    counters.commands_failed.fetch_add(1, Ordering::Relaxed);
                    inbox.push_event(ClientEvent::Reply(Reply::Failed { what: "decision".into(), error: "could not be delivered; the agent is still waiting".into() }));
                }
            }
            Command::SetKnob { session_id, pod, knob } => {
                let (name, knob_field) = match &knob {
                    Knob::ReasoningEffort(level) => (format!("reasoning_effort={level}"), knob_request::Knob::ReasoningEffort(level.clone())),
                    Knob::LayaDial(value) => (format!("laya_dial={value:.2}"), knob_request::Knob::LayaDial(*value)),
                };
                let (mut client, inbox) = (client.clone(), inbox.clone());
                tokio::spawn(async move {
                    let request = KnobRequest { session_id, pod: pod.clone(), knob: Some(knob_field) };
                    let event = match timeout(UNARY_TIMEOUT, client.set_knob(request)).await {
                        Ok(Ok(reply)) => Reply::Knob { pod, knob: name, reply: reply.into_inner() },
                        Ok(Err(status)) => Reply::Failed { what: format!("{pod} {name}"), error: status.message().to_owned() },
                        Err(_) => Reply::Failed { what: format!("{pod} {name}"), error: "the daemon did not answer in time".into() },
                    };
                    inbox.push_event(ClientEvent::Reply(event));
                });
            }
            Command::UnloadMemory { reason, include_ollama } => {
                let (mut client, inbox) = (client.clone(), inbox.clone());
                tokio::spawn(async move {
                    let event = match timeout(Duration::from_secs(60), client.unload_memory(UnloadRequest { reason, include_ollama })).await {
                        Ok(Ok(reply)) => Reply::Unload(reply.into_inner()),
                        Ok(Err(status)) => Reply::Failed { what: "unload memory".into(), error: status.message().to_owned() },
                        Err(_) => Reply::Failed { what: "unload memory".into(), error: "the daemon did not answer in time".into() },
                    };
                    inbox.push_event(ClientEvent::Reply(event));
                });
            }
            Command::SetMode { session_id, mode } => {
                let (mut client, inbox) = (client.clone(), inbox.clone());
                tokio::spawn(async move {
                    let event = match timeout(UNARY_TIMEOUT, client.set_mode(ModeRequest { session_id, mode: mode.clone() })).await {
                        Ok(Ok(reply)) => Reply::Mode { mode, reply: reply.into_inner() },
                        Ok(Err(status)) => Reply::Failed { what: "mode".into(), error: status.message().to_owned() },
                        Err(_) => Reply::Failed { what: "mode".into(), error: "the daemon did not answer in time".into() },
                    };
                    inbox.push_event(ClientEvent::Reply(event));
                });
            }
            Command::ControlPod { session_id, pod, control } => {
                let (mut client, inbox) = (client.clone(), inbox.clone());
                tokio::spawn(async move {
                    let (action, text) = match &control {
                        PodControl::Isolate => (PodAction::Isolate, String::new()),
                        PodControl::Release => (PodAction::Release, String::new()),
                        PodControl::Steer(text) => (PodAction::Steer, text.clone()),
                    };
                    let what = control.label();
                    let request = PodControlRequest { session_id, pod: pod.clone(), action: action as i32, text };
                    let event = match timeout(UNARY_TIMEOUT, client.control_pod(request)).await {
                        Ok(Ok(reply)) => Reply::Pod { pod, what, reply: reply.into_inner() },
                        Ok(Err(status)) => Reply::Failed { what: format!("{what} {pod}"), error: status.message().to_owned() },
                        Err(_) => Reply::Failed { what: format!("{what} {pod}"), error: "the daemon did not answer in time".into() },
                    };
                    inbox.push_event(ClientEvent::Reply(event));
                });
            }
            Command::FetchDiff { id } => {
                let (mut client, inbox) = (client.clone(), inbox.clone());
                tokio::spawn(async move {
                    let event = match timeout(UNARY_TIMEOUT, client.get_diff(DiffRequest { id })).await {
                        Ok(Ok(reply)) => Reply::Diff(reply.into_inner()),
                        Ok(Err(status)) => Reply::Failed { what: "diff".into(), error: status.message().to_owned() },
                        Err(_) => Reply::Failed { what: "diff".into(), error: "the daemon did not answer in time".into() },
                    };
                    inbox.push_event(ClientEvent::Reply(event));
                });
            }
        }
    }
    "the command channel closed".into()
}

async fn ingest_loop(mut client: Client, inbox: &Inbox, ingest: &mut mpsc::Receiver<AstSnippet>) -> String {
    let (tx, rx) = mpsc::channel::<AstSnippet>(256);
    let mut call = tokio::spawn(async move { client.proactive_ingest(ReceiverStream::new(rx)).await });
    loop {
        tokio::select! {
            snippet = ingest.recv() => match snippet {
                Some(snippet) => {
                    if tx.send(snippet).await.is_err() {
                        break;
                    }
                }
                None => return "the ingest channel closed".into(),
            },
            result = &mut call => {
                return match result {
                    Ok(Ok(summary)) => {
                        inbox.push_event(ClientEvent::Reply(Reply::Ingest(summary.into_inner())));
                        "the daemon ended the ingest stream".into()
                    }
                    Ok(Err(status)) => format!("the ingest stream failed: {}", status.message()),
                    Err(_) => "the ingest task panicked".into(),
                };
            }
        }
    }
    match call.await {
        Ok(Err(status)) => format!("the ingest stream failed: {}", status.message()),
        _ => "the ingest stream ended".into(),
    }
}
