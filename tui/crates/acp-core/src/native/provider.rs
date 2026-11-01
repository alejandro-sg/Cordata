//! The seam between the chat loop and an LLM provider. The loop only sees `Provider`, so tests
//! drive it with a scripted fake and never touch the network.

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use genai::chat::{ChatMessage, ChatRequest, ChatStreamEvent};
use genai::resolver::{AuthData, AuthResolver};
use genai::{Client, ModelIden};

use super::keys;

/// One streamed piece of a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delta {
    Text(String),
    Thought(String),
}

/// A conversation turn handed to the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Turn {
    pub(crate) role: Role,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

pub(crate) trait Provider: Send + Sync {
    /// The model name shown in the banner.
    fn model(&self) -> &str;

    /// Start a streamed reply to `history` (ending in the user's latest message).
    fn stream<'a>(
        &'a self,
        system: &'a str,
        history: &'a [Turn],
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

impl Provider for GenaiProvider {
    fn model(&self) -> &str {
        &self.model
    }

    fn stream<'a>(
        &'a self,
        system: &'a str,
        history: &'a [Turn],
    ) -> BoxFuture<'a, Result<BoxStream<'a, Result<Delta, String>>, String>> {
        Box::pin(async move {
            let messages = history
                .iter()
                .map(|turn| match turn.role {
                    Role::User => ChatMessage::user(turn.text.as_str()),
                    Role::Assistant => ChatMessage::assistant(turn.text.as_str()),
                })
                .collect();
            let request = ChatRequest::new(messages).with_system(system);
            let response = self
                .client
                .exec_chat_stream(&self.model, request, None)
                .await
                .map_err(|e| e.to_string())?;
            let stream = response.stream.filter_map(|event| async move {
                match event {
                    Ok(ChatStreamEvent::Chunk(chunk)) => Some(Ok(Delta::Text(chunk.content))),
                    Ok(ChatStreamEvent::ReasoningChunk(chunk)) => Some(Ok(Delta::Thought(chunk.content))),
                    Ok(_) => None,
                    Err(e) => Some(Err(e.to_string())),
                }
            });
            Ok(stream.boxed())
        })
    }
}
