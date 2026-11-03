//! The run bus with a real Claude Code under Seatbelt: the worker reads a peer's message, posts its own and sets its
//! status, all through `cordata-bus mcp`. Spends a little of the Claude Code quota, so it runs only when asked:
//! `CORDATA_LIVE_BUS=1 cargo test -p cordata-orchestrator --test live_bus -- --nocapture` (after `make tui-build`, which
//! builds `cordata-bus`). Without the variable, the `claude` CLI or the binary, it passes without doing anything.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cordata_bus::{Bus, mcp};
use cordata_orchestrator::worker::{EventSink, TaskPacket, Worker, WorkerEvent};
use cordata_orchestrator::workers::claude;
use cordata_orchestrator::workers::cli::Seatbelt;
use cordata_orchestrator::workers::vault::Egress;
use serde_json::json;

/// No proxy: the CLI reaches its provider directly, as with the vault off.
struct Direct;
impl Egress for Direct {
    fn env(&self) -> Result<Vec<(String, String)>, String> {
        Ok(Vec::new())
    }
}

fn release_bus() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/cordata-bus");
    std::fs::canonicalize(path).ok()
}

async fn call(grant: &cordata_bus::BusGrant, op: &'static str, args: serde_json::Value) -> serde_json::Value {
    let (socket, token) = (grant.socket.display().to_string(), grant.token.clone());
    tokio::task::spawn_blocking(move || mcp::call(&socket, &token, op, &args)).await.unwrap().unwrap()
}

#[tokio::test]
async fn a_real_claude_code_worker_talks_through_the_bus() {
    if std::env::var_os("CORDATA_LIVE_BUS").is_none_or(|v| v != "1") {
        eprintln!("skipped: set CORDATA_LIVE_BUS=1 to run against the real claude CLI");
        return;
    }
    let Some(command) = release_bus() else {
        eprintln!("skipped: build cordata-bus first (make tui-build)");
        return;
    };
    let environ: HashMap<String, String> = std::env::vars().collect();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("notes.txt"), "notes\n").unwrap();

    // The real place: under ~/.cordata, which the worker's profile denies.
    let bus = Bus::start(&cordata_bus::default_root(), "live", &command).unwrap();
    eprintln!("bus at {}", bus.socket().display());
    let peer = bus.grant("s0", "execute", "peer:test", &[]).unwrap();
    peer.pid.set(std::process::id());
    let said = call(&peer, "post", json!({"topic": "run", "kind": "finding", "body": "the service listens on port 8080"})).await;
    assert_eq!(said["ok"], true, "{said}");

    let mut task = TaskPacket::new("live:s1:1", "execute", "", &project.path().to_string_lossy());
    task.file_scope = vec!["notes.txt".into()];
    task.timeout_s = 240.0;
    task.model = Some("haiku".into());
    task.prompt = "This is a connectivity test of the run bus; do not edit any file. Do exactly this, in order: \
1) call the cordata_bus `status` tool with text \"live test\"; \
2) call the cordata_bus `read` tool with since 0; \
3) call the cordata_bus `post` tool with topic \"run\", kind \"answer\", body \"claude heard: \" followed by the body of the message from step s0; \
4) reply with exactly the body of the message from step s0."
        .into();
    let grant = bus.grant("s1", "execute", "claude:haiku", &task.file_scope).unwrap();
    task.bus = Some(grant.clone());

    let worker = claude::worker(Arc::new(Direct)).environ(environ).seatbelt(Seatbelt::Auto);
    let seen = Arc::new(Mutex::new(Vec::<WorkerEvent>::new()));
    let log = seen.clone();
    let sink: EventSink = Arc::new(move |e| log.lock().unwrap().push(e));
    let result = worker.run(task, sink).await;
    for e in seen.lock().unwrap().iter().filter(|e| matches!(e.event_type.as_str(), "init" | "tool_use" | "tool_result" | "error")) {
        eprintln!("{} {}", e.event_type, e.payload);
    }
    eprintln!("status={} error={:?}\nanswer={}", result.status, result.error_message, result.raw_output);
    let messages = bus.messages();
    eprintln!("bus: {}", serde_json::to_string_pretty(&messages).unwrap());
    let peers = call(&peer, "peers", json!({})).await;
    eprintln!("peers: {peers}");

    assert_eq!(result.status, "completed", "{:?}", result.error_message);
    assert_ne!(grant.pid.get(), 0);
    let from_claude: Vec<_> = messages.iter().filter(|m| m.from_step == "s1").collect();
    assert_eq!(from_claude.len(), 1, "claude posted once, stamped as its own step");
    assert!(from_claude[0].body.contains("8080"), "it read the peer's message: {}", from_claude[0].body);
    assert!(peers["statuses"].as_array().unwrap().iter().any(|s| s["step"] == "s1" && s["status"] == "live test"));
    assert!(result.raw_output.contains("8080"));
}

/// The shim binary itself, under the worker's real Seatbelt profile, against a bus in `~/.cordata/bus`: what Claude
/// Code's MCP child does, minus the model. Needs no account, but writes under the real `~/.cordata`, so it is opt-in too.
#[tokio::test]
async fn the_shim_reaches_its_bus_from_inside_the_worker_sandbox_and_only_with_the_allowance() {
    if std::env::var_os("CORDATA_LIVE_BUS").is_none_or(|v| v != "1") {
        eprintln!("skipped: set CORDATA_LIVE_BUS=1");
        return;
    }
    let Some(command) = release_bus() else {
        eprintln!("skipped: build cordata-bus first (make tui-build)");
        return;
    };
    if !cordata_orchestrator::workers::cli::seatbelt_works() {
        eprintln!("skipped: sandbox-exec cannot apply a profile here");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(project.path()).unwrap().to_string_lossy().into_owned();
    let home = std::env::var("HOME").unwrap();
    let bus = Bus::start(&cordata_bus::default_root(), "shim", &command).unwrap();
    let socket = bus.socket().display().to_string();
    let peer = bus.grant("s0", "execute", "peer", &[]).unwrap();
    peer.pid.set(std::process::id());
    call(&peer, "post", json!({"topic": "run", "kind": "finding", "body": "port 8080"})).await;

    let session = |bus_allowed: bool, grant: cordata_bus::BusGrant| {
        let profile = cordata_orchestrator::scope::scoped_profile_with_bus(&root, &["notes.txt"], &home, bus_allowed.then_some(socket.as_str()));
        let command = command.clone();
        let root = root.clone();
        async move {
            let input = [
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "status", "arguments": {"text": "sandboxed"}}}),
                json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "read", "arguments": {"since": 0}}}),
                json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "post", "arguments": {"topic": "run", "kind": "answer", "body": "heard 8080"}}}),
            ]
            .iter()
            .map(|v| v.to_string() + "\n")
            .collect::<String>();
            let mut child = tokio::process::Command::new("/usr/bin/sandbox-exec")
                .args(["-p", &profile, &command.display().to_string(), "mcp"])
                .env_clear()
                .envs(grant.env())
                .current_dir(&root)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            grant.pid.set(child.id().unwrap());
            use tokio::io::AsyncWriteExt;
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(input.as_bytes()).await.unwrap();
            drop(stdin);
            let out = child.wait_with_output().await.unwrap();
            let replies: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
            (replies, String::from_utf8_lossy(&out.stderr).into_owned())
        }
    };

    let (replies, stderr) = session(true, bus.grant("s1", "execute", "shim", &["notes.txt".into()]).unwrap()).await;
    eprintln!("allowed: {replies:#?}\n{stderr}");
    assert_eq!(replies.len(), 4, "{stderr}");
    assert_eq!(replies[1]["result"]["isError"], false, "status: {}", replies[1]);
    let read = replies[2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(read.contains("port 8080") && read.contains("<peer-messages>"), "{read}");
    assert_eq!(replies[3]["result"]["isError"], false);
    assert!(bus.messages().iter().any(|m| m.from_step == "s1" && m.body == "heard 8080"), "stamped as the sandboxed step");

    let (replies, _) = session(false, bus.grant("s2", "execute", "shim", &["notes.txt".into()]).unwrap()).await;
    let refused = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
    eprintln!("without the allowance: {refused}");
    assert_eq!(replies[1]["result"]["isError"], true);
    assert!(refused.contains("not reachable"), "{refused}");
}
