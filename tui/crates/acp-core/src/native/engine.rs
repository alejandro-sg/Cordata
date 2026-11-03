//! The native agent loop: history in memory (and on disk, `crate::history`), a streamed model turn, tool calls through
//! the person's rules and the permission gate, repeat until the model answers without a tool. Commands are multiplexed
//! against every await so `Cancel` lands mid-token and mid-command.

use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};

use cordata_economizer::cache_align::{self, PrefixTracker};
use cordata_economizer::loopguard::{GuardDecision, LoopGuard, ToolCall};
use cordata_economizer::microskills::{CommandOutcome, MicroSkills};
use cordata_economizer::toolprune::ToolPruner;
use cordata_economizer::Profile;
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use super::approval::{self, Verdict};
use super::provider::{Call, Delta, GenaiProvider, Msg, Provider};
use super::tools;
use crate::events::{AppEvent, Restored, SessionInfo, StopReason, ToolCallInfo, ToolCallPatch, ToolStatus};
use crate::history;
use crate::permission::ApprovalDecision;
use crate::policy::{self, Network, Policy};
use crate::session::{Attachment, Command, SessionOptions};

const DEFAULT_MODEL: &str = "claude-sonnet-5-5";
/// The default when there is an Ollama key but no Anthropic one. The `ollama_cloud::` prefix is required: genai sends a
/// bare `kimi-…` (or `glm-…`, `deepseek-…`, `minimax-…`, `mimo-…`) tag to that vendor's own API, which needs its own key.
const OLLAMA_CLOUD_DEFAULT: &str = "ollama_cloud::qwen3-coder:480b";

/// The chat model when `CORDATA_NATIVE_MODEL` is unset: Anthropic's, unless only an Ollama cloud key is on hand.
fn default_model(has_anthropic_key: bool, has_ollama_cloud_key: bool) -> &'static str {
    if !has_anthropic_key && has_ollama_cloud_key { OLLAMA_CLOUD_DEFAULT } else { DEFAULT_MODEL }
}
/// Model round-trips per prompt before the turn is cut off.
const MAX_ROUNDS: usize = 25;
const SYSTEM_PROMPT: &str = "You are Cordata, a coding agent running in the user's terminal. \
Answer concisely. Use the tools to read, search and edit the project and to run commands; \
edits and commands are shown to the user for approval first. Commands run offline and may write only inside the \
project: set `network` on a command only when it must reach the internet.";

const MAX_INSTRUCTION_CHARS: usize = 20_000;

/// Guidance files, broad to specific: yours everywhere, then the project's (the first of its candidates that exists),
/// then this machine's. Each is capped so a huge one cannot eat the window.
fn project_instructions(cwd: &std::path::Path) -> Option<String> {
    let home = std::env::var_os("CORDATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cordata")));
    let groups: [(&str, Vec<std::path::PathBuf>); 3] = [
        ("yours", home.into_iter().map(|h| h.join("CORDATA.md")).collect()),
        (
            "this project's",
            ["CORDATA.md", ".cordata/CORDATA.md", "AGENTS.md", "CLAUDE.md"].iter().map(|n| cwd.join(n)).collect(),
        ),
        ("this machine's", vec![cwd.join("CORDATA.local.md")]),
    ];
    let parts: Vec<String> = groups
        .iter()
        .filter_map(|(who, paths)| {
            paths.iter().find_map(|path| {
                let text = std::fs::read_to_string(path).ok()?;
                let text = text.trim();
                if text.is_empty() {
                    return None;
                }
                let name = path.file_name()?.to_string_lossy().into_owned();
                let body: String = text.chars().take(MAX_INSTRUCTION_CHARS).collect();
                Some(format!("Instructions from {who} {name}:\n{body}"))
            })
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

type Events = mpsc::UnboundedSender<AppEvent>;
type Commands = mpsc::UnboundedReceiver<Command>;

/// `CORDATA_NATIVE_MODEL` picks the model (a genai name: `claude-…`, `gemini-…`, `gpt-…`, a local Ollama tag, or
/// `ollama_cloud::<tag>` for Ollama's cloud); unset, it follows whichever provider key exists (`default_model`).
pub(crate) async fn run(options: SessionOptions, commands: Commands, events: Events) {
    let model = options
        .model
        .clone()
        .filter(|m| !m.trim().is_empty())
        .or_else(|| std::env::var("CORDATA_NATIVE_MODEL").ok().filter(|m| !m.trim().is_empty()))
        .unwrap_or_else(|| default_model(super::keys::resolve("anthropic").is_some(), super::keys::resolve("ollama_cloud").is_some()).into());
    match GenaiProvider::new(model) {
        Ok(provider) => {
            let off = std::env::var_os("CORDATA_ECONOMIZER").is_some_and(|v| v == "off");
            let state_dir = if off { None } else { cordata_economizer::default_state_dir() };
            let policy = Policy::load(&options.cwd);
            let sessions_home = policy::cordata_home();
            let history_home = if policy.history { sessions_home.clone() } else { None };
            drive(&provider, options, commands, events, Setup { offload: tools::offload_dir(), state_dir, policy, history_home, sessions_home, switch: genai_provider }).await
        }
        Err(error) => {
            let _ = events.send(AppEvent::ConnectionError(format!("could not start the native agent: {error}")));
            let _ = events.send(AppEvent::Closed);
        }
    }
}

/// What a session starts from besides the provider: where offloaded output goes, the economizer's state, the person's
/// rules, where sessions are saved (`None`: nowhere), and where saved ones live for `/forget` (even with saving off).
pub(crate) struct Setup {
    /// Builds the provider for a model the person switches to.
    pub(crate) switch: fn(&str) -> Result<Box<dyn Provider>, String>,
    pub(crate) offload: PathBuf,
    pub(crate) state_dir: Option<PathBuf>,
    pub(crate) policy: Policy,
    pub(crate) history_home: Option<PathBuf>,
    pub(crate) sessions_home: Option<PathBuf>,
}

fn genai_provider(model: &str) -> Result<Box<dyn Provider>, String> {
    Ok(Box::new(GenaiProvider::new(model)?))
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
                Some(Command::Prompt(..) | Command::Resume(_) | Command::Forget | Command::SetModel(_)) => {
                    let _ = events.send(AppEvent::Notice("a turn is still running; wait or cancel it first".into()));
                }
            },
            out = &mut fut => return Ok(out),
        }
    }
}

struct Agent<'a> {
    provider: &'a dyn Provider,
    /// The model the person switched to, in place of `provider`.
    switched: Option<Box<dyn Provider>>,
    cwd: PathBuf,
    offload: PathBuf,
    /// Tools the user chose "always allow" for, this session (`approval::always_key`).
    always: HashSet<String>,
    economizer: Economizer,
    policy: Policy,
    mcp: super::mcp::McpManager,
}

/// Where this conversation is saved after each turn, if anywhere.
struct Keeper {
    home: Option<PathBuf>,
    id: String,
    /// The first failure is reported; later ones are not.
    warned: bool,
}

impl Keeper {
    fn save(&mut self, cwd: &Path, model: &str, history: &[Msg], events: &Events) {
        let Some(home) = &self.home else { return };
        if history.is_empty() {
            return;
        }
        if let Err(error) = history::save_in(home, cwd, &self.id, model, history)
            && !std::mem::replace(&mut self.warned, true)
        {
            let _ = events.send(AppEvent::Notice(format!("this session could not be saved: {error}")));
        }
    }
}

/// A one-line reminder of what a tool call was given, for a resumed transcript.
fn call_summary(call: &Call) -> String {
    let arg = ["cmd", "path", "pattern"].iter().find_map(|k| call.args.get(*k).and_then(|v| v.as_str())).unwrap_or_default();
    let line = arg.lines().next().unwrap_or_default();
    if line.chars().count() > 80 { format!("{}…", line.chars().take(79).collect::<String>()) } else { line.to_owned() }
}

fn restored(history: &[Msg]) -> Vec<Restored> {
    let mut out = Vec::new();
    for msg in history {
        match msg {
            Msg::User { text } => out.push(Restored::User(text.clone())),
            Msg::Assistant { text, calls } => {
                if !text.is_empty() {
                    out.push(Restored::Agent(text.clone()));
                }
                out.extend(calls.iter().map(|c| Restored::Tool { name: c.name.clone(), summary: call_summary(c) }));
            }
            Msg::ToolResult { .. } => {}
        }
    }
    out
}

/// Token-saving state for one session. With no `state_dir` nothing is read from or written to disk.
struct Economizer {
    profile: Profile,
    state_dir: Option<PathBuf>,
    pruner: ToolPruner,
    skills: MicroSkills,
    guard: LoopGuard,
    prefix: PrefixTracker,
    /// Tools sent so far this session. It only grows, so the tool list (part of the cached prefix) changes rarely.
    active: BTreeSet<String>,
    /// The system prompt, fixed at the first prompt of the session.
    system: Option<String>,
    commands: Vec<CommandOutcome>,
}

impl Economizer {
    fn new(model: &str, state_dir: Option<PathBuf>) -> Self {
        let (pruner, skills) = match &state_dir {
            Some(dir) => (ToolPruner::load(&dir.join("tools.json")), MicroSkills::load(&dir.join("skills.json"))),
            None => (ToolPruner::default(), MicroSkills::default()),
        };
        Self {
            profile: Profile::for_model(model),
            state_dir,
            pruner,
            skills,
            guard: LoopGuard::default(),
            prefix: PrefixTracker::default(),
            active: BTreeSet::new(),
            system: None,
            commands: Vec::new(),
        }
    }

    /// Learned facts and tool usage go to disk when the session ends. Failures are ignored: this is an optimisation.
    fn persist(&mut self, cwd: &Path) {
        let Some(dir) = &self.state_dir else { return };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.skills.fold(cwd, &self.commands, now);
        let _ = self.pruner.save(&dir.join("tools.json"));
        let _ = self.skills.save(&dir.join("skills.json"));
    }
}

pub(crate) async fn drive(
    provider: &dyn Provider,
    options: SessionOptions,
    mut commands: Commands,
    events: Events,
    setup: Setup,
) {
    let Setup { offload, state_dir, policy, history_home, sessions_home, switch } = setup;
    let mut keeper = Keeper { home: history_home, id: history::new_id(), warned: false };
    let _ = events.send(AppEvent::Connected(SessionInfo {
        session_id: keeper.id.clone(),
        cwd: options.cwd.display().to_string(),
        transport: format!("native · {}", provider.model()),
        model: provider.model().to_owned(),
    }));
    for warning in &policy.warnings {
        let _ = events.send(AppEvent::Notice(format!("settings: {warning}")));
    }

    let economizer = Economizer::new(provider.model(), state_dir);
    let mcp = super::mcp::McpManager::load(&options.cwd).await;
    if !mcp.tools().is_empty() {
        let count = mcp.tools().len();
        let _ = events.send(AppEvent::Notice(format!("mcp: discovered {count} tool{} from configured servers", if count == 1 { "" } else { "s" })));
    }
    let mut agent = Agent { provider, switched: None, cwd: options.cwd.clone(), offload, always: HashSet::new(), economizer, policy, mcp };
    let mut history: Vec<Msg> = Vec::new();
    while let Some(command) = commands.recv().await {
        match command {
            Command::Prompt(text, attachments) => {
                history.push(Msg::User { text: with_attachments(text, &attachments) });
                let ended = agent.turn(&mut history, &mut commands, &events).await;
                keeper.save(&options.cwd, agent.llm().model(), &history, &events);
                if let Err(Interrupt::Shutdown) = ended {
                    break;
                }
            }
            Command::Resume(wanted) => {
                let Some(home) = keeper.home.clone() else {
                    let _ = events.send(AppEvent::Notice("session history is off (chat.history), so there is nothing to resume".into()));
                    continue;
                };
                match history::load_in(&home, &options.cwd, &wanted) {
                    Ok(saved) => {
                        history = saved.messages;
                        keeper.id = saved.id;
                        let _ = events.send(AppEvent::SessionRestored { session_id: keeper.id.clone(), entries: restored(&history) });
                    }
                    Err(error) => {
                        let _ = events.send(AppEvent::Notice(error));
                    }
                }
            }
            Command::Forget => {
                // Saving may be off now while sessions from before are still on disk: delete them either way.
                let note = match sessions_home.as_deref().map(|home| history::forget_in(home, &options.cwd)) {
                    None => "no home folder, so nothing was saved".to_owned(),
                    Some(Ok(n)) => {
                        keeper.home = None;
                        format!(
                            "deleted {n} saved session{} of this project; this session will not be saved",
                            if n == 1 { "" } else { "s" }
                        )
                    }
                    Some(Err(error)) => format!("could not delete the saved sessions: {error}"),
                };
                let _ = events.send(AppEvent::Notice(note));
            }
            Command::SetModel(model) => match switch(model.trim()) {
                Ok(next) => {
                    agent.economizer.profile = Profile::for_model(next.model());
                    let name = next.model().to_owned();
                    agent.switched = Some(next);
                    let _ = events.send(AppEvent::ModelChanged(name));
                }
                Err(error) => {
                    let _ = events.send(AppEvent::Notice(format!("could not switch to {model}: {error}")));
                }
            },
            Command::Cancel => {}
            Command::Shutdown => break,
        }
    }
    agent.economizer.persist(&options.cwd);
    let _ = events.send(AppEvent::Closed);
}

impl Agent<'_> {
    /// The model answering now.
    fn llm(&self) -> &dyn Provider {
        self.switched.as_deref().unwrap_or(self.provider)
    }

    /// One prompt: model round-trips until an answer with no tool calls. `Err(Shutdown)` ends the session;
    /// a cancel is reported to the UI and returns `Ok`.
    async fn turn(&mut self, history: &mut Vec<Msg>, commands: &mut Commands, events: &Events) -> Result<(), Interrupt> {
        self.economizer.guard.reset_turn();
        let prompt = history.iter().rev().find_map(|m| if let Msg::User { text } = m { Some(text.clone()) } else { None }).unwrap_or_default();
        let specs = self.choose_tools(&prompt);
        let system = self.system_prompt(&prompt).to_owned();
        let tools_json = specs.iter().map(|s| format!("{}{}", s.name, s.schema)).collect::<String>();
        self.economizer.prefix.observe(&system, &tools_json);
        for _ in 0..MAX_ROUNDS {
            let stream = match interruptible(self.llm().stream(&system, history.clone(), &specs), commands, events).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => return {
                    self.fail(history, events, error);
                    Ok(())
                },
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
                    Some(Ok(Delta::Usage(usage))) => {
                        let _ = events.send(AppEvent::Usage(usage));
                    }
                    Some(Err(error)) => {
                        if !text.is_empty() {
                            history.push(Msg::Assistant { text, calls: Vec::new() });
                        }
                        return {
                            self.fail(history, events, error);
                            Ok(())
                        };
                    }
                    None => break,
                }
            }
            // The reply is read; the stream borrows the model, and running the calls needs the agent.
            drop(stream);
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

    /// The tools for this prompt: core tools plus whatever the prompt or the repository history asks for,
    /// added to those already sent this session.
    fn choose_tools(&mut self, prompt: &str) -> Vec<tools::ToolSpec> {
        let mut all = tools::specs();
        all.extend(self.mcp.tools().iter().cloned());
        let eco = &mut self.economizer;
        let picked: Vec<String> =
            eco.pruner.select_tools(&self.cwd, prompt, &all, &eco.profile).into_iter().map(|t| t.name.to_string()).collect();
        eco.active.extend(picked);
        all.into_iter().filter(|s| eco.active.contains(s.name.as_ref())).collect()
    }

    /// Static prefix plus the micro-skills chosen at the first prompt. Fixed for the session, so it caches.
    fn system_prompt(&mut self, prompt: &str) -> &str {
        let eco = &mut self.economizer;
        eco.system.get_or_insert_with(|| {
            let facts = eco.skills.render(&self.cwd, prompt, 6);
            let base = match project_instructions(&self.cwd) {
                Some(text) => format!("{SYSTEM_PROMPT}\n\n{text}"),
                None => SYSTEM_PROMPT.to_owned(),
            };
            cache_align::layout(&base, &facts, &[]).system
        })
    }

    /// A failed round-trip: the prompt that had no answer is dropped so the next one doesn't follow it.
    fn fail(&self, history: &mut Vec<Msg>, events: &Events, error: String) {
        if matches!(history.last(), Some(Msg::User { .. })) {
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
            diffs,
        }));
        let patch = |status, content: Option<String>| {
            AppEvent::ToolCallUpdated(ToolCallPatch { id: call.id.clone(), status: Some(status), content, ..Default::default() })
        };

        // The pre-tool guard refuses without asking: `.env` and `.git/` writes, recursive deletes, redirects into `.env`.
        let payload = serde_json::json!({"tool": call.name, "input": call.args}).to_string();
        if let Some(reason) = cordata_guard::guard::check(&payload) {
            let _ = events.send(patch(ToolStatus::Failed, Some(reason.clone())));
            return Ok(reason);
        }

        if call.name == "run_command" && policy::wants_network(&call.args) && self.policy.network == Network::Off {
            let reason = "refused: network access for commands is off (chat.network). Run it without `network`, or ask the user to run it.".to_owned();
            let _ = events.send(patch(ToolStatus::Failed, Some(reason.clone())));
            return Ok(reason);
        }
        // The person's rules: a deny refuses without asking, an allow skips the card for a call that would ask.
        let rule = self.policy.decide(&call.name, &call.args, &self.cwd);
        if let policy::Verdict::Deny(rule) = &rule {
            let reason = format!("refused by the rule `{rule}` in the user's settings; do not retry it");
            let _ = events.send(patch(ToolStatus::Failed, Some(reason.clone())));
            return Ok(reason);
        }
        let key = approval::always_key(&call.name, &call.args);
        if tools::needs_approval(&call.name) && let policy::Verdict::Allow(rule) = &rule {
            let _ = events.send(AppEvent::Notice(format!("{} ran without asking: your rule `{rule}`", call.name)));
        } else if tools::needs_approval(&call.name) && !self.always.contains(&key) {
            let (tx, rx) = oneshot::channel::<ApprovalDecision>();
            let _ = events.send(AppEvent::PermissionRequested { info: approval::request(&call.id, &call.name, &call.args), decision: tx });
            // A dropped sender counts as a rejection.
            let decision = interruptible(rx, commands, events).await?.unwrap_or(ApprovalDecision::Reject);
            match approval::interpret(decision) {
                Verdict::Allow { always } => {
                    // A network call is never remembered, whatever the front end sent.
                    if always && !(call.name == "run_command" && policy::wants_network(&call.args)) {
                        self.always.insert(key);
                    }
                }
                Verdict::Deny(reason) => {
                    let _ = events.send(patch(ToolStatus::Failed, Some(reason.clone())));
                    return Ok(reason);
                }
            }
        }

        let tool_call = ToolCall { name: &call.name, args: &call.args };
        if let GuardDecision::Refuse(reason) = self.economizer.guard.precheck(&tool_call) {
            let _ = events.send(patch(ToolStatus::Failed, Some(reason.clone())));
            return Ok(reason);
        }
        self.economizer.pruner.record_tool_invoked(&self.cwd, &call.name);

        let _ = events.send(patch(ToolStatus::InProgress, None));
        let result = if self.mcp.has_tool(&call.name) {
            interruptible(self.mcp.call_tool(&call.name, &call.args), commands, events).await
        } else {
            interruptible(tools::execute(&call.name, &call.args, &self.cwd, &self.policy.writable), commands, events).await
        };
        let (status, output) = match result {
            Err(interrupt) => {
                let _ = events.send(patch(ToolStatus::Failed, Some("cancelled".into())));
                return Err(interrupt);
            }
            Ok(Ok(out)) => (ToolStatus::Completed, out),
            Ok(Err(error)) => (ToolStatus::Failed, format!("error: {error}")),
        };
        if call.name == "run_command" {
            let cmd = call.args.get("cmd").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
            self.economizer.commands.push(CommandOutcome { cmd, ok: status == ToolStatus::Completed });
        }
        let mut output = tools::limit_output(output, &self.offload);
        match self.economizer.guard.inspect(&tool_call, &output) {
            GuardDecision::Allow => {}
            GuardDecision::Nudge(note) | GuardDecision::Refuse(note) => {
                output.push_str("\n\n");
                output.push_str(&note);
            }
        }
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
    use super::{DEFAULT_MODEL, OLLAMA_CLOUD_DEFAULT, default_model};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use futures::future::BoxFuture;
    use futures::stream::{self, BoxStream};

    #[test]
    fn the_default_model_follows_the_key_that_exists() {
        assert_eq!(default_model(true, true), DEFAULT_MODEL, "an Anthropic key keeps the Anthropic default");
        assert_eq!(default_model(true, false), DEFAULT_MODEL);
        assert_eq!(default_model(false, true), OLLAMA_CLOUD_DEFAULT, "only an Ollama key goes to Ollama cloud");
        assert_eq!(default_model(false, false), DEFAULT_MODEL, "no key: the Anthropic error says what to run");
        assert!(OLLAMA_CLOUD_DEFAULT.starts_with("ollama_cloud::"), "a bare kimi tag would be sent to Moonshot");
    }
    use serde_json::json;

    use super::super::tools::ToolSpec;
    use super::*;
    use crate::events::TokenUsage;

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
        Ok(Delta::Call(Call { id: id.into(), name: name.into(), args, thought_signatures: None }))
    }

    /// A model the person switched to: it says who it is and how much conversation it was handed.
    struct Named(String);

    impl Provider for Named {
        fn model(&self) -> &str {
            &self.0
        }
        fn stream<'a>(
            &'a self,
            _system: &'a str,
            history: Vec<Msg>,
            _tools: &'a [ToolSpec],
        ) -> BoxFuture<'a, Result<BoxStream<'a, Result<Delta, String>>, String>> {
            let reply = format!("{} saw {} messages", self.0, history.len());
            Box::pin(async move { Ok(stream::iter(vec![Ok(Delta::Text(reply))]).boxed()) })
        }
    }

    fn fake_switch(model: &str) -> Result<Box<dyn Provider>, String> {
        if model.is_empty() { Err("no model named".into()) } else { Ok(Box::new(Named(model.into()))) }
    }

    /// No rules, nothing saved, no economizer state.
    fn plain(cwd: &std::path::Path) -> Setup {
        Setup { switch: fake_switch, offload: cwd.join(".offload"), state_dir: None, policy: Policy::default(), history_home: None, sessions_home: None }
    }

    fn rules(doc: serde_json::Value) -> Policy {
        Policy::from_files(&[("user".into(), doc, true)])
    }

    /// Sends `commands` in order, each after the previous one's answer, approving every card; returns all events.
    async fn script(provider: &Fake, cwd: &std::path::Path, setup: Setup, commands: Vec<Command>) -> Vec<AppEvent> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let driver = drive(provider, options(cwd), cmd_rx, ev_tx, setup);
        let run = async {
            let mut out = Vec::new();
            for command in commands {
                cmd_tx.send(command).unwrap();
                loop {
                    let event = ev_rx.recv().await.expect("events open");
                    if let AppEvent::PermissionRequested { info, decision } = event {
                        let _ = decision.send(ApprovalDecision::Allow);
                        out.push(AppEvent::Notice(format!("asked:{}", info.tool_name)));
                        continue;
                    }
                    let answered = matches!(event, AppEvent::TurnComplete(_) | AppEvent::TurnFailed(_) | AppEvent::SessionRestored { .. } | AppEvent::ModelChanged(_))
                        || matches!(&event, AppEvent::Notice(n) if !n.contains("without asking"));
                    out.push(event);
                    if answered {
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
        let (_, out) = tokio::join!(driver, run);
        out
    }

    fn prompt(text: &str) -> Command {
        Command::Prompt(text.into(), Vec::new())
    }

    fn notices(events: &[AppEvent]) -> Vec<String> {
        events.iter().filter_map(|e| if let AppEvent::Notice(n) = e { Some(n.clone()) } else { None }).collect()
    }

    #[tokio::test]
    async fn usage_is_reported_per_request() {
        let dir = tempfile::tempdir().unwrap();
        let usage = TokenUsage { input: 1200, output: 40, cached: 1000 };
        let fake = Fake::new(vec![vec![text("hi"), Ok(Delta::Usage(usage))]]);
        let events = script(&fake, dir.path(), plain(dir.path()), vec![prompt("go")]).await;
        assert!(events.iter().any(|e| matches!(e, AppEvent::Usage(u) if *u == usage)));
    }

    #[tokio::test]
    async fn switching_the_model_keeps_the_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(vec![vec![text("first")]]);
        let commands = vec![prompt("one"), Command::SetModel("other-model".into()), prompt("two"), Command::SetModel("  ".into())];
        let events = script(&fake, dir.path(), plain(dir.path()), commands).await;
        assert!(events.iter().any(|e| matches!(e, AppEvent::ModelChanged(m) if m == "other-model")));
        let said: String = events.iter().filter_map(|e| if let AppEvent::AgentText(t) = e { Some(t.as_str()) } else { None }).collect();
        assert!(said.contains("other-model saw 3 messages"), "the new model got the whole conversation: {said}");
        assert!(notices(&events).iter().any(|n| n.starts_with("could not switch")));
    }

    #[tokio::test]
    async fn an_allow_rule_skips_the_card_and_a_deny_rule_refuses_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let edit = |n: &str| json!({"path": n, "target": "", "replacement": "x"});
        let fake = Fake::new(vec![
            vec![call("c1", "edit_file", edit("src/a.txt"))],
            vec![call("c2", "edit_file", edit("migrations/1.sql"))],
            vec![call("c3", "edit_file", edit("other.txt"))],
            vec![text("done")],
        ]);
        let mut setup = plain(dir.path());
        setup.policy = rules(json!({"chat": {"allow": ["edit_file(src/**)", "edit_file(migrations/**)"], "deny": ["edit_file(migrations/**)"]}}));
        let events = script(&fake, dir.path(), setup, vec![prompt("go")]).await;
        assert_eq!(asked(&events), ["edit_file"], "only the call no rule covers asks");
        assert!(dir.path().join("src/a.txt").exists() && !dir.path().join("migrations/1.sql").exists());
        assert!(notices(&events).iter().any(|n| n.contains("your rule `edit_file(src/**)`")));
        assert!(matches!(&fake.seen.lock().unwrap()[2][4], Msg::ToolResult { output, .. } if output.contains("refused by the rule `edit_file(migrations/**)`")));
    }

    #[tokio::test]
    async fn network_requests_ask_separately_and_are_refused_when_off() {
        let dir = tempfile::tempdir().unwrap();
        let online = json!({"cmd": "true", "network": true});
        let fake = Fake::new(vec![vec![call("c1", "run_command", online.clone())], vec![text("done")]]);
        let mut setup = plain(dir.path());
        setup.policy = rules(json!({"chat": {"network": "off"}}));
        let events = script(&fake, dir.path(), setup, vec![prompt("go")]).await;
        assert!(asked(&events).is_empty());
        assert!(matches!(&fake.seen.lock().unwrap()[1][2], Msg::ToolResult { output, .. } if output.contains("network access for commands is off")));
        // Allowing offline commands for the session still asks before one goes online.
        let fake = Fake::new(vec![
            vec![call("c1", "run_command", json!({"cmd": "true"}))],
            vec![call("c2", "run_command", json!({"cmd": "true"}))],
            vec![call("c3", "run_command", online)],
            vec![text("done")],
        ]);
        let events = session(&fake, dir.path(), &["go"], |info| {
            if info.title.contains("network") { ApprovalDecision::Reject } else { ApprovalDecision::AllowAlways }
        })
        .await;
        assert_eq!(asked(&events), ["run_command", "run_command"]);
        // Even an "always" answer to a network card covers only that call.
        let online = json!({"cmd": "true", "network": true});
        let fake = Fake::new(vec![vec![call("c1", "run_command", online.clone())], vec![call("c2", "run_command", online)], vec![text("done")]]);
        let events = session(&fake, dir.path(), &["go"], |_| ApprovalDecision::AllowAlways).await;
        assert_eq!(asked(&events), ["run_command", "run_command"]);
    }

    #[tokio::test]
    async fn a_session_is_saved_resumed_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let saving = |cwd: &std::path::Path| Setup { history_home: Some(home.clone()), sessions_home: Some(home.clone()), ..plain(cwd) };
        let first = Fake::new(vec![vec![text("first answer")]]);
        let events = script(&first, dir.path(), saving(dir.path()), vec![prompt("remember 42")]).await;
        let Some(AppEvent::Connected(info)) = events.first() else { panic!("connected first") };
        let id = info.session_id.clone();

        let second = Fake::new(vec![vec![text("it was 42")]]);
        let events = script(&second, dir.path(), saving(dir.path()), vec![Command::Resume("last".into()), prompt("what was it?")]).await;
        let restored = events.iter().find_map(|e| if let AppEvent::SessionRestored { session_id, entries } = e { Some((session_id, entries)) } else { None });
        let (session_id, entries) = restored.expect("restored");
        assert_eq!(session_id, &id);
        assert_eq!(entries, &[Restored::User("remember 42".into()), Restored::Agent("first answer".into())]);
        assert_eq!(second.seen.lock().unwrap()[0].len(), 3, "the model sees the resumed conversation");
        let saved = history::load_in(&home, dir.path(), &id).unwrap();
        assert_eq!(saved.messages.len(), 4, "the resumed session keeps its id and grows");

        let third = Fake::new(vec![vec![text("x")]]);
        let events = script(&third, dir.path(), saving(dir.path()), vec![Command::Forget, prompt("not kept")]).await;
        assert!(notices(&events).iter().any(|n| n.starts_with("deleted 1 saved session")), "{:?}", notices(&events));
        assert!(history::load_in(&home, dir.path(), "last").is_err(), "nothing kept after /forget, not even this session");

        // With saving switched off later, /forget still deletes what an earlier session saved.
        script(&Fake::new(vec![vec![text("x")]]), dir.path(), saving(dir.path()), vec![prompt("kept")]).await;
        let off = Setup { history_home: None, ..saving(dir.path()) };
        let events = script(&Fake::new(vec![]), dir.path(), off, vec![Command::Forget]).await;
        assert!(notices(&events).iter().any(|n| n.starts_with("deleted 1 saved session")), "{:?}", notices(&events));
        assert!(history::load_in(&home, dir.path(), "last").is_err());
    }

    #[tokio::test]
    async fn with_history_off_nothing_is_written_and_resume_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(vec![vec![text("ok")]]);
        let events = script(&fake, dir.path(), plain(dir.path()), vec![prompt("hi"), Command::Resume("last".into())]).await;
        assert!(notices(&events).iter().any(|n| n.contains("history is off")));
    }

    fn options(cwd: &std::path::Path) -> SessionOptions {
        SessionOptions {
            model: None,
            cwd: cwd.to_path_buf(),
            transport: crate::transport::BackendTransport::Native,
            debug_log: None,
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
        let driver = drive(provider, options(cwd), cmd_rx, ev_tx, plain(cwd));
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
        assert_eq!(fake.seen.lock().unwrap()[1], vec![Msg::User { text: "b".into() }]);
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

    #[test]
    fn instructions_load_broad_to_specific_from_the_first_project_file_and_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        assert!(project_instructions(dir.path()).is_none() || std::env::var_os("HOME").is_some());
        std::fs::write(dir.path().join("CLAUDE.md"), "use tabs").unwrap();
        std::fs::write(dir.path().join("CORDATA.local.md"), "my machine").unwrap();
        let text = project_instructions(dir.path()).unwrap();
        assert!(text.contains("this project's CLAUDE.md:\nuse tabs"), "{text}");
        assert!(text.find("CLAUDE.md").unwrap() < text.find("CORDATA.local.md").unwrap(), "{text}");
        std::fs::write(dir.path().join("AGENTS.md"), "x".repeat(MAX_INSTRUCTION_CHARS + 500)).unwrap();
        let text = project_instructions(dir.path()).unwrap();
        assert!(text.contains("AGENTS.md") && !text.contains("CLAUDE.md:"), "the first project file wins");
        assert!(text.chars().count() < MAX_INSTRUCTION_CHARS + 400);
    }

    #[tokio::test]
    async fn the_guard_refuses_git_and_env_writes_and_recursive_deletes_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(vec![
            vec![call("c1", "edit_file", json!({"path":".git/config","target":"","replacement":"x"}))],
            vec![call("c2", "run_command", json!({"cmd":"rm -rf ~"}))],
            vec![text("done")],
        ]);
        let events = session(&fake, dir.path(), &["go"], |_| ApprovalDecision::Allow).await;
        assert!(asked(&events).is_empty(), "a blocked call is refused before any prompt");
        let seen = fake.seen.lock().unwrap();
        assert!(matches!(&seen[1][2], Msg::ToolResult { output, .. } if output.contains("GUARD REFUSAL")));
        assert!(matches!(&seen[2][4], Msg::ToolResult { output, .. } if output.contains("GUARD REFUSAL")));
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

    #[tokio::test]
    async fn identical_calls_get_a_nudge_then_a_refusal_without_running() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let mut rounds: Vec<_> = (0..6).map(|i| vec![call(&format!("c{i}"), "read_file", json!({"path":"a.txt"}))]).collect();
        rounds.push(vec![text("done")]);
        let fake = Fake::new(rounds);
        session(&fake, dir.path(), &["loop"], |_| ApprovalDecision::Reject).await;
        let seen = fake.seen.lock().unwrap();
        let result = |round: usize| match seen[round].last() {
            Some(Msg::ToolResult { output, .. }) => output.clone(),
            other => panic!("{other:?}"),
        };
        assert!(!result(1).contains("Notice") && !result(2).contains("Notice"));
        assert!(result(3).contains("3 times consecutively"));
        assert!(result(5).starts_with("Refused"), "{}", result(5));
    }

    #[tokio::test]
    async fn only_the_core_tools_go_out_on_a_small_model_and_the_system_prompt_is_stable() {
        use std::sync::Mutex;
        struct Spy(Mutex<Vec<(String, Vec<String>)>>);
        impl Provider for Spy {
            fn model(&self) -> &str {
                "llama3"
            }
            fn stream<'a>(
                &'a self,
                system: &'a str,
                _history: Vec<Msg>,
                tools: &'a [ToolSpec],
            ) -> BoxFuture<'a, Result<BoxStream<'a, Result<Delta, String>>, String>> {
                self.0.lock().unwrap().push((system.to_owned(), tools.iter().map(|t| t.name.to_string()).collect()));
                Box::pin(async { Ok(stream::iter(vec![Ok(Delta::Text("ok".into()))]).boxed()) })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let spy = Spy(Mutex::new(Vec::new()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let driver = drive(&spy, options(dir.path()), cmd_rx, ev_tx, plain(dir.path()));
        let script = async {
            for prompt in ["hello", "again"] {
                cmd_tx.send(Command::Prompt(prompt.into(), Vec::new())).unwrap();
                while !matches!(ev_rx.recv().await, Some(AppEvent::TurnComplete(_))) {}
            }
            cmd_tx.send(Command::Shutdown).unwrap();
        };
        tokio::join!(driver, script);
        let seen = spy.0.lock().unwrap();
        assert_eq!(seen[0].1, ["read_file", "edit_file", "run_command", "code_search"]);
        assert_eq!(seen[0], seen[1]);
    }

    #[tokio::test]
    async fn a_fixed_command_becomes_a_fact_for_the_next_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let fake = Fake::new(vec![
            vec![call("c1", "run_command", json!({"cmd":"false"}))],
            vec![call("c2", "run_command", json!({"cmd":"false || true"}))],
            vec![text("done")],
        ]);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let driver = drive(&fake, options(dir.path()), cmd_rx, ev_tx, Setup { state_dir: Some(state.clone()), ..plain(dir.path()) });
        let script = async {
            cmd_tx.send(Command::Prompt("go".into(), Vec::new())).unwrap();
            loop {
                match ev_rx.recv().await {
                    Some(AppEvent::PermissionRequested { decision, .. }) => {
                        let _ = decision.send(ApprovalDecision::AllowAlways);
                    }
                    Some(AppEvent::TurnComplete(_)) => break,
                    _ => {}
                }
            }
            cmd_tx.send(Command::Shutdown).unwrap();
        };
        tokio::join!(driver, script);
        // Commands only run under Seatbelt; where the sandbox is unavailable both fail and no fact is learned.
        let tools = cordata_economizer::toolprune::ToolPruner::load(&state.join("tools.json"));
        assert_eq!(tools.usage(dir.path(), "run_command"), 2);
        assert!(state.join("skills.json").exists());
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
