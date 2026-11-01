//! The native chat loop: history in memory, one streamed turn at a time, commands multiplexed
//! against the stream so `Cancel` lands mid-token.

use futures::StreamExt;
use tokio::sync::mpsc;

use super::provider::{Delta, GenaiProvider, Provider, Role, Turn};
use crate::events::{AppEvent, SessionInfo, StopReason};
use crate::session::{Attachment, Command, SessionOptions};

const DEFAULT_MODEL: &str = "claude-sonnet-5-5";
const SYSTEM_PROMPT: &str = "You are Cordata, a coding agent running in the user's terminal. \
Answer concisely. You cannot read files or run commands in this session yet; say so rather than guessing.";

type Events = mpsc::UnboundedSender<AppEvent>;

/// `CORDATA_NATIVE_MODEL` picks the model (a genai name: `claude-…`, `gemini-…`, `gpt-…`, or any Ollama tag).
pub(crate) async fn run(options: SessionOptions, commands: mpsc::UnboundedReceiver<Command>, events: Events) {
    let model = std::env::var("CORDATA_NATIVE_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    match GenaiProvider::new(model) {
        Ok(provider) => drive(&provider, options, commands, events).await,
        Err(error) => {
            let _ = events.send(AppEvent::ConnectionError(format!("could not start the native agent: {error}")));
            let _ = events.send(AppEvent::Closed);
        }
    }
}

pub(crate) async fn drive(
    provider: &dyn Provider,
    options: SessionOptions,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: Events,
) {
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

    let mut history: Vec<Turn> = Vec::new();
    while let Some(command) = commands.recv().await {
        match command {
            Command::Prompt(text, attachments) => {
                history.push(Turn { role: Role::User, text: with_attachments(text, &attachments) });
                match turn(provider, &history, &mut commands, &events).await {
                    TurnEnd::Done(reply) => {
                        if !reply.is_empty() {
                            history.push(Turn { role: Role::Assistant, text: reply });
                        }
                    }
                    // The unanswered prompt would otherwise be followed by a second user turn.
                    TurnEnd::Dropped => {
                        history.pop();
                    }
                    TurnEnd::Shutdown => break,
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

enum TurnEnd {
    /// The reply text to remember (possibly partial after a cancel).
    Done(String),
    /// Failed before producing anything usable.
    Dropped,
    Shutdown,
}

async fn turn(
    provider: &dyn Provider,
    history: &[Turn],
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &Events,
) -> TurnEnd {
    let mut stream = match provider.stream(SYSTEM_PROMPT, history).await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = events.send(AppEvent::TurnFailed(error));
            return TurnEnd::Dropped;
        }
    };
    let mut reply = String::new();
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(Command::Cancel) => {
                    let _ = events.send(AppEvent::TurnComplete(StopReason::Cancelled));
                    return TurnEnd::Done(reply);
                }
                Some(Command::Shutdown) | None => return TurnEnd::Shutdown,
                Some(Command::Prompt(..)) => {
                    let _ = events.send(AppEvent::Notice("a turn is still running; wait or cancel it first".into()));
                }
                Some(_) => {}
            },
            item = stream.next() => match item {
                Some(Ok(Delta::Text(text))) => {
                    reply.push_str(&text);
                    let _ = events.send(AppEvent::AgentText(text));
                }
                Some(Ok(Delta::Thought(text))) => {
                    let _ = events.send(AppEvent::AgentThought(text));
                }
                Some(Err(error)) => {
                    let _ = events.send(AppEvent::TurnFailed(error));
                    return if reply.is_empty() { TurnEnd::Dropped } else { TurnEnd::Done(reply) };
                }
                None => {
                    let _ = events.send(AppEvent::TurnComplete(StopReason::EndTurn));
                    return TurnEnd::Done(reply);
                }
            },
        }
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
    use futures::future::BoxFuture;
    use futures::stream::{self, BoxStream};

    use super::*;

    struct Fake(Vec<Result<Delta, String>>);

    impl Provider for Fake {
        fn model(&self) -> &str {
            "fake"
        }
        fn stream<'a>(
            &'a self,
            _system: &'a str,
            _history: &'a [Turn],
        ) -> BoxFuture<'a, Result<BoxStream<'a, Result<Delta, String>>, String>> {
            let items = self.0.clone();
            Box::pin(async move { Ok(stream::iter(items).boxed()) })
        }
    }

    fn options() -> SessionOptions {
        SessionOptions {
            backend: crate::config::BackendConfig { backend_dir: Default::default(), python: Default::default(), server: Default::default() },
            cwd: std::env::temp_dir(),
            transport: crate::transport::BackendTransport::Native,
            resume: None,
            debug_log: None,
            continue_latest: false,
            hub_handle: None,
        }
    }

    /// Sends each prompt only after the previous turn ended, then shuts the session down.
    async fn collect(provider: Fake, prompts: &[&str]) -> Vec<AppEvent> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(async move { drive(&provider, options(), cmd_rx, ev_tx).await });
        let mut out = Vec::new();
        for prompt in prompts {
            cmd_tx.send(Command::Prompt((*prompt).into(), Vec::new())).unwrap();
            loop {
                let event = ev_rx.recv().await.expect("events open");
                let ended = matches!(event, AppEvent::TurnComplete(_) | AppEvent::TurnFailed(_));
                out.push(event);
                if ended {
                    break;
                }
            }
        }
        cmd_tx.send(Command::Shutdown).unwrap();
        driver.await.unwrap();
        while let Some(event) = ev_rx.recv().await {
            out.push(event);
        }
        out
    }

    #[tokio::test]
    async fn streams_text_then_ends_turn() {
        let events = collect(Fake(vec![Ok(Delta::Text("he".into())), Ok(Delta::Text("llo".into()))]), &["hi"]).await;
        assert!(matches!(&events[0], AppEvent::Connected(info) if info.transport == "native · fake"));
        let text: String = events
            .iter()
            .filter_map(|e| if let AppEvent::AgentText(t) = e { Some(t.as_str()) } else { None })
            .collect();
        assert_eq!(text, "hello");
        assert!(events.iter().any(|e| matches!(e, AppEvent::TurnComplete(StopReason::EndTurn))));
        assert!(matches!(events.last(), Some(AppEvent::Closed)));
    }

    #[tokio::test]
    async fn stream_error_fails_the_turn_and_session_survives() {
        let events = collect(Fake(vec![Err("boom".into())]), &["a", "b"]).await;
        let failures = events.iter().filter(|e| matches!(e, AppEvent::TurnFailed(m) if m == "boom")).count();
        assert_eq!(failures, 2);
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
