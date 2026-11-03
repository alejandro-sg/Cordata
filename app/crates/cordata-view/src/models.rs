//! The models the chat engine can answer with, as the model switcher offers them: the genai name acp-core takes
//! (`SessionHandle::set_model`, `CORDATA_NATIVE_MODEL`), who serves it, and its context window for the context gauge.
//! Any other genai name works too (typed into the switcher); its window is then unknown and the gauge shows tokens only.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    pub id: &'static str,
    pub label: &'static str,
    /// The provider key it needs (`acp_core::native::keys`' names): `anthropic`, `openai`, `gemini`, `ollama_cloud`.
    pub provider: &'static str,
    /// Context window in tokens.
    pub window: u64,
    /// One word on what it is for.
    pub note: &'static str,
}

pub const CATALOG: &[Model] = &[
    Model { id: "claude-opus-5-5", label: "Claude Opus 5.5", provider: "anthropic", window: 200_000, note: "deepest" },
    Model { id: "claude-sonnet-5-5", label: "Claude Sonnet 5.5", provider: "anthropic", window: 200_000, note: "balanced" },
    Model { id: "claude-haiku-5-5", label: "Claude Haiku 5.5", provider: "anthropic", window: 200_000, note: "fastest" },
    Model { id: "gpt-5", label: "GPT-5", provider: "openai", window: 400_000, note: "openai" },
    Model { id: "gpt-5-mini", label: "GPT-5 mini", provider: "openai", window: 400_000, note: "cheap" },
    Model { id: "gemini-2.5-pro", label: "Gemini 2.5 Pro", provider: "gemini", window: 1_048_576, note: "long context" },
    Model { id: "gemini-2.5-flash", label: "Gemini 2.5 Flash", provider: "gemini", window: 1_048_576, note: "fast" },
    Model { id: "ollama_cloud::qwen3-coder:480b", label: "Qwen3 Coder 480B", provider: "ollama_cloud", window: 262_144, note: "best coding" },
    Model { id: "ollama_cloud::kimi-k3", label: "Kimi K3", provider: "ollama_cloud", window: 262_144, note: "balanced" },
    Model { id: "ollama_cloud::deepseek-v4.1-flash", label: "DeepSeek V4.1 Flash", provider: "ollama_cloud", window: 160_000, note: "cost-effective" },
    Model { id: "ollama_cloud::glm-5.3-flash", label: "GLM 5.3 Flash", provider: "ollama_cloud", window: 131_072, note: "fastest" },
];

pub fn find(id: &str) -> Option<&'static Model> {
    CATALOG.iter().find(|m| m.id == id)
}

/// The context window for a model name, from the catalog or its family; `None` when not known (a local Ollama tag).
pub fn window(model: &str) -> Option<u64> {
    if let Some(m) = find(model) {
        return Some(m.window);
    }
    let name = model.rsplit_once("::").map_or(model, |(_, m)| m);
    match name {
        n if n.starts_with("claude") => Some(200_000),
        n if n.starts_with("gpt-5") || n.starts_with("gpt-4.1") => Some(400_000),
        n if n.starts_with("gpt-4o") || n.starts_with("o3") || n.starts_with("o4") => Some(128_000),
        n if n.starts_with("gemini") => Some(1_048_576),
        n if n.starts_with("kimi") || n.starts_with("qwen3-coder") => Some(262_144),
        n if n.starts_with("deepseek") => Some(160_000),
        n if n.starts_with("glm") || n.starts_with("minimax") => Some(131_072),
        _ => None,
    }
}

/// Which provider key a model name needs (`None`: a local Ollama tag, no key).
pub fn provider(model: &str) -> Option<&'static str> {
    if let Some(m) = find(model) {
        return Some(m.provider);
    }
    match model {
        m if m.starts_with("ollama_cloud::") => Some("ollama_cloud"),
        m if m.starts_with("claude") => Some("anthropic"),
        m if m.starts_with("gpt") || m.starts_with("o3") || m.starts_with("o4") => Some("openai"),
        m if m.starts_with("gemini") => Some("gemini"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_come_from_the_catalog_then_the_family() {
        assert_eq!(window("claude-sonnet-5-5"), Some(200_000));
        assert_eq!(window("claude-3-7-sonnet-latest"), Some(200_000));
        assert_eq!(window("ollama_cloud::kimi-k3"), Some(262_144));
        assert_eq!(window("ollama_cloud::qwen3-coder:480b"), Some(262_144));
        assert_eq!(window("ollama_cloud::deepseek-v4.1-flash"), Some(160_000));
        assert_eq!(window("ollama_cloud::glm-5.3-flash"), Some(131_072));
        assert_eq!(window("llama3.2:3b"), None, "a local tag: unknown");
        assert_eq!((provider("gpt-5"), provider("llama3.2:3b")), (Some("openai"), None));
        assert_eq!(provider("ollama_cloud::deepseek-v4.1-flash"), Some("ollama_cloud"));
    }
}
