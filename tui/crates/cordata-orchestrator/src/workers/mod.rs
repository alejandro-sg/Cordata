//! The subprocess workers: Claude Code (`claude -p`) and Antigravity (`agy`) behind
//! one engine ([`cli::CliWorker`]) that spawns, confines, streams, meters and stops the CLI.
//!
//! Not ported: the Ollama worker. It is not a subprocess — it is an in-process model loop with its own tool set — and
//! is a natural client of `acp-core`'s native engine rather than another CLI wrapper.

pub mod agy;
pub mod claude;
pub mod cli;
pub mod prompting;
pub mod tree;
pub mod vault;

/// The vendor behind a model, for the harden role's "different family than the executor" rule.
pub fn model_family_of(family: &str, model: Option<&str>) -> String {
    match family {
        "claude_code" => "anthropic".into(),
        "antigravity" => agy::model_family(model).into(),
        _ => family.into(),
    }
}
