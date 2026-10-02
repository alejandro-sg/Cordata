//! Runs one scenario: one backend process, one session, N prompts.
//!
//! Each scenario gets its own `acp_server.py` process on purpose — the
//! backend's `_build_session` does a process-wide `os.chdir`, so concurrent
//! sessions in one process would trample each other's working directory.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use acp_core::{
    AppEvent, BackendConfig, SessionHandle, SessionInfo, SessionOptions, StopReason, ToolStatus,
};
use tokio::sync::{mpsc, watch};

use crate::report::{PermissionRecord, ScenarioOutcome, Sink, ToolRecord, TurnRecord};
use crate::scenario::{Decision, Scenario};

pub struct RunConfig {
    pub backend: BackendConfig,
    pub startup_timeout: Duration,
    pub turn_timeout: Duration,
}

type Events = mpsc::UnboundedReceiver<AppEvent>;

enum Wait {
    Event(AppEvent),
    Timeout,
    Aborted,
    Ended,
}

async fn wait(events: &mut Events, timeout: Duration, abort: &mut watch::Receiver<bool>) -> Wait {
    tokio::select! {
        outcome = tokio::time::timeout(timeout, events.recv()) => match outcome {
            Ok(Some(event)) => Wait::Event(event),
            Ok(None) => Wait::Ended,
            Err(_) => Wait::Timeout,
        },
        _ = abort.wait_for(|aborted| *aborted) => Wait::Aborted,
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

pub async fn run_scenario(
    config: &RunConfig,
    scenario: Scenario,
    mut abort: watch::Receiver<bool>,
    sink: &Sink,
) -> ScenarioOutcome {
    let cwd = scenario
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| ".".into());
    let (session, mut events) = SessionHandle::spawn(SessionOptions {
        backend: config.backend.clone(),
        cwd,
        resume: None,
        debug_log: None,
    });

    let mut outcome = ScenarioOutcome {
        name: scenario.name.clone(),
        error: None,
        turns: Vec::new(),
    };
    let mut session_id = None;

    let setup = setup_session(config, &scenario, &session, &mut events, &mut abort).await;
    let failure = match setup {
        Ok(info) => {
            session_id = Some(info.session_id);
            run_prompts(
                config,
                &scenario,
                &session,
                &mut events,
                &mut abort,
                session_id.clone(),
                sink,
                &mut outcome,
            )
            .await
        }
        Err(error) => Some(error),
    };

    if let Some(error) = failure {
        // Every prompt still gets a line, so one-line-per-turn holds even when
        // a session dies. A turn that already has its own record (it ran and
        // failed) is not repeated; only the prompts that never ran are added.
        // If setup failed, the first prompt carries the cause and the rest
        // point back at it.
        let setup_failed = outcome.turns.is_empty();
        for (index, prompt) in scenario
            .prompts
            .iter()
            .enumerate()
            .skip(outcome.turns.len())
        {
            let message = if setup_failed && index == 0 {
                error.clone()
            } else {
                format!("skipped: {error}")
            };
            let record = TurnRecord {
                scenario: scenario.name.clone(),
                session_id: session_id.clone(),
                prompt_index: Some(index),
                prompt: truncate(prompt, 200),
                mode: scenario.mode.clone(),
                started_unix_ms: unix_ms(),
                error: Some(message),
                ..Default::default()
            };
            sink.write(&record);
            outcome.turns.push(record);
        }
        outcome.error = Some(error);
    }

    session.shutdown().await;
    outcome
}

async fn setup_session(
    config: &RunConfig,
    scenario: &Scenario,
    session: &SessionHandle,
    events: &mut Events,
    abort: &mut watch::Receiver<bool>,
) -> Result<SessionInfo, String> {
    let deadline = Instant::now() + config.startup_timeout;
    let info = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match wait(events, remaining, abort).await {
            Wait::Event(AppEvent::Connected(info)) => break info,
            Wait::Event(AppEvent::ConnectionError(error)) => {
                return Err(format!("startup failed: {error}"));
            }
            Wait::Event(AppEvent::Closed) | Wait::Ended => {
                return Err("backend closed before the session started".into());
            }
            Wait::Event(_) => {}
            Wait::Timeout => {
                return Err(format!(
                    "startup timed out after {}s",
                    config.startup_timeout.as_secs()
                ));
            }
            Wait::Aborted => return Err("aborted".into()),
        }
    };

    if let Some(mode) = &scenario.mode
        && *mode != info.current_mode
    {
        session.set_mode(mode.clone()).map_err(|e| e.to_string())?;
        await_confirmation(
            events,
            abort,
            Duration::from_secs(30),
            |event| match event {
                AppEvent::ModeChanged(applied) if applied == mode => Some(Ok(())),
                AppEvent::Notice(note) => Some(Err(note.clone())),
                _ => None,
            },
        )
        .await
        .map_err(|e| format!("setting mode {mode}: {e}"))?;
    }

    for (id, wanted) in [
        ("model", &scenario.model),
        ("temperature", &scenario.temperature),
    ] {
        let Some(wanted) = wanted else { continue };
        let option = info.config_options.iter().find(|option| option.id == id);
        let Some(option) = option else {
            return Err(format!("backend offers no {id} option"));
        };
        // The backend silently ignores an unavailable value; catch it here.
        if !option.choices.iter().any(|choice| choice.value == *wanted) {
            let choices: Vec<&str> = option.choices.iter().map(|c| c.value.as_str()).collect();
            return Err(format!(
                "{id} {wanted:?} is not offered (choices: {})",
                choices.join(", ")
            ));
        }
        if option.current_value == *wanted {
            continue;
        }
        session
            .set_config_option(id, wanted.clone())
            .map_err(|e| e.to_string())?;
        let applied = await_confirmation(
            events,
            abort,
            Duration::from_secs(30),
            |event| match event {
                AppEvent::ConfigOptionsChanged(options) => Some(Ok(options
                    .iter()
                    .find(|o| o.id == id)
                    .map(|o| o.current_value.clone())
                    .unwrap_or_default())),
                AppEvent::Notice(note) => Some(Err(note.clone())),
                _ => None,
            },
        )
        .await
        .map_err(|e| format!("setting {id}: {e}"))?;
        if applied != *wanted {
            return Err(format!(
                "backend did not apply {id}={wanted:?} (still {applied:?})"
            ));
        }
    }
    Ok(info)
}

async fn await_confirmation<T>(
    events: &mut Events,
    abort: &mut watch::Receiver<bool>,
    timeout: Duration,
    mut matcher: impl FnMut(&AppEvent) -> Option<Result<T, String>>,
) -> Result<T, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match wait(events, remaining, abort).await {
            Wait::Event(AppEvent::ConnectionError(error)) => return Err(error),
            Wait::Event(AppEvent::Closed) | Wait::Ended => return Err("backend closed".into()),
            Wait::Event(event) => {
                if let Some(result) = matcher(&event) {
                    return result;
                }
            }
            Wait::Timeout => return Err("timed out waiting for the backend".into()),
            Wait::Aborted => return Err("aborted".into()),
        }
    }
}

/// Runs every prompt in order. Returns the error that stopped the scenario, if any.
#[allow(clippy::too_many_arguments)]
async fn run_prompts(
    config: &RunConfig,
    scenario: &Scenario,
    session: &SessionHandle,
    events: &mut Events,
    abort: &mut watch::Receiver<bool>,
    session_id: Option<String>,
    sink: &Sink,
    outcome: &mut ScenarioOutcome,
) -> Option<String> {
    for (index, prompt) in scenario.prompts.iter().enumerate() {
        let (record, fatal) = run_turn(
            config,
            scenario,
            session,
            events,
            abort,
            session_id.clone(),
            index,
            prompt,
        )
        .await;
        sink.write(&record);
        outcome.turns.push(record);
        if let Some(fatal) = fatal {
            return Some(fatal);
        }
    }
    None
}

/// Returns the turn's record plus, when the session can't continue, why.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    config: &RunConfig,
    scenario: &Scenario,
    session: &SessionHandle,
    events: &mut Events,
    abort: &mut watch::Receiver<bool>,
    session_id: Option<String>,
    index: usize,
    prompt: &str,
) -> (TurnRecord, Option<String>) {
    let mut record = TurnRecord {
        scenario: scenario.name.clone(),
        session_id,
        prompt_index: Some(index),
        prompt: truncate(prompt, 200),
        mode: scenario.mode.clone(),
        started_unix_ms: unix_ms(),
        ..Default::default()
    };
    let started = Instant::now();
    if let Err(error) = session.send_prompt(prompt) {
        record.error = Some(error.to_string());
        return (record, Some(error.to_string()));
    }

    let mut tools: Vec<(String, String, ToolStatus)> = Vec::new();
    let mut timed_out = false;
    let mut deadline = started + config.turn_timeout;

    let fatal = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match wait(events, remaining, abort).await {
            Wait::Event(AppEvent::AgentText(text)) => {
                record
                    .ttft_ms
                    .get_or_insert_with(|| started.elapsed().as_millis() as u64);
                record.response_chars += text.chars().count();
            }
            Wait::Event(AppEvent::AgentThought(_)) => {
                record
                    .ttft_ms
                    .get_or_insert_with(|| started.elapsed().as_millis() as u64);
            }
            Wait::Event(AppEvent::ToolCallStarted(call)) => {
                tools.push((call.id, call.name, call.status))
            }
            Wait::Event(AppEvent::ToolCallUpdated(patch)) => {
                if let Some(entry) = tools.iter_mut().find(|(id, _, _)| *id == patch.id) {
                    if let Some(status) = patch.status {
                        entry.2 = status;
                    }
                    if entry.1 == "context_usage" && entry.2 == ToolStatus::Completed {
                        record.context_usage =
                            patch.content.as_deref().map(|text| truncate(text, 500));
                    }
                }
            }
            Wait::Event(AppEvent::PermissionRequested { info, decision }) => {
                let chosen = scenario.approval_policy.decide(&info.tool_name);
                record.permission_requests += 1;
                record.permissions.push(PermissionRecord {
                    tool: info.tool_name.clone(),
                    decision: match chosen {
                        Decision::Allow => "allow",
                        Decision::Reject => "reject",
                    }
                    .into(),
                });
                let _ = decision.send(chosen.into());
            }
            Wait::Event(AppEvent::TurnComplete(reason)) => {
                record.stop_reason = Some(reason.as_str().to_owned());
                if timed_out {
                    record.error = Some(format!(
                        "turn timed out after {}s (cancelled)",
                        config.turn_timeout.as_secs()
                    ));
                    break Some("turn timed out".to_owned());
                }
                record.latency_ms = Some(started.elapsed().as_millis() as u64);
                if matches!(reason, StopReason::Refusal) {
                    record.error = Some("model refused".into());
                }
                break None;
            }
            Wait::Event(AppEvent::TurnFailed(error)) => {
                record.latency_ms = Some(started.elapsed().as_millis() as u64);
                record.error = Some(error);
                break None;
            }
            Wait::Event(AppEvent::ConnectionError(error)) => {
                record.error = Some(error.clone());
                break Some(error);
            }
            Wait::Event(AppEvent::Closed) | Wait::Ended => {
                record.error = Some("backend closed mid-turn".into());
                break Some("backend closed mid-turn".into());
            }
            Wait::Event(_) => {}
            Wait::Timeout => {
                if timed_out {
                    // Cancel didn't land either (e.g. a request wedged server-side).
                    record.error = Some("turn timed out and cancel was not honoured".into());
                    break Some("turn timed out".into());
                }
                timed_out = true;
                let _ = session.cancel();
                // Give session/cancel a bounded window to produce a stop reason.
                deadline = Instant::now() + Duration::from_secs(15);
            }
            Wait::Aborted => {
                record.error = Some("aborted".into());
                break Some("aborted".into());
            }
        }
    };

    for (_, name, status) in &tools {
        record.tool_calls.push(ToolRecord {
            name: name.clone(),
            status: status.label().to_owned(),
        });
    }
    (record, fatal)
}
