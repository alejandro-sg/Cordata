//! The in-process agent: a streaming chat loop over `genai` (Anthropic, Gemini, OpenAI, Ollama), its tools, the
//! permission gate and sandboxed commands. Front ends drive it through `SessionHandle` and read `AppEvent`s.

mod approval;
mod engine;
mod keys;
mod provider;
mod sandbox;
mod tools;

pub(crate) use engine::run;
pub(crate) use provider::Msg;
#[cfg(test)]
pub(crate) use provider::Call;
