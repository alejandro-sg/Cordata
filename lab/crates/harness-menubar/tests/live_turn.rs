//! The deck's turn against the **real Python bridge**: `harness-capture` → `harness-client` →
//! `SubmitTurn` → the daemon's log and ledger.
//!
//! The capture is read from a fake OS source (CI has no Safari and no Accessibility grant), but
//! everything after it is the real path the deck takes: the allowlist verdict, client-side masking,
//! `Preview::to_context`, the gRPC client, the bridge's policy re-check, the hand-off to the session,
//! the audit row and the log line. Run by `backend/_lab_gui_live_smoke_test.py` (`make lab-live`),
//! which then reads the demo daemon's `daemon.log` and `state.db` for what this left behind.
//!
//!   CORDATA_HOME=<demo home> cargo test -p harness-menubar --test live_turn -- --ignored --nocapture

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use harness_capture::fake::FakeCaptureSource;
use harness_capture::{Allowlist, CaptureSource as _, capture_app};
use harness_client::{ClientConfig, ClientEvent, Command, DaemonConfig, DeckContext, Handle, LabModel, Reply, spawn};

/// Pump the client into the model until `done` says so, or the time runs out.
fn pump_until(handle: &Handle, model: &mut LabModel, timeout: Duration, mut done: impl FnMut(&LabModel, &[ClientEvent]) -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        let events = handle.inbox().drain();
        let seen = events.clone();
        for event in events {
            model.apply(event, Instant::now());
        }
        if done(model, &seen) {
            return true;
        }
        if Instant::now() > end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The turn reply among this batch of events, if one arrived.
fn turn_reply(events: &[ClientEvent]) -> Option<harness_proto::v1::TurnReply> {
    events.iter().find_map(|event| match event {
        ClientEvent::Reply(Reply::Turn { reply }) => Some(reply.clone()),
        _ => None,
    })
}

#[test]
#[ignore = "needs the demo daemon: run `make lab-live`"]
fn a_captured_turn_reaches_the_daemon_masked_and_a_password_manager_never_does() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let daemon = DaemonConfig::discover().expect("CORDATA_HOME must point at a running demo daemon");
    let handle = spawn(ClientConfig::new(daemon).with_name("harness-menubar"), runtime.handle());
    let mut model = LabModel::default();
    assert!(pump_until(&handle, &mut model, Duration::from_secs(20), |m, _| m.connection.is_online() && m.focus.is_some()), "never came online with a session: {:?}", model.connection);
    let session = model.focus.clone().unwrap();

    // ---- the capture the deck would make over Safari, allowed by name ------------------------------
    let safari = FakeCaptureSource::new("Safari", "com.apple.Safari")
        .with_title("GitHub PR #123 — key is sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUV-0123456789")
        .with_selection("raise the deck timeout to 30s");
    let target = safari.frontmost().unwrap();
    let capture = capture_app(&safari, &target, &Allowlist::of(["com.apple.Safari"]));
    let captured_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    let context = capture.preview().expect("Safari is allowed").to_context(captured_ms);
    assert!(!context.window_title.contains("ABCDEFGHIJKLMNOPQRSTUV"), "masked before it left the process: {}", context.window_title);

    handle.send(Command::SubmitTurn { session_id: session.clone(), text: "what am I looking at?".into(), context, client: "harness-menubar".into(), conversation: String::new(), cwd: String::new() }).unwrap();
    let mut reply = None;
    assert!(
        pump_until(&handle, &mut model, Duration::from_secs(15), |_, events| {
            reply = reply.clone().or_else(|| turn_reply(events));
            reply.is_some()
        }),
        "the bridge never answered the turn: {:?}",
        model.notices
    );
    let reply = reply.unwrap();
    assert!(reply.ok, "the turn was refused: {}", reply.error);
    assert_eq!(reply.turn_id.len(), 12);
    println!("menubar turn: route={} turn={} notices={:?}", reply.route, reply.turn_id, reply.notices);

    // ---- a scripted client that skips the Rust policy is still refused by the daemon ---------------
    let bypass = DeckContext { app_name: "1Password".into(), app_bundle_id: "com.1password.1password".into(), window_title: "Vault — 214 items".into(), ..Default::default() };
    handle.send(Command::SubmitTurn { session_id: session, text: "read this".into(), context: bypass, client: "scripted".into(), conversation: String::new(), cwd: String::new() }).unwrap();
    let mut refused = None;
    assert!(
        pump_until(&handle, &mut model, Duration::from_secs(15), |_, events| {
            refused = refused.clone().or_else(|| turn_reply(events));
            refused.is_some()
        }),
        "the bridge never answered the bypass"
    );
    let refused = refused.unwrap();
    assert!(!refused.ok && refused.error.contains("never-read"), "a never-read app must be refused by the daemon too: {refused:?}");
    println!("menubar bypass: refused ({})", refused.error);
    assert!(model.connection.is_online(), "a refusal is a reply, not an outage");
}
