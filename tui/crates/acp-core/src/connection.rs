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
    SetSessionConfigOptionRequest, SetSessionModeRequest, TextContent,
};
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, ActiveSession, Agent, Client, ConnectionTo, Dispatch, LineDirection,
    Responder, SessionMessage,
};
use tokio::sync::{mpsc, oneshot};

use crate::error::describe_acp_error;
use crate::events::{self, AppEvent, SessionInfo, StopReason};
use crate::permission::{self, ApprovalDecision, PermissionInfo};
use crate::session::{Command, SessionOptions};

type Events = mpsc::UnboundedSender<AppEvent>;

pub(crate) async fn run(
    options: SessionOptions,
    commands: mpsc::UnboundedReceiver<Command>,
    events: Events,
) {
    if let Err(error) = options.backend.validate() {
        let _ = events.send(AppEvent::ConnectionError(error.to_string()));
        let _ = events.send(AppEvent::Closed);
        return;
    }

    let mut config =
        AcpAgentConfig::new(&options.backend.python).arg(options.backend.server.to_string_lossy());
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

    let session_events = events.clone();
    let result = Client
        .builder()
        .connect_with(agent, async move |connection: ConnectionTo<Agent>| {
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
    let (mut session, info, replay) = match options.resume.clone() {
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

fn dispatch_command(
    command: Command,
    connection: &ConnectionTo<Agent>,
    _session: &ActiveSession<'_, Agent>,
    session_id: &agent_client_protocol::schema::v1::SessionId,
    turn_tx: &mpsc::UnboundedSender<Result<StopReason, String>>,
    events: &Events,
) -> Result<(), agent_client_protocol::Error> {
    match command {
        Command::Prompt(text) => {
            let turn_tx = turn_tx.clone();
            let request = PromptRequest::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new(text))],
            );
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
                let decision = decision_rx.await.ok();
                responder.respond(permission_response(&request, decision))
            },
        )
        .await
        .otherwise(async |message| match message {
            Dispatch::Request(_, responder) => {
                responder.respond_with_error(agent_client_protocol::Error::method_not_found())
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
        tool_name,
        title,
    }
}

/// Maps a human/scripted decision onto the outcome sent back to the server.
///
/// Allow selects the server's allow option. Reject sends `cancelled` rather
/// than selecting the server's reject option: older `acp_server.py` revisions
/// approved on *any* `selected` outcome (`AllowedOutcome` only means
/// "outcome == selected"), so selecting "reject" would have run the tool.
/// `cancelled` is a denial on every revision — and harmless on one that
/// honours option ids, because this backend keeps the turn going after a
/// denial. `None` (consumer vanished) is also a denial.
fn permission_response(
    request: &RequestPermissionRequest,
    decision: Option<ApprovalDecision>,
) -> RequestPermissionResponse {
    let allow = matches!(decision, Some(ApprovalDecision::Allow))
        .then(|| {
            request.options.iter().find(|option| {
                matches!(
                    option.kind,
                    PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                )
            })
        })
        .flatten();
    RequestPermissionResponse::new(match allow {
        Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        )),
        None => RequestPermissionOutcome::Cancelled,
    })
}
