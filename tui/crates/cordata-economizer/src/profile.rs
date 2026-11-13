//! Per-provider limits. The model name decides the provider the way genai does.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Models whose name says they are small (a size tag of 14b or less, e.g. `llama3.1:8b`): prune hardest.
    Ollama,
    Anthropic,
    Gemini,
    /// Everything else (hosted providers, and local models whose name does not say they are small): the standard limits.
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
        } else if size_in_billions(name).is_some_and(|b| b <= 14.0) {
            Self { kind: ProviderKind::Ollama, max_tools: 4, output_chars: 3_000, age_after_turns: 1, age_reads_only: false }
        } else {
            Self { kind: ProviderKind::Other, ..Self::for_model("claude") }
        }
    }
}

/// The parameter count a model tag states, like `8b` in `llama3.1:8b`, `1.5b` in `qwen2.5-coder-1.5b` or `360m`.
fn size_in_billions(name: &str) -> Option<f64> {
    // A `b` tag wins over an `m` one: `qwen2.5-1m:32b` is a 32b model with a one-million-token context.
    let parts = || name.split([':', '-', '_']);
    parts().find_map(|p| p.strip_suffix('b')?.parse::<f64>().ok())
        .or_else(|| parts().find_map(|p| p.strip_suffix('m')?.parse::<f64>().ok().map(|m| m / 1000.0)))
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
        assert_eq!(Profile::for_model("qwen2.5-coder-1.5b").kind, ProviderKind::Ollama);
        assert_eq!(Profile::for_model("smollm2:360m").kind, ProviderKind::Ollama);
    }

    #[test]
    fn unknown_or_large_models_get_the_standard_profile() {
        for model in ["qwen2.5-1m:32b", "llama3", "qwen3-coder", "deepseek-v4-pro:0813", "gpt-oss:120b", "qwen3:32b", "some-new-model"] {
            let p = Profile::for_model(model);
            assert_eq!((p.kind, p.max_tools, p.output_chars), (ProviderKind::Other, 8, 6_000), "{model}");
        }
    }
}
