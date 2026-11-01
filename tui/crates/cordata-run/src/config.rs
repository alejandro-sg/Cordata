//! The default worker roster and the settings that shape a run (`backend/orchestrator/config.py`).
//!
//! The roster is every (worker, model) pair the dispatcher may choose from, each with its tier and a relative price
//! (strong = 1). A family is in the roster when `orchestrator.workers` names it and it is usable here (its CLI is
//! installed). The Ollama family is listed so settings that name it still parse, but it has no native worker yet.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use cordata_doctor::Settings;
use cordata_orchestrator::dispatch::{Candidate, Spec};
use cordata_orchestrator::machine::RunConfig;
use cordata_orchestrator::worker::Worker;
use cordata_orchestrator::workers::vault::{Egress, JsonPrefs, VaultEgress};
use cordata_orchestrator::workers::{agy, claude, model_family_of};
use serde_json::Value;

pub const DEFAULT_WORKERS: [&str; 3] = ["claude_code", "antigravity", "ollama"];
pub const KNOWN_FAMILIES: [&str; 3] = ["claude_code", "antigravity", "ollama"];
/// Families this binary can run.
pub const NATIVE_FAMILIES: [&str; 2] = ["claude_code", "antigravity"];

type Row = (&'static str, &'static str, &'static str, f64, Option<&'static [&'static str]>);

/// family, model, tier, price, roles (`None` = every role)
pub const ROSTER: [Row; 12] = [
    ("claude_code", "haiku", "fast", 0.10, None),
    ("claude_code", "sonnet", "standard", 0.35, None),
    ("claude_code", "opus", "strong", 1.00, None),
    ("antigravity", "gemini-3.8-flash-medium", "fast", 0.05, None),
    ("antigravity", "claude-sonnet-5-5-medium", "standard", 0.30, None),
    ("antigravity", "gemini-3.1-pro-high", "strong", 0.60, None),
    ("ollama", "deepseek-v4.1-flash", "fast", 0.02, Some(&["execute", "harden", "audit"])),
    ("ollama", "glm-5.3-flash", "fast", 0.03, Some(&["plan", "execute", "audit"])),
    ("ollama", "kimi-k2.7-code", "standard", 0.05, None),
    ("ollama", "minimax-m3.1-flash", "fast", 0.02, Some(&["execute", "audit"])),
    ("ollama", "gemma4:31b", "standard", 0.04, Some(&["plan", "audit"])),
    ("ollama", "mistral-large-4", "strong", 0.10, None),
];

pub fn setting<'a>(settings: &'a Settings, key: &str) -> Option<&'a Value> {
    settings.get(key).filter(|v| !v.is_null())
}

pub fn setting_f64(settings: &Settings, key: &str, default: f64) -> f64 {
    setting(settings, key).and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(default)
}

pub fn setting_bool(settings: &Settings, key: &str, default: bool) -> bool {
    setting(settings, key).and_then(Value::as_bool).unwrap_or(default)
}

/// `orchestrator.workers` (comma-separated), restricted to known families.
pub fn enabled_families(settings: &Settings) -> Vec<String> {
    let raw = setting(settings, "orchestrator.workers").and_then(Value::as_str).map_or_else(|| DEFAULT_WORKERS.join(","), str::to_owned);
    raw.split(',').map(str::trim).filter(|f| KNOWN_FAMILIES.contains(f)).map(str::to_owned).collect()
}

/// Is the family's CLI installed (`$CORDATA_*_BIN`, on `PATH`, or `~/.local/bin`)?
pub fn family_available(family: &str, environ: &HashMap<String, String>) -> bool {
    let (env_var, bin) = match family {
        "claude_code" => ("CORDATA_CLAUDE_BIN", "claude"),
        "antigravity" => ("CORDATA_AGY_BIN", "agy"),
        _ => return false,
    };
    // A `$CORDATA_*_BIN` override counts only when it names a file that exists: a stale one must not hide the fallback.
    environ.get(env_var).is_some_and(|v| !v.is_empty() && Path::new(v).exists())
        || environ.get("PATH").is_some_and(|p| p.split(':').any(|d| Path::new(d).join(bin).is_file()))
        || environ.get("HOME").is_some_and(|h| Path::new(h).join(".local/bin").join(bin).exists())
}

pub fn run_config(settings: &Settings) -> RunConfig {
    let max_tokens = setting_f64(settings, "orchestrator.maxTokens", 0.0);
    RunConfig {
        max_usd: setting_f64(settings, "orchestrator.maxUsd", 2.0),
        max_tokens: (max_tokens > 0.0).then_some(max_tokens as i64),
        harden: setting_bool(settings, "orchestrator.harden", true),
        tier2: setting_bool(settings, "orchestrator.tier2", true),
        research: true, // a code default, not a setting: `run --no-research` turns it off for one run
        ..RunConfig::default()
    }
}

pub fn pools(settings: &Settings) -> Vec<cordata_ledger::Pool> {
    cordata_ledger::pools_from_settings(setting(settings, "orchestrator.pools"))
}

/// The specs for `families` that this binary can run, in roster order.
pub fn specs(families: &[String]) -> Vec<Spec> {
    ROSTER
        .iter()
        .filter(|(family, ..)| families.iter().any(|f| f == family) && NATIVE_FAMILIES.contains(family))
        .map(|(family, model, tier, price, roles)| {
            let candidate = Candidate {
                worker_id: (*family).into(),
                family: (*family).into(),
                model: Some((*model).into()),
                effort: None,
                model_family: model_family_of(family, Some(model)),
            };
            let mut spec = Spec::new(candidate, tier);
            spec.price = Some(*price);
            if let Some(roles) = roles {
                spec = spec.roles(roles);
            }
            spec
        })
        .collect()
}

/// The ready workers by family id, sharing one egress policy built from the settings.
pub fn workers(settings: &Settings, environ: &HashMap<String, String>) -> HashMap<String, Arc<dyn Worker>> {
    let egress: Arc<dyn Egress> = Arc::new(VaultEgress::new(JsonPrefs(settings.merged().clone()), environ.clone()));
    let mut out: HashMap<String, Arc<dyn Worker>> = HashMap::new();
    out.insert("claude_code".into(), Arc::new(claude::worker(egress.clone()).environ(environ.clone())));
    out.insert("antigravity".into(), Arc::new(agy::worker(egress).environ(environ.clone())));
    out
}

/// Which of the named families are dropped and why (for a warning).
pub fn unsupported(families: &[String]) -> Vec<String> {
    let native: HashSet<&str> = NATIVE_FAMILIES.into_iter().collect();
    families.iter().filter(|f| !native.contains(f.as_str())).cloned().collect()
}
