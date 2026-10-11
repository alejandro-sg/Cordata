//! The terminal's half of the conversation hub, against a **real daemon** (with a scripted agent).
//!
//! Run by `backend/_hub_live_smoke_test.py`, which starts a throwaway daemon, points
//! `SIERRALAB_DAEMON_CONFIG` at it and `HUB_LIVE_CWD` at a project folder, then checks what this left
//! behind. `#[ignore]`d so a plain `cargo test` never needs Python.
//!
//!   SIERRALAB_DAEMON_CONFIG=<file> HUB_LIVE_CWD=<dir> cargo test -p acp-core --test hub_live -- --ignored --nocapture

use std::path::{Path, PathBuf};
use std::time::Duration;

use acp_core::{AppEvent, BackendConfig, BackendTransport, DaemonConfig, SessionHandle, SessionOptions, hub};
use tokio::sync::mpsc::UnboundedReceiver;

fn options(daemon: &DaemonConfig, cwd: &Path, conversation: &str) -> SessionOptions {
    SessionOptions {
        // Unused by the hub transport (the daemon spawns the agent), but the struct wants one.
        backend: BackendConfig::discover(None).unwrap_or_else(|_| BackendConfig { backend_dir: ".".into(), python: "python3".into(), server: "acp_server.py".into() }),
        cwd: cwd.to_path_buf(),
        transport: BackendTransport::Hub { daemon: daemon.clone(), conversation: conversation.into() },
        resume: None,
        debug_log: None,
        continue_latest: false,
        hub_handle: None,
    }
}

async fn until(events: &mut UnboundedReceiver<AppEvent>, mut done: impl FnMut(&AppEvent) -> bool) -> Vec<AppEvent> {
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), events.recv()).await.expect("an event in time").expect("the session is open");
        let stop = done(&event);
        seen.push(event);
        if stop {
            return seen;
        }
    }
}

#[tokio::test]
#[ignore = "needs a daemon: run backend/_hub_live_smoke_test.py"]
async fn a_terminal_opens_a_shared_conversation_and_takes_a_turn() {
    let daemon = DaemonConfig::discover().expect("SIERRALAB_DAEMON_CONFIG must point at a running daemon");
    let cwd = PathBuf::from(std::env::var("HUB_LIVE_CWD").expect("HUB_LIVE_CWD"));
    assert!(hub::reachable(&daemon).await, "the daemon answers on its port");

    let (session, mut events) = SessionHandle::spawn(options(&daemon, &cwd, "new"));
    let opened = until(&mut events, |e| matches!(e, AppEvent::Connected(_) | AppEvent::ConnectionError(_))).await;
    let handle = opened
        .iter()
        .find_map(|e| match e {
            AppEvent::Connected(info) => info.transport.strip_prefix("hub · ").map(str::to_owned),
            AppEvent::ConnectionError(error) => panic!("the conversation did not open: {error}"),
            _ => None,
        })
        .expect("connected over the hub, with the handle in the banner");
    assert!(opened.iter().any(|e| matches!(e, AppEvent::Notice(text) if text.contains(&handle) && text.contains("Discord"))), "the terminal is told the handle and that the conversation is shared");
    println!("hub: opened {handle}");

    session.send_prompt("hello hub").unwrap();
    let turn = until(&mut events, |e| matches!(e, AppEvent::TurnComplete(_) | AppEvent::TurnFailed(_))).await;
    let said: String = turn.iter().filter_map(|e| if let AppEvent::AgentText(text) = e { Some(text.as_str()) } else { None }).collect();
    assert!(said.contains("You said: hello hub"), "the agent answered through the hub: {said:?}");
    assert!(matches!(turn.last(), Some(AppEvent::TurnComplete(_))), "the turn completed: {:?}", turn.last());
    println!("hub: turn answered ({} chars)", said.len());

    // The conversation is listed for every door, with this turn counted.
    let rows = hub::list(&daemon, Some(&cwd)).await.unwrap();
    let row = rows.iter().find(|r| r.handle == handle).expect("listed");
    assert!(row.turns >= 1 && row.origin == "terminal" && !row.busy, "{row:?}");

    // A second terminal on the same conversation is refused while the first holds it.
    let (_second, mut second_events) = SessionHandle::spawn(options(&daemon, &cwd, &handle));
    let refused = until(&mut second_events, |e| matches!(e, AppEvent::Connected(_) | AppEvent::ConnectionError(_) | AppEvent::Closed)).await;
    assert!(!refused.iter().any(|e| matches!(e, AppEvent::Connected(_))), "a second terminal must not attach: {refused:?}");
    println!("hub: second terminal refused");

    // `latest` in this folder resolves to the same conversation — the default a terminal uses.
    let latest = hub::open(&daemon, "latest", &cwd).await.unwrap();
    assert_eq!(latest.handle, handle);
    println!("hub: latest is {handle}");
    session.shutdown().await;
}
