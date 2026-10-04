//! End-to-end tests of `acp-core` against a scripted fake ACP agent
//! (tests/fixtures/fake_agent.py), so the protocol paths — streaming,
//! permission round-trip, cancel, replay, errors — are covered with no LLM.

use std::path::PathBuf;
use std::time::Duration;

use acp_core::{
    AppEvent, ApprovalDecision, BackendConfig, SessionHandle, SessionOptions, StopReason,
    ToolStatus,
};
use tokio::sync::mpsc::UnboundedReceiver;

fn fake_backend() -> BackendConfig {
    let mut backend = BackendConfig::discover(None).expect("backend discoverable");
    backend.server = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.py");
    backend
}

fn start(resume: Option<&str>) -> (SessionHandle, UnboundedReceiver<AppEvent>) {
    SessionHandle::spawn(SessionOptions {
        backend: fake_backend(),
        cwd: std::env::temp_dir(),
        transport: acp_core::BackendTransport::Stdio,
        resume: resume.map(str::to_owned),
        debug_log: None,
        continue_latest: false,
    })
}

async fn next(events: &mut UnboundedReceiver<AppEvent>) -> AppEvent {
    tokio::time::timeout(Duration::from_secs(20), events.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event channel closed")
}

async fn connect(events: &mut UnboundedReceiver<AppEvent>) -> acp_core::SessionInfo {
    match next(events).await {
        AppEvent::Connected(info) => info,
        other => panic!("expected Connected, got {other:?}"),
    }
}

#[derive(Default, Debug)]
struct Turn {
    laya: Vec<acp_core::LayaEvent>,
    text: String,
    thought: String,
    tool_statuses: Vec<ToolStatus>,
    tool_content: Vec<String>,
    untrusted: bool,
    permission_titles: Vec<String>,
    always_labels: Vec<String>,
    stop: Option<StopReason>,
    failed: Option<String>,
}

/// Runs events until the turn ends, answering permission cards with `policy`.
async fn run_turn(events: &mut UnboundedReceiver<AppEvent>, policy: ApprovalDecision) -> Turn {
    let mut turn = Turn::default();
    loop {
        match next(events).await {
            AppEvent::AgentText(text) => turn.text.push_str(&text),
            AppEvent::AgentThought(text) => turn.thought.push_str(&text),
            AppEvent::Laya(event) => turn.laya.push(event),
            AppEvent::ToolCallStarted(call) => turn.tool_statuses.push(call.status),
            AppEvent::ToolCallUpdated(patch) => {
                turn.tool_statuses.extend(patch.status);
                turn.untrusted |= patch.untrusted;
                turn.tool_content.extend(patch.content);
            }
            AppEvent::PermissionRequested { info, decision } => {
                turn.permission_titles.push(info.title);
                turn.always_labels.extend(info.always_label);
                decision.send(policy).unwrap();
            }
            AppEvent::TurnComplete(reason) => {
                turn.stop = Some(reason);
                return turn;
            }
            AppEvent::TurnFailed(error) => {
                turn.failed = Some(error);
                return turn;
            }
            AppEvent::ConnectionError(error) => panic!("connection error: {error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn plain_reply_streams_pong_and_ends_turn() {
    let (session, mut events) = start(None);
    let info = connect(&mut events).await;
    assert_eq!(info.current_mode, "build");
    assert_eq!(
        info.modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["plan", "build", "auto"]
    );

    session
        .send_prompt("Reply with exactly the word: pong")
        .unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
    assert_eq!(turn.text, "pong");
    assert_eq!(turn.stop, Some(StopReason::EndTurn));
    assert!(turn.permission_titles.is_empty());
    session.shutdown().await;
}

#[tokio::test]
async fn thought_chunks_use_their_own_channel() {
    let (session, mut events) = start(None);
    connect(&mut events).await;
    session.send_prompt("thought please").unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
    assert_eq!(turn.thought, "pondering...");
    assert_eq!(turn.text, "done thinking");
    session.shutdown().await;
}

#[tokio::test]
async fn permission_allow_runs_the_tool() {
    let (session, mut events) = start(None);
    connect(&mut events).await;
    session
        .send_prompt("shell: echo acp-approved-marker")
        .unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
    assert_eq!(turn.permission_titles, ["Run shell"]);
    assert!(
        turn.tool_content
            .iter()
            .any(|c| c.contains("acp-approved-marker"))
    );
    assert_eq!(turn.tool_statuses.last(), Some(&ToolStatus::Completed));
    assert_eq!(turn.stop, Some(StopReason::EndTurn));
    session.shutdown().await;
}

#[tokio::test]
async fn always_allow_selects_the_servers_always_option() {
    let (session, mut events) = start(None);
    connect(&mut events).await;
    session.send_prompt("shell: echo remember-me").unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::AllowAlways).await;
    assert_eq!(turn.always_labels, ["Always allow shell(echo remember-me)"]);
    assert!(
        turn.tool_content.iter().any(|c| c.contains("[saved-rule]")),
        "the server saw the allow_always option id: {:?}",
        turn.tool_content
    );
    // A plain Allow must pick allow-once, never the always option.
    session.send_prompt("shell: echo just-once").unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
    assert!(
        turn.tool_content
            .iter()
            .all(|c| !c.contains("[saved-rule]"))
    );
    session.shutdown().await;
}

#[tokio::test]
async fn permission_reject_fails_the_tool_but_turn_still_completes() {
    let (session, mut events) = start(None);
    connect(&mut events).await;
    session.send_prompt("shell: echo should-not-run").unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Reject).await;
    assert_eq!(turn.permission_titles.len(), 1);
    assert_eq!(turn.tool_statuses.last(), Some(&ToolStatus::Failed));
    assert_eq!(turn.text, "not run");
    assert_eq!(turn.stop, Some(StopReason::EndTurn));
    session.shutdown().await;
}

#[tokio::test]
async fn dropping_the_decision_sender_denies_instead_of_hanging() {
    let (session, mut events) = start(None);
    connect(&mut events).await;
    session.send_prompt("shell: echo x").unwrap();
    loop {
        match next(&mut events).await {
            AppEvent::PermissionRequested { decision, .. } => drop(decision),
            AppEvent::TurnComplete(reason) => {
                assert_eq!(reason, StopReason::EndTurn);
                break;
            }
            _ => {}
        }
    }
    session.shutdown().await;
}

#[tokio::test]
async fn plan_mode_blocks_with_no_permission_card_and_auto_runs_with_none() {
    let (session, mut events) = start(None);
    connect(&mut events).await;

    session.set_mode("plan").unwrap();
    loop {
        if let AppEvent::ModeChanged(mode) = next(&mut events).await {
            assert_eq!(mode, "plan");
            break;
        }
    }
    session.send_prompt("shell: echo blocked").unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
    assert!(
        turn.permission_titles.is_empty(),
        "plan mode must not prompt"
    );
    assert_eq!(turn.tool_statuses.last(), Some(&ToolStatus::Failed));

    session.set_mode("auto").unwrap();
    loop {
        if let AppEvent::ModeChanged(mode) = next(&mut events).await {
            assert_eq!(mode, "auto");
            break;
        }
    }
    session.send_prompt("shell: echo auto-mode-marker").unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Reject).await;
    assert!(
        turn.permission_titles.is_empty(),
        "auto mode must not prompt"
    );
    assert_eq!(turn.tool_statuses.last(), Some(&ToolStatus::Completed));
    session.shutdown().await;
}

#[tokio::test]
async fn untrusted_markers_are_flagged_on_the_result() {
    let (session, mut events) = start(None);
    connect(&mut events).await;
    session.send_prompt("untrusted").unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
    assert!(turn.untrusted);
    session.shutdown().await;
}

#[tokio::test]
async fn cancel_mid_stream_yields_cancelled() {
    let (session, mut events) = start(None);
    connect(&mut events).await;
    session.send_prompt("slow").unwrap();
    loop {
        if let AppEvent::AgentText(_) = next(&mut events).await {
            break;
        }
    }
    session.cancel().unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
    assert_eq!(turn.stop, Some(StopReason::Cancelled));

    // Back to idle: a fresh turn works.
    session.send_prompt("pong").unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
    assert_eq!(turn.stop, Some(StopReason::EndTurn));
    session.shutdown().await;
}

#[tokio::test]
async fn failed_prompt_is_a_turn_error_and_the_session_survives() {
    let (session, mut events) = start(None);
    connect(&mut events).await;
    session.send_prompt("error").unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
    assert!(
        turn.failed
            .as_deref()
            .is_some_and(|e| e.contains("fake agent exploded")),
        "{turn:?}"
    );

    session.send_prompt("pong").unwrap();
    assert_eq!(
        run_turn(&mut events, ApprovalDecision::Allow).await.text,
        "pong"
    );
    session.shutdown().await;
}

#[tokio::test]
async fn config_option_change_is_reflected() {
    let (session, mut events) = start(None);
    let info = connect(&mut events).await;
    let model = info
        .config_options
        .iter()
        .find(|o| o.id == "model")
        .unwrap();
    let other = model
        .choices
        .iter()
        .find(|c| c.value != model.current_value)
        .unwrap()
        .value
        .clone();

    session.set_config_option("model", other.clone()).unwrap();
    loop {
        if let AppEvent::ConfigOptionsChanged(options) = next(&mut events).await {
            let model = options.iter().find(|o| o.id == "model").unwrap();
            assert_eq!(model.current_value, other);
            break;
        }
    }
    session.shutdown().await;
}

#[tokio::test]
async fn resume_replays_text_history_then_signals_completion() {
    let (session, mut events) = start(Some("old-session-id"));
    let info = connect(&mut events).await;
    assert!(info.resumed);
    assert_eq!(info.current_mode, "build");

    let mut user = String::new();
    let mut agent = String::new();
    loop {
        match next(&mut events).await {
            AppEvent::UserText(text) => user.push_str(&text),
            AppEvent::AgentText(text) => agent.push_str(&text),
            AppEvent::ReplayComplete => break,
            other => panic!("unexpected during replay: {other:?}"),
        }
    }
    assert_eq!(
        (user.as_str(), agent.as_str()),
        ("earlier question", "earlier answer")
    );
    session.shutdown().await;
}

#[tokio::test]
async fn agent_crash_reports_connection_error_then_closed() {
    let (session, mut events) = start(None);
    connect(&mut events).await;
    session.send_prompt("crash").unwrap();
    let mut saw_error = false;
    loop {
        match next(&mut events).await {
            AppEvent::ConnectionError(_) | AppEvent::TurnFailed(_) => saw_error = true,
            AppEvent::Closed => break,
            _ => {}
        }
    }
    assert!(
        saw_error,
        "a dead backend must surface as an error, not a hang"
    );
    session.shutdown().await;
}

#[tokio::test]
async fn every_chunk_arrives_before_turn_complete_even_when_the_response_overtakes_them() {
    // The prompt response callback can run while notifications that preceded it
    // on the wire are still queued inside the SDK; TurnComplete must still be the
    // last event of the turn, with all the text before it. Repeated: the race is
    // timing-dependent.
    for round in 0..8 {
        let (session, mut events) = start(None);
        connect(&mut events).await;
        session.send_prompt("burst").unwrap();
        let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
        assert_eq!(
            turn.text.len(),
            300,
            "round {round}: lost {} chunk(s) to the response",
            300 - turn.text.len()
        );
        session.shutdown().await;
    }
}

#[tokio::test]
async fn laya_extension_notifications_arrive_as_events_before_the_turn_completes() {
    let (session, mut events) = start(None);
    connect(&mut events).await;
    session.send_prompt("laya").unwrap();
    let turn = run_turn(&mut events, ApprovalDecision::Allow).await;
    assert_eq!(turn.text, "ok");
    assert_eq!(
        turn.laya.len(),
        2,
        "a decision and an outcome: {:?}",
        turn.laya
    );
    let acp_core::LayaEvent::Decision(decision) = &turn.laya[0] else {
        panic!("{:?}", turn.laya)
    };
    assert_eq!(decision.route.as_deref(), Some("files_and_code"));
    assert_eq!((decision.route_confidence, decision.gate), (0.98, 0.85));
    assert_eq!(
        decision.hint.as_deref(),
        Some("use the file and code tools")
    );
    let acp_core::LayaEvent::Outcome(outcome) = &turn.laya[1] else {
        panic!()
    };
    assert_eq!(
        (outcome.agreed, outcome.tools.as_slice()),
        (Some(true), ["file_read".to_owned()].as_slice())
    );
    session.shutdown().await;
}
