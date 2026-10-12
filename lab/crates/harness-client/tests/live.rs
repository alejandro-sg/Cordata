//! The Rust client against the **real Python bridge** — not the Rust fake.
//!
//! `tests/client.rs` proves the client against a scripted fake daemon, and `backend/_grpc_bridge_smoke_test.py`
//! proves the bridge against a Python client. Neither proves the two *agree*: this does. Run by
//! `backend/_lab_gui_live_smoke_test.py` (`make lab-live`), which starts the demo daemon
//! (`backend/lab_demo.py`) in a throwaway home, points `CORDATA_HOME` at it and then checks the
//! ledger this test leaves behind. It is `#[ignore]`d so a plain `cargo test` never needs Python.
//!
//!   CORDATA_HOME=<demo home> cargo test -p harness-client --test live -- --ignored --nocapture

use std::time::{Duration, Instant};

use harness_client::{ClientConfig, ClientEvent, Command, DaemonConfig, Handle, Knob, LabModel, PodControl, Reply, spawn};
use harness_proto::v1::{AstSnippet, Outcome, PodStatus};

/// What the "UI thread" measures about itself while it pumps: if a thousand updates ever made it
/// wait, these numbers say so.
#[derive(Default)]
struct Meter {
    max_backlog: usize,
    max_drain: Duration,
    max_apply: Duration,
    frames: u64,
}

thread_local! {
    static METER: std::cell::RefCell<Meter> = std::cell::RefCell::new(Meter::default());
}

fn pump_until(handle: &Handle, model: &mut LabModel, timeout: Duration, mut done: impl FnMut(&LabModel, &[ClientEvent]) -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        let backlog = handle.inbox().len();
        let started = Instant::now();
        let events = handle.inbox().drain();
        let drained = started.elapsed();
        let seen = events.clone();
        let mut slowest = Duration::ZERO;
        for event in events {
            let applying = Instant::now();
            model.apply(event, Instant::now());
            slowest = slowest.max(applying.elapsed());
        }
        METER.with(|m| {
            let mut m = m.borrow_mut();
            m.max_backlog = m.max_backlog.max(backlog);
            m.max_drain = m.max_drain.max(drained);
            m.max_apply = m.max_apply.max(slowest);
            m.frames = model.stats.frames;
        });
        if done(model, &seen) {
            return true;
        }
        if Instant::now() > end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pod<'a>(model: &'a LabModel, id: &str) -> Option<&'a harness_proto::v1::Pod> {
    model.lab()?.pods.iter().find(|p| p.id == id)
}

#[test]
#[ignore = "needs the demo daemon: run `make lab-live`"]
fn the_rust_client_and_the_python_bridge_agree() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let daemon = DaemonConfig::discover().expect("CORDATA_HOME must point at a running demo daemon");
    let handle = spawn(ClientConfig::new(daemon), runtime.handle());
    let mut model = LabModel::default();

    // ---- the first frame makes the picture whole --------------------------------------------
    assert!(
        pump_until(&handle, &mut model, Duration::from_secs(20), |m, _| m.connection.is_online() && m.lab().is_some_and(|l| l.pods.len() >= 3) && m.omlx.is_some() && m.daemon.is_some()),
        "never came online with pods, memory and daemon state: {:?}",
        model.connection
    );
    let omlx = model.omlx.clone().unwrap();
    assert!(omlx.up && omlx.budget_gb > 30.0 && omlx.models.iter().any(|m| m.loaded), "{omlx:?}");
    assert_eq!(model.daemon.as_ref().unwrap().version, "1");
    println!("online: {} pods, {} models, budget {:.1} GB", model.lab().unwrap().pods.len(), omlx.models.len(), omlx.budget_gb);

    // ---- the knobs reach the child, and a refusal is a reply, not an outage ------------------
    let session = model.focus.clone().expect("a focused session");
    let send_knob = |pod: &str, knob: Knob| handle.send(Command::SetKnob { session_id: session.clone(), pod: pod.into(), knob }).unwrap();
    send_knob("orchestrator", Knob::ReasoningEffort("high".into()));
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| pod(m, "orchestrator").is_some_and(|p| p.effort == "high")), "the effort knob never showed up in the telemetry");
    send_knob("architect", Knob::ReasoningEffort("high".into())); // a model with no such knob
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| m.notices.iter().any(|n| n.text.contains("architect") && n.text.contains("refused"))), "{:?}", model.notices);
    assert!(model.connection.is_online(), "a refused knob must not look like a dead daemon");
    send_knob("debugger", Knob::LayaDial(0.05)); // clamped up to 0.30 by the bridge, so a client can never loosen — or over-tighten — the gate
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| pod(m, "debugger").is_some_and(|p| (p.dial - 0.30).abs() < 1e-6)), "dial: {:?}", pod(&model, "debugger").map(|p| p.dial));
    send_knob("debugger", Knob::LayaDial(0.95)); // a value above the gate is "no dial", never a looser gate
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| pod(m, "debugger").is_some_and(|p| (p.dial - 0.80).abs() < 1e-6)));
    println!("knobs: effort applied, unsupported refused inline, dial clamped to [0.30, 0.80]");

    // ---- autonomy: the mode is the daemon's, a refusal is a reply, and the team can be told things ------
    assert_eq!(model.lab().unwrap().mode, "build", "the session starts in build mode");
    assert!(!model.lab().unwrap().mission.is_empty(), "the team's mission rides on telemetry");
    handle.send(Command::SetMode { session_id: session.clone(), mode: "auto".into() }).unwrap();
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| m.lab().is_some_and(|l| l.mode == "auto")), "the mode change never showed up in telemetry");
    handle.send(Command::SetMode { session_id: session.clone(), mode: "yolo".into() }).unwrap();
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| m.notices.iter().any(|n| n.text.contains("mode not changed"))), "{:?}", model.notices);
    assert!(model.lab().unwrap().mode == "auto" && model.connection.is_online(), "a refused mode changes nothing and is not an outage");
    handle.send(Command::SetMode { session_id: session.clone(), mode: "build".into() }).unwrap(); // the rest of the scenario asks in build mode
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| m.lab().is_some_and(|l| l.mode == "build")));
    assert!(model.feed.iter().any(|f| f.text == "Mode is now auto"), "switching to auto is in the feed");
    println!("autonomy: build → auto → build (yolo refused as a reply)");

    let control = |pod: &str, control: PodControl| handle.send(Command::ControlPod { session_id: session.clone(), pod: pod.into(), control }).unwrap();
    control("claude_code_engineer", PodControl::Steer("focus on the failing tests first".into()));
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| pod(m, "claude_code_engineer").is_some_and(|p| p.guidance_pending >= 1)), "the directive was never queued");
    control("architect", PodControl::Isolate);
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| pod(m, "architect").is_some_and(|p| p.isolated)), "Stop did not stop the agent");
    control("architect", PodControl::Steer("hello".into())); // a stopped agent refuses to be steered, as a reply
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| m.notices.iter().any(|n| n.text.contains("architect") && n.text.contains("refused"))), "{:?}", model.notices);
    control("architect", PodControl::Release);
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| pod(m, "architect").is_some_and(|p| !p.isolated)), "Release did not release the agent");
    control("nobody", PodControl::Isolate);
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| m.notices.iter().any(|n| n.text.contains("nobody") && n.text.contains("refused"))));
    assert!(model.feed.iter().any(|f| f.text == "You stopped architect") && model.feed.iter().any(|f| f.text == "You released architect"), "{:?}", model.feed.iter().map(|f| f.text.clone()).collect::<Vec<_>>());
    println!("agents: steered, stopped, refused while stopped, released; an unknown agent is a reply");

    // ---- an armed, destructive request: masked, rendered, rejected and isolated -------------
    assert!(pump_until(&handle, &mut model, Duration::from_secs(60), |m, _| m.current_intervention().is_some()), "the scenario never asked for an approval");
    let open = model.current_intervention().unwrap().clone();
    let request = &open.request;
    assert!(request.armed && request.risk_score.is_some_and(|s| s >= 0.9) && request.risk_band == "destructive", "{request:?}");
    assert!(request.command.contains("git clean -fdx"), "the exact command is shown: {}", request.command);
    assert!(!request.command.contains("ghp_"), "a credential reached the screen: {}", request.command);
    assert!(request.redactions >= 1, "the mask was not counted");
    assert!(request.reason.contains("Laya Guard"), "the prompt says it was Laya Guard that held it: {}", request.reason);
    println!("modal: pod={} tool={} risk={:?} redactions={}", request.pod, request.tool, request.risk_score, request.redactions);
    let scored = model.lab().unwrap().pods.iter().find(|p| p.risk_last.is_some()).expect("no agent carries a Laya Guard reading yet");
    assert!(harness_client::words::guard_line(scored).is_some() && scored.risk_peak > 0.0, "Laya Guard's per-agent reading rides on telemetry");
    handle.send(Command::Decide { intervention_id: request.intervention_id.clone(), approve: false, isolate: true, note: "live test".into() }).unwrap();
    assert!(
        pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| m.resolved.iter().any(|r| r.intervention_id == request.intervention_id && r.outcome == Outcome::Rejected)),
        "the rejection never came back resolved"
    );
    assert!(model.current_intervention().is_none(), "the modal should be gone");
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| pod(m, "debugger").is_some_and(|p| p.isolated || p.status == PodStatus::Isolated as i32)), "Reject & Isolate did not isolate the pod");
    println!("rejected + isolated: debugger isolated, modal dismissed");

    // ---- a second request, approved --------------------------------------------------------------
    assert!(pump_until(&handle, &mut model, Duration::from_secs(60), |m, _| m.current_intervention().is_some()), "no second approval");
    let second = model.current_intervention().unwrap().request.clone();
    assert!(!second.diff.is_empty() && second.diff.contains("@@"), "the diff payload is shown: {second:?}");
    handle.send(Command::Decide { intervention_id: second.intervention_id.clone(), approve: true, isolate: false, note: "live test".into() }).unwrap();
    assert!(pump_until(&handle, &mut model, Duration::from_secs(10), |m, _| m.resolved.iter().any(|r| r.intervention_id == second.intervention_id && r.outcome == Outcome::Approved)));
    println!("approved: the suspended agent resumed");
    assert!(
        pump_until(&handle, &mut model, Duration::from_secs(30), |m, _| pod(m, "claude_code_engineer").is_some_and(|p| p.guidance_pending == 0 && p.trace.iter().any(|t| t.tool == "GUIDANCE"))),
        "the directive was never read by the agent it was sent to"
    );
    println!("steer: the agent read the directive before its next call");

    // ---- the engineer's diff: referenced in telemetry, fetched whole on demand -----------------------
    assert!(pump_until(&handle, &mut model, Duration::from_secs(60), |m, _| !m.diffs.is_empty()), "no diff was referenced");
    let diff = model.diffs[0].clone();
    handle.send(Command::FetchDiff { id: diff.id.clone() }).unwrap();
    let mut body_lines = 0usize;
    assert!(
        pump_until(&handle, &mut model, Duration::from_secs(10), |_, events| {
            for event in events {
                if let ClientEvent::Reply(Reply::Diff(artifact)) = event {
                    body_lines = artifact.unified.lines().count();
                }
            }
            body_lines > 0
        }),
        "the diff body never arrived"
    );
    assert!(body_lines >= 10_000, "only {body_lines} lines of a 10,000-line diff");
    println!("diff {}: {body_lines} lines fetched", diff.id);

    // ---- the stress gate on the real wire: 1,000 updates back to back ------------------------------------
    let seq_before = model.stats.last_lab_seq;
    assert!(
        pump_until(&handle, &mut model, Duration::from_secs(40), |m, _| pod(m, "orchestrator").is_some_and(|p| p.stream_tail.contains("burst 1000/1000"))),
        "the burst never finished: {:?}",
        pod(&model, "orchestrator").map(|p| p.stream_tail.clone())
    );
    METER.with(|m| {
        let m = m.borrow();
        println!("stress: {} frames applied of 1000 sent, lab seq advanced by {}; inbox backlog ≤ {}, slowest drain {:?}, slowest apply {:?}", m.frames, model.stats.last_lab_seq.saturating_sub(seq_before), m.max_backlog, m.max_drain, m.max_apply);
        assert!(m.max_backlog <= 6, "telemetry queued {} frames on the UI side: it must conflate", m.max_backlog);
        assert!(m.max_drain < Duration::from_millis(8), "draining took {:?} on the UI thread", m.max_drain);
        assert!(m.max_apply < Duration::from_millis(8), "applying a frame took {:?} on the UI thread", m.max_apply);
    });
    assert!(model.connection.is_online(), "a thousand updates took the connection down");

    // ---- the proactive observer: save a file, parse it locally, send only what changed ----------------------
    {
        use harness_observer::{Change, Observer, ObserverConfig};
        let watched = tempfile::tempdir().unwrap();
        let root = watched.path().canonicalize().unwrap();
        let sender = handle.inbox().clone(); // keeps the type honest: the sink below only needs `handle`
        drop(sender);
        let (tx, rx) = std::sync::mpsc::channel::<Vec<harness_observer::Snippet>>();
        let mut observer_config = ObserverConfig::new(&root);
        observer_config.quiet = Duration::from_millis(150);
        observer_config.max_wait = Duration::from_millis(800);
        let observer = Observer::start(observer_config, move |batch| {
            let _ = tx.send(batch);
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let v1 = "pub fn first_fn() -> u32 {\n    1\n}\n\npub fn second_fn() -> u32 {\n    2\n}\n";
        std::fs::write(root.join("lib.rs"), v1).unwrap();
        let added = rx.recv_timeout(Duration::from_secs(5)).expect("the first save was not noticed");
        assert_eq!(added.len(), 2, "a new file reports each symbol once");
        std::fs::write(root.join("lib.rs"), v1.replace("    2\n", "    22\n")).unwrap();
        let changed = rx.recv_timeout(Duration::from_secs(5)).expect("the second save was not noticed");
        assert_eq!(changed.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["second_fn"], "only the modified AST node is extracted");
        assert_eq!(changed[0].change, Change::Modified);
        let accepted_before = model.daemon.as_ref().map_or(0, |d| d.ingest_accepted);
        for snippet in added.iter().chain(changed.iter()) {
            assert!(handle.ingest(AstSnippet {
                workspace: snippet.workspace.to_string_lossy().into_owned(),
                path: snippet.path.clone(),
                language: snippet.language.to_owned(),
                node_kind: snippet.node_kind.clone(),
                name: snippet.name.clone(),
                start_line: snippet.start_line,
                end_line: snippet.end_line,
                text: snippet.text.clone(),
                content_hash: snippet.content_hash.clone(),
                change: if snippet.change == Change::Added { "added".into() } else { "modified".into() },
                saved_ms: snippet.saved_ms,
            }));
        }
        assert!(
            pump_until(&handle, &mut model, Duration::from_secs(15), |m, _| m.daemon.as_ref().is_some_and(|d| d.ingest_accepted >= accepted_before + 3)),
            "the daemon accepted {:?} of 3 snippets",
            model.daemon.as_ref().map(|d| d.ingest_accepted - accepted_before.min(d.ingest_accepted))
        );
        observer.stop();
        println!("observer: saved a file, tree-sitter extracted 2 + 1 changed symbols, the daemon accepted them");
    }

    // ---- proactive ingest: a changed symbol reaches the daemon's ledger --------------------------------
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("lib.rs"), "pub fn changed() -> u32 { 42 }\n").unwrap();
    assert!(handle.ingest(AstSnippet {
        workspace: workspace.path().canonicalize().unwrap().to_string_lossy().into_owned(),
        path: "lib.rs".into(),
        language: "rust".into(),
        node_kind: "function_item".into(),
        name: "changed".into(),
        start_line: 1,
        end_line: 1,
        text: "pub fn changed() -> u32 { 42 }".into(),
        content_hash: "livetest".into(),
        change: "modified".into(),
        saved_ms: 1,
    }));
    assert!(pump_until(&handle, &mut model, Duration::from_secs(15), |m, _| m.daemon.as_ref().is_some_and(|d| d.ingest_accepted >= 1)), "the daemon never counted the snippet");
    println!("ingest: accepted by the daemon ({} so far)", model.daemon.as_ref().unwrap().ingest_accepted);

    // ---- the emergency unload ------------------------------------------------------------------------------
    handle.send(Command::UnloadMemory { reason: "live test".into(), include_ollama: false }).unwrap();
    assert!(
        pump_until(&handle, &mut model, Duration::from_secs(15), |m, _| m.omlx.as_ref().is_some_and(|o| o.loaded_gb < 0.1) && m.notices.iter().any(|n| n.text.contains("memory unloaded"))),
        "weights never left: {:?} / {:?}",
        model.omlx.as_ref().map(|o| o.loaded_gb),
        model.notices
    );
    println!("unload: oMLX now holds {:.1} GB", model.omlx.as_ref().unwrap().loaded_gb);
    assert!(model.connection.is_online());
}
