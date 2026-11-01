//! The only file that touches `agent-client-protocol`'s connection API.
//!
//! Wiring: `AcpAgent` (owns spawning + stdio; no reader/writer to get
//! backwards on the client side) → `Client.builder().connect_with(..)`.
//! The *agent*-side stream-ordering footgun documented in `acp_server.py`
//! (`run_agent`'s misleadingly-named writer/reader parameters) does not exist
//! here: the client API never exposes raw streams.
//!
//! Inside the connection one loop multiplexes UI commands against the
//! session's update stream, so `session/cancel`, `set_mode`, etc. can go out
//! while a turn is streaming.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigOptionValue, SessionNotification,
    SetSessionConfigOptionRequest, SetSessionModeRequest, TextContent, ImageContent, EmbeddedResource,
    EmbeddedResourceResource, TextResourceContents,
};
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, ActiveSession, Agent, Client, ConnectionTo, Dispatch, DynConnectTo,
    LineDirection, Responder, SessionMessage, UntypedMessage,
};
use tokio::sync::{mpsc, oneshot};

use crate::error::describe_acp_error;
use crate::events::{self, AppEvent, SessionInfo, StopReason};
use crate::permission::{self, ApprovalDecision, PermissionInfo};
use crate::session::{Command, SessionOptions};
use crate::transport::{BackendTransport, WsTransport};

type Events = mpsc::UnboundedSender<AppEvent>;

pub(crate) async fn run(
    mut options: SessionOptions,
    commands: mpsc::UnboundedReceiver<Command>,
    events: Events,
) {
    // A hub conversation is resolved before anything connects: the daemon starts its agent and says
    // which ACP session to load, and the socket attaches by handle — so this terminal continues the
    // conversation another door started rather than opening one of its own.
    let mut hub_url = None;
    if let BackendTransport::Hub { daemon, conversation } = &options.transport {
        match crate::hub::open(daemon, conversation, &options.cwd).await {
            Ok(row) => {
                hub_url = Some(daemon.conversation_url(&row.handle));
                options.resume = Some(row.acp_session_id.clone());
                options.continue_latest = false;
                let title = if row.title.is_empty() {
                    String::new()
                } else {
                    format!(" · “{}”", row.title)
                };
                let _ = events.send(AppEvent::Notice(format!(
                    "conversation {}{title} — shared with Discord, the menu bar and the web app",
                    row.handle
                )));
                options.hub_handle = Some(row.handle);
            }
            Err(error) => {
                let _ = events.send(AppEvent::ConnectionError(format!("could not open the conversation: {error}")));
                let _ = events.send(AppEvent::Closed);
                return;
            }
        }
    }
    // Only the stdio transport needs the local backend: with `--transport ws`
    // the *daemon* spawns `acp_server.py`, possibly on this machine but not
    // necessarily from this checkout, and validating our own copy would report
    // a problem the connection does not actually have.
    let transport: DynConnectTo<Client> = match &options.transport {
        BackendTransport::Stdio => {
            if let Err(error) = options.backend.validate() {
                let _ = events.send(AppEvent::ConnectionError(error.to_string()));
                let _ = events.send(AppEvent::Closed);
                return;
            }

            let mut config = AcpAgentConfig::new(&options.backend.python)
                .arg(options.backend.server.to_string_lossy());
            // Unbuffered stdout matters on the Python side: ACP frames are single
            // lines and a block-buffered pipe would delay streamed chunks.
            config = config.env("PYTHONUNBUFFERED", "1");
            let mut agent = AcpAgent::new(config);
            if let Some(path) = options.debug_log.clone()
                && let Ok(file) = OpenOptions::new().create(true).append(true).open(&path)
            {
                let file = Mutex::new(file);
                agent = agent.with_debug(move |line, direction| {
                    let tag = match direction {
                        LineDirection::Stdin => "->",
                        LineDirection::Stdout => "<-",
                        LineDirection::Stderr => "!!",
                    };
                    if let Ok(mut file) = file.lock() {
                        let _ = writeln!(file, "{tag} {line}");
                    }
                });
            }
            DynConnectTo::new(agent)
        }
        BackendTransport::Ws { url } => DynConnectTo::new(WsTransport::new(url.clone())),
        BackendTransport::Hub { .. } => DynConnectTo::new(WsTransport::new(hub_url.clone().unwrap_or_default())),
        // `SessionHandle::spawn` routes Native to `crate::native` before this function runs.
        BackendTransport::Native => unreachable!("native sessions do not use the ACP connection"),
    };

    let session_events = events.clone();
    let result = Client
        .builder()
        .connect_with(transport, async move |connection: ConnectionTo<Agent>| {
            drive(connection, options, commands, session_events).await
        })
        .await;

    if let Err(error) = result {
        let _ = events.send(AppEvent::ConnectionError(describe_acp_error(&error)));
    }
    let _ = events.send(AppEvent::Closed);
}

async fn drive(
    connection: ConnectionTo<Agent>,
    options: SessionOptions,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: Events,
) -> Result<(), agent_client_protocol::Error> {
    let init = connection
        .send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await?;

    let cwd = options.cwd.clone();
    let mut resume = options.resume.clone();
    if resume.is_none() && options.continue_latest {
        resume = latest_session(&connection, &cwd).await;
        if resume.is_none() {
            let _ = events.send(AppEvent::Notice(
                "--continue: no earlier session in this folder — starting a new one".into(),
            ));
        }
    }
    let (mut session, info, replay) = match resume {
        Some(session_id) => {
            if !init.agent_capabilities.load_session {
                return Err(agent_client_protocol::util::internal_error(
                    "agent does not advertise loadSession; cannot --resume",
                ));
            }
            let restored = connection
                .load_session(session_id, &cwd)
                .block_task()
                .start_session()
                .await?;
            let (session, response) = restored.into_parts();
            let (current_mode, modes) = events::mode_infos(response.modes.as_ref());
            let info = SessionInfo {
                session_id: session.session_id().to_string(),
                cwd: cwd.display().to_string(),
                transport: transport_label(&options),
                resumed: true,
                current_mode,
                modes,
                config_options: events::config_infos(
                    response.config_options.as_deref().unwrap_or_default(),
                ),
            };
            (session, info, true)
        }
        None => {
            let session = connection
                .build_session(&cwd)
                .block_task()
                .start_session()
                .await?;
            let (current_mode, modes) = events::mode_infos(session.modes());
            let info = SessionInfo {
                session_id: session.session_id().to_string(),
                cwd: cwd.display().to_string(),
                transport: transport_label(&options),
                resumed: false,
                current_mode,
                modes,
                config_options: events::config_infos(session.config_options().unwrap_or_default()),
            };
            (session, info, false)
        }
    };

    if events.send(AppEvent::Connected(info)).is_err() {
        return Ok(());
    }

    // `session/load` queues its history replay on the session before the
    // load response returns, so by now every replayed update is already
    // sitting in the channel: drain them without waiting.
    if replay {
        while let Ok(message) = tokio::time::timeout(Duration::ZERO, session.read_update()).await {
            if !handle_message(message?, &events).await? {
                return Ok(());
            }
        }
        let _ = events.send(AppEvent::ReplayComplete);
    }

    // Prompt results come back over our own channel (not
    // ActiveSession::send_prompt) so a failed `session/prompt` becomes a
    // `TurnFailed` event instead of tearing the whole connection down.
    let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<Result<StopReason, String>>();
    let session_id = session.session_id().clone();

    loop {
        tokio::select! {
            message = session.read_update() => {
                if !handle_message(message?, &events).await? {
                    return Ok(());
                }
            }
            turn = turn_rx.recv() => {
                let Some(turn) = turn else { continue };
                // The dispatch loop is sequential: every notification that
                // preceded the prompt response is already queued. Flush
                // them so TurnComplete is really the last event of the turn.
                while let Ok(message) = tokio::time::timeout(Duration::ZERO, session.read_update()).await {
                    if !handle_message(message?, &events).await? {
                        return Ok(());
                    }
                }
                let event = match turn {
                    Ok(reason) => AppEvent::TurnComplete(reason),
                    Err(error) => AppEvent::TurnFailed(error),
                };
                if events.send(event).is_err() {
                    return Ok(());
                }
            }
            command = commands.recv() => {
                match command {
                    None | Some(Command::Shutdown) => return Ok(()),
                    Some(command) => {
                        dispatch_command(command, &connection, &session, &session_id, &turn_tx, &events)?;
                    }
                }
            }
        }
    }
}

/// `session/list` for `cwd`, newest first (the backend sorts); `None` when the
/// agent doesn't support it or has nothing for this folder.
async fn latest_session(connection: &ConnectionTo<Agent>, cwd: &std::path::Path) -> Option<String> {
    let request = UntypedMessage::new("session/list", serde_json::json!({"cwd": cwd})).ok()?;
    let reply = connection.send_request(request).block_task().await.ok()?;
    reply
        .get("sessions")?
        .as_array()?
        .first()?
        .get("sessionId")?
        .as_str()
        .map(str::to_owned)
}

/// The prompt's content blocks: the text, then each attachment (images as `image`, files as embedded resources).
fn prompt_blocks(text: String, attachments: Vec<crate::session::Attachment>) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::Text(TextContent::new(text))];
    for attachment in attachments {
        blocks.push(match attachment {
            crate::session::Attachment::Image { mime, data_base64 } => ContentBlock::Image(ImageContent::new(data_base64, mime)),
            crate::session::Attachment::Text { uri, text } => ContentBlock::Resource(EmbeddedResource::new(EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(text, uri)))),
        });
    }
    blocks
}

fn dispatch_command(
    command: Command,
    connection: &ConnectionTo<Agent>,
    _session: &ActiveSession<'_, Agent>,
    session_id: &agent_client_protocol::schema::v1::SessionId,
    turn_tx: &mpsc::UnboundedSender<Result<StopReason, String>>,
    events: &Events,
) -> Result<(), agent_client_protocol::Error> {
    match command {
        Command::Prompt(text, attachments) => {
            let turn_tx = turn_tx.clone();
            let request = PromptRequest::new(session_id.clone(), prompt_blocks(text, attachments));
            connection
                .send_request(request)
                .on_receiving_result(async move |result| {
                    let outcome = result
                        .map(|response| StopReason::from(response.stop_reason))
                        .map_err(|error| describe_acp_error(&error));
                    let _ = turn_tx.send(outcome);
                    Ok(())
                })?;
        }
        Command::Cancel => {
            connection.send_notification(CancelNotification::new(session_id.clone()))?;
        }
        Command::SetMode(mode_id) => {
            let events = events.clone();
            let request = SetSessionModeRequest::new(session_id.clone(), mode_id.clone());
            // acp_server.py never pushes a current_mode_update for a client-
            // initiated change, so a successful response is the signal.
            connection
                .send_request(request)
                .on_receiving_result(async move |result| {
                    let _ = events.send(match result {
                        Ok(_) => AppEvent::ModeChanged(mode_id),
                        Err(error) => AppEvent::Notice(format!(
                            "set_mode failed: {}",
                            describe_acp_error(&error)
                        )),
                    });
                    Ok(())
                })?;
        }
        Command::SetConfigOption { id, value } => {
            let events = events.clone();
            let request = SetSessionConfigOptionRequest::new(
                session_id.clone(),
                id.clone(),
                SessionConfigOptionValue::value_id(value),
            );
            connection
                .send_request(request)
                .on_receiving_result(async move |result| {
                    let _ = events.send(match result {
                        Ok(response) => AppEvent::ConfigOptionsChanged(events::config_infos(
                            &response.config_options,
                        )),
                        Err(error) => AppEvent::Notice(format!(
                            "set {id} failed: {}",
                            describe_acp_error(&error)
                        )),
                    });
                    Ok(())
                })?;
        }
        Command::Ext {
            method,
            mut params,
            tag,
        } => {
            let events = events.clone();
            if let Some(object) = params.as_object_mut() {
                object
                    .entry("sessionId")
                    .or_insert_with(|| serde_json::Value::String(session_id.to_string()));
            }
            let request = UntypedMessage::new(&method, params)?;
            connection
                .send_request(request)
                .on_receiving_result(async move |result| {
                    let _ = events.send(AppEvent::ExtResult {
                        tag,
                        result: result.map_err(|error| describe_acp_error(&error)),
                    });
                    Ok(())
                })?;
        }
        Command::Shutdown => {}
    }
    Ok(())
}

/// Returns `Ok(false)` once the consumer has gone away.
async fn handle_message(
    message: SessionMessage,
    events: &Events,
) -> Result<bool, agent_client_protocol::Error> {
    let SessionMessage::SessionMessage(dispatch) = message else {
        // StopReason only comes from ActiveSession::send_prompt, which we
        // don't use; the turn result arrives via our own channel.
        return Ok(true);
    };

    let alive = AtomicBool::new(true);
    MatchDispatch::new(dispatch)
        .if_notification(async |notification: SessionNotification| {
            for event in events::translate_update(notification.update) {
                if events.send(event).is_err() {
                    alive.store(false, Ordering::Relaxed);
                }
            }
            Ok(())
        })
        .await
        .if_request(
            async |request: RequestPermissionRequest,
                   responder: Responder<RequestPermissionResponse>| {
                let (decision_tx, decision_rx) = oneshot::channel();
                let info = permission_info(&request);
                if events
                    .send(AppEvent::PermissionRequested {
                        info,
                        decision: decision_tx,
                    })
                    .is_err()
                {
                    alive.store(false, Ordering::Relaxed);
                }
                // The server is blocked on this response; a dropped sender
                // (consumer quit) must still answer it so nothing hangs.
                //
                // Answered from its own task, not awaited here: this handler runs
                // in the one loop that reads every incoming message, so waiting
                // for the human inside it would hold back everything the backend
                // sends *after* the request — the Lab's state updates (the very
                // pod statuses the approval screen is drawn from), task progress,
                // the rest of the turn's stream — until the person answers.
                tokio::spawn(async move {
                    let decision = decision_rx.await.ok();
                    // A failed send means the connection is already gone, and
                    // there is no one left to tell.
                    let _ = responder.respond(permission_response(&request, decision));
                });
                Ok(())
            },
        )
        .await
        .otherwise(async |message| match message {
            Dispatch::Request(_, responder) => {
                responder.respond_with_error(agent_client_protocol::Error::method_not_found())
            }
            // The backend's live Laya triage: an ACP extension notification.
            Dispatch::Notification(notification) if notification.method == events::LAYA_METHOD => {
                if let Some(event) = events::parse_laya(&notification.params)
                    && events.send(AppEvent::Laya(event)).is_err()
                {
                    alive.store(false, Ordering::Relaxed);
                }
                Ok(())
            }
            // Tasks / self-heal / plugins / LSP: the harness's live state.
            Dispatch::Notification(notification) => {
                if let Some(event) =
                    events::parse_harness(&notification.method, &notification.params)
                    && events.send(AppEvent::Harness(event)).is_err()
                {
                    alive.store(false, Ordering::Relaxed);
                }
                Ok(())
            }
            _ => Ok(()),
        })
        .await?;
    Ok(alive.load(Ordering::Relaxed))
}

fn permission_info(request: &RequestPermissionRequest) -> PermissionInfo {
    let title = request.tool_call.fields.title.clone().unwrap_or_default();
    let tool_name = permission::tool_name_from_title(&title).to_owned();
    PermissionInfo {
        tool_call_id: request.tool_call.tool_call_id.to_string(),
        untrusted_source: permission::tool_may_return_untrusted(&tool_name),
        kind: request
            .tool_call
            .fields
            .kind
            .map(Into::into)
            .unwrap_or(events::ToolKind::Other),
        raw_input: request.tool_call.fields.raw_input.clone(),
        always_label: request
            .options
            .iter()
            .find(|option| matches!(option.kind, PermissionOptionKind::AllowAlways))
            .map(|option| option.name.clone()),
        options: request
            .options
            .iter()
            .map(|option| permission::PermissionChoice {
                id: option.option_id.to_string(),
                name: option.name.clone(),
                kind: match option.kind {
                    PermissionOptionKind::AllowOnce => permission::ChoiceKind::AllowOnce,
                    PermissionOptionKind::AllowAlways => permission::ChoiceKind::AllowAlways,
                    PermissionOptionKind::RejectAlways => permission::ChoiceKind::RejectAlways,
                    _ => permission::ChoiceKind::RejectOnce,
                },
            })
            .collect(),
        tool_name,
        title,
    }
}

/// Maps a human/scripted decision onto the outcome sent back to the server.
///
/// Allow selects the server's allow-once option (AllowAlways its allow-always
/// one, falling back to allow-once if the server offered none). Reject sends
/// `cancelled` rather than selecting the server's reject option: older
/// `acp_server.py` revisions approved on *any* `selected` outcome
/// (`AllowedOutcome` only means "outcome == selected"), so selecting "reject"
/// would have run the tool. `cancelled` is a denial on every revision — and
/// harmless on one that honours option ids, because this backend keeps the turn
/// going after a denial. `None` (consumer vanished) is also a denial.
fn permission_response(
    request: &RequestPermissionRequest,
    decision: Option<ApprovalDecision>,
) -> RequestPermissionResponse {
    let find =
        |wanted: PermissionOptionKind| request.options.iter().find(|option| option.kind == wanted);
    let allow = match &decision {
        Some(ApprovalDecision::Allow) => find(PermissionOptionKind::AllowOnce),
        Some(ApprovalDecision::AllowAlways) => find(PermissionOptionKind::AllowAlways)
            .or_else(|| find(PermissionOptionKind::AllowOnce)),
        Some(ApprovalDecision::Choose(id)) => {
            request.options.iter().find(|option| option.option_id.to_string() == *id)
        }
        _ => None,
    };
    let mut response = RequestPermissionResponse::new(match allow {
        Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        )),
        None => RequestPermissionOutcome::Cancelled,
    });
    // The person's own words ride in ACP's `_meta` slot (`cordata.note`), which the backend reads as the reason for a
    // refusal or the answer to a question; a client that does not know it ignores it.
    if let Some(ApprovalDecision::Say(text)) = decision {
        let mut cordata = serde_json::Map::new();
        cordata.insert("note".into(), serde_json::Value::String(text));
        let mut meta = serde_json::Map::new();
        meta.insert("cordata".into(), serde_json::Value::Object(cordata));
        response.meta = Some(meta);
    }
    response
}

/// The banner's word for how this session is reached — with the conversation's handle for the hub,
/// because the handle is what a person types into another door to continue it.
fn transport_label(options: &SessionOptions) -> String {
    match &options.hub_handle {
        Some(handle) => format!("hub · {handle}"),
        None => options.transport.label().to_owned(),
    }
}
