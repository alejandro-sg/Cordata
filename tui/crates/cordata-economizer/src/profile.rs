//! Per-provider limits. The model name decides the provider the way genai does.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Local models with small contexts: prune hardest.
    Ollama,
    Anthropic,
    Gemini,
    /// Any other hosted provider (OpenAI and friends): the standard limits.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub kind: ProviderKind,
    /// Most tool schemas sent per request (the core tools always count, even above this).
    pub max_tools: usize,
    /// Tool output over this many characters is compacted.
    pub output_chars: usize,
    /// Tool results older than this many turns are replaced by a one-line digest.
    pub age_after_turns: usize,
    /// Only age raw reads and searches; keep command and edit results verbatim.
    pub age_reads_only: bool,
}

impl Profile {
    pub fn for_model(model: &str) -> Self {
        let model = model.to_ascii_lowercase();
        let bare = model.rsplit_once("::").map_or(&model[..], |(_, m)| m);
        let name = bare.rsplit('/').next().unwrap_or(bare);
        if model.starts_with("ollama_cloud::") {
            Self { kind: ProviderKind::Other, ..Self::for_model("claude") }
        } else if name.starts_with("claude") {
            Self { kind: ProviderKind::Anthropic, max_tools: 8, output_chars: 6_000, age_after_turns: 3, age_reads_only: false }
        } else if name.starts_with("gemini") {
            Self { kind: ProviderKind::Gemini, max_tools: 6, output_chars: 12_000, age_after_turns: 3, age_reads_only: true }
        } else if name.starts_with("gpt") || name.starts_with("o1") || name.starts_with("o3") || name.starts_with("o4") {
            Self { kind: ProviderKind::Other, ..Self::for_model("claude") }
        } else {
            Self { kind: ProviderKind::Ollama, max_tools: 4, output_chars: 3_000, age_after_turns: 1, age_reads_only: false }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_pick_the_provider() {
        assert_eq!(Profile::for_model("claude-sonnet-5-5").kind, ProviderKind::Anthropic);
        assert_eq!(Profile::for_model("gemini-3-pro").kind, ProviderKind::Gemini);
        assert_eq!(Profile::for_model("gpt-5").kind, ProviderKind::Other);
        assert_eq!(Profile::for_model("ollama_cloud::qwen3-coder:480b").kind, ProviderKind::Other);
        assert_eq!(Profile::for_model("ollama_cloud::qwen3-coder:480b").max_tools, 8);
        assert_eq!(Profile::for_model("llama3.1:8b").kind, ProviderKind::Ollama);
        assert_eq!(Profile::for_model("llama3.1:8b").max_tools, 4);
    }
}
