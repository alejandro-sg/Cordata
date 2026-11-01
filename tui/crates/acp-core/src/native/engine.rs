//! The native agent loop: history in memory, a streamed model turn, tool calls through the
//! permission gate, repeat until the model answers without a tool. Commands are multiplexed
//! against every await so `Cancel` lands mid-token and mid-command.

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use super::approval::{self, Verdict};
use super::provider::{Call, Delta, GenaiProvider, Msg, Provider};
use super::tools;
use crate::events::{AppEvent, SessionInfo, StopReason, ToolCallInfo, ToolCallPatch, ToolStatus};
use crate::permission::ApprovalDecision;
use crate::session::{Attachment, Command, SessionOptions};

const DEFAULT_MODEL: &str = "claude-sonnet-5-5";
/// Model round-trips per prompt before the turn is cut off.
const MAX_ROUNDS: usize = 25;
const SYSTEM_PROMPT: &str = "You are Cordata, a coding agent running in the user's terminal. \
Answer concisely. Use the tools to read, search and edit the project and to run commands; \
edits and commands are shown to the user for approval first.";

type Events = mpsc::UnboundedSender<AppEvent>;
type Commands = mpsc::UnboundedReceiver<Command>;

/// `CORDATA_NATIVE_MODEL` picks the model (a genai name: `claude-…`, `gemini-…`, `gpt-…`, or any Ollama tag).
pub(crate) async fn run(options: SessionOptions, commands: Commands, events: Events) {
    let model = std::env::var("CORDATA_NATIVE_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    match GenaiProvider::new(model) {
        Ok(provider) => drive(&provider, options, commands, events, tools::offload_dir()).await,
        Err(error) => {
            let _ = events.send(AppEvent::ConnectionError(format!("could not start the native agent: {error}")));
            let _ = events.send(AppEvent::Closed);
        }
    }
}

enum Interrupt {
    Cancel,
    Shutdown,
}

/// Awaits `fut`, but gives up the moment the user cancels or the session is shut down.
async fn interruptible<F: Future>(fut: F, commands: &mut Commands, events: &Events) -> Result<F::Output, Interrupt> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(Command::Cancel) => return Err(Interrupt::Cancel),
                Some(Command::Shutdown) | None => return Err(Interrupt::Shutdown),
                Some(Command::Prompt(..)) => {
                    let _ = events.send(AppEvent::Notice("a turn is still running; wait or cancel it first".into()));
                }
                Some(_) => {}
            },
            out = &mut fut => return Ok(out),
        }
    }
}

struct Agent<'a> {
    provider: &'a dyn Provider,
    cwd: PathBuf,
    offload: PathBuf,
    /// Tools the user chose "always allow" for, this session.
    always: HashSet<String>,
}

pub(crate) async fn drive(provider: &dyn Provider, options: SessionOptions, mut commands: Commands, events: Events, offload: PathBuf) {
    if options.resume.is_some() || options.continue_latest {
        let _ = events.send(AppEvent::Notice("native sessions are not persisted yet; starting fresh".into()));
    }
    let _ = events.send(AppEvent::Connected(SessionInfo {
        session_id: format!("native-{}", std::process::id()),
        cwd: options.cwd.display().to_string(),
        transport: format!("native · {}", provider.model()),
        resumed: false,
        current_mode: String::new(),
        modes: Vec::new(),
        config_options: Vec::new(),
    }));

    let mut agent = Agent { provider, cwd: options.cwd.clone(), offload, always: HashSet::new() };
    let mut history: Vec<Msg> = Vec::new();
    while let Some(command) = commands.recv().await {
        match command {
            Command::Prompt(text, attachments) => {
                history.push(Msg::User(with_attachments(text, &attachments)));
                if let Err(Interrupt::Shutdown) = agent.turn(&mut history, &mut commands, &events).await {
                    break;
                }
            }
            Command::Cancel => {}
            Command::SetMode(_) | Command::SetConfigOption { .. } => {
                let _ = events.send(AppEvent::Notice("modes and config options are not available in native sessions yet".into()));
            }
            Command::Ext { tag, .. } => {
                let _ = events.send(AppEvent::ExtResult { tag, result: Err("not supported by native sessions".into()) });
            }
            Command::Shutdown => break,
        }
    }
    let _ = events.send(AppEvent::Closed);
}

impl Agent<'_> {
    /// One prompt: model round-trips until an answer with no tool calls. `Err(Shutdown)` ends the session;
    /// a cancel is reported to the UI and returns `Ok`.
    async fn turn(&mut self, history: &mut Vec<Msg>, commands: &mut Commands, events: &Events) -> Result<(), Interrupt> {
        let specs = tools::specs();
        for _ in 0..MAX_ROUNDS {
            let stream = match interruptible(self.provider.stream(SYSTEM_PROMPT, history.clone(), &specs), commands, events).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => return Ok(self.fail(history, events, error)),
                Err(interrupt) => return self.interrupted(interrupt, history, String::new(), events),
            };
            let mut stream = stream;
            let (mut text, mut calls) = (String::new(), Vec::<Call>::new());
            loop {
                let item = match interruptible(stream.next(), commands, events).await {
                    Ok(item) => item,
                    Err(interrupt) => return self.interrupted(interrupt, history, text, events),
                };
                match item {
                    Some(Ok(Delta::Text(t))) => {
                        text.push_str(&t);
                        let _ = events.send(AppEvent::AgentText(t));
                    }
                    Some(Ok(Delta::Thought(t))) => {
                        let _ = events.send(AppEvent::AgentThought(t));
                    }
                    Some(Ok(Delta::Call(call))) => calls.push(call),
                    Some(Err(error)) => {
                        if !text.is_empty() {
                            history.push(Msg::Assistant { text, calls: Vec::new() });
                        }
                        return Ok(self.fail(history, events, error));
                    }
                    None => break,
                }
            }
            if calls.is_empty() {
                if !text.is_empty() {
                    history.push(Msg::Assistant { text, calls });
                }
                let _ = events.send(AppEvent::TurnComplete(StopReason::EndTurn));
                return Ok(());
            }
            history.push(Msg::Assistant { text, calls: calls.clone() });
            for (index, call) in calls.iter().enumerate() {
                match self.run_call(call, commands, events).await {
                    Ok(output) => history.push(Msg::ToolResult { id: call.id.clone(), output }),
                    Err(interrupt) => {
                        // Every call the model made needs a result, or the next request is malformed.
                        for rest in &calls[index..] {
                            history.push(Msg::ToolResult { id: rest.id.clone(), output: "cancelled by the user".into() });
                        }
                        return self.interrupted(interrupt, history, String::new(), events);
                    }
                }
            }
        }
        let _ = events.send(AppEvent::TurnComplete(StopReason::MaxTurnRequests));
        Ok(())
    }

    /// A failed round-trip: the prompt that had no answer is dropped so the next one doesn't follow it.
    fn fail(&self, history: &mut Vec<Msg>, events: &Events, error: String) {
        if matches!(history.last(), Some(Msg::User(_))) {
            history.pop();
        }
        let _ = events.send(AppEvent::TurnFailed(error));
    }

    fn interrupted(&self, interrupt: Interrupt, history: &mut Vec<Msg>, partial: String, events: &Events) -> Result<(), Interrupt> {
        if !partial.is_empty() {
            history.push(Msg::Assistant { text: partial, calls: Vec::new() });
        }
        match interrupt {
            Interrupt::Cancel => {
                let _ = events.send(AppEvent::TurnComplete(StopReason::Cancelled));
                Ok(())
            }
            Interrupt::Shutdown => Err(Interrupt::Shutdown),
        }
    }

    /// Announces the call, asks for approval when it mutates, runs it, reports the result.
    /// The returned text is what the model sees; failures are results, not errors.
    async fn run_call(&mut self, call: &Call, commands: &mut Commands, events: &Events) -> Result<String, Interrupt> {
        let diffs = tools::preview(&call.name, &call.args, &self.cwd).unwrap_or_default();
        let _ = events.send(AppEvent::ToolCallStarted(ToolCallInfo {
            id: call.id.clone(),
            name: call.name.clone(),
            kind: tools::kind_of(&call.name),
            status: ToolStatus::Pending,
            raw_input: Some(call.args.clone()),
            content: String::new(),
            untrusted_source: false,
            untrusted: false,
            diffs,
        }));
        let patch = |status, content: Option<String>| {
            AppEvent::ToolCallUpdated(ToolCallPatch { id: call.id.clone(), status: Some(status), content, ..Default::default() })
        };

        if tools::needs_approval(&call.name) && !self.always.contains(&call.name) {
            let (tx, rx) = oneshot::channel::<ApprovalDecision>();
            let _ = events.send(AppEvent::PermissionRequested { info: approval::request(&call.id, &call.name, &call.args), decision: tx });
            // A dropped sender counts as a rejection.
            let decision = interruptible(rx, commands, events).await?.unwrap_or(ApprovalDecision::Reject);
            match approval::interpret(decision) {
                Verdict::Allow { always } => {
                    if always {
                        self.always.insert(call.name.clone());
                    }
                }
                Verdict::Deny(reason) => {
                    let _ = events.send(patch(ToolStatus::Failed, Some(reason.clone())));
                    return Ok(reason);
                }
            }
        }

        let _ = events.send(patch(ToolStatus::InProgress, None));
        let result = interruptible(tools::execute(&call.name, &call.args, &self.cwd), commands, events).await;
        let (status, output) = match result {
            Err(interrupt) => {
                let _ = events.send(patch(ToolStatus::Failed, Some("cancelled".into())));
                return Err(interrupt);
            }
            Ok(Ok(out)) => (ToolStatus::Completed, out),
            Ok(Err(error)) => (ToolStatus::Failed, format!("error: {error}")),
        };
        let output = tools::limit_output(output, &self.offload);
        let _ = events.send(patch(status, Some(output.clone())));
        Ok(output)
    }
}

/// Text files travel inline; images need a vision-capable request shape and are skipped for now.
fn with_attachments(mut text: String, attachments: &[Attachment]) -> String {
    for attachment in attachments {
        if let Attachment::Text { uri, text: body } = attachment {
            text.push_str(&format!("\n\n<file uri=\"{uri}\">\n{body}\n</file>"));
        }
    }
    text
}


#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use futures::future::BoxFuture;
    use futures::stream::{self, BoxStream};
    use serde_json::json;

    use super::super::tools::ToolSpec;
    use super::*;

    /// Replays one scripted round per `stream` call and records the history it was given.
    struct Fake {
        rounds: Mutex<VecDeque<Vec<Result<Delta, String>>>>,
        seen: Mutex<Vec<Vec<Msg>>>,
    }

    impl Fake {
        fn new(rounds: Vec<Vec<Result<Delta, String>>>) -> Self {
            Self { rounds: Mutex::new(rounds.into()), seen: Mutex::new(Vec::new()) }
        }
    }

    impl Provider for Fake {
        fn model(&self) -> &str {
            "fake"
        }
        fn stream<'a>(
            &'a self,
            _system: &'a str,
            history: Vec<Msg>,
            _tools: &'a [ToolSpec],
        ) -> BoxFuture<'a, Result<BoxStream<'a, Result<Delta, String>>, String>> {
            self.seen.lock().unwrap().push(history);
            let items = self.rounds.lock().unwrap().pop_front().unwrap_or_else(|| vec![Ok(Delta::Text("(script exhausted)".into()))]);
            Box::pin(async move { Ok(stream::iter(items).boxed()) })
        }
    }

    fn text(t: &str) -> Result<Delta, String> {
        Ok(Delta::Text(t.into()))
    }

    fn call(id: &str, name: &str, args: serde_json::Value) -> Result<Delta, String> {
        Ok(Delta::Call(Call { id: id.into(), name: name.into(), args }))
    }

    fn options(cwd: &std::path::Path) -> SessionOptions {
        SessionOptions {
            backend: crate::config::BackendConfig { backend_dir: Default::default(), python: Default::default(), server: Default::default() },
            cwd: cwd.to_path_buf(),
            transport: crate::transport::BackendTransport::Native,
            resume: None,
            debug_log: None,
            continue_latest: false,
            hub_handle: None,
        }
    }

    /// Sends each prompt after the previous turn ended, answering permission cards with `policy`.
    async fn session(
        provider: &Fake,
        cwd: &std::path::Path,
        prompts: &[&str],
        policy: impl Fn(&crate::permission::PermissionInfo) -> ApprovalDecision,
    ) -> Vec<AppEvent> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let offload = cwd.join(".offload");
        let driver = drive(provider, options(cwd), cmd_rx, ev_tx, offload);
        let script = async {
            let mut out = Vec::new();
            for prompt in prompts {
                cmd_tx.send(Command::Prompt((*prompt).into(), Vec::new())).unwrap();
                loop {
                    let event = ev_rx.recv().await.expect("events open");
                    let ended = matches!(event, AppEvent::TurnComplete(_) | AppEvent::TurnFailed(_));
                    if let AppEvent::PermissionRequested { info, decision } = event {
                        let _ = decision.send(policy(&info));
                        out.push(AppEvent::Notice(format!("asked:{}", info.tool_name)));
                        continue;
                    }
                    out.push(event);
                    if ended {
                        break;
                    }
                }
            }
            cmd_tx.send(Command::Shutdown).unwrap();
            while let Some(event) = ev_rx.recv().await {
                out.push(event);
            }
            out
        };
        let (_, out) = tokio::join!(driver, script);
        out
    }

    fn texts(events: &[AppEvent]) -> String {
        events.iter().filter_map(|e| if let AppEvent::AgentText(t) = e { Some(t.as_str()) } else { None }).collect()
    }

    fn asked(events: &[AppEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| if let AppEvent::Notice(n) = e { n.strip_prefix("asked:").map(str::to_owned) } else { None })
            .collect()
    }

    #[tokio::test]
    async fn streams_text_then_ends_turn() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(vec![vec![text("he"), text("llo")]]);
        let events = session(&fake, dir.path(), &["hi"], |_| ApprovalDecision::Reject).await;
        assert!(matches!(&events[0], AppEvent::Connected(info) if info.transport == "native · fake"));
        assert_eq!(texts(&events), "hello");
        assert!(events.iter().any(|e| matches!(e, AppEvent::TurnComplete(StopReason::EndTurn))));
        assert!(matches!(events.last(), Some(AppEvent::Closed)));
    }

    #[tokio::test]
    async fn stream_error_fails_the_turn_and_drops_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(vec![vec![Err("boom".into())], vec![text("ok")]]);
        let events = session(&fake, dir.path(), &["a", "b"], |_| ApprovalDecision::Reject).await;
        assert_eq!(events.iter().filter(|e| matches!(e, AppEvent::TurnFailed(m) if m == "boom")).count(), 1);
        // "a" got no answer, so the second request carries only "b".
        assert_eq!(fake.seen.lock().unwrap()[1], vec![Msg::User("b".into())]);
    }

    #[tokio::test]
    async fn read_tool_runs_without_asking_and_feeds_the_next_round() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let fake = Fake::new(vec![vec![call("c1", "read_file", json!({"path":"a.txt"}))], vec![text("done")]]);
        let events = session(&fake, dir.path(), &["read it"], |_| ApprovalDecision::Reject).await;
        assert!(asked(&events).is_empty());
        assert_eq!(texts(&events), "done");
        let seen = fake.seen.lock().unwrap();
        assert!(matches!(&seen[1][2], Msg::ToolResult { id, output } if id == "c1" && output.contains("alpha")));
        assert!(events.iter().any(|e| matches!(e, AppEvent::ToolCallUpdated(p) if p.status == Some(ToolStatus::Completed))));
    }

    #[tokio::test]
    async fn edit_asks_first_and_a_rejection_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old").unwrap();
        let args = json!({"path":"a.txt","target":"old","replacement":"new"});
        let fake = Fake::new(vec![vec![call("c1", "edit_file", args)], vec![text("ok")]]);
        let events = session(&fake, dir.path(), &["edit"], |_| ApprovalDecision::Reject).await;
        assert_eq!(asked(&events), ["edit_file"]);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "old");
        // The card carried the diff before anything ran.
        let started = events.iter().find_map(|e| if let AppEvent::ToolCallStarted(i) = e { Some(i) } else { None }).unwrap();
        assert_eq!(started.diffs[0].new_text, "new");
        assert!(matches!(&fake.seen.lock().unwrap()[1][2], Msg::ToolResult { output, .. } if output.contains("rejected")));
    }

    #[tokio::test]
    async fn allow_always_asks_once_per_tool() {
        let dir = tempfile::tempdir().unwrap();
        let edit = |n: &str| json!({"path":n,"target":"","replacement":"x"});
        let fake = Fake::new(vec![
            vec![call("c1", "edit_file", edit("one.txt"))],
            vec![call("c2", "edit_file", edit("two.txt"))],
            vec![text("done")],
        ]);
        let events = session(&fake, dir.path(), &["go"], |_| ApprovalDecision::AllowAlways).await;
        assert_eq!(asked(&events), ["edit_file"]);
        assert!(dir.path().join("one.txt").exists() && dir.path().join("two.txt").exists());
    }

    #[tokio::test]
    async fn round_limit_stops_a_runaway_loop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let rounds = (0..MAX_ROUNDS + 5).map(|i| vec![call(&format!("c{i}"), "read_file", json!({"path":"a.txt"}))]).collect();
        let fake = Fake::new(rounds);
        let events = session(&fake, dir.path(), &["loop"], |_| ApprovalDecision::Reject).await;
        assert!(events.iter().any(|e| matches!(e, AppEvent::TurnComplete(StopReason::MaxTurnRequests))));
        assert_eq!(fake.seen.lock().unwrap().len(), MAX_ROUNDS);
    }

    #[test]
    fn text_attachments_are_inlined_images_skipped() {
        let out = with_attachments(
            "see".into(),
            &[
                Attachment::Text { uri: "file:///a".into(), text: "body".into() },
                Attachment::Image { mime: "image/png".into(), data_base64: "AA".into() },
            ],
        );
        assert!(out.contains("<file uri=\"file:///a\">\nbody\n</file>"));
        assert!(!out.contains("AA"));
    }
}
