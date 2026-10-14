//! A session's cost, estimated in the window and nowhere else. The backend deliberately reports tokens and no money
//! (only the orchestrator's pools have a price), so the price per model is the app's: Anthropic's first-party list
//! prices, matched by model name, with `[prices]` in prefs.toml to override or add a model. A model without a price —
//! a local Ollama or oMLX model, a subscription pool — shows no cost rather than a guess.

use std::collections::BTreeMap;

/// Dollars per million tokens, (input, output). Most specific first: `opus-5` would otherwise match `opus-5-5`.
/// Anthropic list prices as of October 2026 (Haiku 5.5's is the up-to-100K-prompt rate).
const LIST: [(&str, f64, f64); 13] = [
    ("fable-5-1", 10.0, 50.0),
    ("mythos-5-1", 10.0, 50.0),
    ("fable-5", 10.0, 50.0),
    ("opus-5-5", 4.0, 20.0),
    ("opus-5", 5.0, 25.0),
    ("opus-4-8", 5.0, 25.0),
    ("opus-4-7", 5.0, 25.0),
    ("opus-4-6", 5.0, 25.0),
    ("sonnet-5-5", 2.0, 10.0),
    ("sonnet-5", 2.0, 10.0),
    ("sonnet-4-6", 3.0, 15.0),
    ("haiku-5-5", 0.10, 0.50),
    ("haiku-4-5", 1.0, 5.0),
];

/// `claude-opus-5-5`, `us.anthropic.claude-opus-5-5`, `Claude Opus 5.5` all read `claude-opus-5-5`.
fn normalised(model: &str) -> String {
    model.trim().to_ascii_lowercase().replace(['.', ' ', '_'], "-")
}

/// The price of `model`: an override from prefs first (its key matched the same way), then the list. `None` when
/// nothing matches — which is the answer for every non-Claude model.
pub fn price(model: &str, overrides: &BTreeMap<String, [f64; 2]>) -> Option<(f64, f64)> {
    let name = normalised(model);
    if name.is_empty() {
        return None;
    }
    let mut custom: Vec<(String, [f64; 2])> = overrides.iter().map(|(k, v)| (normalised(k), *v)).filter(|(k, _)| !k.is_empty()).collect();
    custom.sort_by_key(|(k, _)| std::cmp::Reverse(k.len())); // the longest key is the most specific
    if let Some((_, [input, output])) = custom.iter().find(|(k, _)| name.contains(k.as_str())) {
        return Some((*input, *output));
    }
    let claude = name.contains("claude") || ["fable", "mythos", "opus", "sonnet", "haiku"].iter().any(|f| name.starts_with(f));
    claude.then(|| LIST.iter().find(|(key, _, _)| name.contains(key)).map(|(_, i, o)| (*i, *o))).flatten()
}

/// What `tokens_in` and `tokens_out` cost at `model`'s price.
pub fn estimate(model: &str, tokens_in: u64, tokens_out: u64, overrides: &BTreeMap<String, [f64; 2]>) -> Option<f64> {
    let (input, output) = price(model, overrides)?;
    Some((tokens_in as f64 * input + tokens_out as f64 * output) / 1_000_000.0)
}

/// `$0.00`, `$0.042`, `$1.27`, `$134`: cents once it is worth cents, three places below a cent.
pub fn dollars(amount: f64) -> String {
    match amount {
        a if a <= 0.0 => "$0.00".into(),
        a if a < 0.01 => format!("${a:.3}"),
        a if a < 100.0 => format!("${a:.2}"),
        a => format!("${a:.0}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_models_are_priced_and_others_are_not() {
        let none = BTreeMap::new();
        assert_eq!(price("claude-opus-5-5", &none), Some((4.0, 20.0)));
        assert_eq!(price("claude-opus-5", &none), Some((5.0, 25.0)));
        assert_eq!(price("us.anthropic.claude-sonnet-5-5", &none), Some((2.0, 10.0)));
        assert_eq!(price("Claude Haiku 5.5", &none), Some((0.10, 0.50)));
        assert_eq!(price("claude-fable-5-1", &none), Some((10.0, 50.0)));
        assert_eq!(price("glm-4.6:cloud", &none), None);
        assert_eq!(price("qwen2.5-coder:7b", &none), None);
        assert_eq!(price("my-opus-5-5-finetune", &none), None, "only Claude names take Claude's price");
        assert_eq!(price("", &none), None);
    }

    #[test]
    fn overrides_win_and_add_models() {
        let mut overrides = BTreeMap::new();
        overrides.insert("claude-opus-5-5".into(), [1.0, 2.0]);
        overrides.insert("glm-4.6".into(), [0.6, 2.2]);
        overrides.insert("glm".into(), [9.0, 9.0]);
        assert_eq!(price("claude-opus-5-5", &overrides), Some((1.0, 2.0)));
        assert_eq!(price("glm-4.6:cloud", &overrides), Some((0.6, 2.2)), "the most specific key");
        assert_eq!(price("claude-sonnet-5-5", &overrides), Some((2.0, 10.0)));
    }

    #[test]
    fn estimates_read_like_money() {
        let none = BTreeMap::new();
        let cost = estimate("claude-opus-5-5", 250_000, 10_000, &none).unwrap();
        assert!((cost - 1.2).abs() < 1e-9);
        assert_eq!(dollars(cost), "$1.20");
        assert_eq!(dollars(0.0042), "$0.004");
        assert_eq!(dollars(0.0), "$0.00");
        assert_eq!(dollars(250.4), "$250");
        assert_eq!(estimate("llama3", 1, 1, &none), None);
    }
}
