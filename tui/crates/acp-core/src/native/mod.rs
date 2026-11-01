//! `NativeSession`: an in-process agent, no Python child. Selected with `BackendTransport::Native`
//! (`acp-tui --transport native`); it speaks the same `Command` / `AppEvent` contract as the ACP
//! connection, so the UI cannot tell the difference.
//!
//! Prototype scope: a streaming chat loop over `genai` (Anthropic, Gemini, OpenAI, Ollama).
//! Tools, the permission gate and sandboxed commands are the next step (migration plan, Phase 2);
//! until then a native session answers but does not act.

mod approval;
mod engine;
mod keys;
mod provider;
mod sandbox;
mod tools;

pub(crate) use engine::run;
