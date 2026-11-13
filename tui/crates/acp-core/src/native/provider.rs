//! The seam between the chat loop and an LLM provider. The loop only sees `Provider`, so tests
//! drive it with a scripted fake and never touch the network.

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use genai::chat::{CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart, MessageContent, Tool, ToolCall, ToolResponse};
use genai::resolver::{AuthData, AuthResolver};
use genai::{Client, ModelIden};
use serde_json::Value;

use super::keys;
use super::tools::ToolSpec;

/// A tool call the model asked for.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Call {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) args: Value,
    /// The provider's opaque reasoning signatures, sent back with the call: Gemini refuses a tool round whose calls
    /// lost them. Only valid for the model that made them, so they are not saved with the session.
    #[serde(skip)]
    pub(crate) thought_signatures: Option<Vec<String>>,
}

/// One streamed piece of a reply.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Delta {
    Text(String),
    Thought(String),
    Call(Call),
    /// What the request cost, reported once at the end of the reply (when the provider says).
    Usage(crate::events::TokenUsage),
}

/// One entry of the conversation handed to the provider (and kept on disk, `crate::history`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub(crate) enum Msg {
    User { text: String },
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
    /// Local Ollama models go straight to Ollama, with an explicit context window (`super::ollama`).
    ollama: Option<super::ollama::OllamaChat>,
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
        let model = model.into();
        Ok(Self { ollama: super::ollama::OllamaChat::for_model(&model), client, model })
    }
}

fn to_message(msg: &Msg) -> ChatMessage {
    match msg {
        Msg::User { text } => ChatMessage::user(text.as_str()),
        Msg::Assistant { text, calls } if calls.is_empty() => ChatMessage::assistant(text.as_str()),
        Msg::Assistant { text, calls } => {
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(ContentPart::Text(text.clone()));
            }
            parts.extend(calls.iter().map(|c| {
                ContentPart::ToolCall(ToolCall { call_id: c.id.clone(), fn_name: c.name.clone(), fn_arguments: c.args.clone(), thought_signatures: c.thought_signatures.clone() })
            }));
            ChatMessage::assistant(MessageContent::from_parts(parts))
        }
        Msg::ToolResult { id, output } => ChatMessage::from(ToolResponse::new(id.as_str(), output.as_str())),
    }
}

/// The request's messages with two prompt-cache breakpoints, for the providers that need them marked (Anthropic):
/// after the system prompt, which caches tools and system for the session, and on the newest message, which caches
/// the conversation so far for the next round. Providers that cache on their own ignore the marks.
fn cached_messages(system: &str, history: &[Msg]) -> Vec<ChatMessage> {
    let mut messages = vec![ChatMessage::system(system).with_options(CacheControl::Ephemeral)];
    messages.extend(history.iter().map(to_message));
    if !history.is_empty()
        && let Some(last) = messages.pop()
    {
        messages.push(last.with_options(CacheControl::Ephemeral));
    }
    messages
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
            if let Some(ollama) = &self.ollama {
                return ollama.stream(ollama.body(&self.model, system, &history, tools)).await;
            }
            let genai_tools: Vec<Tool> =
                tools.iter().map(|t| Tool::new(t.name.as_ref()).with_description(t.description.as_ref()).with_schema(t.schema.clone())).collect();
            let request = ChatRequest::new(cached_messages(system, &history)).with_tools(genai_tools);
            let options = ChatOptions::default().with_capture_tool_calls(true).with_capture_usage(true);
            let response = self.client.exec_chat_stream(&self.model, request, Some(&options)).await.map_err(|e| e.to_string())?;
            let stream = response.stream.flat_map(|event| {
                let items: Vec<Result<Delta, String>> = match event {
                    Ok(ChatStreamEvent::Chunk(c)) => vec![Ok(Delta::Text(c.content))],
                    Ok(ChatStreamEvent::ReasoningChunk(c)) => vec![Ok(Delta::Thought(c.content))],
                    // Tool calls arrive whole at the end, never as partial argument fragments.
                    Ok(ChatStreamEvent::End(end)) => end
                        .captured_usage
                        .as_ref()
                        .map(|u| Ok(Delta::Usage(usage_of(u))))
                        .into_iter()
                        .chain(
                            end.captured_content
                                .map(|content| content.into_tool_calls())
                                .unwrap_or_default()
                                .into_iter()
                                .map(|c| Ok(Delta::Call(Call { id: c.call_id, name: c.fn_name, args: c.fn_arguments, thought_signatures: c.thought_signatures }))),
                        )
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

/// genai's usage in the engine's terms. `prompt_tokens` already counts cached input; Anthropic reports reads and writes apart.
fn usage_of(u: &genai::chat::Usage) -> crate::events::TokenUsage {
    let cached = u.prompt_tokens_details.as_ref().map_or(0, |d| d.cached_tokens.unwrap_or(0).max(0) as u64);
    crate::events::TokenUsage {
        input: u.prompt_tokens.unwrap_or(0).max(0) as u64,
        output: u.completion_tokens.unwrap_or(0).max(0) as u64,
        cached,
    }
}

#[cfg(test)]
mod tests {
    use genai::chat::ChatRole;
    use serde_json::json;

    use super::*;

    fn cache(message: &ChatMessage) -> Option<CacheControl> {
        message.options.as_ref().and_then(|o| o.cache_control.clone())
    }

    #[test]
    fn the_system_prompt_and_the_newest_message_are_cache_breakpoints() {
        let history = vec![Msg::User { text: "hi".into() }, Msg::Assistant { text: "yo".into(), calls: Vec::new() }, Msg::User { text: "more".into() }];
        let messages = cached_messages("sys", &history);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, ChatRole::System);
        assert!(matches!(cache(&messages[0]), Some(CacheControl::Ephemeral)));
        assert!(cache(&messages[1]).is_none() && cache(&messages[2]).is_none());
        assert!(matches!(cache(&messages[3]), Some(CacheControl::Ephemeral)));
        assert_eq!(cached_messages("sys", &[]).len(), 1);
    }

    #[test]
    fn thought_signatures_go_back_with_their_call_but_are_not_saved() {
        let call = Call { id: "c1".into(), name: "read_file".into(), args: json!({"path": "a"}), thought_signatures: Some(vec!["sig".into()]) };
        let message = to_message(&Msg::Assistant { text: String::new(), calls: vec![call.clone()] });
        let sent = message.content.tool_calls();
        assert_eq!(sent[0].thought_signatures.as_deref(), Some(&["sig".to_owned()][..]));
        let saved: Call = serde_json::from_str(&serde_json::to_string(&call).unwrap()).unwrap();
        assert_eq!(saved.thought_signatures, None);
    }
}
