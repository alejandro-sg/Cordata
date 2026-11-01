//! The seam between the chat loop and an LLM provider. The loop only sees `Provider`, so tests
//! drive it with a scripted fake and never touch the network.

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart, MessageContent, Tool, ToolCall, ToolResponse};
use genai::resolver::{AuthData, AuthResolver};
use genai::{Client, ModelIden};
use serde_json::Value;

use super::keys;
use super::tools::ToolSpec;

/// A tool call the model asked for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Call {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) args: Value,
}

/// One streamed piece of a reply.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Delta {
    Text(String),
    Thought(String),
    Call(Call),
}

/// One entry of the conversation handed to the provider.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Msg {
    User(String),
    Assistant { text: String, calls: Vec<Call> },
    ToolResult { id: String, output: String },
}

pub(crate) trait Provider: Send + Sync {
    /// The model name shown in the banner.
    fn model(&self) -> &str;

    /// Start a streamed reply to `history`.
    fn stream<'a>(
        &'a self,
        system: &'a str,
        history: Vec<Msg>,
        tools: &'a [ToolSpec],
    ) -> BoxFuture<'a, Result<BoxStream<'a, Result<Delta, String>>, String>>;
}

pub(crate) struct GenaiProvider {
    client: Client,
    model: String,
}

impl GenaiProvider {
    pub(crate) fn new(model: impl Into<String>) -> Result<Self, String> {
        let resolver = AuthResolver::from_resolver_fn(|iden: ModelIden| {
            let provider = iden.adapter_kind.as_lower_str();
            // `None` falls through to genai's own default (its env var, or no auth for local Ollama).
            Ok(keys::resolve(provider).map(AuthData::from_single))
        });
        let client = Client::builder().with_auth_resolver(resolver).build().map_err(|e| e.to_string())?;
        Ok(Self { client, model: model.into() })
    }
}

fn to_message(msg: &Msg) -> ChatMessage {
    match msg {
        Msg::User(text) => ChatMessage::user(text.as_str()),
        Msg::Assistant { text, calls } if calls.is_empty() => ChatMessage::assistant(text.as_str()),
        Msg::Assistant { text, calls } => {
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(ContentPart::Text(text.clone()));
            }
            parts.extend(calls.iter().map(|c| {
                ContentPart::ToolCall(ToolCall { call_id: c.id.clone(), fn_name: c.name.clone(), fn_arguments: c.args.clone(), thought_signatures: None })
            }));
            ChatMessage::assistant(MessageContent::from_parts(parts))
        }
        Msg::ToolResult { id, output } => ChatMessage::from(ToolResponse::new(id.as_str(), output.as_str())),
    }
}

impl Provider for GenaiProvider {
    fn model(&self) -> &str {
        &self.model
    }

    fn stream<'a>(
        &'a self,
        system: &'a str,
        history: Vec<Msg>,
        tools: &'a [ToolSpec],
    ) -> BoxFuture<'a, Result<BoxStream<'a, Result<Delta, String>>, String>> {
        Box::pin(async move {
            let genai_tools: Vec<Tool> =
                tools.iter().map(|t| Tool::new(t.name).with_description(t.description).with_schema(t.schema.clone())).collect();
            let request = ChatRequest::new(history.iter().map(to_message).collect()).with_system(system).with_tools(genai_tools);
            let options = ChatOptions::default().with_capture_tool_calls(true);
            let response = self.client.exec_chat_stream(&self.model, request, Some(&options)).await.map_err(|e| e.to_string())?;
            let stream = response.stream.flat_map(|event| {
                let items: Vec<Result<Delta, String>> = match event {
                    Ok(ChatStreamEvent::Chunk(c)) => vec![Ok(Delta::Text(c.content))],
                    Ok(ChatStreamEvent::ReasoningChunk(c)) => vec![Ok(Delta::Thought(c.content))],
                    // Tool calls arrive whole at the end, never as partial argument fragments.
                    Ok(ChatStreamEvent::End(end)) => end
                        .captured_content
                        .map(|content| content.into_tool_calls())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| Ok(Delta::Call(Call { id: c.call_id, name: c.fn_name, args: c.fn_arguments })))
                        .collect(),
                    Ok(_) => Vec::new(),
                    Err(e) => vec![Err(e.to_string())],
                };
                stream::iter(items)
            });
            Ok(stream.boxed())
        })
    }
}
