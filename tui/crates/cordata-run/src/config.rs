//! The default worker roster and the settings that shape a run.
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
pub const NATIVE_FAMILIES: [&str; 3] = ["claude_code", "antigravity", "ollama"];

type Row = (&'static str, &'static str, Option<&'static str>, &'static str, f64, Option<&'static [&'static str]>);

/// family, model, effort, tier, price, roles (`None` = every role). Checked 2026-11-03 against the providers' docs:
/// Anthropic recommends one strong model tuned by effort over splitting work across models, so Claude Code is listed
/// at two efforts per model and the bandit learns which earns its price per role. Claude Code's own default is
/// `medium`; Haiku at `low` skips checks on long agent prompts, so it runs at `medium`. agy bakes effort into the
/// model id (`agy models`), and ids the installed CLI does not list are dropped at run time (`live_specs`). Ollama
/// Cloud ids are the bare API names from `ollama.com/api/tags`.
pub const ROSTER: [Row; 19] = [
    ("claude_code", "haiku", Some("medium"), "fast", 0.10, None),
    ("claude_code", "sonnet", Some("medium"), "standard", 0.35, None),
    ("claude_code", "sonnet", Some("high"), "standard", 0.45, None),
    ("claude_code", "opus", Some("medium"), "strong", 1.00, None),
    ("claude_code", "opus", Some("high"), "strong", 1.30, None),
    ("claude_code", "fable", Some("high"), "strong", 2.00, Some(&["plan", "research", "conformance"])),
    ("antigravity", "gemini-3.8-flash-medium", None, "fast", 0.05, None),
    ("antigravity", "claude-sonnet-5-5-medium", None, "standard", 0.30, None),
    ("antigravity", "gpt-oss-120b-medium", None, "standard", 0.10, Some(&["plan", "audit"])),
    ("antigravity", "claude-opus-5-5-high", None, "strong", 0.70, None),
    ("antigravity", "gemini-4-argon-high", None, "strong", 0.60, None),
    ("ollama", "deepseek-v4.1-flash", None, "fast", 0.02, Some(&["execute", "harden", "audit"])),
    ("ollama", "glm-5.3-flash", None, "fast", 0.03, Some(&["plan", "execute", "audit"])),
    ("ollama", "minimax-m3.1-flash", None, "fast", 0.02, Some(&["execute", "audit"])),
    ("ollama", "kimi-k3", None, "standard", 0.05, None),
    ("ollama", "gpt-oss:120b", Some("medium"), "standard", 0.04, Some(&["plan", "audit"])),
    ("ollama", "glm-5.3", None, "strong", 0.08, None),
    ("ollama", "kimi-k2.7-code", None, "strong", 0.08, Some(&["execute", "harden"])),
    ("ollama", "deepseek-v4-pro:0813", None, "strong", 0.10, None),
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
        // Ollama Cloud runs in Cordata's own engine: it needs a key, not a CLI. The Keychain is asked for the
        // account in `environ` (none, as in the hermetic tests, means no Keychain).
        "ollama" => {
            return environ.get("OLLAMA_API_KEY").is_some_and(|k| !k.trim().is_empty())
                || (environ.get("USER").is_some_and(|u| !u.is_empty()) && acp_core::native::key_present("ollama_cloud"));
        }
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
        step_check: setting_bool(settings, "orchestrator.stepCheck", true),
        screen: setting_bool(settings, "orchestrator.screen", true),
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
        .map(|(family, model, effort, tier, price, roles)| {
            let candidate = Candidate {
                worker_id: (*family).into(),
                family: (*family).into(),
                model: Some((*model).into()),
                effort: effort.map(str::to_owned),
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

/// `orchestrator.models`: a list of model ids. A family with at least one model named there runs only the named
/// ones; a family with none named keeps its whole roster. This is how a person keeps a run on the one model of a
/// family that still has quota.
pub fn only_models(specs: Vec<Spec>, settings: &Settings) -> Vec<Spec> {
    let named: Vec<&str> = setting(settings, "orchestrator.models").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    narrow_models(specs, &named)
}

fn narrow_models(specs: Vec<Spec>, named: &[&str]) -> Vec<Spec> {
    let is_named = |s: &Spec| s.candidate.model.as_deref().is_some_and(|m| named.contains(&m));
    let narrowed: HashSet<String> = specs.iter().filter(|s| is_named(s)).map(|s| s.candidate.family.clone()).collect();
    specs.into_iter().filter(|s| !narrowed.contains(&s.candidate.family) || is_named(s)).collect()
}

/// The ready workers by family id, sharing one egress policy built from the settings.
pub fn workers(settings: &Settings, environ: &HashMap<String, String>) -> HashMap<String, Arc<dyn Worker>> {
    let egress: Arc<dyn Egress> = Arc::new(VaultEgress::new(JsonPrefs(settings.merged().clone()), environ.clone()));
    let mut out: HashMap<String, Arc<dyn Worker>> = HashMap::new();
    out.insert("claude_code".into(), Arc::new(claude::worker(egress.clone()).environ(environ.clone())));
    out.insert("antigravity".into(), Arc::new(agy::worker(egress).environ(environ.clone())));
    out.insert("ollama".into(), Arc::new(crate::native_worker::NativeWorker::ollama_cloud()));
    out
}

/// The coding CLIs' egress, checked once before a run spends anything: `Err` is the refusal every worker would hit.
pub fn egress_preflight(settings: &Settings, environ: &HashMap<String, String>) -> Result<(), String> {
    VaultEgress::new(JsonPrefs(settings.merged().clone()), environ.clone()).env().map(|_| ())
}

/// `specs` without the agy models the installed CLI does not offer (`agy models`), and a note naming them. When the
/// CLI cannot be asked, nothing is dropped: a wrong guess fails one step, a missing worker fails the run.
pub fn live_specs(specs: Vec<Spec>, environ: &HashMap<String, String>) -> (Vec<Spec>, Option<String>) {
    if !specs.iter().any(|s| s.candidate.family == "antigravity") || !family_available("antigravity", environ) {
        return (specs, None);
    }
    let Some(offered) = agy::live_models(environ) else { return (specs, None) };
    let (kept, dropped): (Vec<Spec>, Vec<Spec>) = specs.into_iter().partition(|s| s.candidate.family != "antigravity" || s.candidate.model.as_ref().is_some_and(|m| offered.contains(m)));
    let note = (!dropped.is_empty()).then(|| format!("agy does not offer {}; left out of this run", dropped.iter().filter_map(|s| s.candidate.model.clone()).collect::<Vec<_>>().join(", ")));
    (kept, note)
}

/// Which of the named families are dropped and why (for a warning).
pub fn unsupported(families: &[String]) -> Vec<String> {
    let native: HashSet<&str> = NATIVE_FAMILIES.into_iter().collect();
    families.iter().filter(|f| !native.contains(f.as_str())).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naming_a_model_narrows_only_its_own_family() {
        let all = specs(&["antigravity".to_owned(), "ollama".to_owned()]);
        let ollama = all.iter().filter(|s| s.candidate.family == "ollama").count();
        let kept = narrow_models(all.clone(), &["gemini-3.8-flash-medium", "no-such-model"]);
        let agy: Vec<_> = kept.iter().filter(|s| s.candidate.family == "antigravity").filter_map(|s| s.candidate.model.clone()).collect();
        assert_eq!(agy, ["gemini-3.8-flash-medium"]);
        assert_eq!(kept.iter().filter(|s| s.candidate.family == "ollama").count(), ollama, "a family with no model named keeps its roster");
        assert_eq!(narrow_models(all.clone(), &[]).len(), all.len());
    }
}
